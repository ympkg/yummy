use anyhow::{bail, Result};
use console::style;

use crate::config;

pub fn execute(dep: &str, unpin: bool) -> Result<()> {
    let (config_path, mut cfg) = config::load_or_find_config()?;

    let resolutions = cfg.resolutions.get_or_insert_with(Default::default);

    if unpin {
        if resolutions.remove(dep).is_some() {
            config::save_config(&config_path, &cfg)?;
            println!(
                "  {} Unpinned {}",
                style("✓").green(),
                style(dep).cyan()
            );
        } else {
            println!(
                "  {} {} is not pinned",
                style("!").yellow(),
                dep
            );
        }
        return Ok(());
    }

    // Find the current version of this dependency
    let deps = cfg.dependencies.as_ref().cloned().unwrap_or_default();
    let dev_deps = cfg.dev_dependencies.as_ref().cloned().unwrap_or_default();

    let version = deps
        .get(dep)
        .or_else(|| dev_deps.get(dep));

    match version {
        Some(v) => {
            resolutions.insert(dep.to_string(), v.clone());
            config::save_config(&config_path, &cfg)?;
            println!(
                "  {} Pinned {} to {}",
                style("✓").green(),
                style(dep).cyan(),
                style(v).bold()
            );
            println!(
                "  {} This version won't be changed by 'ym upgrade'",
                style("→").dim()
            );
        }
        None => {
            bail!(
                "Dependency '{}' not found. Use groupId:artifactId format.",
                dep
            );
        }
    }

    Ok(())
}
