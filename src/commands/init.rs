use anyhow::{bail, Result};
use console::style;
use dialoguer::{Confirm, Input, Select};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::config;
use crate::config::schema::YmConfig;
use crate::jdk_manager;

/// Entry point with options — aligned with yarn init
pub fn execute_with_options(
    name: Option<String>,
    template: Option<String>,
    yes: bool,
) -> Result<()> {
    let dir = resolve_dir(name.as_deref())?;

    let config_path = dir.join(config::CONFIG_FILE);
    let existing = if config_path.exists() {
        Some(config::load_config(&config_path)?)
    } else {
        None
    };

    if let Some(tpl) = template {
        if existing.is_some() {
            bail!("ym.json already exists, cannot use --template");
        }
        return execute_from_template(&dir, &tpl);
    }

    if yes && existing.is_none() {
        execute_defaults(&dir)
    } else {
        execute_interactive(&dir, existing.as_ref())
    }
}

/// Resolve the target directory path. Does NOT create the directory.
fn resolve_dir(name: Option<&str>) -> Result<PathBuf> {
    let cwd = std::env::current_dir()?;

    match name {
        None => Ok(cwd),
        Some(project_name) => Ok(cwd.join(project_name)),
    }
}

/// Non-interactive mode — all defaults, no prompts
fn execute_defaults(dir: &Path) -> Result<()> {
    let dir_name = dir_name(dir);
    let author = git_user_name();
    let pkg = default_package(&dir_name);

    let mut config = YmConfig {
        name: dir_name,
        version: Some("1.0.0".to_string()),
        target: Some("21".to_string()),
        ..Default::default()
    };

    if let Some(ref a) = author {
        config.author = Some(a.clone());
    }
    config.license = Some("MIT".to_string());
    config.package = Some(pkg.clone());
    config.main = Some(format!("{}.Main", pkg));
    config.dependencies = Some(BTreeMap::new());
    config.dev_dependencies = Some(BTreeMap::new());
    config.env = Some({
        let mut env = BTreeMap::new();
        env.insert("ARTIFACT".to_string(), config.name.clone());
        env
    });
    config.scripts = Some(default_scripts());

    write_project(dir, &config)?;

    let main_class = config.main.as_deref().unwrap_or("Main");
    let main_path = main_class.replace('.', "/");
    println!("  {} Created ym.json", style("✓").green());
    println!("  {} Created src/main/java/{}.java", style("✓").green(), main_path);

    Ok(())
}

