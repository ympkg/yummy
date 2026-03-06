use anyhow::Result;
use console::style;

use crate::config;

pub fn execute(json: bool, outdated_only: bool) -> Result<()> {
    let (config_path, cfg) = config::load_or_find_config()?;
    let project = config::project_dir(&config_path);

    let lock_path = project.join(config::LOCK_FILE);
    let lock = config::load_lock(&lock_path)?;

    let direct_deps = cfg.dependencies.as_ref().cloned().unwrap_or_default();
    let dev_deps = cfg.dev_dependencies.as_ref().cloned().unwrap_or_default();

    if outdated_only {
        return print_outdated(&direct_deps, &dev_deps, json);
    }

    if json {
        print_json(&lock, &direct_deps, &dev_deps)?;
    } else {
        print_flat(&lock, &direct_deps, &dev_deps);
    }

    Ok(())
}

fn print_outdated(
    direct: &std::collections::BTreeMap<String, String>,
    dev: &std::collections::BTreeMap<String, String>,
    json: bool,
) -> Result<()> {
    let mut entries: Vec<serde_json::Value> = Vec::new();

    for (coord, version) in direct.iter().chain(dev.iter()) {
        let parts: Vec<&str> = coord.split(':').collect();
        if parts.len() != 2 {
            continue;
        }
        match crate::workspace::resolver::fetch_latest_version(parts[0], parts[1]) {
            Ok(latest) if latest != *version => {
                if json {
                    entries.push(serde_json::json!({
                        "coordinate": coord,
                        "current": version,
                        "latest": latest,
                    }));
                } else {
                    println!(
                        "  {} {} → {}",
                        style(coord).cyan(),
                        style(version).yellow(),
                        style(&latest).green()
                    );
                }
            }
            _ => {}
        }
    }

    if json {
        println!("{}", serde_json::to_string_pretty(&entries)?);
    } else if entries.is_empty() && !json {
        // Check if we printed anything (non-json mode)
        // In non-json mode we printed directly, so just show summary
    }

    Ok(())
}

fn print_flat(
    lock: &config::schema::LockFile,
    direct: &std::collections::BTreeMap<String, String>,
    dev: &std::collections::BTreeMap<String, String>,
) {
    // Build sets of direct dep keys
    let direct_keys: std::collections::HashSet<String> = direct
        .iter()
        .map(|(k, v)| format!("{}:{}", k, v))
        .collect();
    let dev_keys: std::collections::HashSet<String> = dev
        .iter()
        .map(|(k, v)| format!("{}:{}", k, v))
        .collect();

    let total = lock.dependencies.len();
    let direct_count = direct.len();
    let dev_count = dev.len();
    let transitive_count = total.saturating_sub(direct_count + dev_count);

    println!();
    for key in lock.dependencies.keys() {
        let kind = if direct_keys.contains(key) {
            style("direct").green()
        } else if dev_keys.contains(key) {
            style("dev").yellow()
        } else {
            style("transitive").dim()
        };
        println!("  {} {}", kind, key);
    }

    println!();
    println!(
        "  {} total: {} ({} direct, {} dev, {} transitive)",
        style("■").cyan(),
        total,
        direct_count,
        dev_count,
        transitive_count
    );
    println!();
}

fn print_json(
    lock: &config::schema::LockFile,
    direct: &std::collections::BTreeMap<String, String>,
    dev: &std::collections::BTreeMap<String, String>,
) -> Result<()> {
    let direct_keys: std::collections::HashSet<String> = direct
        .iter()
        .map(|(k, v)| format!("{}:{}", k, v))
        .collect();
    let dev_keys: std::collections::HashSet<String> = dev
        .iter()
        .map(|(k, v)| format!("{}:{}", k, v))
        .collect();

    let entries: Vec<serde_json::Value> = lock
        .dependencies
        .iter()
        .map(|(key, dep)| {
            let kind = if direct_keys.contains(key) {
                "direct"
            } else if dev_keys.contains(key) {
                "dev"
            } else {
                "transitive"
            };
            let parts: Vec<&str> = key.split(':').collect();
            let mut obj = serde_json::json!({
                "coordinate": key,
                "type": kind,
            });
            if parts.len() == 3 {
                obj["groupId"] = serde_json::json!(parts[0]);
                obj["artifactId"] = serde_json::json!(parts[1]);
                obj["version"] = serde_json::json!(parts[2]);
            }
            if let Some(ref sha) = dep.sha256 {
                obj["sha256"] = serde_json::json!(sha);
            }
            obj
        })
        .collect();

    println!("{}", serde_json::to_string_pretty(&entries)?);
    Ok(())
}
