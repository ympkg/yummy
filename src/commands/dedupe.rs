use anyhow::Result;
use console::style;
use std::collections::BTreeMap;

use crate::config;

pub fn execute(dry_run: bool) -> Result<()> {
    let (config_path, _cfg) = config::load_or_find_config()?;
    let project = config::project_dir(&config_path);

    let lock_path = project.join(config::LOCK_FILE);
    let mut lock = config::load_lock(&lock_path)?;

    if lock.dependencies.is_empty() {
        println!("  No dependencies in lock file.");
        return Ok(());
    }

    // Group by groupId:artifactId
    let mut versions_map: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();
    for key in lock.dependencies.keys() {
        let parts: Vec<&str> = key.split(':').collect();
        if parts.len() == 3 {
            let ga = format!("{}:{}", parts[0], parts[1]);
            versions_map
                .entry(ga)
                .or_default()
                .push((parts[2].to_string(), key.clone()));
        }
    }

    let mut deduped_count = 0;

    for (ga, versions) in &versions_map {
        if versions.len() <= 1 {
            continue;
        }

        // Keep the highest version, remove the rest
        let mut sorted: Vec<_> = versions.clone();
        sorted.sort_by(|a, b| version_cmp(&b.0, &a.0));

        let keep = &sorted[0];
        let remove: Vec<_> = sorted[1..].to_vec();

        if dry_run {
            println!(
                "  {} {} — keep {}, remove {}",
                style("~").yellow(),
                style(ga).bold(),
                style(&keep.0).green(),
                remove
                    .iter()
                    .map(|(v, _)| style(v).red().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        } else {
            for (_, key) in &remove {
                lock.dependencies.remove(key);
            }

            // Update any dependency references pointing to removed versions
            let remove_keys: Vec<String> = remove.iter().map(|(_, k)| k.clone()).collect();
            for entry in lock.dependencies.values_mut() {
                if let Some(ref mut deps) = entry.dependencies {
                    for dep_key in deps.iter_mut() {
                        for (_, old_key) in &remove {
                            if dep_key == old_key {
                                *dep_key = keep.1.clone();
                            }
                        }
                    }
                    deps.retain(|d| !remove_keys.contains(d));
                    deps.dedup();
                }
            }

            println!(
                "  {} {} — kept {}, removed {} duplicate(s)",
                style("✓").green(),
                style(ga).bold(),
                style(&keep.0).green(),
                remove.len()
            );
        }

        deduped_count += remove.len();
    }

    if deduped_count == 0 {
        println!("  {} No duplicate dependencies found.", style("✓").green());
    } else if dry_run {
        println!(
            "\n  {} {} duplicate(s) found. Run without --dry-run to apply.",
            style("!").yellow(),
            deduped_count
        );
    } else {
        config::save_lock(&lock_path, &lock)?;
        println!(
            "\n  {} Removed {} duplicate(s) from lock file.",
            style("✓").green(),
            deduped_count
        );
    }

    Ok(())
}

/// Simple version comparison: split on '.'/'-' and compare segments
fn version_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    let parse = |s: &str| -> Vec<u64> {
        s.split(|c: char| c == '.' || c == '-')
            .filter_map(|part| part.parse::<u64>().ok())
            .collect()
    };
    let va = parse(a);
    let vb = parse(b);
    va.cmp(&vb)
}
