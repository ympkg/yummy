use anyhow::{bail, Result};
use console::style;
use rayon::prelude::*;
use std::collections::{BTreeMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};

use crate::config::schema::{LockFile, LockedDependency};

/// A parsed Maven coordinate
pub struct MavenCoord {
    pub group_id: String,
    pub artifact_id: String,
    pub version: String,
}

impl MavenCoord {
    pub fn parse(coord: &str, version: &str) -> Result<Self> {
        let parts: Vec<&str> = coord.split(':').collect();
        if parts.len() != 2 {
            bail!(
                "Invalid dependency coordinate: '{}'. Expected format: groupId:artifactId",
                coord
            );
        }
        // Strip npm-style version prefixes: ^2.19.0 → 2.19.0, ~1.5.0 → 1.5.0
        let clean_version = version.trim_start_matches('^').trim_start_matches('~');
        Ok(MavenCoord {
            group_id: parts[0].to_string(),
            artifact_id: parts[1].to_string(),
            version: clean_version.to_string(),
        })
    }

    #[allow(dead_code)]
    pub fn key(&self) -> String {
        format!("{}:{}", self.group_id, self.artifact_id)
    }

    pub fn versioned_key(&self) -> String {
        format!("{}:{}:{}", self.group_id, self.artifact_id, self.version)
    }

    fn group_path(&self) -> String {
        self.group_id.replace('.', "/")
    }

    pub fn jar_url(&self, repo: &str) -> String {
        format!(
            "{}/{}/{}/{}/{}-{}.jar",
            repo,
            self.group_path(),
            self.artifact_id,
            self.version,
            self.artifact_id,
            self.version
        )
    }

    pub fn pom_url(&self, repo: &str) -> String {
        format!(
            "{}/{}/{}/{}/{}-{}.pom",
            repo,
            self.group_path(),
            self.artifact_id,
            self.version,
            self.artifact_id,
            self.version
        )
    }

    pub fn jar_path(&self, cache: &Path) -> PathBuf {
        cache
            .join(&self.group_id)
            .join(&self.artifact_id)
            .join(&self.version)
            .join(format!("{}-{}.jar", self.artifact_id, self.version))
    }

    pub fn pom_path(&self, cache: &Path) -> PathBuf {
        cache
            .join(&self.group_id)
            .join(&self.artifact_id)
            .join(&self.version)
            .join(format!("{}-{}.pom", self.artifact_id, self.version))
    }
}

const DEFAULT_REPO: &str = "https://repo1.maven.org/maven2";

/// Resolve all dependencies (including transitive) and download JARs.
/// Returns list of JAR paths.
///
/// Fast path: if the lock file already contains all requested deps and
/// every JAR is in the local cache, no HTTP requests are made.
pub fn resolve_and_download(
    dependencies: &BTreeMap<String, String>,
    cache_dir: &Path,
    lock: &mut LockFile,
) -> Result<Vec<PathBuf>> {
    resolve_and_download_with_repos(dependencies, cache_dir, lock, &[])
}

/// Resolve with custom Maven repository list and exclusions.
/// Tries each repo in order, falls back to Maven Central.
pub fn resolve_and_download_with_repos(
    dependencies: &BTreeMap<String, String>,
    cache_dir: &Path,
    lock: &mut LockFile,
    repos: &[String],
) -> Result<Vec<PathBuf>> {
    resolve_and_download_full(dependencies, cache_dir, lock, repos, &[])
}

