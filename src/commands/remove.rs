use anyhow::{bail, Result};
use console::style;
use dialoguer::Select;

use crate::config;

pub fn execute(dep: &str) -> Result<()> {
    let (config_path, mut cfg) = config::load_or_find_config()?;

    let deps = match cfg.dependencies.as_mut() {
        Some(d) => d,
        None => bail!("No dependencies in ym.json"),
    };

    // Exact match
    if let Some(value) = deps.remove(dep) {
        config::save_config(&config_path, &cfg)?;
        let version = value.version().unwrap_or("").to_string();
        println!(
            "  {} Removed {} {}",
            style("✓").green(),
            style(dep).cyan(),
            style(&version).dim()
        );
        return Ok(());
    }

    // Fuzzy match by artifactId (when input has no colon or @scope)
    if !crate::config::schema::is_maven_dep(dep) {
        let matching: Vec<String> = deps
            .keys()
            .filter(|k| crate::config::schema::artifact_id_from_key(k) == dep)
            .cloned()
            .collect();

        if matching.len() == 1 {
            let key = &matching[0];
            let value = deps.remove(key).unwrap();
            config::save_config(&config_path, &cfg)?;
            let version = value.version().unwrap_or("").to_string();
            println!(
                "  {} Removed {} {}",
                style("✓").green(),
                style(key).cyan(),
                style(&version).dim()
            );
            return Ok(());
        }

        if matching.len() > 1 {
            // Non-interactive: error with full coordinates
            if !std::io::IsTerminal::is_terminal(&std::io::stdin()) {
                bail!(
                    "Multiple dependencies match '{}': {}. Use full coordinate.",
                    dep,
                    matching.join(", ")
                );
            }
            // Interactive: let user choose
            let selection = Select::new()
                .with_prompt(format!("Multiple matches for '{}'. Select one to remove", dep))
                .items(&matching)
                .default(0)
                .interact()?;

            let key = &matching[selection];
            let value = deps.remove(key).unwrap();
            config::save_config(&config_path, &cfg)?;
            let version = value.version().unwrap_or("").to_string();
            println!(
                "  {} Removed {} {}",
                style("✓").green(),
                style(key).cyan(),
                style(&version).dim()
            );
            return Ok(());
        }
    }

    bail!("Dependency '{}' not found in package.toml", dep);
}