/// Interactive mode — ask all fields like yarn init
/// If `existing` is Some, use its values as defaults (re-init mode).
fn execute_interactive(dir: &Path, existing: Option<&YmConfig>) -> Result<()> {
    let default_name = existing
        .map(|c| c.name.clone())
        .unwrap_or_else(|| dir_name(dir));
    let default_author = existing
        .and_then(|c| c.author.clone())
        .or_else(|| git_user_name())
        .unwrap_or_default();
    let default_version = existing
        .and_then(|c| c.version.clone())
        .unwrap_or_else(|| "1.0.0".to_string());
    let default_description = existing
        .and_then(|c| c.description.clone())
        .unwrap_or_default();
    let default_license = existing
        .and_then(|c| c.license.clone())
        .unwrap_or_else(|| "MIT".to_string());

    if existing.is_some() {
        println!("  {} Re-initializing existing project", style("→").blue());
        println!();
    }

    let name: String = Input::new()
        .with_prompt("name")
        .default(default_name)
        .interact_text()?;

    let version: String = Input::new()
        .with_prompt("version")
        .default(default_version)
        .interact_text()?;

    let description: String = Input::new()
        .with_prompt("description")
        .default(default_description)
        .allow_empty(true)
        .interact_text()?;

    let default_package = existing
        .and_then(|c| c.package.clone())
        .unwrap_or_else(|| default_package(&name));
    let package: String = Input::new()
        .with_prompt("package")
        .default(default_package)
        .interact_text()?;

    let default_main = existing
        .and_then(|c| c.main.clone())
        .unwrap_or_else(|| format!("{}.{}", package, to_class_name(&name)));
    let main_class: String = Input::new()
        .with_prompt("main class")
        .default(default_main)
        .interact_text()?;

    let author: String = Input::new()
        .with_prompt("author")
        .default(default_author)
        .allow_empty(true)
        .interact_text()?;

    let license: String = Input::new()
        .with_prompt("license")
        .default(default_license)
        .interact_text()?;

    // --- JDK selection ---
    println!();
    let term = console::Term::stderr();
    let _ = term.write_line(&format!("  {} Scanning JDKs...", style("→").blue()));
    let jdks = jdk_manager::scan_jdks();
    let _ = term.clear_last_lines(1);

    let dev_jdk_path = select_jdk("Select DEV JDK (for development & hot reload)", &jdks, true)?;
    let prod_jdk_path = select_jdk("Select PROD JDK (for build & production)", &jdks, false)?;

    // Determine java version from dev JDK
    let java_version = dev_jdk_path.as_ref()
        .and_then(|p| {
            jdks.iter()
                .find(|j| &j.path == p)
                .map(|j| {
                    // Extract major version
                    let v = &j.version;
                    if v.starts_with("1.") {
                        v.split('.').nth(1).unwrap_or("21").to_string()
                    } else {
                        v.split('.').next().unwrap_or("21").to_string()
                    }
                })
        })
        .unwrap_or_else(|| "21".to_string());

    // --- GraalVM native compilation ---
    // Default to yes if PROD JDK is already GraalVM
    let prod_is_graalvm = prod_jdk_path.as_ref()
        .and_then(|p| jdks.iter().find(|j| &j.path == p))
        .map(|j| {
            let name_lower = j.display_name().to_lowercase();
            name_lower.contains("graalvm") || name_lower.contains("graal")
        })
        .unwrap_or(false);

    println!();
    let use_graalvm = Confirm::new()
        .with_prompt("  Enable GraalVM native-image compilation?")
        .default(prod_is_graalvm)
        .interact()?;

    let graalvm_path = if use_graalvm {
        let graalvm_jdks: Vec<&jdk_manager::DetectedJdk> = jdks.iter()
            .filter(|j| {
                let name_lower = j.display_name().to_lowercase();
                name_lower.contains("graalvm") || name_lower.contains("graal")
            })
            .collect();

        if graalvm_jdks.is_empty() {
            println!("  {} No GraalVM found locally.", style("!").yellow());
            let download = Confirm::new()
                .with_prompt("  Download GraalVM?")
                .default(true)
                .interact()?;
            if download {
                download_jdk_interactive()?
            } else {
                println!("  {} Set GRAALVM_HOME in env later to enable native builds.", style("→").dim());
                None
            }
        } else {
            let graalvm_only: Vec<jdk_manager::DetectedJdk> = graalvm_jdks.into_iter().cloned().collect();
            select_jdk("Select GraalVM (for native-image)", &graalvm_only, false)?
        }
    } else {
        None
    };

    // Build env and scripts
    let env = build_env(&name, &dev_jdk_path, &prod_jdk_path, &graalvm_path);
    let scripts = build_scripts(use_graalvm);

    // Start from existing config or blank
    let mut config = existing.cloned().unwrap_or_default();
    config.name = name;
    config.version = Some(version);
    config.target = Some(java_version);

    if !description.is_empty() {
        config.description = Some(description);
    } else {
        config.description = None;
    }
    if !author.is_empty() {
        config.author = Some(author);
    }
    if !license.is_empty() {
        config.license = Some(license);
    }

    config.package = Some(package);
    config.main = Some(main_class);

    // Merge env: init-managed keys overwrite, user-added keys preserved
    {
        let mut merged_env = config.env.unwrap_or_default();
        for (k, v) in env {
            merged_env.insert(k, v);
        }
        config.env = Some(merged_env);
    }

    if config.dependencies.is_none() {
        config.dependencies = Some(BTreeMap::new());
    }
    if config.dev_dependencies.is_none() {
        config.dev_dependencies = Some(BTreeMap::new());
    }

    // Merge scripts: init-managed scripts overwrite, user-added scripts preserved
    {
        let mut merged_scripts = config.scripts.unwrap_or_default();
        for (k, v) in scripts {
            merged_scripts.insert(k, v);
        }
        config.scripts = Some(merged_scripts);
    }

    // JSON preview
    println!();
    let json = serde_json::to_string_pretty(&config)?;
    println!("{}", json);
    println!();

    let ok = Confirm::new()
        .with_prompt("Is this OK?")
        .default(true)
        .interact()?;

    if !ok {
        println!("Aborted.");
        return Ok(());
    }

    write_project(dir, &config)?;

    let main_class = config.main.as_deref().unwrap_or("Main");
    let main_path = main_class.replace('.', "/");
    println!("  {} Created ym.json", style("✓").green());
    println!("  {} Created src/main/java/{}.java", style("✓").green(), main_path);

    Ok(())
}

