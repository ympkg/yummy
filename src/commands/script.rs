use anyhow::{bail, Result};
use console::style;

use crate::config;

/// Run a named script from ym.json "scripts" section.
/// With no arguments, lists all available scripts.
pub fn execute(name: Option<String>) -> Result<()> {
    let (config_path, cfg) = config::load_or_find_config()?;
    let project = config::project_dir(&config_path);

    let scripts = cfg.scripts.as_ref().cloned().unwrap_or_default();

    match name {
        None => {
            // List all scripts
            if scripts.is_empty() {
                println!("  No scripts defined in ym.json.");
                println!();
                println!("  Add scripts to ym.json:");
                println!(
                    "    {}",
                    style(r#""scripts": { "hello": "echo Hello!" }"#).dim()
                );
                return Ok(());
            }

            println!();
            println!("  {}", style("Available scripts:").bold());
            println!();
            for (name, cmd) in &scripts {
                println!(
                    "  {} {}  {}",
                    style("▸").blue(),
                    style(name).bold(),
                    style(cmd).dim()
                );
            }
            println!();
            println!("  Run with: {} <name>", style("ym run").cyan());
            println!();
            Ok(())
        }
        Some(name) => {
            let cmd = match scripts.get(&name) {
                Some(c) => c.clone(),
                None => {
                    let available: Vec<&String> = scripts.keys().collect();
                    if available.is_empty() {
                        bail!("Script '{}' not found. No scripts defined in ym.json.", name);
                    } else {
                        bail!(
                            "Script '{}' not found. Available: {}",
                            name,
                            available
                                .iter()
                                .map(|s| s.as_str())
                                .collect::<Vec<_>>()
                                .join(", ")
                        );
                    }
                }
            };

            println!(
                "  {} Running script: {} ({})",
                style("→").blue(),
                style(&name).bold(),
                style(&cmd).dim()
            );

            let shell = if cfg!(windows) { "cmd" } else { "sh" };
            let flag = if cfg!(windows) { "/C" } else { "-c" };

            let status = std::process::Command::new(shell)
                .arg(flag)
                .arg(&cmd)
                .current_dir(&project)
                .status()?;

            if !status.success() {
                bail!(
                    "Script '{}' failed with exit code {:?}",
                    name,
                    status.code()
                );
            }

            Ok(())
        }
    }
}
