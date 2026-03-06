use anyhow::Result;
use console::style;
use std::collections::HashSet;

use crate::config;

pub fn execute(max_depth: usize, json: bool, flat: bool) -> Result<()> {
    if json {
        return execute_json();
    }
    if flat {
        return execute_flat();
    }
    execute_text(max_depth)
}

fn execute_flat() -> Result<()> {
    let (config_path, cfg) = config::load_or_find_config()?;
    let project = config::project_dir(&config_path);

    let lock_path = project.join(config::LOCK_FILE);
    let lock = config::load_lock(&lock_path)?;

    let deps = cfg.dependencies.as_ref().cloned().unwrap_or_default();

    // Collect all unique dependencies (direct + transitive)
    let mut all_deps: std::collections::BTreeMap<String, String> = std::collections::BTreeMap::new();

    for (coord, version) in &deps {
        all_deps.insert(coord.clone(), version.clone());
        let key = format!("{}:{}", coord, version);
        collect_transitive(&lock, &key, &mut all_deps, &mut HashSet::new());
    }

    println!();
    println!(
        "  {} ({} total dependencies)",
        style(&cfg.name).bold(),
        all_deps.len()
    );
    println!();

    let direct_set: HashSet<_> = deps.keys().collect();

    for (coord, version) in &all_deps {
        let marker = if direct_set.contains(coord) { "●" } else { "○" };
        println!(
            "  {} {} {}",
            style(marker).dim(),
            style(coord).cyan(),
            style(version).dim()
        );
    }

    println!();
    println!(
        "  {} = direct, {} = transitive",
        style("●").dim(),
        style("○").dim()
    );
    println!();

    Ok(())
}

fn collect_transitive(
    lock: &config::schema::LockFile,
    key: &str,
    all: &mut std::collections::BTreeMap<String, String>,
    seen: &mut HashSet<String>,
) {
    if seen.contains(key) {
        return;
    }
    seen.insert(key.to_string());

    if let Some(locked) = lock.dependencies.get(key) {
        if let Some(ref deps) = locked.dependencies {
            for dep_key in deps {
                let parts: Vec<&str> = dep_key.split(':').collect();
                if parts.len() == 3 {
                    let coord = format!("{}:{}", parts[0], parts[1]);
                    all.entry(coord).or_insert_with(|| parts[2].to_string());
                }
                collect_transitive(lock, dep_key, all, seen);
            }
        }
    }
}

fn execute_json() -> Result<()> {
    let (config_path, cfg) = config::load_or_find_config()?;
    let project = config::project_dir(&config_path);

    let lock_path = project.join(config::LOCK_FILE);
    let lock = config::load_lock(&lock_path)?;

    let deps = cfg.dependencies.as_ref().cloned().unwrap_or_default();

    let mut tree = Vec::new();
    for (coord, version) in &deps {
        let key = format!("{}:{}", coord, version);
        let children = build_json_tree(&lock, &key, &mut HashSet::new());
        tree.push(serde_json::json!({
            "coordinate": coord,
            "version": version,
            "dependencies": children,
        }));
    }

    println!("{}", serde_json::to_string_pretty(&tree).unwrap_or_else(|_| "[]".to_string()));
    Ok(())
}

fn build_json_tree(
    lock: &config::schema::LockFile,
    key: &str,
    seen: &mut HashSet<String>,
) -> Vec<serde_json::Value> {
    if seen.contains(key) {
        return vec![];
    }
    seen.insert(key.to_string());

    let locked = match lock.dependencies.get(key) {
        Some(l) => l,
        None => return vec![],
    };

    let deps = match &locked.dependencies {
        Some(d) => d,
        None => return vec![],
    };

    deps.iter()
        .map(|dep_key| {
            let parts: Vec<&str> = dep_key.split(':').collect();
            let children = build_json_tree(lock, dep_key, seen);
            if parts.len() == 3 {
                serde_json::json!({
                    "coordinate": format!("{}:{}", parts[0], parts[1]),
                    "version": parts[2],
                    "dependencies": children,
                })
            } else {
                serde_json::json!({
                    "coordinate": dep_key,
                    "dependencies": children,
                })
            }
        })
        .collect()
}

