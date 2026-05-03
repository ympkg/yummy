use anyhow::{bail, Result};
use console::style;
use std::collections::BTreeMap;
use std::time::Duration;

use crate::config;

pub fn execute(
    module: String,
    groups: Vec<String>,
    registries: Vec<String>,
    json: bool,
) -> Result<()> {
    if groups.is_empty() {
        bail!("--group is required (specify at least one internal groupId prefix)");
    }
    if registries.is_empty() {
        bail!("--registry is required (specify at least one Maven registry URL)");
    }

    let (config_path, root_cfg) = config::load_or_find_config()?;
    let project = config::project_dir(&config_path);

    // Locate the target module via WorkspaceGraph.
    let ws = crate::workspace::graph::WorkspaceGraph::build(&project)?;
    if ws.get_package(&module).is_none() {
        let available: Vec<String> = ws.all_packages();
        bail!(
            "module '{}' not found in workspace.\nAvailable: {}",
            module,
            available.join(", ")
        );
    }

    // Walk the full transitive workspace closure (target + every workspace-internal
    // dep, recursively) and merge each package's Maven deps. The previous
    // implementation only collected the target's *direct* Maven deps, missing the
    // case where target depends on a workspace-internal module that itself pulls
    // in external Maven artifacts. Result: prepare reported "all clear" while
    // build later failed with `package X does not exist` because a transitively-
    // required jar wasn't in the registry.
    let mut closure = ws.transitive_closure(&module)?;
    closure.push(module.clone());

    let mut deps: BTreeMap<String, String> = BTreeMap::new();
    for ws_name in &closure {
        if let Some(p) = ws.get_package(ws_name) {
            for (k, v) in p.config.maven_dependencies_with_root(&root_cfg) {
                deps.insert(k, v);
            }
        }
    }

    // Filter to those whose groupId matches any --group prefix (.-segment alignment).
    let internal: Vec<(String, String, String)> = deps
        .iter()
        .filter_map(|(coord, version)| {
            let (group, artifact) = split_ga(coord)?;
            if groups.iter().any(|p| group_matches_prefix(group, p)) {
                Some((group.to_string(), artifact.to_string(), version.clone()))
            } else {
                None
            }
        })
        .collect();

    if !json {
        println!(
            "  {} checking {} internal dep(s) of '{}' across {} registr{}",
            style("➜").green(),
            internal.len(),
            module,
            registries.len(),
            if registries.len() == 1 { "y" } else { "ies" }
        );
    }

    let client = build_http_client()?;
    let creds_map = load_creds_map();

    let mut missing: Vec<String> = Vec::new();
    for (group, artifact, version) in &internal {
        let exists = registries.iter().any(|reg| {
            let creds = creds_map.get(&normalize_url(reg)).cloned();
            check_pom_exists(&client, reg, group, artifact, version, creds.as_ref())
        });
        if !exists {
            missing.push(format!("{}:{}:{}", group, artifact, version));
        }
    }

    if json {
        let report = serde_json::json!({
            "module": module,
            "missing": missing,
            "checked": internal.len(),
            "missing_count": missing.len(),
        });
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else if missing.is_empty() {
        println!(
            "  {} all {} internal dep(s) available in registry",
            style("✓").green(),
            internal.len()
        );
    } else {
        println!(
            "  {} {} missing module(s) in registry:",
            style("✗").red(),
            missing.len()
        );
        for gav in &missing {
            println!("    {}", gav);
        }
    }

    if !missing.is_empty() {
        std::process::exit(1);
    }
    Ok(())
}

/// Split a "groupId:artifactId" coord into its parts.
/// Returns None if the coord has more than 2 parts (e.g. classifier-bearing
/// 3-part coord) — those won't be inspected by prepare.
fn split_ga(coord: &str) -> Option<(&str, &str)> {
    let parts: Vec<&str> = coord.split(':').collect();
    if parts.len() == 2 && !parts[0].is_empty() && !parts[1].is_empty() {
        Some((parts[0], parts[1]))
    } else {
        None
    }
}

/// Match `group` against `prefix` by `.`-segment alignment.
///
/// `com.summer.jarvis` matches:
///   - `com.summer.jarvis` (exact)
///   - `com.summer.jarvis.shared` (sub-segment)
/// But NOT:
///   - `com.summer.jarvisX` (no segment boundary)
///   - `com.summer` (prefix is longer)
fn group_matches_prefix(group: &str, prefix: &str) -> bool {
    if group == prefix {
        return true;
    }
    if group.len() > prefix.len() && group.starts_with(prefix) {
        return group.as_bytes()[prefix.len()] == b'.';
    }
    false
}

fn build_http_client() -> Result<reqwest::blocking::Client> {
    Ok(reqwest::blocking::Client::builder()
        .user_agent(concat!("ym/", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(5))
        .connect_timeout(Duration::from_secs(5))
        .build()?)
}

/// HEAD `<registry>/<group-path>/<artifact>/<version>/<artifact>-<version>.pom`.
/// Returns true on 2xx, false on 4xx/5xx, false (with stderr warning) on
/// network errors so prepare won't silently swallow connectivity issues.
fn check_pom_exists(
    client: &reqwest::blocking::Client,
    registry: &str,
    group: &str,
    artifact: &str,
    version: &str,
    creds: Option<&BasicAuth>,
) -> bool {
    let group_path = group.replace('.', "/");
    let url = format!(
        "{}/{}/{}/{}/{}-{}.pom",
        registry.trim_end_matches('/'),
        group_path,
        artifact,
        version,
        artifact,
        version
    );

    let mut req = client.head(&url);
    if let Some(auth) = creds {
        req = req.basic_auth(&auth.username, Some(&auth.password));
    }

    match req.send() {
        Ok(resp) => resp.status().is_success(),
        Err(e) => {
            eprintln!(
                "  {} network error on {}: {}",
                style("!").yellow(),
                url,
                e
            );
            false
        }
    }
}

#[derive(Clone)]
struct BasicAuth {
    username: String,
    password: String,
}

fn normalize_url(url: &str) -> String {
    url.trim_end_matches('/').to_string()
}

/// Load `~/.ym/credentials.json` into a registry-URL → BasicAuth map.
/// Missing or malformed file → empty map (anonymous HEAD).
fn load_creds_map() -> BTreeMap<String, BasicAuth> {
    let path = crate::home_dir().join(".ym").join("credentials.json");
    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return BTreeMap::new(),
    };
    let raw: BTreeMap<String, serde_json::Value> = match serde_json::from_str(&content) {
        Ok(m) => m,
        Err(_) => return BTreeMap::new(),
    };
    let mut out = BTreeMap::new();
    for (url, v) in raw {
        let u = v["username"].as_str().unwrap_or("").to_string();
        let p = v["password"].as_str().unwrap_or("").to_string();
        if !u.is_empty() {
            out.insert(normalize_url(&url), BasicAuth { username: u, password: p });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefix_match_exact() {
        assert!(group_matches_prefix("com.summer.jarvis", "com.summer.jarvis"));
    }

    #[test]
    fn prefix_match_sub_segment() {
        assert!(group_matches_prefix("com.summer.jarvis.shared", "com.summer.jarvis"));
        assert!(group_matches_prefix("com.summer.jarvis.platform.user", "com.summer.jarvis"));
    }

    #[test]
    fn prefix_match_rejects_partial_segment() {
        // "jarvis" prefix should not match "jarvisX" — must align on `.`
        assert!(!group_matches_prefix("com.summer.jarvisX", "com.summer.jarvis"));
        assert!(!group_matches_prefix("com.summer.jarvisextra", "com.summer.jarvis"));
    }

    #[test]
    fn prefix_match_rejects_shorter_group() {
        // group is shorter than prefix → no match
        assert!(!group_matches_prefix("com.summer", "com.summer.jarvis"));
        assert!(!group_matches_prefix("com", "com.summer.jarvis"));
    }

    #[test]
    fn prefix_match_rejects_unrelated() {
        assert!(!group_matches_prefix("org.springframework", "com.summer.jarvis"));
        assert!(!group_matches_prefix("com.google.guava", "com.summer.jarvis"));
    }

    #[test]
    fn split_ga_two_parts() {
        assert_eq!(split_ga("com.acme:foo"), Some(("com.acme", "foo")));
    }

    #[test]
    fn split_ga_three_parts_rejected() {
        // 3-part coord (classifier) is not a plain GA — split returns None.
        assert_eq!(split_ga("com.acme:foo:linux"), None);
    }

    #[test]
    fn split_ga_empty_parts_rejected() {
        assert_eq!(split_ga(":foo"), None);
        assert_eq!(split_ga("com.acme:"), None);
        assert_eq!(split_ga(""), None);
    }

    #[test]
    fn normalize_url_trims_trailing_slash() {
        assert_eq!(normalize_url("https://nexus.acme.com/repo/"), "https://nexus.acme.com/repo");
        assert_eq!(normalize_url("https://nexus.acme.com/repo"), "https://nexus.acme.com/repo");
    }
}