/// Full resolve with repos and exclusions.
pub fn resolve_and_download_full(
    dependencies: &BTreeMap<String, String>,
    cache_dir: &Path,
    lock: &mut LockFile,
    repos: &[String],
    exclusions: &[String],
) -> Result<Vec<PathBuf>> {
    let exclusion_set: HashSet<String> = exclusions.iter().cloned().collect();
    // Fast path: try to resolve entirely from lock file + local cache
    if let Some(jars) = try_resolve_from_lock(dependencies, cache_dir, lock) {
        return Ok(jars);
    }

    // Slow path: resolve from network
    // Phase 1: resolve the full dependency graph (BFS), collecting coordinates to download
    let mut visited = HashSet::new();
    let mut queue = VecDeque::new();
    let mut ordered_keys = Vec::new(); // track order for jar collection

    for (coord, version) in dependencies {
        let mc = MavenCoord::parse(coord, version)?;
        queue.push_back(mc);
    }

    let client = reqwest::blocking::Client::builder()
        .user_agent("ym/0.1.0")
        .timeout(std::time::Duration::from_secs(30))
        .build()?;

    let repo_urls = build_repo_list(repos);

    // Resolve transitive graph (sequential — needs POM parsing)
    let mut coords_to_download: Vec<(String, MavenCoord)> = Vec::new();
    let mut dep_map: BTreeMap<String, Vec<String>> = BTreeMap::new();

    while let Some(coord) = queue.pop_front() {
        let key = coord.versioned_key();
        if visited.contains(&key) {
            continue;
        }
        visited.insert(key.clone());
        ordered_keys.push(key.clone());

        // Download POM for transitive deps (must be sequential for graph resolution)
        let transitive = resolve_transitive(&client, &coord, cache_dir, &repo_urls)?;

        // Apply exclusions: filter out excluded transitive dependencies
        let transitive: Vec<MavenCoord> = transitive
            .into_iter()
            .filter(|dep| {
                let dep_key = format!("{}:{}", dep.group_id, dep.artifact_id);
                !exclusion_set.contains(&dep_key)
            })
            .collect();

        let dep_keys: Vec<String> = transitive.iter().map(|c| c.versioned_key()).collect();
        dep_map.insert(key.clone(), dep_keys);

        // Queue JAR download if not cached
        let jar_path = coord.jar_path(cache_dir);
        if !jar_path.exists() {
            coords_to_download.push((key, coord));
        }

        for dep in transitive {
            queue.push_back(dep);
        }
    }

    // Phase 2: parallel JAR downloads
    if !coords_to_download.is_empty() {
        println!(
            "  {} Downloading {} artifact(s)...",
            style("↓").blue(),
            coords_to_download.len()
        );

        let download_results: Vec<(String, Result<String>)> = coords_to_download
            .into_par_iter()
            .map(|(key, coord)| {
                let jar_path = coord.jar_path(cache_dir);
                let hash_result = download_from_repos(&client, &coord, &jar_path, &repo_urls, |c, r| c.jar_url(r));
                println!(
                    "  {} {}:{}:{}",
                    style("✓").green(),
                    coord.group_id,
                    coord.artifact_id,
                    coord.version
                );
                (key, hash_result)
            })
            .collect();

        // Record hashes in lock
        for (key, hash_result) in download_results {
            if let Ok(hash) = hash_result {
                if let Some(entry) = lock.dependencies.get_mut(&key) {
                    entry.sha256 = Some(hash);
                }
            }
        }
    }

    // Phase 3: build lock entries and collect JAR paths
    let mut all_jars = Vec::new();
    for key in &ordered_keys {
        let parts: Vec<&str> = key.split(':').collect();
        if parts.len() != 3 {
            continue;
        }
        let coord = MavenCoord {
            group_id: parts[0].to_string(),
            artifact_id: parts[1].to_string(),
            version: parts[2].to_string(),
        };
        all_jars.push(coord.jar_path(cache_dir));

        let dep_keys = dep_map.remove(key).unwrap_or_default();
        // Insert lock entry if not already present (download phase may have set sha)
        lock.dependencies.entry(key.clone()).or_insert(LockedDependency {
            sha256: None,
            dependencies: if dep_keys.is_empty() {
                None
            } else {
                Some(dep_keys)
            },
        });
    }

    Ok(all_jars)
}

/// Try to resolve all dependencies from lock file without network access.
/// Returns None if any dep is missing from lock or cache.
fn try_resolve_from_lock(
    dependencies: &BTreeMap<String, String>,
    cache_dir: &Path,
    lock: &LockFile,
) -> Option<Vec<PathBuf>> {
    if lock.dependencies.is_empty() {
        return None;
    }

    let mut all_jars = Vec::new();
    let mut visited = HashSet::new();
    let mut queue = VecDeque::new();

    for (coord, version) in dependencies {
        let mc = MavenCoord::parse(coord, version).ok()?;
        queue.push_back(mc);
    }

    while let Some(coord) = queue.pop_front() {
        let key = coord.versioned_key();
        if visited.contains(&key) {
            continue;
        }
        visited.insert(key.clone());

        // Check JAR exists in cache
        let jar_path = coord.jar_path(cache_dir);
        if !jar_path.exists() {
            return None; // Cache miss
        }

        // Check lock file entry exists
        let locked = lock.dependencies.get(&key)?;

        // Verify SHA-256 integrity if hash is recorded
        if let Some(ref expected_sha) = locked.sha256 {
            if let Ok(data) = std::fs::read(&jar_path) {
                let actual = crate::compiler::incremental::hash_bytes(&data);
                if &actual != expected_sha {
                    return None; // Integrity mismatch, force re-download
                }
            }
        }

        all_jars.push(jar_path);
        if let Some(ref dep_keys) = locked.dependencies {
            for dep_key in dep_keys {
                let parts: Vec<&str> = dep_key.split(':').collect();
                if parts.len() == 3 {
                    queue.push_back(MavenCoord {
                        group_id: parts[0].to_string(),
                        artifact_id: parts[1].to_string(),
                        version: parts[2].to_string(),
                    });
                }
            }
        }
    }

    Some(all_jars)
}

