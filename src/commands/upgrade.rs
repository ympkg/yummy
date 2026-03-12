use anyhow::Result;
use console::style;

use crate::config;
use crate::config::schema::DependencyValue;
use crate::workspace::resolver;

pub fn execute(interactive: bool, yes: bool, json: bool) -> Result<()> {
    let (config_path, mut cfg) = config::load_or_find_config()?;

    // Check for pinned versions (resolutions)
    let pinned = cfg.resolutions.as_ref().cloned().unwrap_or_default();

    // Collect all upgradable Maven dependencies
    let mut candidates: Vec<UpgradeCandidate> = Vec::new();

    if let Some(ref deps) = cfg.dependencies {
        for (coord, value) in deps {
            // Skip workspace refs and non-Maven deps
            if !crate::config::schema::is_maven_dep(coord) || value.is_workspace() {
                continue;
            }
            if pinned.contains_key(coord) {
                continue;
            }
            let current = match value.version() {
                Some(v) => v.to_string(),
                None => continue,
            };
            let resolved = cfg.resolve_key(coord);
            let parts: Vec<&str> = resolved.split(':').collect();
            if parts.len() != 2 {
                continue;
            }
            match resolver::fetch_latest_version(parts[0], parts[1]) {
                Ok(latest) if latest != current => {
                    candidates.push(UpgradeCandidate {
                        coordinate: coord.clone(),
                        current,
                        latest,
                    });
                }
                _ => {}
            }
        }
    }

    if candidates.is_empty() {
        if !json {
            println!("  {} All dependencies are up to date!", style("✓").green());
            if !pinned.is_empty() {
                println!(
                    "  {} {} pinned dependenc{} skipped",
                    style("→").dim(),
                    pinned.len(),
                    if pinned.len() == 1 { "y" } else { "ies" }
                );
            }
        }
        if json {
            println!("[]");
        }
        return Ok(());
    }

    // JSON output mode (no modification)
    if json {
        let json_output: Vec<serde_json::Value> = candidates
            .iter()
            .map(|c| serde_json::json!({
                "coordinate": c.coordinate,
                "current": c.current,
                "latest": c.latest,
            }))
            .collect();
        println!("{}", serde_json::to_string_pretty(&json_output)?);
        return Ok(());
    }

    // Print preview
    println!();
    println!("  {:<55} {:>10}  {:>10}", "Package", "Current", "Latest");
    for c in &candidates {
        println!(
            "  {:<55} {:>10}  {} {:>10}",
            style(&c.coordinate).cyan(),
            style(&c.current).dim(),
            style("→").dim(),
            style(&c.latest).green()
        );
    }
    println!();

    let selected = if interactive {
        select_interactively(&candidates)?
    } else if yes {
        (0..candidates.len()).collect()
    } else {
        // Non-interactive without -y: check TTY
        use std::io::IsTerminal;
        if !std::io::stdin().is_terminal() {
            anyhow::bail!("Use -y to upgrade all in non-interactive mode");
        }

        // Show confirmation
        let confirm = dialoguer::Confirm::new()
            .with_prompt(format!("  Upgrade {} dependencies?", candidates.len()))
            .default(false)
            .interact()?;

        if confirm {
            (0..candidates.len()).collect()
        } else {
            return Ok(());
        }
    };

    if selected.is_empty() {
        println!("  No dependencies selected for upgrade.");
        return Ok(());
    }

    let mut updated = 0;

    for idx in &selected {
        let c = &candidates[*idx];
        if let Some(ref mut deps) = cfg.dependencies {
            if let Some(value) = deps.get_mut(&c.coordinate) {
                match value {
                    DependencyValue::Simple(v) => {
                        *v = c.latest.clone();
                    }
                    DependencyValue::Detailed(spec) => {
                        spec.version = Some(c.latest.clone());
                    }
                }
                println!(
                    "  {} {} {} → {}",
                    style("↑").green(),
                    style(&c.coordinate).cyan(),
                    style(&c.current).dim(),
                    style(&c.latest).green()
                );
                updated += 1;
            }
        }
    }

    if updated > 0 {
        config::save_config(&config_path, &cfg)?;
        println!();
        println!(
            "  {} Upgraded {} dependenc{}",
            style("✓").green(),
            updated,
            if updated == 1 { "y" } else { "ies" }
        );
    }

    Ok(())
}

struct UpgradeCandidate {
    coordinate: String,
    current: String,
    latest: String,
}

fn select_interactively(candidates: &[UpgradeCandidate]) -> Result<Vec<usize>> {
    let items: Vec<String> = candidates
        .iter()
        .map(|c| {
            format!(
                "{} {} → {}",
                c.coordinate, c.current, c.latest
            )
        })
        .collect();

    let defaults = vec![true; items.len()];

    let selected = dialoguer::MultiSelect::new()
        .with_prompt("Select dependencies to upgrade")
        .items(&items)
        .defaults(&defaults)
        .interact()?;

    Ok(selected)
}