/// Interactive JDK selection with arrow keys.
fn select_jdk(label: &str, jdks: &[jdk_manager::DetectedJdk], prefer_dcevm: bool) -> Result<Option<PathBuf>> {
    println!();
    println!("  {}", style(label).bold());

    if jdks.is_empty() {
        println!("  No JDKs found locally.");
        let download = Confirm::new()
            .with_prompt("  Download a JDK?")
            .default(true)
            .interact()?;

        if download {
            return download_jdk_interactive();
        }
        return Ok(None);
    }

    // Sort: when prefer_dcevm, put JBR (has_dcevm) entries first
    let sorted_jdks: Vec<&jdk_manager::DetectedJdk> = if prefer_dcevm {
        let mut dcevm: Vec<_> = jdks.iter().filter(|j| j.has_dcevm).collect();
        let mut rest: Vec<_> = jdks.iter().filter(|j| !j.has_dcevm).collect();
        dcevm.append(&mut rest);
        dcevm
    } else {
        jdks.iter().collect()
    };

    // Build selection items
    let mut items: Vec<String> = Vec::new();

    for jdk in &sorted_jdks {
        let mut label = format!(
            "{:<20} {}  ({})",
            jdk.display_name(),
            style(jdk.path.display()).dim(),
            jdk.source,
        );
        if prefer_dcevm && jdk.has_dcevm {
            label = format!("{} {}", label, style("★ hot reload").yellow());
        }
        items.push(label);
    }
    items.push(format!("{}", style("Download other...").cyan()));

    let selection = Select::new()
        .items(&items)
        .default(0)
        .interact()?;

    // "Download other..." is the last item
    if selection == sorted_jdks.len() {
        return download_jdk_interactive();
    }

    let selected = sorted_jdks[selection];
    println!(
        "  {} {}",
        style("✓").green(),
        selected.display_name()
    );

    Ok(Some(selected.path.clone()))
}

/// Interactive JDK download: select vendor then version, or paste a URL.
fn download_jdk_interactive() -> Result<Option<PathBuf>> {
    let mut vendor_labels: Vec<String> = jdk_manager::DOWNLOAD_VENDORS
        .iter()
        .map(|(label, _)| label.to_string())
        .collect();
    vendor_labels.push(format!("{}", style("Paste download URL...").cyan()));

    println!();
    let vendor_idx = Select::new()
        .with_prompt("  Vendor")
        .items(&vendor_labels)
        .default(0)
        .interact()?;

    // Last item = custom URL
    if vendor_idx == jdk_manager::DOWNLOAD_VENDORS.len() {
        let url: String = Input::new()
            .with_prompt("  URL (.tar.gz)")
            .interact_text()?;
        let name: String = Input::new()
            .with_prompt("  Name (e.g. jdk-25)")
            .interact_text()?;
        let path = jdk_manager::download_jdk_from_url(&url, &name)?;
        return Ok(Some(path));
    }

    let (_, vendor_key) = jdk_manager::DOWNLOAD_VENDORS[vendor_idx];

    let version_labels: Vec<&str> = jdk_manager::JDK_VERSIONS
        .iter()
        .map(|(label, _)| *label)
        .collect();

    let version_idx = Select::new()
        .with_prompt("  Version")
        .items(&version_labels)
        .default(0)
        .interact()?;

    let (_, version_key) = jdk_manager::JDK_VERSIONS[version_idx];

    let path = jdk_manager::download_jdk(vendor_key, version_key)?;
    Ok(Some(path))
}

