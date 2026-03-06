use anyhow::Result;
use console::style;
use std::process::Command;

/// Diagnose environment issues
pub fn execute(fix: bool) -> Result<()> {
    println!();
    println!("  {}", style("ym doctor").bold());
    println!();

    let mut ok = true;

    // Check Java
    ok &= check_command("java", &["-version"], "Java Runtime");

    // Check javac
    ok &= check_command("javac", &["-version"], "Java Compiler (javac)");

    // Check JAVA_HOME
    check_java_home();

    // Check jar
    ok &= check_command("jar", &["--version"], "JAR tool");

    // Check git
    check_command("git", &["--version"], "Git");

    // Check ym.json
    check_config();

    // Check Maven cache
    check_maven_cache();

    // Check project structure
    check_project_structure(fix);

    println!();
    if ok {
        println!(
            "  {} All critical checks passed",
            style("✓").green().bold()
        );
    } else {
        if fix {
            println!(
                "  {} Some issues were auto-fixed, but missing tools require manual installation",
                style("!").yellow().bold()
            );
        } else {
            println!(
                "  {} Some checks failed. Run {} to auto-fix or install missing tools.",
                style("✗").red().bold(),
                style("ym doctor --fix").cyan()
            );
        }
    }
    println!();

    Ok(())
}

fn check_project_structure(fix: bool) {
    let cwd = std::env::current_dir().unwrap_or_default();
    let config_path = crate::config::find_config(&cwd);

    if config_path.is_none() {
        return;
    }

    let project = config_path.as_ref().unwrap().parent().unwrap_or(&cwd);

    // Check src directory
    let src = project.join("src");
    if !src.exists() {
        if fix {
            let _ = std::fs::create_dir_all(&src);
            println!(
                "  {} Created missing src/ directory",
                style("✓").green()
            );
        } else {
            println!(
                "  {} src/ directory missing (run --fix to create)",
                style("!").yellow()
            );
        }
    } else {
        println!(
            "  {} src/ directory",
            style("✓").green()
        );
    }

    // Check .gitignore
    let gitignore = project.join(".gitignore");
    if !gitignore.exists() {
        if fix {
            let _ = std::fs::write(&gitignore, ".ym/\nout/\n*.class\n.idea/\n*.iml\n");
            println!(
                "  {} Created .gitignore",
                style("✓").green()
            );
        } else {
            println!(
                "  {} .gitignore missing (run --fix to create)",
                style("!").yellow()
            );
        }
    } else {
        // Check if .ym/ is in gitignore
        let content = std::fs::read_to_string(&gitignore).unwrap_or_default();
        if !content.contains(".ym/") {
            if fix {
                let _ = std::fs::write(&gitignore, format!("{}\n.ym/\nout/\n", content));
                println!(
                    "  {} Added .ym/ and out/ to .gitignore",
                    style("✓").green()
                );
            } else {
                println!(
                    "  {} .gitignore exists but missing .ym/ entry (run --fix)",
                    style("!").yellow()
                );
            }
        } else {
            println!(
                "  {} .gitignore",
                style("✓").green()
            );
        }
    }
}

fn check_command(cmd: &str, args: &[&str], label: &str) -> bool {
    match Command::new(cmd).args(args).output() {
        Ok(output) if output.status.success() => {
            let ver = String::from_utf8_lossy(&output.stdout);
            let ver_err = String::from_utf8_lossy(&output.stderr);
            // java -version outputs to stderr
            let version_line = if ver.trim().is_empty() {
                ver_err.lines().next().unwrap_or("").trim().to_string()
            } else {
                ver.lines().next().unwrap_or("").trim().to_string()
            };
            println!(
                "  {} {}  {}",
                style("✓").green(),
                label,
                style(&version_line).dim()
            );
            true
        }
        _ => {
            println!(
                "  {} {}  {}",
                style("✗").red(),
                label,
                style("not found").red()
            );
            false
        }
    }
}

fn check_java_home() {
    match std::env::var("JAVA_HOME") {
        Ok(home) if !home.is_empty() => {
            let exists = std::path::Path::new(&home).exists();
            if exists {
                println!(
                    "  {} JAVA_HOME  {}",
                    style("✓").green(),
                    style(&home).dim()
                );
            } else {
                println!(
                    "  {} JAVA_HOME  {} (path does not exist)",
                    style("!").yellow(),
                    style(&home).dim()
                );
            }
        }
        _ => {
            println!(
                "  {} JAVA_HOME  {}",
                style("!").yellow(),
                style("not set (ym will use java from PATH)").dim()
            );
        }
    }
}

fn check_config() {
    let cwd = std::env::current_dir().unwrap_or_default();
    if let Some(path) = crate::config::find_config(&cwd) {
        match crate::config::load_config(&path) {
            Ok(cfg) => {
                println!(
                    "  {} ym.json  {} ({})",
                    style("✓").green(),
                    style(&cfg.name).dim(),
                    style(path.display()).dim()
                );
            }
            Err(e) => {
                println!(
                    "  {} ym.json  {} ({})",
                    style("✗").red(),
                    style("parse error").red(),
                    style(e).dim()
                );
            }
        }
    } else {
        println!(
            "  {} ym.json  {}",
            style("-").dim(),
            style("not found in current directory tree").dim()
        );
    }
}

fn check_maven_cache() {
    let cwd = std::env::current_dir().unwrap_or_default();
    let cache = crate::config::maven_cache_dir(&cwd);
    if cache.exists() {
        let size = dir_size(&cache);
        println!(
            "  {} Maven cache  {}",
            style("✓").green(),
            style(format_size(size)).dim()
        );
    } else {
        println!(
            "  {} Maven cache  {}",
            style("-").dim(),
            style("empty").dim()
        );
    }
}

fn dir_size(path: &std::path::Path) -> u64 {
    walkdir::WalkDir::new(path)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .filter_map(|e| e.metadata().ok())
        .map(|m| m.len())
        .sum()
}

fn format_size(bytes: u64) -> String {
    if bytes >= 1_073_741_824 {
        format!("{:.1} GB", bytes as f64 / 1_073_741_824.0)
    } else if bytes >= 1_048_576 {
        format!("{:.1} MB", bytes as f64 / 1_048_576.0)
    } else if bytes >= 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{} B", bytes)
    }
}