fn resolve_transitive(
    client: &reqwest::blocking::Client,
    coord: &MavenCoord,
    cache_dir: &Path,
    repos: &[String],
) -> Result<Vec<MavenCoord>> {
    let pom_path = coord.pom_path(cache_dir);

    if !pom_path.exists() {
        if download_from_repos(client, coord, &pom_path, repos, |c, r| c.pom_url(r)).is_err() {
            return Ok(vec![]); // POM not found is non-fatal
        }
    }

    let pom_content = std::fs::read_to_string(&pom_path)?;

    // Collect parent POM properties (up to 3 levels)
    let mut all_properties = std::collections::HashMap::new();
    resolve_parent_properties(client, &pom_content, cache_dir, repos, &mut all_properties, 0)?;

    parse_pom_dependencies_with_props(&pom_content, &all_properties)
}

/// Recursively fetch parent POM properties.
fn resolve_parent_properties(
    client: &reqwest::blocking::Client,
    pom_content: &str,
    cache_dir: &Path,
    repos: &[String],
    properties: &mut std::collections::HashMap<String, String>,
    depth: u8,
) -> Result<()> {
    if depth > 3 {
        return Ok(());
    }

    let doc = roxmltree::Document::parse(pom_content)?;

    // Find parent reference
    for node in doc.root_element().children() {
        if node.tag_name().name() == "parent" {
            let mut pg = None;
            let mut pa = None;
            let mut pv = None;
            for child in node.children() {
                match child.tag_name().name() {
                    "groupId" => pg = child.text(),
                    "artifactId" => pa = child.text(),
                    "version" => pv = child.text(),
                    _ => {}
                }
            }
            if let (Some(g), Some(a), Some(v)) = (pg, pa, pv) {
                let parent_coord = MavenCoord {
                    group_id: g.to_string(),
                    artifact_id: a.to_string(),
                    version: v.to_string(),
                };
                let parent_pom_path = parent_coord.pom_path(cache_dir);
                if !parent_pom_path.exists() {
                    let _ = download_from_repos(client, &parent_coord, &parent_pom_path, repos, |c, r| c.pom_url(r));
                }
                if parent_pom_path.exists() {
                    let parent_content = std::fs::read_to_string(&parent_pom_path)?;
                    // Recurse into grandparent first (so child overrides parent)
                    resolve_parent_properties(client, &parent_content, cache_dir, repos, properties, depth + 1)?;
                    // Then merge parent properties
                    let parent_doc = roxmltree::Document::parse(&parent_content)?;
                    let parent_props = collect_pom_properties(&parent_doc);
                    for (k, v) in parent_props {
                        properties.entry(k).or_insert(v);
                    }
                    // Parent managed versions as fallback
                    let managed = collect_managed_versions(&parent_doc, properties);
                    for (k, v) in managed {
                        properties.entry(format!("managed:{}", k)).or_insert(v);
                    }
                }
            }
            break;
        }
    }

    // Merge current POM properties (child overrides parent)
    let current_props = collect_pom_properties(&doc);
    for (k, v) in current_props {
        properties.insert(k, v);
    }

    Ok(())
}