/// Build env map from selected JDKs.
fn build_env(name: &str, dev_jdk: &Option<PathBuf>, prod_jdk: &Option<PathBuf>, graalvm: &Option<PathBuf>) -> BTreeMap<String, String> {
    let mut env = BTreeMap::new();
    env.insert("ARTIFACT".to_string(), name.to_string());
    if let Some(path) = dev_jdk {
        env.insert("DEV_JAVA_HOME".to_string(), shorten_home(path));
    }
    if let Some(path) = prod_jdk {
        env.insert("PROD_JAVA_HOME".to_string(), shorten_home(path));
    }
    if let Some(path) = graalvm {
        env.insert("GRAALVM_HOME".to_string(), shorten_home(path));
    }
    env
}

/// Replace $HOME prefix with ~ for shorter, portable paths.
fn shorten_home(path: &Path) -> String {
    if let Ok(home) = std::env::var("HOME") {
        let s = path.display().to_string();
        if let Some(rest) = s.strip_prefix(&home) {
            return format!("~{}", rest);
        }
    }
    path.display().to_string()
}

/// Build scripts map (env vars are handled by the "env" field).
fn build_scripts(with_native: bool) -> BTreeMap<String, String> {
    let mut scripts = BTreeMap::new();
    scripts.insert("dev".to_string(), "JAVA_HOME=$DEV_JAVA_HOME ymc dev".to_string());
    scripts.insert("build".to_string(), "JAVA_HOME=$PROD_JAVA_HOME ymc build".to_string());
    scripts.insert("test".to_string(), "JAVA_HOME=$PROD_JAVA_HOME ymc test".to_string());
    scripts.insert("start".to_string(), "JAVA_HOME=$PROD_JAVA_HOME ymc run".to_string());
    scripts.insert("docker:build".to_string(), "docker build -t $ARTIFACT .".to_string());
    scripts.insert("docker:push".to_string(), "docker push $ARTIFACT".to_string());
    if with_native {
        scripts.insert("native".to_string(), "ymc build --release && $GRAALVM_HOME/bin/native-image -jar out/$ARTIFACT.jar -o out/$ARTIFACT".to_string());
        scripts.insert("native:docker".to_string(), "docker run --rm -v $(pwd):/workspace -w /workspace yummy:jdk25-graal sh -c 'ymc build --release && native-image -jar out/$ARTIFACT.jar -o out/$ARTIFACT'".to_string());
    }
    scripts
}

