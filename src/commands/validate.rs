use anyhow::Result;
use console::style;

use crate::config;

pub fn execute() -> Result<()> {
    let (config_path, cfg) = config::load_or_find_config()?;
    let project = config::project_dir(&config_path);

    let mut errors: Vec<String> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();

    // Name is required and non-empty
    if cfg.name.is_empty() {
        errors.push("\"name\" is empty".to_string());
    }

    // Version format check
    if let Some(ref v) = cfg.version {
        if !v.chars().next().map(|c| c.is_ascii_digit()).unwrap_or(false) {
            warnings.push(format!("\"version\" '{}' doesn't start with a digit", v));
        }
    }

    // Java version check
    if let Some(ref j) = cfg.target {
        if j.parse::<u32>().is_err() {
            warnings.push(format!("\"java\" '{}' is not a valid Java version number", j));
        }
    }

    // Dependencies format check
    if let Some(ref deps) = cfg.dependencies {
        for (coord, version) in deps {
            if !coord.contains(':') {
                errors.push(format!(
                    "Dependency '{}' is not in groupId:artifactId format",
                    coord
                ));
            }
            if version.is_empty() {
                errors.push(format!("Dependency '{}' has empty version", coord));
            }
        }
    }

    // DevDependencies format check
    if let Some(ref deps) = cfg.dev_dependencies {
        for (coord, version) in deps {
            if !coord.contains(':') {
                errors.push(format!(
                    "DevDependency '{}' is not in groupId:artifactId format",
                    coord
                ));
            }
            if version.is_empty() {
                errors.push(format!("DevDependency '{}' has empty version", coord));
            }
        }
    }

    // Workspace dependencies check
    if let Some(ref ws_deps) = cfg.workspace_dependencies {
        if cfg.workspaces.is_some() {
            warnings.push(
                "Root config has both 'workspaces' and 'workspaceDependencies' — usually only packages have workspaceDependencies".to_string()
            );
        }
        for dep in ws_deps {
            if dep.is_empty() {
                errors.push("Empty workspace dependency name".to_string());
            }
        }
    }

    // Source directory exists
    let src = config::source_dir_for(&project, &cfg);
    if !src.exists() {
        warnings.push(format!("Source directory '{}' does not exist", src.display()));
    }

    // Workspaces patterns check
    if let Some(ref patterns) = cfg.workspaces {
        if patterns.is_empty() {
            warnings.push("\"workspaces\" is empty".to_string());
        }
    }

    // Compiler engine check
    if let Some(ref compiler) = cfg.compiler {
        if let Some(ref engine) = compiler.engine {
            if engine != "javac" && engine != "ecj" {
                errors.push(format!(
                    "Unknown compiler engine '{}' (expected 'javac' or 'ecj')",
                    engine
                ));
            }
        }
    }

    // Registries URL check
    if let Some(ref registries) = cfg.registries {
        for (name, url) in registries {
            if !url.starts_with("http://") && !url.starts_with("https://") {
                warnings.push(format!(
                    "Registry '{}' URL '{}' doesn't start with http:// or https://",
                    name, url
                ));
            }
        }
    }

    // Main class format check
    if let Some(ref main) = cfg.main {
        if main.contains('/') || main.contains('\\') {
            errors.push(format!(
                "\"main\" '{}' contains path separators — use dot notation (e.g., com.example.Main)",
                main
            ));
        }
    }

    // Print results
    println!();
    println!(
        "  Validating {}",
        style(config_path.display()).bold()
    );
    println!();

    for err in &errors {
        println!("  {} {}", style("error").red().bold(), err);
    }
    for warn in &warnings {
        println!("  {} {}", style("warn").yellow().bold(), warn);
    }

    if errors.is_empty() && warnings.is_empty() {
        println!(
            "  {} Configuration is valid",
            style("✓").green()
        );
    } else {
        println!();
        println!(
            "  {} errors, {} warnings",
            errors.len(),
            warnings.len()
        );
    }
    println!();

    if !errors.is_empty() {
        std::process::exit(1);
    }

    Ok(())
}
