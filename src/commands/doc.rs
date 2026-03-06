use anyhow::{bail, Result};
use console::style;
use std::path::{Path, PathBuf};

use crate::config;
use crate::workspace::graph::WorkspaceGraph;

pub fn execute(target: Option<String>, open: bool) -> Result<()> {
    let (config_path, cfg) = config::load_or_find_config()?;
    let project = config::project_dir(&config_path);

    if cfg.workspaces.is_some() {
        let target = target.as_deref().unwrap_or_else(|| {
            eprintln!("  In workspace mode, specify a target: ym doc <module>");
            std::process::exit(1);
        });
        return doc_workspace(&project, target, open);
    }

    let jars = super::build::resolve_deps(&project, &cfg)?;
    let src = config::source_dir(&project);
    let out = project.join("out").join("docs");
    std::fs::create_dir_all(&out)?;

    generate_javadoc(&[src], &jars, &out, cfg.target.as_deref())?;

    println!(
        "  {} Generated Javadoc for {} → {}",
        style("✓").green(),
        style(&cfg.name).bold(),
        style(out.display()).dim()
    );

    if open {
        open_in_browser(&out)?;
    }

    Ok(())
}

fn doc_workspace(root: &Path, target: &str, open: bool) -> Result<()> {
    let ws = WorkspaceGraph::build(root)?;
    let packages = ws.transitive_closure(target)?;

    let mut all_sources: Vec<PathBuf> = Vec::new();
    let mut all_jars: Vec<PathBuf> = Vec::new();

    let mut java_version = None;

    for pkg_name in &packages {
        let pkg = ws.get_package(pkg_name).unwrap();
        let src = config::source_dir(&pkg.path);
        if src.exists() {
            all_sources.push(src);
        }
        let jars = super::build::resolve_deps(&pkg.path, &pkg.config)?;
        all_jars.extend(jars);
        // Use target's java version if specified
        if pkg_name == target {
            java_version = pkg.config.target.clone();
        }
    }

    all_jars.sort();
    all_jars.dedup();

    let out = root.join("out").join("docs").join(target);
    std::fs::create_dir_all(&out)?;

    generate_javadoc(&all_sources, &all_jars, &out, java_version.as_deref())?;

    println!(
        "  {} Generated Javadoc for {} ({} source roots) → {}",
        style("✓").green(),
        style(target).bold(),
        all_sources.len(),
        style(out.display()).dim()
    );

    if open {
        open_in_browser(&out)?;
    }

    Ok(())
}

fn generate_javadoc(
    source_dirs: &[PathBuf],
    classpath: &[PathBuf],
    output_dir: &Path,
    java_version: Option<&str>,
) -> Result<()> {
    // Collect all .java files from source dirs
    let mut java_files: Vec<PathBuf> = Vec::new();
    for dir in source_dirs {
        if !dir.exists() {
            continue;
        }
        for entry in walkdir::WalkDir::new(dir) {
            let entry = entry?;
            if entry.path().extension().and_then(|e| e.to_str()) == Some("java") {
                java_files.push(entry.path().to_path_buf());
            }
        }
    }

    if java_files.is_empty() {
        println!("  {} No Java source files found", style("!").yellow());
        return Ok(());
    }

    let mut cmd = std::process::Command::new("javadoc");
    cmd.arg("-d").arg(output_dir);
    cmd.arg("-quiet");
    cmd.arg("-encoding").arg("UTF-8");
    cmd.arg("-charset").arg("UTF-8");

    if let Some(ver) = java_version {
        cmd.arg("-source").arg(ver);
    }

    // Classpath
    if !classpath.is_empty() {
        let sep = if cfg!(windows) { ";" } else { ":" };
        let cp = classpath
            .iter()
            .map(|p| p.to_string_lossy().to_string())
            .collect::<Vec<_>>()
            .join(sep);
        cmd.arg("-classpath").arg(cp);
    }

    // Use @argfile for many files
    if java_files.len() > 50 {
        let argfile = output_dir.join(".javadoc-files.txt");
        let content = java_files
            .iter()
            .map(|f| f.to_string_lossy().to_string())
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(&argfile, &content)?;
        cmd.arg(format!("@{}", argfile.display()));
    } else {
        for f in &java_files {
            cmd.arg(f);
        }
    }

    let output = cmd.output()?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        // javadoc warnings are common, only fail on actual errors
        if stderr.contains("error:") || stderr.contains("error -") {
            eprintln!("{}", crate::compiler::colorize_errors(&stderr));
            bail!("Javadoc generation failed");
        }
    }

    Ok(())
}

fn open_in_browser(doc_dir: &Path) -> Result<()> {
    let index = doc_dir.join("index.html");
    if index.exists() {
        let open_cmd = if cfg!(target_os = "macos") {
            "open"
        } else if cfg!(target_os = "windows") {
            "start"
        } else {
            "xdg-open"
        };
        let _ = std::process::Command::new(open_cmd)
            .arg(&index)
            .spawn();
    }
    Ok(())
}