/// Non-interactive init from template
fn execute_from_template(dir: &Path, template: &str) -> Result<()> {
    let dir_name = dir_name(dir);

    let mut config = YmConfig {
        name: dir_name.clone(),
        version: Some("1.0.0".to_string()),
        target: Some("21".to_string()),
        ..Default::default()
    };

    let mut deps = BTreeMap::new();
    let mut dev_deps = BTreeMap::new();

    std::fs::create_dir_all(dir)?;
    let src_dir = dir.join("src").join("main").join("java");
    std::fs::create_dir_all(&src_dir)?;

    let resources_dir = dir.join("src").join("main").join("resources");
    std::fs::create_dir_all(&resources_dir)?;

    let test_java_dir = dir.join("src").join("test").join("java");
    std::fs::create_dir_all(&test_java_dir)?;

    match template {
        "spring" => {
            deps.insert(
                "org.springframework.boot:spring-boot-starter-web".to_string(),
                "3.4.0".to_string(),
            );
            deps.insert(
                "org.springframework.boot:spring-boot-starter".to_string(),
                "3.4.0".to_string(),
            );
            dev_deps.insert(
                "org.springframework.boot:spring-boot-starter-test".to_string(),
                "3.4.0".to_string(),
            );
            config.package = Some("com.example".to_string());
            config.main = Some("com.example.Application".to_string());

            let pkg_dir = src_dir.join("com").join("example");
            std::fs::create_dir_all(&pkg_dir)?;
            let main_content = r#"package com.example;

import org.springframework.boot.SpringApplication;
import org.springframework.boot.autoconfigure.SpringBootApplication;

@SpringBootApplication
public class Application {
    public static void main(String[] args) {
        SpringApplication.run(Application.class, args);
    }
}
"#;
            std::fs::write(pkg_dir.join("Application.java"), main_content)?;
        }
        "cli" => {
            let pkg = default_package(&dir_name);
            config.package = Some(pkg.clone());
            config.main = Some(format!("{}.Main", pkg));
            let pkg_dir = src_dir.join(pkg.replace('.', "/"));
            std::fs::create_dir_all(&pkg_dir)?;
            let main_content = format!(
                r#"package {};

public class Main {{
    public static void main(String[] args) {{
        if (args.length == 0) {{
            System.out.println("Usage: {} <command>");
            System.exit(1);
        }}
        System.out.println("Running: " + args[0]);
    }}
}}
"#,
                pkg, dir_name
            );
            std::fs::write(pkg_dir.join("Main.java"), main_content)?;
        }
        "lib" | "library" => {
            dev_deps.insert(
                "org.junit.jupiter:junit-jupiter".to_string(),
                "5.11.0".to_string(),
            );
            let pkg = default_package(&dir_name);
            config.package = Some(pkg.clone());
            let class_name = to_class_name(&dir_name);
            let pkg_path = pkg.replace('.', "/");
            let pkg_dir = src_dir.join(&pkg_path);
            std::fs::create_dir_all(&pkg_dir)?;
            let lib_content = format!(
                r#"package {};

public class {} {{
    public String greet(String name) {{
        return "Hello, " + name + "!";
    }}
}}
"#,
                pkg, class_name
            );
            std::fs::write(pkg_dir.join(format!("{}.java", class_name)), lib_content)?;

            let test_pkg_dir = test_java_dir.join(&pkg_path);
            std::fs::create_dir_all(&test_pkg_dir)?;
            let test_content = format!(
                r#"package {};

import org.junit.jupiter.api.Test;
import static org.junit.jupiter.api.Assertions.*;

class {}Test {{
    @Test
    void testGreet() {{
        {} lib = new {}();
        assertEquals("Hello, World!", lib.greet("World"));
    }}
}}
"#,
                pkg, class_name, class_name, class_name
            );
            std::fs::write(
                test_pkg_dir.join(format!("{}Test.java", class_name)),
                test_content,
            )?;
        }
        _ => {
            // Default "app" template
            let pkg = default_package(&dir_name);
            config.package = Some(pkg.clone());
            config.main = Some(format!("{}.Main", pkg));
            create_sample_main(&src_dir, &config)?;
        }
    }

    if !deps.is_empty() {
        config.dependencies = Some(deps);
    } else {
        config.dependencies = Some(BTreeMap::new());
    }
    if !dev_deps.is_empty() {
        config.dev_dependencies = Some(dev_deps);
    }

    let config_path = dir.join(config::CONFIG_FILE);
    config::save_config(&config_path, &config)?;

    // Create .gitignore
    let gitignore_path = dir.join(".gitignore");
    if !gitignore_path.exists() {
        std::fs::write(&gitignore_path, ".ym/\nout/\n*.class\n.idea/\n*.iml\n")?;
    }

    println!();
    println!(
        "  {} Created {} project from '{}' template",
        style("✓").green(),
        style(&dir_name).bold(),
        style(template).cyan()
    );
    if let Some(ref d) = config.dependencies {
        if !d.is_empty() {
            println!(
                "  {} {} dependencies pre-configured",
                style("→").dim(),
                d.len()
            );
        }
    }
    println!();
    println!("  Run {} to start developing", style("ym dev").cyan());

    Ok(())
}

