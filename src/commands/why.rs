use anyhow::{bail, Result};
use console::style;

use crate::config;

/// Explain why a dependency is included.
/// Shows the dependency chain from root deps to the target.
pub fn execute(dep: &str) -> Result<()> {
    let (config_path, cfg) = config::load_or_find_config()?;
    let project = config::project_dir(&config_path);

    let lock_path = project.join(config::LOCK_FILE);
    let lock = config::load_lock(&lock_path)?;

    if lock.dependencies.is_empty() {
        bail!("No lock file found. Run 'ym build' first.");
    }

    let dep_lower = dep.to_lowercase();

    // Find all lock entries matching the query
    let matches: Vec<&String> = lock
        .dependencies
        .keys()
        .filter(|k| k.to_lowercase().contains(&dep_lower))
        .collect();

    if matches.is_empty() {
        bail!(
            "Dependency '{}' not found in lock file. It may not be installed.",
            dep
        );
    }

    let deps = cfg.dependencies.as_ref().cloned().unwrap_or_default();
    let dev_deps = cfg.dev_dependencies.as_ref().cloned().unwrap_or_default();

    for matched in &matches {
        println!();
        println!("  {}", style(matched).cyan().bold());

        // Check if it's a direct dependency
        for (coord, version) in deps.iter().chain(dev_deps.iter()) {
            let key = format!("{}:{}", coord, version);
            if &key == *matched {
                let label = if dev_deps.contains_key(coord) {
                    "devDependencies"
                } else {
                    "dependencies"
                };
                println!(
                    "  └── directly declared in {} of ym.json",
                    style(label).green()
                );
            }
        }

        // Search for what pulls this in transitively
        let mut found_parent = false;
        for (parent_key, parent_locked) in &lock.dependencies {
            if let Some(ref children) = parent_locked.dependencies {
                if children.iter().any(|c| c == *matched) {
                    found_parent = true;
                    println!("  └── required by {}", style(parent_key).yellow());
                }
            }
        }

        if !found_parent {
            // It's a root dep with no parent in the graph
            let parts: Vec<&str> = matched.split(':').collect();
            if parts.len() >= 2 {
                let coord = format!("{}:{}", parts[0], parts[1]);
                if deps.contains_key(&coord) {
                    println!("  └── direct dependency (dependencies)");
                } else if dev_deps.contains_key(&coord) {
                    println!("  └── direct dependency (devDependencies)");
                }
            }
        }
    }

    println!();
    Ok(())
}
