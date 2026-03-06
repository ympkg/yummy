use anyhow::{bail, Result};
use console::style;

use crate::config;

pub fn execute(dep: &str, from_dev: bool) -> Result<()> {
    let (config_path, mut cfg) = config::load_or_find_config()?;

    if from_dev {
        return remove_from_section(&config_path, &mut cfg, dep, true);
    }

    // Try dependencies first, then devDependencies
    if try_remove(&mut cfg.dependencies, dep).is_some() {
        config::save_config(&config_path, &cfg)?;
        return Ok(());
    }

    if try_remove(&mut cfg.dev_dependencies, dep).is_some() {
        config::save_config(&config_path, &cfg)?;
        return Ok(());
    }

    // Try fuzzy match by artifact name
    let all_keys: Vec<(String, bool)> = cfg
        .dependencies
        .as_ref()
        .map(|d| d.keys().map(|k| (k.clone(), false)).collect::<Vec<_>>())
        .unwrap_or_default()
        .into_iter()
        .chain(
            cfg.dev_dependencies
                .as_ref()
                .map(|d| d.keys().map(|k| (k.clone(), true)).collect::<Vec<_>>())
                .unwrap_or_default(),
        )
        .collect();

    let matching: Vec<&(String, bool)> = all_keys
        .iter()
        .filter(|(k, _)| k.split(':').last().is_some_and(|a| a == dep))
        .collect();

    if matching.len() == 1 {
        let (key, is_dev) = matching[0];
        return remove_from_section(&config_path, &mut cfg, key, *is_dev);
    }

    if matching.len() > 1 {
        bail!(
            "Multiple dependencies match '{}': {}. Use full coordinate.",
            dep,
            matching.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>().join(", ")
        );
    }

    bail!("Dependency '{}' not found in ym.json", dep);
}

fn remove_from_section(
    config_path: &std::path::Path,
    cfg: &mut config::schema::YmConfig,
    dep: &str,
    from_dev: bool,
) -> Result<()> {
    let section = if from_dev {
        &mut cfg.dev_dependencies
    } else {
        &mut cfg.dependencies
    };

    if let Some(result) = try_remove(section, dep) {
        config::save_config(config_path, cfg)?;
        let label = if from_dev { " (dev)" } else { "" };
        println!(
            "  {} Removed {} {}{}",
            style("✓").green(),
            style(&result.0).cyan(),
            style(&result.1).dim(),
            label
        );
        Ok(())
    } else {
        let label = if from_dev { "devDependencies" } else { "dependencies" };
        bail!("Dependency '{}' not found in {}", dep, label);
    }
}

fn try_remove(
    section: &mut Option<std::collections::BTreeMap<String, String>>,
    dep: &str,
) -> Option<(String, String)> {
    let map = section.as_mut()?;

    // Exact match
    if let Some(version) = map.remove(dep) {
        println!(
            "  {} Removed {} {}",
            style("✓").green(),
            style(dep).cyan(),
            style(&version).dim()
        );
        return Some((dep.to_string(), version));
    }

    // Match by artifact name
    let key = map
        .keys()
        .find(|k| k.split(':').last().is_some_and(|a| a == dep))
        .cloned();

    if let Some(key) = key {
        let version = map.remove(&key).unwrap();
        return Some((key, version));
    }

    None
}