/// Parse POM dependencies with pre-collected properties (including parent).
fn parse_pom_dependencies_with_props(
    pom: &str,
    extra_properties: &std::collections::HashMap<String, String>,
) -> Result<Vec<MavenCoord>> {
    let doc = roxmltree::Document::parse(pom)?;

    // Merge local properties with inherited
    let mut properties = extra_properties.clone();
    let local_props = collect_pom_properties(&doc);
    for (k, v) in local_props {
        properties.insert(k, v);
    }

    let managed = collect_managed_versions(&doc, &properties);

    let mut deps = Vec::new();

    for node in doc.descendants() {
        if node.tag_name().name() != "dependencies" {
            continue;
        }
        if let Some(parent) = node.parent() {
            if parent.tag_name().name() == "dependencyManagement" {
                continue;
            }
        }

        for dep in node.children() {
            if dep.tag_name().name() != "dependency" {
                continue;
            }

            let mut group_id = None;
            let mut artifact_id = None;
            let mut version = None;
            let mut scope = None;
            let mut optional = false;

            for child in dep.children() {
                match child.tag_name().name() {
                    "groupId" => group_id = child.text().map(|s| s.to_string()),
                    "artifactId" => artifact_id = child.text().map(|s| s.to_string()),
                    "version" => version = child.text().map(|s| s.to_string()),
                    "scope" => scope = child.text().map(|s| s.to_string()),
                    "optional" => optional = child.text() == Some("true"),
                    _ => {}
                }
            }

            if let Some(ref s) = scope {
                if s == "test" || s == "provided" || s == "system" {
                    continue;
                }
            }
            if optional {
                continue;
            }

            if let (Some(g), Some(a)) = (group_id, artifact_id) {
                let resolved_version = version
                    .map(|v| resolve_properties(&v, &properties))
                    .or_else(|| managed.get(&format!("{}:{}", g, a)).cloned())
                    .or_else(|| extra_properties.get(&format!("managed:{}:{}", g, a)).cloned());

                if let Some(v) = resolved_version {
                    if !v.contains("${") {
                        deps.push(MavenCoord {
                            group_id: g,
                            artifact_id: a,
                            version: v,
                        });
                    }
                }
            }
        }
    }

    Ok(deps)
}

/// Collect <properties> from POM.
fn collect_pom_properties(doc: &roxmltree::Document) -> std::collections::HashMap<String, String> {
    let mut props = std::collections::HashMap::new();

    // Also grab project.version
    for node in doc.descendants() {
        if node.tag_name().name() == "version" {
            if let Some(parent) = node.parent() {
                if parent.tag_name().name() == "project" || parent.tag_name().name() == "parent" {
                    if let Some(v) = node.text() {
                        props.insert("project.version".to_string(), v.to_string());
                    }
                }
            }
        }
    }

    for node in doc.descendants() {
        if node.tag_name().name() == "properties" {
            for child in node.children() {
                if child.is_element() {
                    if let Some(val) = child.text() {
                        props.insert(child.tag_name().name().to_string(), val.to_string());
                    }
                }
            }
        }
    }

    props
}

/// Collect versions from <dependencyManagement>.
fn collect_managed_versions(
    doc: &roxmltree::Document,
    properties: &std::collections::HashMap<String, String>,
) -> std::collections::HashMap<String, String> {
    let mut managed = std::collections::HashMap::new();

    for node in doc.descendants() {
        if node.tag_name().name() != "dependencyManagement" {
            continue;
        }
        for deps_node in node.children() {
            if deps_node.tag_name().name() != "dependencies" {
                continue;
            }
            for dep in deps_node.children() {
                if dep.tag_name().name() != "dependency" {
                    continue;
                }
                let mut g = None;
                let mut a = None;
                let mut v = None;
                for child in dep.children() {
                    match child.tag_name().name() {
                        "groupId" => g = child.text(),
                        "artifactId" => a = child.text(),
                        "version" => v = child.text(),
                        _ => {}
                    }
                }
                if let (Some(g), Some(a), Some(v)) = (g, a, v) {
                    let resolved = resolve_properties(v, properties);
                    managed.insert(format!("{}:{}", g, a), resolved);
                }
            }
        }
    }

    managed
}

/// Replace ${property.name} placeholders with values from properties map.
fn resolve_properties(value: &str, properties: &std::collections::HashMap<String, String>) -> String {
    let mut result = value.to_string();
    // Iterate until no more substitutions (handles nested refs)
    for _ in 0..5 {
        let prev = result.clone();
        for (key, val) in properties {
            result = result.replace(&format!("${{{}}}", key), val);
        }
        if result == prev {
            break;
        }
    }
    result
}

/// Build the ordered list of Maven repos to try.
/// Custom repos come first, Maven Central is always appended as fallback.
fn build_repo_list(custom_repos: &[String]) -> Vec<String> {
    let mut repos: Vec<String> = custom_repos
        .iter()
        .map(|r| r.trim_end_matches('/').to_string())
        .collect();
    let central = DEFAULT_REPO.to_string();
    if !repos.contains(&central) {
        repos.push(central);
    }
    repos
}

/// Try to download an artifact from multiple repos, stopping at the first success.
/// Returns the SHA-256 hash of the downloaded file.
fn download_from_repos(
    client: &reqwest::blocking::Client,
    coord: &MavenCoord,
    path: &Path,
    repos: &[String],
    url_fn: impl Fn(&MavenCoord, &str) -> String,
) -> Result<String> {
    let mut last_err = None;
    for repo in repos {
        let url = url_fn(coord, repo);
        match download_file(client, &url, path) {
            Ok(hash) => return Ok(hash),
            Err(e) => last_err = Some(e),
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow::anyhow!("No repositories configured")))
}

