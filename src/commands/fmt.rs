use anyhow::{bail, Result};
use console::style;
use std::path::PathBuf;

use crate::config;

/// Format Java source files using google-java-format
pub fn execute(check: bool, diff: bool) -> Result<()> {
    let (config_path, _cfg) = config::load_or_find_config()?;
    let project = config::project_dir(&config_path);
    let src = config::source_dir(&project);

    if !src.exists() {
        println!("  No source directory found.");
        return Ok(());
    }

    // Find google-java-format jar or command
    let formatter = find_formatter(&project)?;

    // Collect all Java files
    let mut java_files = Vec::new();
    for entry in walkdir::WalkDir::new(&src) {
        let entry = entry?;
        if entry.path().extension().and_then(|e| e.to_str()) == Some("java") {
            java_files.push(entry.path().to_path_buf());
        }
    }

    // Also check test dir
    let test_dir = config::test_dir(&project);
    if test_dir.exists() {
        for entry in walkdir::WalkDir::new(&test_dir) {
            let entry = entry?;
            if entry.path().extension().and_then(|e| e.to_str()) == Some("java") {
                java_files.push(entry.path().to_path_buf());
            }
        }
    }

    if java_files.is_empty() {
        println!("  No Java files to format.");
        return Ok(());
    }

    if diff {
        println!(
            "  Showing diff for {} Java files...",
            java_files.len()
        );
        let jar_path = match &formatter {
            Formatter::Jar(p) => Some(p.clone()),
            Formatter::Command(_) => None,
        };
        return show_diff(&jar_path, &java_files);
    }

    println!(
        "  {} {} Java files...",
        if check { "Checking" } else { "Formatting" },
        java_files.len()
    );

    match formatter {
        Formatter::Jar(jar_path) => run_with_jar(&jar_path, &java_files, check),
        Formatter::Command(cmd) => run_with_command(&cmd, &java_files, check),
    }
}

/// Show diff of what formatting would change (without modifying)
fn show_diff(jar_path: &Option<PathBuf>, files: &[PathBuf]) -> Result<()> {
    let mut changed_count = 0;

    for file in files {
        let original = std::fs::read_to_string(file)?;

        let output = if let Some(jar) = jar_path {
            std::process::Command::new("java")
                .arg("-jar")
                .arg(jar)
                .arg(file)
                .output()?
        } else {
            std::process::Command::new("google-java-format")
                .arg(file)
                .output()?
        };

        let formatted = String::from_utf8_lossy(&output.stdout);

        if original != *formatted {
            changed_count += 1;
            println!(
                "\n  {} {}",
                style("M").yellow().bold(),
                file.display()
            );

            // Simple line-by-line diff
            let orig_lines: Vec<&str> = original.lines().collect();
            let fmt_lines: Vec<&str> = formatted.lines().collect();

            let max = orig_lines.len().max(fmt_lines.len());
            for i in 0..max {
                let orig = orig_lines.get(i).copied().unwrap_or("");
                let fmt = fmt_lines.get(i).copied().unwrap_or("");
                if orig != fmt {
                    if !orig.is_empty() {
                        println!("    {} {}", style("-").red(), style(orig).red());
                    }
                    if !fmt.is_empty() {
                        println!("    {} {}", style("+").green(), style(fmt).green());
                    }
                }
            }
        }
    }

    if changed_count == 0 {
        println!("  {} All files are properly formatted", style("✓").green());
    } else {
        println!();
        println!(
            "  {} {} file(s) would be changed",
            style("!").yellow(),
            changed_count
        );
    }

    Ok(())
}

enum Formatter {
    Jar(PathBuf),
    Command(String),
}

fn find_formatter(project: &std::path::Path) -> Result<Formatter> {
    // Check for google-java-format in cache
    let cache = config::cache_dir(project);
    let gjf_dir = cache.join("tools");

    // Look for google-java-format jar in cache
    if gjf_dir.exists() {
        for entry in std::fs::read_dir(&gjf_dir)? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with("google-java-format") && name.ends_with(".jar") {
                return Ok(Formatter::Jar(entry.path()));
            }
        }
    }

    // Check if google-java-format is available as a command
    if std::process::Command::new("google-java-format")
        .arg("--version")
        .output()
        .is_ok()
    {
        return Ok(Formatter::Command("google-java-format".to_string()));
    }

    // Download google-java-format
    println!(
        "  {} Downloading google-java-format...",
        style("↓").blue()
    );

    std::fs::create_dir_all(&gjf_dir)?;
    let version = "1.25.2";
    let url = format!(
        "https://github.com/google/google-java-format/releases/download/v{}/google-java-format-{}-all-deps.jar",
        version, version
    );

    let client = reqwest::blocking::Client::builder()
        .user_agent("ym/0.1.0")
        .timeout(std::time::Duration::from_secs(120))
        .build()?;

    let response = client.get(&url).send()?;
    if !response.status().is_success() {
        bail!(
            "Failed to download google-java-format: HTTP {}",
            response.status()
        );
    }

    let jar_path = gjf_dir.join(format!("google-java-format-{}-all-deps.jar", version));
    let bytes = response.bytes()?;
    std::fs::write(&jar_path, &bytes)?;

    println!(
        "  {} Downloaded google-java-format {}",
        style("✓").green(),
        version
    );

    Ok(Formatter::Jar(jar_path))
}

fn run_with_jar(jar: &std::path::Path, files: &[PathBuf], check: bool) -> Result<()> {
    let mut cmd = std::process::Command::new("java");
    cmd.arg("-jar").arg(jar);

    if check {
        cmd.arg("--dry-run").arg("--set-exit-if-changed");
    } else {
        cmd.arg("--replace");
    }

    for f in files {
        cmd.arg(f);
    }

    let output = cmd.output()?;

    if check {
        if output.status.success() {
            println!("  {} All files are properly formatted", style("✓").green());
        } else {
            let stdout = String::from_utf8_lossy(&output.stdout);
            if !stdout.is_empty() {
                println!("  Files that need formatting:");
                for line in stdout.lines() {
                    println!("    {}", style(line).yellow());
                }
            }
            bail!("Some files are not properly formatted. Run 'ym fmt' to fix.");
        }
    } else {
        println!(
            "  {} Formatted {} files",
            style("✓").green(),
            files.len()
        );
    }

    Ok(())
}

fn run_with_command(cmd_name: &str, files: &[PathBuf], check: bool) -> Result<()> {
    let mut cmd = std::process::Command::new(cmd_name);

    if check {
        cmd.arg("--dry-run").arg("--set-exit-if-changed");
    } else {
        cmd.arg("--replace");
    }

    for f in files {
        cmd.arg(f);
    }

    let output = cmd.output()?;

    if check {
        if output.status.success() {
            println!("  {} All files are properly formatted", style("✓").green());
        } else {
            let stdout = String::from_utf8_lossy(&output.stdout);
            if !stdout.is_empty() {
                println!("  Files that need formatting:");
                for line in stdout.lines() {
                    println!("    {}", style(line).yellow());
                }
            }
            bail!("Some files are not properly formatted. Run 'ym fmt' to fix.");
        }
    } else {
        println!(
            "  {} Formatted {} files",
            style("✓").green(),
            files.len()
        );
    }

    Ok(())
}