fn execute_text(max_depth: usize) -> Result<()> {
    let (config_path, cfg) = config::load_or_find_config()?;
    let project = config::project_dir(&config_path);

    println!();
    println!(
        "  {} {}",
        style(&cfg.name).bold(),
        style(cfg.version.as_deref().unwrap_or("")).dim()
    );

    let lock_path = project.join(config::LOCK_FILE);
    let lock = config::load_lock(&lock_path)?;

    let deps = cfg.dependencies.as_ref().cloned().unwrap_or_default();
    let dep_count = deps.len();

    for (i, (coord, version)) in deps.iter().enumerate() {
        let is_last = i == dep_count - 1 && cfg.dev_dependencies.is_none();
        let prefix = if is_last { "  └── " } else { "  ├── " };
        let child_prefix = if is_last { "      " } else { "  │   " };

        println!("{}{} {}", prefix, style(coord).cyan(), style(version).dim());

        // Show transitive deps from lock file
        if max_depth != 1 {
            let key = format!("{}:{}", coord, version);
            if let Some(locked) = lock.dependencies.get(&key) {
                print_transitive(&lock, locked, child_prefix, &mut HashSet::new(), 2, max_depth);
            }
        }
    }

    if let Some(ref dev_deps) = cfg.dev_dependencies {
        if !dev_deps.is_empty() {
            println!("  │");
            println!("  {} (devDependencies)", style("▪").dim());

            let dev_count = dev_deps.len();
            for (i, (coord, version)) in dev_deps.iter().enumerate() {
                let is_last = i == dev_count - 1;
                let prefix = if is_last { "  └── " } else { "  ├── " };
                let child_prefix = if is_last { "      " } else { "  │   " };

                println!(
                    "{}{}  {}",
                    prefix,
                    style(coord).cyan(),
                    style(version).dim()
                );

                if max_depth != 1 {
                    let key = format!("{}:{}", coord, version);
                    if let Some(locked) = lock.dependencies.get(&key) {
                        print_transitive(&lock, locked, child_prefix, &mut HashSet::new(), 2, max_depth);
                    }
                }
            }
        }
    }

    println!();
    Ok(())
}

fn print_transitive(
    lock: &config::schema::LockFile,
    locked: &config::schema::LockedDependency,
    prefix: &str,
    seen: &mut HashSet<String>,
    current_depth: usize,
    max_depth: usize,
) {
    // max_depth == 0 means unlimited
    if max_depth > 0 && current_depth > max_depth {
        return;
    }

    let deps = match &locked.dependencies {
        Some(d) => d,
        None => return,
    };

    let count = deps.len();
    for (i, dep_key) in deps.iter().enumerate() {
        let is_last = i == count - 1;
        let connector = if is_last { "└── " } else { "├── " };
        let child_prefix = if is_last {
            format!("{}    ", prefix)
        } else {
            format!("{}│   ", prefix)
        };

        // Parse dep key: group:artifact:version
        let parts: Vec<&str> = dep_key.split(':').collect();
        let display = if parts.len() == 3 {
            format!("{}:{} {}", parts[0], parts[1], style(parts[2]).dim())
        } else {
            dep_key.clone()
        };

        if seen.contains(dep_key) {
            println!("{}{}{} {}", prefix, connector, display, style("(deduped)").dim());
            continue;
        }
        seen.insert(dep_key.clone());

        println!("{}{}{}", prefix, connector, display);

        // Recurse
        if seen.len() < 100 {
            if let Some(child_locked) = lock.dependencies.get(dep_key) {
                print_transitive(lock, child_locked, &child_prefix, seen, current_depth + 1, max_depth);
            }
        }
    }
}