/// Download a file and return its SHA-256 hash.
fn download_file(client: &reqwest::blocking::Client, url: &str, path: &Path) -> Result<String> {
    let mut request = client.get(url);

    // Apply credentials if available for this URL
    if let Some((username, password)) = load_credentials_for_url(url) {
        request = request.basic_auth(username, Some(password));
    }

    let response = request.send()?;
    if !response.status().is_success() {
        bail!("HTTP {} for {}", response.status(), url);
    }

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let bytes = response.bytes()?;
    let hash = crate::compiler::incremental::hash_bytes(&bytes);
    std::fs::write(path, &bytes)?;
    Ok(hash)
}

/// Load credentials from ~/.ym/credentials.json if the URL matches the stored registry.
fn load_credentials_for_url(url: &str) -> Option<(String, String)> {
    let home = std::env::var("HOME").ok()?;
    let creds_path = PathBuf::from(home).join(".ym").join("credentials.json");
    let content = std::fs::read_to_string(&creds_path).ok()?;
    let creds: serde_json::Value = serde_json::from_str(&content).ok()?;

    let registry = creds["registry"].as_str()?;
    if url.starts_with(registry) {
        let username = creds["username"].as_str()?.to_string();
        let password = creds["password"].as_str()?.to_string();
        Some((username, password))
    } else {
        None
    }
}

/// Check for dependency version conflicts in the resolved dependency set.
/// Returns a list of (groupId:artifactId, [versions]) for artifacts with multiple versions.
pub fn check_conflicts(lock: &LockFile) -> Vec<(String, Vec<String>)> {
    let mut versions_map: BTreeMap<String, Vec<String>> = BTreeMap::new();

    for key in lock.dependencies.keys() {
        let parts: Vec<&str> = key.split(':').collect();
        if parts.len() == 3 {
            let ga = format!("{}:{}", parts[0], parts[1]);
            versions_map
                .entry(ga)
                .or_default()
                .push(parts[2].to_string());
        }
    }

    versions_map
        .into_iter()
        .filter(|(_, versions)| versions.len() > 1)
        .collect()
}

/// Search Maven Central for an artifact by keyword
pub fn search_maven(query: &str) -> Result<Vec<(String, String, String)>> {
    let client = reqwest::blocking::Client::builder()
        .user_agent("ym/0.1.0")
        .build()?;

    let is_plain = !query.contains(':') && !query.contains('*') && !query.contains(" AND ") && !query.contains(" OR ");

    // For plain keywords, search both exact and prefix to get better results
    let queries: Vec<String> = if is_plain {
        vec![
            format!("a:{}-*", query),  // e.g. jackson-databind, jackson-core
            query.to_string(),          // full-text fallback
        ]
    } else {
        vec![query.to_string()]
    };

    let mut results = Vec::new();
    let mut seen = std::collections::HashSet::new();

    for q in &queries {
        let response = client
            .get("https://search.maven.org/solrsearch/select")
            .query(&[("q", q.as_str()), ("rows", "20"), ("wt", "json")])
            .send()?;
        let body: serde_json::Value = response.json()?;

        if let Some(docs) = body["response"]["docs"].as_array() {
            for doc in docs {
                let g = doc["g"].as_str().unwrap_or("").to_string();
                let a = doc["a"].as_str().unwrap_or("").to_string();
                let v = doc["latestVersion"].as_str().unwrap_or("").to_string();
                let key = format!("{}:{}", g, a);
                if !g.is_empty() && !a.is_empty() && !v.is_empty() && seen.insert(key) {
                    results.push((g, a, v));
                }
            }
        }
    }

    Ok(results)
}

/// Fetch the latest version of a specific artifact from Maven Central
pub fn fetch_latest_version(group_id: &str, artifact_id: &str) -> Result<String> {
    let client = reqwest::blocking::Client::builder()
        .user_agent("ym/0.1.0")
        .build()?;

    let url = format!(
        "https://search.maven.org/solrsearch/select?q=g:\"{}\" AND a:\"{}\"&rows=1&wt=json",
        group_id, artifact_id
    );

    let response = client.get(&url).send()?;
    let body: serde_json::Value = response.json()?;

    if let Some(docs) = body["response"]["docs"].as_array() {
        if let Some(doc) = docs.first() {
            if let Some(v) = doc["latestVersion"].as_str() {
                return Ok(v.to_string());
            }
        }
    }

    bail!("Could not find {}:{} on Maven Central", group_id, artifact_id)
}
