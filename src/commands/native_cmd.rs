use anyhow::{bail, Result};
use console::style;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::commands::build;
use crate::config;

pub fn execute(docker: bool, out: Option<String>, platform: Option<String>, install: bool) -> Result<()> {
    if platform.is_some() && !docker {
        bail!("--platform requires --docker (GraalVM native-image does not support cross-compilation)");
    }

    let (config_path, cfg) = config::load_or_find_config()?;
    let project = config::project_dir(&config_path);

    if cfg.main.is_none() {
        bail!("Native compilation requires a main class. Add 'main' to package.toml.");
    }

    build::ensure_jdk_for_config(&cfg)?;

    if cfg.workspaces.is_some() {
        bail!("Native compilation is not supported for workspace root. Run in a specific module.");
    }

    // Build fat JAR: compile + copy resources + resolve deps + package
    let compile_jars = build::resolve_deps_with_scopes(&project, &cfg, &["compile", "provided"])?;
    build::compile_project(&project, &cfg, &compile_jars)?;

    // Copy resources so they end up in the release JAR
    let src_dir = config::source_dir(&project);
    let out_dir = config::output_classes_dir(&project);
    let custom_res_ext = cfg.compiler.as_ref().and_then(|c| c.resource_extensions.as_ref());
    crate::resources::copy_resources_with_extensions(&src_dir, &out_dir, custom_res_ext.map(|v| v.as_slice()))?;
    let resources_dir = project.join("src").join("main").join("resources");
    if resources_dir.exists() {
        crate::resources::copy_resources_with_extensions(&resources_dir, &out_dir, custom_res_ext.map(|v| v.as_slice()))?;
    }

    let runtime_jars = build::resolve_deps_with_scopes(&project, &cfg, &["compile", "runtime"])?;
    build::build_release_jar(&project, &cfg, &runtime_jars, None)?;

    let version = cfg.version.as_deref().unwrap_or("0.0.0");
    let jar_name = format!("{}-{}.jar", cfg.name, version);
    let release_dir = project.join("out").join("release");
    let jar_path = release_dir.join(&jar_name);

    if !jar_path.exists() {
        bail!("Release JAR not found: {}", jar_path.display());
    }

    let output_name = out.as_deref().unwrap_or(&cfg.name);
    let output_path = release_dir.join(output_name);

    let native_config = cfg.native.as_ref();
    let extra_args: Vec<String> = native_config
        .and_then(|n| n.args.clone())
        .unwrap_or_default();

    if docker {
        run_docker_native(&project, &jar_path, &output_path, &cfg, &extra_args, platform.as_deref())?;
    } else {
        run_local_native(&jar_path, &output_path, &extra_args)?;
    }

    // Print result
    let size = std::fs::metadata(&output_path)
        .map(|m| m.len())
        .unwrap_or(0);
    let size_mb = size as f64 / 1_048_576.0;
    println!(
        "\n  {} native binary: {} ({:.1} MB)",
        style("✓").green(),
        output_path.display(),
        size_mb
    );

    if install {
        install_binary(&output_path, output_name)?;
    }

    Ok(())
}

fn install_binary(binary_path: &Path, name: &str) -> Result<()> {
    let home = if cfg!(windows) {
        std::env::var("USERPROFILE").or_else(|_| std::env::var("HOME"))
    } else {
        std::env::var("HOME")
    }
    .map_err(|_| anyhow::anyhow!("Cannot determine home directory"))?;

    let bin_dir = PathBuf::from(&home).join(".ym").join("bin");
    std::fs::create_dir_all(&bin_dir)?;

    let dest_name = if cfg!(windows) && !name.ends_with(".exe") {
        format!("{}.exe", name)
    } else {
        name.to_string()
    };
    let dest = bin_dir.join(&dest_name);

    std::fs::copy(binary_path, &dest)?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&dest)?.permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&dest, perms)?;
    }

    println!(
        "  {} installed to {}",
        style("✓").green(),
        dest.display()
    );

    // Hint: check if ~/.ym/bin is in PATH
    if let Ok(path_var) = std::env::var("PATH") {
        let bin_str = bin_dir.display().to_string();
        if !path_var.split(if cfg!(windows) { ';' } else { ':' }).any(|p| p == bin_str) {
            println!(
                "\n  {} add to PATH: export PATH=\"{}:$PATH\"",
                style("hint").yellow(),
                bin_dir.display()
            );
        }
    }

    Ok(())
}

