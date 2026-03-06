use anyhow::Result;
use console::style;

use crate::config;
use crate::workspace::resolver;

pub fn execute(interactive: bool) -> Result<()> {
    let (config_path, mut cfg) = config::load_or_find_config()?;

    // Check for pinned versions (resolutions)
    let pinned = cfg.resolutions.as_ref().cloned().unwrap_or_default();

    // Collect all upgradable dependencies
    let mut candidates: Vec<UpgradeCandidate> = Vec::new();

    if let Some(ref deps) = cfg.dependencies {
        for (coord, version) in deps {
            if pinned.contains_key(coord) {
                continue; // Skip pinned deps
            }
            let parts: Vec<&str> = coord.split(':').collect();
            if parts.len() != 2 {
                continue;
            }
            match resolver::fetch_latest_version(parts[0], parts[1]) {
                Ok(latest) if latest != *version => {
                    candidates.push(UpgradeCandidate {
                        coordinate: coord.clone(),
                        current: version.clone(),
                        latest,
                        dev: false,
                    });
                }
                _ => {}
            }
        }
    }

    if let Some(ref deps) = cfg.dev_dependencies {
        for (coord, version) in deps {
            if pinned.contains_key(coord) {
                continue;
            }
            let parts: Vec<&str> = coord.split(':').collect();
            if parts.len() != 2 {
                continue;
            }
            match resolver::fetch_latest_version(parts[0], parts[1]) {
                Ok(latest) if latest != *version => {
                    candidates.push(UpgradeCandidate {
                        coordinate: coord.clone(),
                        current: version.clone(),
                        latest,
                        dev: true,
                    });
                }
                _ => {}
            }
        }
    }

    if candidates.is_empty() {
        println!("  {} All dependencies are up to date!", style("✓").green());
        if !pinned.is_empty() {
            println!(
                "  {} {} pinned dependenc{} skipped",
                style("→").dim(),
                pinned.len(),
                if pinned.len() == 1 { "y" } else { "ies" }
            );
        }
        return Ok(());
    }

    let selected = if interactive {
        select_interactively(&candidates)?
    } else {
        (0..candidates.len()).collect()
    };

    if selected.is_empty() {
        println!("  No dependencies selected for upgrade.");
        return Ok(());
    }

    let mut updated = 0;

    for idx in &selected {
        let c = &candidates[*idx];
        if c.dev {
            if let Some(ref mut deps) = cfg.dev_dependencies {
                if let Some(v) = deps.get_mut(&c.coordinate) {
                    println!(
                        "  {} {} {} → {} (dev)",
                        style("↑").green(),
                        style(&c.coordinate).cyan(),
                        style(&c.current).dim(),
                        style(&c.latest).green()
                    );
                    *v = c.latest.clone();
                    updated += 1;
                }
            }
        } else if let Some(ref mut deps) = cfg.dependencies {
            if let Some(v) = deps.get_mut(&c.coordinate) {
                println!(
                    "  {} {} {} → {}",
                    style("↑").green(),
                    style(&c.coordinate).cyan(),
                    style(&c.current).dim(),
                    style(&c.latest).green()
                );
                *v = c.latest.clone();
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
    dev: bool,
}

fn select_interactively(candidates: &[UpgradeCandidate]) -> Result<Vec<usize>> {
    let items: Vec<String> = candidates
        .iter()
        .map(|c| {
            let suffix = if c.dev { " (dev)" } else { "" };
            format!(
                "{} {} → {}{}",
                c.coordinate, c.current, c.latest, suffix
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
