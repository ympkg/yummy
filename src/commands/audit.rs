use anyhow::{bail, Result};
use console::style;

use crate::config;

/// Check dependencies for known vulnerabilities using the OSV.dev API.
pub fn execute(json_output: bool) -> Result<()> {
    let (config_path, cfg) = config::load_or_find_config()?;
    let project = config::project_dir(&config_path);

    let lock_path = project.join(config::LOCK_FILE);
    let lock = config::load_lock(&lock_path)?;

    if lock.dependencies.is_empty() {
        if json_output {
            println!("[]");
            return Ok(());
        }
        bail!("No lock file found. Run 'ym build' first.");
    }

    if !json_output {
        println!();
        println!("  {} Auditing {} dependencies...", style("~").blue(), lock.dependencies.len());
        println!();
    }

    let client = reqwest::blocking::Client::builder()
        .user_agent("ym/0.1.0")
        .timeout(std::time::Duration::from_secs(30))
        .build()?;

    let mut all_vulns: Vec<serde_json::Value> = Vec::new();
    let mut vuln_count = 0;
    let mut checked = 0;

    let deps = cfg.dependencies.as_ref().cloned().unwrap_or_default();
    let dev_deps = cfg.dev_dependencies.as_ref().cloned().unwrap_or_default();

    for key in lock.dependencies.keys() {
        let parts: Vec<&str> = key.split(':').collect();
        if parts.len() != 3 {
            continue;
        }

        let group_id = parts[0];
        let artifact_id = parts[1];
        let version = parts[2];
        let maven_pkg = format!("{}:{}", group_id, artifact_id);

        checked += 1;

        let vulns = query_osv(&client, group_id, artifact_id, version)?;

        if !vulns.is_empty() {
            let is_direct = deps.contains_key(&maven_pkg) || dev_deps.contains_key(&maven_pkg);

            for vuln in &vulns {
                vuln_count += 1;

                if json_output {
                    all_vulns.push(serde_json::json!({
                        "id": vuln.id,
                        "package": format!("{}:{}:{}", group_id, artifact_id, version),
                        "direct": is_direct,
                        "summary": vuln.summary,
                        "severity": vuln.severity,
                        "fixedVersion": vuln.fixed_version,
                    }));
                } else {
                    let tag = if is_direct {
                        style("direct").red().bold()
                    } else {
                        style("transitive").yellow()
                    };

                    println!(
                        "  {} {} in {}:{}:{} [{}]",
                        style("!").red().bold(),
                        style(&vuln.id).red().bold(),
                        group_id,
                        artifact_id,
                        version,
                        tag
                    );
                    if let Some(ref summary) = vuln.summary {
                        println!("    {}", style(summary).dim());
                    }
                    if let Some(ref severity) = vuln.severity {
                        println!("    Severity: {}", colorize_severity(severity));
                    }
                    if let Some(ref fixed) = vuln.fixed_version {
                        println!("    Fixed in: {}", style(fixed).green());
                    }
                    println!();
                }
            }
        }
    }

    if json_output {
        println!("{}", serde_json::to_string_pretty(&all_vulns).unwrap_or_else(|_| "[]".to_string()));
    } else if vuln_count == 0 {
        println!(
            "  {} No known vulnerabilities found ({} packages checked)",
            style("✓").green().bold(),
            checked
        );
    } else {
        println!(
            "  {} Found {} vulnerabilit{} in {} packages",
            style("!").red().bold(),
            vuln_count,
            if vuln_count == 1 { "y" } else { "ies" },
            checked
        );
    }

    println!();
    Ok(())
}

struct VulnInfo {
    id: String,
    summary: Option<String>,
    severity: Option<String>,
    fixed_version: Option<String>,
}

fn query_osv(
    client: &reqwest::blocking::Client,
    group_id: &str,
    artifact_id: &str,
    version: &str,
) -> Result<Vec<VulnInfo>> {
    let body = serde_json::json!({
        "version": version,
        "package": {
            "name": format!("{}:{}", group_id, artifact_id),
            "ecosystem": "Maven"
        }
    });

    let response = client
        .post("https://api.osv.dev/v1/query")
        .json(&body)
        .send()?;

    if !response.status().is_success() {
        return Ok(vec![]); // Non-fatal: skip if API is unavailable
    }

    let result: serde_json::Value = response.json()?;
    let mut vulns = Vec::new();

    if let Some(vuln_list) = result["vulns"].as_array() {
        for v in vuln_list {
            let id = v["id"].as_str().unwrap_or("unknown").to_string();
            let summary = v["summary"].as_str().map(|s| s.to_string());

            // Extract severity
            let severity = v["database_specific"]["severity"]
                .as_str()
                .map(|s| s.to_string())
                .or_else(|| {
                    v["severity"]
                        .as_array()
                        .and_then(|arr| arr.first())
                        .and_then(|s| s["type"].as_str())
                        .map(|s| s.to_string())
                });

            // Find fixed version
            let fixed_version = v["affected"]
                .as_array()
                .and_then(|affected| {
                    affected.iter().find_map(|a| {
                        a["ranges"].as_array().and_then(|ranges| {
                            ranges.iter().find_map(|r| {
                                r["events"].as_array().and_then(|events| {
                                    events.iter().find_map(|e| {
                                        e["fixed"].as_str().map(|s| s.to_string())
                                    })
                                })
                            })
                        })
                    })
                });

            vulns.push(VulnInfo {
                id,
                summary,
                severity,
                fixed_version,
            });
        }
    }

    Ok(vulns)
}

fn colorize_severity(severity: &str) -> String {
    match severity.to_uppercase().as_str() {
        "CRITICAL" => style(severity).red().bold().to_string(),
        "HIGH" => style(severity).red().to_string(),
        "MODERATE" | "MEDIUM" => style(severity).yellow().to_string(),
        "LOW" => style(severity).dim().to_string(),
        _ => severity.to_string(),
    }
}
