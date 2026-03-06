use anyhow::Result;
use console::style;
use std::time::Instant;

use crate::config;
use crate::config::schema::LockFile;

/// Regenerate the lock file from scratch (ignoring existing lock).
pub fn execute(check: bool) -> Result<()> {
    if check {
        return check_lock_freshness();
    }
    regenerate_lock()
}

/// Check if lock file is up-to-date with ym.json dependencies.
/// Exit 1 if not — useful for CI pipelines.
fn check_lock_freshness() -> Result<()> {
    let (config_path, cfg) = config::load_or_find_config()?;
    let project = config::project_dir(&config_path);
    let lock_path = project.join(config::LOCK_FILE);

    if !lock_path.exists() {
        println!(
            "  {} Lock file missing. Run {} to generate.",
            style("✗").red(),
            style("ym lock").cyan()
        );
        std::process::exit(1);
    }

    let lock = config::load_lock(&lock_path)?;
    let deps = cfg.dependencies.as_ref().cloned().unwrap_or_default();

    // Check that every declared dep has a matching lock entry
    let mut missing = Vec::new();
    for (coord, version) in &deps {
        let key = format!("{}:{}", coord, version);
        if !lock.dependencies.contains_key(&key) {
            missing.push(format!("{}@{}", coord, version));
        }
    }

    if missing.is_empty() {
        println!(
            "  {} Lock file is up to date ({} entries)",
            style("✓").green(),
            lock.dependencies.len()
        );
        Ok(())
    } else {
        println!(
            "  {} Lock file is outdated. Missing entries:",
            style("✗").red()
        );
        for m in &missing {
            println!("    {} {}", style("-").red(), m);
        }
        println!();
        println!(
            "  Run {} to update",
            style("ym lock").cyan()
        );
        std::process::exit(1);
    }
}

fn regenerate_lock() -> Result<()> {
    let (config_path, cfg) = config::load_or_find_config()?;
    let project = config::project_dir(&config_path);

    let lock_path = project.join(config::LOCK_FILE);

    // Start fresh
    let mut lock = LockFile::default();

    let mut deps = cfg.dependencies.as_ref().cloned().unwrap_or_default();
    let mut registries: Vec<String> = Vec::new();

    // Inherit deps from workspace root
    if let Some(ws_root) = config::find_workspace_root(&project) {
        if ws_root != project {
            let root_config_path = ws_root.join(config::CONFIG_FILE);
            if let Ok(root_cfg) = config::load_config(&root_config_path) {
                if let Some(root_deps) = root_cfg.dependencies {
                    for (k, v) in root_deps {
                        deps.entry(k).or_insert(v);
                    }
                }
                if let Some(resolutions) = root_cfg.resolutions {
                    for (k, v) in resolutions {
                        if deps.contains_key(&k) {
                            deps.insert(k, v);
                        }
                    }
                }
                if let Some(regs) = root_cfg.registries {
                    registries.extend(regs.values().cloned());
                }
            }
        }
    }

    if let Some(regs) = &cfg.registries {
        for url in regs.values() {
            if !registries.contains(url) {
                registries.insert(0, url.clone());
            }
        }
    }

    if deps.is_empty() {
        println!("  No dependencies to resolve.");
        config::save_lock(&lock_path, &lock)?;
        return Ok(());
    }

    println!(
        "  {} Resolving {} dependencies...",
        style("→").blue(),
        deps.len()
    );

    let start = Instant::now();
    let cache = config::maven_cache_dir(&project);
    let exclusions = cfg.exclusions.as_ref().cloned().unwrap_or_default();

    let jars = crate::workspace::resolver::resolve_and_download_full(
        &deps, &cache, &mut lock, &registries, &exclusions,
    )?;

    config::save_lock(&lock_path, &lock)?;
    let elapsed = start.elapsed();

    println!(
        "  {} Generated {} with {} entries ({} JARs)       {}ms",
        style("✓").green(),
        style("ym.lock").bold(),
        lock.dependencies.len(),
        jars.len(),
        elapsed.as_millis()
    );

    Ok(())
}