fn find_native_image() -> Option<PathBuf> {
    let cmd_name = if cfg!(windows) { "native-image.cmd" } else { "native-image" };

    // 1. $GRAALVM_HOME/bin/native-image
    if let Ok(graalvm) = std::env::var("GRAALVM_HOME") {
        let p = PathBuf::from(&graalvm).join("bin").join(cmd_name);
        if p.exists() {
            return Some(p);
        }
    }

    // 2. $JAVA_HOME/bin/native-image
    if let Ok(java_home) = std::env::var("JAVA_HOME") {
        let p = PathBuf::from(&java_home).join("bin").join(cmd_name);
        if p.exists() {
            return Some(p);
        }
    }

    // 3. PATH (which)
    if let Ok(output) = Command::new(if cfg!(windows) { "where" } else { "which" })
        .arg("native-image")
        .output()
    {
        if output.status.success() {
            let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !path.is_empty() {
                return Some(PathBuf::from(path.lines().next().unwrap_or(&path)));
            }
        }
    }

    // 4. ~/.ym/jdks/graalvm-*/bin/native-image
    if let Ok(home) = std::env::var("HOME") {
        let jdks_dir = PathBuf::from(home).join(".ym").join("jdks");
        if jdks_dir.exists() {
            if let Ok(entries) = std::fs::read_dir(&jdks_dir) {
                for entry in entries.flatten() {
                    let name = entry.file_name().to_string_lossy().to_string();
                    if name.starts_with("graalvm") {
                        let p = entry.path().join("bin").join(cmd_name);
                        if p.exists() {
                            return Some(p);
                        }
                    }
                }
            }
        }
    }

    None
}

fn run_local_native(jar_path: &Path, output_path: &Path, extra_args: &[String]) -> Result<()> {
    let native_image = find_native_image().ok_or_else(|| {
        anyhow::anyhow!(
            "GraalVM native-image not found.\n\n\
             1. Set GRAALVM_HOME environment variable\n\
             2. Or use: ymc native --docker\n\
             3. Or install GraalVM and ensure native-image is on PATH"
        )
    })?;

    println!(
        "  {} native-image: {}",
        style("➜").green(),
        native_image.display()
    );

    let mut cmd = Command::new(&native_image);
    cmd.arg("-jar").arg(jar_path)
        .arg("-o").arg(output_path);

    for arg in extra_args {
        cmd.arg(arg);
    }

    let status = cmd
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit())
        .status()?;

    if !status.success() {
        bail!("native-image failed with exit code {}", status.code().unwrap_or(-1));
    }

    Ok(())
}

fn run_docker_native(
    project: &Path,
    jar_path: &Path,
    output_path: &Path,
    cfg: &config::schema::YmConfig,
    extra_args: &[String],
    platform: Option<&str>,
) -> Result<()> {
    let target = cfg.target.as_deref().unwrap_or("21");
    let image = cfg.native.as_ref()
        .and_then(|n| n.docker_image.clone())
        .unwrap_or_else(|| format!("ghcr.io/graalvm/native-image-community:{}", target));

    println!(
        "  {} docker image: {}",
        style("➜").green(),
        image
    );

    // Convert paths to be relative to project root for Docker volume mount
    let jar_rel = jar_path.strip_prefix(project)
        .unwrap_or(jar_path);
    let output_rel = output_path.strip_prefix(project)
        .unwrap_or(output_path);

    let mut cmd = Command::new("docker");
    cmd.arg("run").arg("--rm");

    if let Some(p) = platform {
        cmd.arg("--platform").arg(p);
    }

    cmd.arg("-v").arg(format!("{}:/app", project.display()))
        .arg("-w").arg("/app")
        .arg(&image)
        .arg("-jar").arg(format!("/app/{}", jar_rel.display()))
        .arg("-o").arg(format!("/app/{}", output_rel.display()));

    for arg in extra_args {
        cmd.arg(arg);
    }

    let status = cmd
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit())
        .status()?;

    if !status.success() {
        bail!("docker native-image failed with exit code {}", status.code().unwrap_or(-1));
    }

    Ok(())
}
