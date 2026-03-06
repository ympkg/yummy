use anyhow::{bail, Result};
use console::style;

use crate::config;

pub fn execute(key: Option<String>, value: Option<String>) -> Result<()> {
    let (config_path, mut cfg) = config::load_or_find_config()?;

    match (key, value) {
        (None, _) => {
            // Show all config
            let json = serde_json::to_string_pretty(&cfg)?;
            println!("{}", json);
        }
        (Some(key), None) => {
            // Get a specific key
            let val = get_config_value(&cfg, &key);
            match val {
                Some(v) => println!("{}", v),
                None => {
                    eprintln!(
                        "  {} Key '{}' not found in ym.json",
                        style("!").yellow(),
                        key
                    );
                    std::process::exit(1);
                }
            }
        }
        (Some(key), Some(value)) => {
            // Set a specific key
            set_config_value(&mut cfg, &key, &value)?;
            config::save_config(&config_path, &cfg)?;
            println!(
                "  {} Set {} = {}",
                style("✓").green(),
                style(&key).bold(),
                style(&value).cyan()
            );
        }
    }

    Ok(())
}

fn get_config_value(cfg: &config::schema::YmConfig, key: &str) -> Option<String> {
    match key {
        "name" => Some(cfg.name.clone()),
        "version" => cfg.version.clone(),
        "target" => cfg.target.clone(),
        "main" => cfg.main.clone(),
        "private" => cfg.private.map(|b| b.to_string()),
        "sourceDir" => cfg.source_dir.clone(),
        "testDir" => cfg.test_dir.clone(),
        "compiler.engine" => cfg.compiler.as_ref().and_then(|c| c.engine.clone()),
        "compiler.encoding" => cfg.compiler.as_ref().and_then(|c| c.encoding.clone()),
        "jvm.vendor" => cfg.jvm.as_ref().and_then(|j| j.vendor.clone()),
        "jvm.version" => cfg.jvm.as_ref().and_then(|j| j.version.clone()),
        "jvm.autoDownload" => cfg
            .jvm
            .as_ref()
            .and_then(|j| j.auto_download)
            .map(|b| b.to_string()),
        "hotReload.enabled" => cfg
            .hot_reload
            .as_ref()
            .and_then(|h| h.enabled)
            .map(|b| b.to_string()),
        _ => None,
    }
}

fn set_config_value(cfg: &mut config::schema::YmConfig, key: &str, value: &str) -> Result<()> {
    match key {
        "name" => cfg.name = value.to_string(),
        "version" => cfg.version = Some(value.to_string()),
        "target" => cfg.target = Some(value.to_string()),
        "main" => cfg.main = Some(value.to_string()),
        "private" => cfg.private = Some(value.parse().unwrap_or(false)),
        "sourceDir" => cfg.source_dir = Some(value.to_string()),
        "testDir" => cfg.test_dir = Some(value.to_string()),
        "compiler.engine" => {
            let compiler = cfg.compiler.get_or_insert_with(|| config::schema::CompilerConfig {
                engine: None,
                encoding: None,
                annotation_processors: None,
                lint: None,
                args: None,
            });
            compiler.engine = Some(value.to_string());
        }
        "compiler.encoding" => {
            let compiler = cfg.compiler.get_or_insert_with(|| config::schema::CompilerConfig {
                engine: None,
                encoding: None,
                annotation_processors: None,
                lint: None,
                args: None,
            });
            compiler.encoding = Some(value.to_string());
        }
        "jvm.version" => {
            let jvm = cfg.jvm.get_or_insert_with(|| config::schema::JvmConfig {
                vendor: None,
                version: None,
                auto_download: None,
            });
            jvm.version = Some(value.to_string());
        }
        "jvm.vendor" => {
            let jvm = cfg.jvm.get_or_insert_with(|| config::schema::JvmConfig {
                vendor: None,
                version: None,
                auto_download: None,
            });
            jvm.vendor = Some(value.to_string());
        }
        _ => bail!("Unknown config key: '{}'", key),
    }
    Ok(())
}
