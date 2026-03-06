use anyhow::{bail, Result};
use console::style;
use std::path::PathBuf;

use crate::config;

pub fn execute(target: Option<String>, list: bool, unlink: bool) -> Result<()> {
    if list {
        return list_links();
    }

    if unlink {
        let name = target.as_deref().unwrap_or_else(|| {
            eprintln!("  Usage: ym link --unlink <package-name>");
            std::process::exit(1);
        });
        return unlink_package(name);
    }

    let (config_path, cfg) = config::load_or_find_config()?;
    let project = config::project_dir(&config_path);

    match target {
        None => register_link(&project, &cfg),
        Some(ref name) => create_link(&project, name),
    }
}

/// List all globally registered links.
fn list_links() -> Result<()> {
    let link_dir = global_link_dir();
    if !link_dir.exists() {
        println!("  No linked packages");
        return Ok(());
    }

    let entries: Vec<_> = std::fs::read_dir(&link_dir)?
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("json"))
        .collect();

    if entries.is_empty() {
        println!("  No linked packages");
        return Ok(());
    }

    println!();
    for entry in &entries {
        let content = std::fs::read_to_string(entry.path()).unwrap_or_default();
        if let Ok(info) = serde_json::from_str::<serde_json::Value>(&content) {
            let name = info["name"].as_str().unwrap_or("?");
            let path = info["path"].as_str().unwrap_or("?");
            let version = info["version"].as_str().unwrap_or("-");
            println!(
                "  {} {} {} -> {}",
                style("→").blue(),
                style(name).bold(),
                style(version).dim(),
                style(path).dim()
            );
        }
    }
    println!();
    println!("  {} linked package(s)", entries.len());

    Ok(())
}

/// Remove a global link registration.
fn unlink_package(name: &str) -> Result<()> {
    let link_dir = global_link_dir();
    let link_file = link_dir.join(format!("{}.json", name));

    if !link_file.exists() {
        println!(
            "  {} Package '{}' is not linked",
            style("!").yellow(),
            name
        );
        return Ok(());
    }

    std::fs::remove_file(&link_file)?;
    println!(
        "  {} Unlinked {}",
        style("✓").green(),
        style(name).bold()
    );

    Ok(())
}

/// Register the current package in the global link directory.
fn register_link(
    project: &std::path::Path,
    cfg: &config::schema::YmConfig,
) -> Result<()> {
    let link_dir = global_link_dir();
    std::fs::create_dir_all(&link_dir)?;

    let link_file = link_dir.join(format!("{}.json", cfg.name));
    let link_info = serde_json::json!({
        "name": cfg.name,
        "path": project.to_string_lossy(),
        "version": cfg.version,
    });

    std::fs::write(&link_file, serde_json::to_string_pretty(&link_info)?)?;

    println!(
        "  {} Registered {} for linking",
        style("✓").green(),
        style(&cfg.name).bold()
    );
    println!(
        "    In another project, run: {} {}",
        style("ym link").cyan(),
        &cfg.name
    );

    Ok(())
}

/// Create a link to a globally registered package.
fn create_link(project: &std::path::Path, name: &str) -> Result<()> {
    let link_dir = global_link_dir();
    let link_file = link_dir.join(format!("{}.json", name));

    if !link_file.exists() {
        bail!(
            "Package '{}' is not linked. Run 'ym link' in the {} project first.",
            name,
            name
        );
    }

    let content = std::fs::read_to_string(&link_file)?;
    let info: serde_json::Value = serde_json::from_str(&content)?;
    let source_path = info["path"]
        .as_str()
        .unwrap_or("")
        .to_string();

    if source_path.is_empty() {
        bail!("Invalid link file for '{}'", name);
    }

    let links_dir = project.join(".ym").join("links");
    std::fs::create_dir_all(&links_dir)?;

    let link_target = links_dir.join(name);

    if link_target.exists() {
        std::fs::remove_file(&link_target).or_else(|_| std::fs::remove_dir_all(&link_target))?;
    }

    #[cfg(unix)]
    std::os::unix::fs::symlink(&source_path, &link_target)?;

    #[cfg(windows)]
    std::os::windows::fs::symlink_dir(&source_path, &link_target)?;

    println!(
        "  {} Linked {} -> {}",
        style("✓").green(),
        style(name).bold(),
        style(&source_path).dim()
    );

    Ok(())
}

fn global_link_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home).join(".ym").join("links")
}
