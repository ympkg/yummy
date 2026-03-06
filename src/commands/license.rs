use anyhow::Result;
use console::style;
use std::collections::BTreeMap;

use crate::config;

pub fn execute(json: bool) -> Result<()> {
    let (config_path, _cfg) = config::load_or_find_config()?;
    let project = config::project_dir(&config_path);

    let lock_path = project.join(config::LOCK_FILE);
    let lock = config::load_lock(&lock_path)?;
    let cache = config::maven_cache_dir(&project);

    if lock.dependencies.is_empty() {
        println!("  No dependencies found. Run 'ym build' first.");
        return Ok(());
    }

    let mut license_map: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut entries: Vec<LicenseEntry> = Vec::new();

    for key in lock.dependencies.keys() {
        let parts: Vec<&str> = key.split(':').collect();
        if parts.len() != 3 {
            continue;
        }
        let (group, artifact, version) = (parts[0], parts[1], parts[2]);

        let pom_path = cache
            .join(group)
            .join(artifact)
            .join(version)
            .join(format!("{}-{}.pom", artifact, version));

        let license = if pom_path.exists() {
            extract_license_from_pom(&pom_path)
        } else {
            "Unknown".to_string()
        };

        license_map
            .entry(license.clone())
            .or_default()
            .push(key.clone());

        entries.push(LicenseEntry {
            coordinate: key.clone(),
            license,
        });
    }

    if json {
        let json_entries: Vec<serde_json::Value> = entries
            .iter()
            .map(|e| {
                serde_json::json!({
                    "coordinate": e.coordinate,
                    "license": e.license,
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&json_entries)?);
        return Ok(());
    }

    // Group by license
    println!();
    for (license, deps) in &license_map {
        let color = license_color(license);
        println!(
            "  {} ({} packages)",
            color,
            deps.len()
        );
        for dep in deps {
            println!("    {}", style(dep).dim());
        }
        println!();
    }

    // Summary
    let total = entries.len();
    let unknown = license_map.get("Unknown").map(|v| v.len()).unwrap_or(0);
    println!(
        "  {} {} dependencies, {} license types, {} unknown",
        style("■").cyan(),
        total,
        license_map.len(),
        unknown
    );
    println!();

    Ok(())
}

struct LicenseEntry {
    coordinate: String,
    license: String,
}

fn extract_license_from_pom(pom_path: &std::path::Path) -> String {
    let content = match std::fs::read_to_string(pom_path) {
        Ok(c) => c,
        Err(_) => return "Unknown".to_string(),
    };

    let doc = match roxmltree::Document::parse(&content) {
        Ok(d) => d,
        Err(_) => return "Unknown".to_string(),
    };

    // Look for <licenses><license><name>...</name></license></licenses>
    for node in doc.descendants() {
        if node.tag_name().name() == "licenses" {
            for license_node in node.children() {
                if license_node.tag_name().name() == "license" {
                    for child in license_node.children() {
                        if child.tag_name().name() == "name" {
                            if let Some(name) = child.text() {
                                return normalize_license(name.trim());
                            }
                        }
                    }
                }
            }
        }
    }

    "Unknown".to_string()
}

fn normalize_license(name: &str) -> String {
    let lower = name.to_lowercase();
    if lower.contains("apache") && lower.contains("2") {
        "Apache-2.0".to_string()
    } else if lower.contains("mit") {
        "MIT".to_string()
    } else if lower.contains("bsd") && lower.contains("3") {
        "BSD-3-Clause".to_string()
    } else if lower.contains("bsd") && lower.contains("2") {
        "BSD-2-Clause".to_string()
    } else if lower.contains("lgpl") && lower.contains("2.1") {
        "LGPL-2.1".to_string()
    } else if lower.contains("lgpl") && lower.contains("3") {
        "LGPL-3.0".to_string()
    } else if lower.contains("gpl") && lower.contains("3") {
        "GPL-3.0".to_string()
    } else if lower.contains("gpl") && lower.contains("2") {
        "GPL-2.0".to_string()
    } else if lower.contains("epl") && lower.contains("2") {
        "EPL-2.0".to_string()
    } else if lower.contains("epl") || lower.contains("eclipse") {
        "EPL-1.0".to_string()
    } else if lower.contains("cddl") {
        "CDDL-1.0".to_string()
    } else if lower.contains("public domain") || lower.contains("cc0") {
        "Public Domain".to_string()
    } else {
        name.to_string()
    }
}

fn license_color(license: &str) -> console::StyledObject<&str> {
    match license {
        "Apache-2.0" | "MIT" | "BSD-3-Clause" | "BSD-2-Clause" | "Public Domain" => {
            style(license).green().bold()
        }
        "LGPL-2.1" | "LGPL-3.0" | "EPL-1.0" | "EPL-2.0" | "CDDL-1.0" => {
            style(license).yellow().bold()
        }
        "GPL-2.0" | "GPL-3.0" => style(license).red().bold(),
        "Unknown" => style(license).dim().bold(),
        _ => style(license).cyan().bold(),
    }
}
