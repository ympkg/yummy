use anyhow::Result;
use console::style;

use crate::config;
use crate::workspace::resolver;

/// Install all dependencies from ym.json.
pub fn execute() -> Result<()> {
    let (config_path, cfg) = config::load_or_find_config()?;
    let project = config::project_dir(&config_path);

    let mut deps = cfg.dependencies.clone().unwrap_or_default();
    if let Some(dev_deps) = &cfg.dev_dependencies {
        deps.extend(dev_deps.clone());
    }

    if deps.is_empty() {
        println!("  No dependencies to install.");
        return Ok(());
    }

    println!(
        "  {} Installing {} dependencies...",
        style("→").blue(),
        deps.len()
    );

    let cache = config::maven_cache_dir(&project);
    let lock_path = project.join(config::LOCK_FILE);
    let mut lock = config::load_lock(&lock_path)?;

    let registries: Vec<String> = cfg.registries.as_ref()
        .map(|r| r.values().cloned().collect())
        .unwrap_or_default();
    let exclusions = cfg.exclusions.clone().unwrap_or_default();

    let jars = resolver::resolve_and_download_full(
        &deps, &cache, &mut lock, &registries, &exclusions,
    )?;

    config::save_lock(&lock_path, &lock)?;

    println!(
        "  {} Installed {} artifacts",
        style("✓").green(),
        jars.len()
    );

    Ok(())
}