/// Write ym.json + create directories/files
fn write_project(dir: &Path, config: &YmConfig) -> Result<()> {
    std::fs::create_dir_all(dir)?;

    let config_path = dir.join(config::CONFIG_FILE);
    config::save_config(&config_path, config)?;

    let src_dir = dir.join("src").join("main").join("java");
    std::fs::create_dir_all(&src_dir)?;
    create_sample_main(&src_dir, config)?;

    let resources_dir = dir.join("src").join("main").join("resources");
    std::fs::create_dir_all(&resources_dir)?;

    let test_dir = dir.join("src").join("test").join("java");
    std::fs::create_dir_all(&test_dir)?;

    // Create .gitignore
    let gitignore_path = dir.join(".gitignore");
    if !gitignore_path.exists() {
        std::fs::write(&gitignore_path, ".ym/\nout/\n*.class\n.idea/\n*.iml\n")?;
    }

    Ok(())
}

/// my-app → myapp
pub fn sanitize_package_name(name: &str) -> String {
    name.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect::<String>()
        .to_lowercase()
}

/// my-app → com.example.myapp
pub fn default_package(name: &str) -> String {
    format!("com.example.{}", sanitize_package_name(name))
}

fn dir_name(dir: &Path) -> String {
    dir.file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("my-app")
        .to_string()
}

fn git_user_name() -> Option<String> {
    std::process::Command::new("git")
        .args(["config", "user.name"])
        .output()
        .ok()
        .and_then(|o| {
            if o.status.success() {
                Some(String::from_utf8_lossy(&o.stdout).trim().to_string())
            } else {
                None
            }
        })
        .filter(|s| !s.is_empty())
}

fn to_class_name(name: &str) -> String {
    name.split(['-', '_'])
        .map(|part| {
            let mut c = part.chars();
            match c.next() {
                None => String::new(),
                Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
            }
        })
        .collect()
}

fn default_scripts() -> BTreeMap<String, String> {
    let mut scripts = BTreeMap::new();
    scripts.insert("dev".to_string(), "ymc dev".to_string());
    scripts.insert("build".to_string(), "ymc build".to_string());
    scripts.insert("test".to_string(), "ymc test".to_string());
    scripts.insert("start".to_string(), "ymc run".to_string());
    scripts.insert("docker:build".to_string(), "docker build -t $ARTIFACT .".to_string());
    scripts.insert("docker:push".to_string(), "docker push $ARTIFACT".to_string());
    scripts
}

fn create_sample_main(src_dir: &Path, config: &YmConfig) -> Result<()> {
    let main_class = config.main.as_deref().unwrap_or("Main");

    // Support qualified names like "com.example.Application"
    let (pkg_dir, class_name) = if let Some(idx) = main_class.rfind('.') {
        let pkg = &main_class[..idx];
        let cls = &main_class[idx + 1..];
        let pkg_path = src_dir.join(pkg.replace('.', "/"));
        (pkg_path, cls)
    } else {
        (src_dir.to_path_buf(), main_class)
    };

    std::fs::create_dir_all(&pkg_dir)?;
    let main_file = pkg_dir.join(format!("{}.java", class_name));
    if main_file.exists() {
        return Ok(());
    }

    let package_decl = if main_class.contains('.') {
        format!("package {};\n\n", &main_class[..main_class.rfind('.').unwrap()])
    } else {
        String::new()
    };

    let content = format!(
        r#"{}public class {} {{
    public static void main(String[] args) {{
        System.out.println("Hello World!");
    }}
}}
"#,
        package_decl, class_name
    );

    std::fs::write(&main_file, content)?;
    Ok(())
}
