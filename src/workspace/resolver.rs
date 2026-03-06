use anyhow::{bail, Result};
use console::style;
use rayon::prelude::*;
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

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

    let client = reqwest::blocking::Client::builder()
        .user_agent("ym/0.1.0")
        .timeout(std::time::Duration::from_secs(30))
        .build()?;

    let repo_urls = build_repo_list(repos);

    // Resolve transitive graph with nearest-wins strategy (Maven convention).
    // Track the depth at which each groupId:artifactId was first resolved.
    let mut coords_to_download: Vec<(String, MavenCoord)> = Vec::new();
    let mut dep_map: BTreeMap<String, Vec<String>> = BTreeMap::new();
    // Maps groupId:artifactId -> (depth, chosen version)
    let mut resolved_versions: HashMap<String, (usize, String)> = HashMap::new();
    // Track BFS depth per queued item
    let mut depth_map: HashMap<String, usize> = HashMap::new();

    // In-memory POM cache to avoid re-parsing the same POM
    let pom_cache = PomCache::new();

    // Initialize direct dependencies at depth 0
    for (coord, version) in dependencies {
        let mc = MavenCoord::parse(coord, version)?;
        let ga_key = format!("{}:{}", mc.group_id, mc.artifact_id);
        depth_map.insert(mc.versioned_key(), 0);
        resolved_versions.insert(ga_key, (0, mc.version.clone()));
        queue.push_back(mc);
    }

    while let Some(coord) = queue.pop_front() {
        let key = coord.versioned_key();
        let ga_key = format!("{}:{}", coord.group_id, coord.artifact_id);
        let current_depth = depth_map.get(&key).copied().unwrap_or(0);

        if visited.contains(&key) {
            continue;
        }

        // Nearest-wins: if we already resolved this GA at a shallower depth
        // with a different version, skip this deeper version
        if let Some(&(resolved_depth, ref resolved_ver)) = resolved_versions.get(&ga_key) {
            if resolved_ver != &coord.version && resolved_depth < current_depth {
                continue; // A nearer version was already chosen
            }
        }

        visited.insert(key.clone());
        ordered_keys.push(key.clone());
        resolved_versions.entry(ga_key).or_insert((current_depth, coord.version.clone()));

        // Download POM for transitive deps with in-memory + disk cache
        let transitive = resolve_transitive_cached(&client, &coord, cache_dir, &repo_urls, Some(&pom_cache))?;

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

        let child_depth = current_depth + 1;
        for dep in transitive {
            let dep_vk = dep.versioned_key();
            depth_map.entry(dep_vk).or_insert(child_depth);
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

/// In-memory cache for parsed POM transitive dependencies.
/// Key: groupId:artifactId:version, Value: list of dependency coords.
struct PomCache {
    entries: Mutex<HashMap<String, Vec<(String, String, String)>>>,
}

impl PomCache {
    fn new() -> Self {
        PomCache {
            entries: Mutex::new(HashMap::new()),
        }
    }

    fn get(&self, key: &str) -> Option<Vec<MavenCoord>> {
        let entries = self.entries.lock().unwrap();
        entries.get(key).map(|v| {
            v.iter()
                .map(|(g, a, ver)| MavenCoord {
                    group_id: g.clone(),
                    artifact_id: a.clone(),
                    version: ver.clone(),
                })
                .collect()
        })
    }

    fn insert(&self, key: &str, deps: &[MavenCoord]) {
        let mut entries = self.entries.lock().unwrap();
        entries.insert(
            key.to_string(),
            deps.iter()
                .map(|d| (d.group_id.clone(), d.artifact_id.clone(), d.version.clone()))
                .collect(),
        );
    }
}

fn resolve_transitive(
    client: &reqwest::blocking::Client,
    coord: &MavenCoord,
    cache_dir: &Path,
    repos: &[String],
) -> Result<Vec<MavenCoord>> {
    resolve_transitive_cached(client, coord, cache_dir, repos, None)
}

fn resolve_transitive_cached(
    client: &reqwest::blocking::Client,
    coord: &MavenCoord,
    cache_dir: &Path,
    repos: &[String],
    pom_cache: Option<&PomCache>,
) -> Result<Vec<MavenCoord>> {
    let cache_key = coord.versioned_key();

    // Check in-memory cache first
    if let Some(cache) = pom_cache {
        if let Some(cached) = cache.get(&cache_key) {
            return Ok(cached);
        }
    }

    // Check on-disk POM cache (.ym/pom-cache/)
    let pom_cache_dir = cache_dir
        .parent()  // up from maven/ to cache/
        .and_then(|p| p.parent())  // up from cache/ to .ym/
        .unwrap_or(cache_dir)
        .join("pom-cache");
    let pom_cache_file = pom_cache_dir
        .join(&coord.group_id)
        .join(&coord.artifact_id)
        .join(format!("{}.json", coord.version));

    if pom_cache_file.exists() {
        if let Ok(content) = std::fs::read_to_string(&pom_cache_file) {
            if let Ok(cached_deps) = serde_json::from_str::<Vec<(String, String, String)>>(&content) {
                let deps: Vec<MavenCoord> = cached_deps
                    .iter()
                    .map(|(g, a, v)| MavenCoord {
                        group_id: g.clone(),
                        artifact_id: a.clone(),
                        version: v.clone(),
                    })
                    .collect();
                if let Some(cache) = pom_cache {
                    cache.insert(&cache_key, &deps);
                }
                return Ok(deps);
            }
        }
    }

    let pom_path = coord.pom_path(cache_dir);

    if !pom_path.exists() {
        if download_from_repos(client, coord, &pom_path, repos, |c, r| c.pom_url(r)).is_err() {
            return Ok(vec![]); // POM not found is non-fatal
        }
    }

    let pom_content = std::fs::read_to_string(&pom_path)?;

    // Collect parent POM properties (unlimited depth with cycle detection)
    let mut all_properties = HashMap::new();
    let mut visited_poms = HashSet::new();
    resolve_parent_properties(client, &pom_content, cache_dir, repos, &mut all_properties, 0, &mut visited_poms)?;

    let deps = parse_pom_dependencies_with_props(&pom_content, &all_properties, client, cache_dir, repos)?;

    // Write to disk cache
    if let Some(parent) = pom_cache_file.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let serializable: Vec<(String, String, String)> = deps
        .iter()
        .map(|d| (d.group_id.clone(), d.artifact_id.clone(), d.version.clone()))
        .collect();
    let _ = std::fs::write(&pom_cache_file, serde_json::to_string(&serializable).unwrap_or_default());

    // Store in memory cache
    if let Some(cache) = pom_cache {
        cache.insert(&cache_key, &deps);
    }

    Ok(deps)
}

/// Recursively fetch parent POM properties.
/// Uses cycle detection via visited_poms set and a depth limit of 20.
fn resolve_parent_properties(
    client: &reqwest::blocking::Client,
    pom_content: &str,
    cache_dir: &Path,
    repos: &[String],
    properties: &mut HashMap<String, String>,
    depth: u8,
    visited_poms: &mut HashSet<String>,
) -> Result<()> {
    if depth > 20 {
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
                let parent_key = format!("{}:{}:{}", g, a, v);
                if visited_poms.contains(&parent_key) {
                    break; // Cycle detected
                }
                visited_poms.insert(parent_key);

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
                    resolve_parent_properties(client, &parent_content, cache_dir, repos, properties, depth + 1, visited_poms)?;
                    // Then merge parent properties
                    let parent_doc = roxmltree::Document::parse(&parent_content)?;
                    let parent_props = collect_pom_properties(&parent_doc);
                    for (k, v) in parent_props {
                        properties.entry(k).or_insert(v);
                    }
                    // Parent managed versions (including BOM imports)
                    let managed = collect_managed_versions_with_bom(
                        &parent_doc, properties, client, cache_dir, repos, 0,
                    );
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
/// Now supports BOM imports in dependencyManagement.
fn parse_pom_dependencies_with_props(
    pom: &str,
    extra_properties: &HashMap<String, String>,
    client: &reqwest::blocking::Client,
    cache_dir: &Path,
    repos: &[String],
) -> Result<Vec<MavenCoord>> {
    let doc = roxmltree::Document::parse(pom)?;

    // Merge local properties with inherited
    let mut properties = extra_properties.clone();
    let local_props = collect_pom_properties(&doc);
    for (k, v) in local_props {
        properties.insert(k, v);
    }

    let managed = collect_managed_versions_with_bom(
        &doc, &properties, client, cache_dir, repos, 0,
    );

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

            // Skip import scope (handled in dependencyManagement)
            if let Some(ref s) = scope {
                if s == "test" || s == "provided" || s == "system" || s == "import" {
                    continue;
                }
            }
            if optional {
                continue;
            }

            if let (Some(g), Some(a)) = (group_id, artifact_id) {
                let resolved_g = resolve_properties(&g, &properties);
                let resolved_a = resolve_properties(&a, &properties);
                let resolved_version = version
                    .map(|v| resolve_properties(&v, &properties))
                    .or_else(|| managed.get(&format!("{}:{}", resolved_g, resolved_a)).cloned())
                    .or_else(|| extra_properties.get(&format!("managed:{}:{}", resolved_g, resolved_a)).cloned());

                if let Some(v) = resolved_version {
                    if !v.contains("${") {
                        deps.push(MavenCoord {
                            group_id: resolved_g,
                            artifact_id: resolved_a,
                            version: v,
                        });
                    }
                }
            }
        }
    }

    Ok(deps)
}

/// Collect <properties> from POM, including built-in Maven properties.
fn collect_pom_properties(doc: &roxmltree::Document) -> HashMap<String, String> {
    let mut props = HashMap::new();
    let root = doc.root_element();

    // Collect project-level coordinates as built-in properties
    let mut project_group_id = None;
    let mut project_artifact_id = None;
    let mut project_version = None;
    let mut parent_group_id = None;
    let mut parent_version = None;

    for node in root.children() {
        match node.tag_name().name() {
            "groupId" => project_group_id = node.text().map(|s| s.to_string()),
            "artifactId" => project_artifact_id = node.text().map(|s| s.to_string()),
            "version" => project_version = node.text().map(|s| s.to_string()),
            "parent" => {
                for child in node.children() {
                    match child.tag_name().name() {
                        "groupId" => parent_group_id = child.text().map(|s| s.to_string()),
                        "version" => parent_version = child.text().map(|s| s.to_string()),
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }

    // project.version: own version or inherited from parent
    if let Some(ref v) = project_version {
        props.insert("project.version".to_string(), v.clone());
        props.insert("pom.version".to_string(), v.clone());
    } else if let Some(ref v) = parent_version {
        props.insert("project.version".to_string(), v.clone());
        props.insert("pom.version".to_string(), v.clone());
    }

    // project.groupId: own or inherited from parent
    if let Some(ref g) = project_group_id {
        props.insert("project.groupId".to_string(), g.clone());
        props.insert("pom.groupId".to_string(), g.clone());
    } else if let Some(ref g) = parent_group_id {
        props.insert("project.groupId".to_string(), g.clone());
        props.insert("pom.groupId".to_string(), g.clone());
    }

    if let Some(ref a) = project_artifact_id {
        props.insert("project.artifactId".to_string(), a.clone());
        props.insert("pom.artifactId".to_string(), a.clone());
    }

    // parent.* properties
    if let Some(ref g) = parent_group_id {
        props.insert("parent.groupId".to_string(), g.clone());
        props.insert("project.parent.groupId".to_string(), g.clone());
    }
    if let Some(ref v) = parent_version {
        props.insert("parent.version".to_string(), v.clone());
        props.insert("project.parent.version".to_string(), v.clone());
    }

    // Collect explicit <properties>
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

/// Collect versions from <dependencyManagement>, including BOM imports.
/// When a dependency has scope=import and type=pom, recursively fetch and
/// merge its dependencyManagement entries (outer takes precedence).
fn collect_managed_versions_with_bom(
    doc: &roxmltree::Document,
    properties: &HashMap<String, String>,
    client: &reqwest::blocking::Client,
    cache_dir: &Path,
    repos: &[String],
    bom_depth: u8,
) -> HashMap<String, String> {
    if bom_depth > 10 {
        return HashMap::new(); // Prevent infinite BOM recursion
    }

    let mut managed = HashMap::new();

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
                let mut scope = None;
                let mut dep_type = None;
                for child in dep.children() {
                    match child.tag_name().name() {
                        "groupId" => g = child.text(),
                        "artifactId" => a = child.text(),
                        "version" => v = child.text(),
                        "scope" => scope = child.text(),
                        "type" => dep_type = child.text(),
                        _ => {}
                    }
                }

                if let (Some(g), Some(a)) = (g, a) {
                    let resolved_g = resolve_properties(g, properties);
                    let resolved_a = resolve_properties(a, properties);

                    // Handle BOM import: scope=import + type=pom
                    if scope == Some("import") && dep_type == Some("pom") {
                        if let Some(v) = v {
                            let resolved_v = resolve_properties(v, properties);
                            if !resolved_v.contains("${") {
                                // Download and parse the BOM POM
                                let bom_coord = MavenCoord {
                                    group_id: resolved_g.clone(),
                                    artifact_id: resolved_a.clone(),
                                    version: resolved_v,
                                };
                                let bom_pom_path = bom_coord.pom_path(cache_dir);
                                if !bom_pom_path.exists() {
                                    let _ = download_from_repos(
                                        client, &bom_coord, &bom_pom_path, repos,
                                        |c, r| c.pom_url(r),
                                    );
                                }
                                if bom_pom_path.exists() {
                                    if let Ok(bom_content) = std::fs::read_to_string(&bom_pom_path) {
                                        if let Ok(bom_doc) = roxmltree::Document::parse(&bom_content) {
                                            // Collect BOM's own properties
                                            let mut bom_props = properties.clone();
                                            let bom_local_props = collect_pom_properties(&bom_doc);
                                            for (k, val) in bom_local_props {
                                                bom_props.entry(k).or_insert(val);
                                            }

                                            // Also resolve BOM's parent properties
                                            let mut bom_visited = HashSet::new();
                                            let _ = resolve_parent_properties(
                                                client, &bom_content, cache_dir, repos,
                                                &mut bom_props, 0, &mut bom_visited,
                                            );

                                            // Recursively collect managed versions from BOM
                                            let bom_managed = collect_managed_versions_with_bom(
                                                &bom_doc, &bom_props, client, cache_dir, repos,
                                                bom_depth + 1,
                                            );
                                            // Outer definitions take precedence
                                            for (k, val) in bom_managed {
                                                managed.entry(k).or_insert(val);
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        continue;
                    }

                    // Regular managed dependency
                    if let Some(v) = v {
                        let resolved = resolve_properties(v, properties);
                        managed.insert(format!("{}:{}", resolved_g, resolved_a), resolved);
                    }
                }
            }
        }
    }

    managed
}

/// Replace ${property.name} placeholders with values from properties map.
/// Iterates up to 10 rounds to handle transitive property references.
fn resolve_properties(value: &str, properties: &HashMap<String, String>) -> String {
    let mut result = value.to_string();
    for _ in 0..10 {
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

/// Resolve all Maven dependencies for a workspace at once, then distribute per module.
/// This avoids redundant POM resolution across modules that share dependencies.
pub fn resolve_workspace_deps(
    all_module_deps: &[(String, BTreeMap<String, String>)],
    cache_dir: &Path,
    lock: &mut LockFile,
    repos: &[String],
    exclusions: &[String],
) -> Result<HashMap<String, Vec<PathBuf>>> {
    // 1. Merge all module deps into a single set (collect all unique coords)
    let mut merged_deps = BTreeMap::new();
    for (_name, deps) in all_module_deps {
        for (coord, version) in deps {
            merged_deps.entry(coord.clone()).or_insert(version.clone());
        }
    }

    if merged_deps.is_empty() {
        return Ok(all_module_deps.iter().map(|(name, _)| (name.clone(), vec![])).collect());
    }

    // 2. Resolve once for the entire workspace
    let all_jars = resolve_and_download_full(&merged_deps, cache_dir, lock, repos, exclusions)?;

    // 3. Build a lookup: groupId:artifactId -> jar path
    let mut jar_lookup: HashMap<String, PathBuf> = HashMap::new();
    for jar in &all_jars {
        // Extract group:artifact from the cache path structure: cache/group/artifact/version/artifact-version.jar
        if let Some(version_dir) = jar.parent() {
            if let Some(artifact_dir) = version_dir.parent() {
                if let Some(group_dir) = artifact_dir.parent() {
                    let artifact = artifact_dir.file_name().unwrap().to_string_lossy();
                    let group = group_dir.file_name().unwrap().to_string_lossy();
                    let version = version_dir.file_name().unwrap().to_string_lossy();
                    let key = format!("{}:{}:{}", group, artifact, version);
                    jar_lookup.insert(key, jar.clone());
                    // Also insert without version for simpler lookup
                    let ga_key = format!("{}:{}", group, artifact);
                    jar_lookup.entry(ga_key).or_insert(jar.clone());
                }
            }
        }
    }

    // 4. Distribute jars per module: each module gets the full resolved set
    //    (since transitive deps are shared across the workspace)
    let mut per_module = HashMap::new();
    for (name, _deps) in all_module_deps {
        // Each module gets all resolved jars (workspace shares classpath)
        per_module.insert(name.clone(), all_jars.clone());
    }

    Ok(per_module)
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

#[cfg(test)]
mod tests {
    use super::*;

    // --- MavenCoord tests ---

    #[test]
    fn test_maven_coord_parse_basic() {
        let mc = MavenCoord::parse("org.springframework:spring-core", "5.3.0").unwrap();
        assert_eq!(mc.group_id, "org.springframework");
        assert_eq!(mc.artifact_id, "spring-core");
        assert_eq!(mc.version, "5.3.0");
    }

    #[test]
    fn test_maven_coord_parse_strips_prefix() {
        let mc = MavenCoord::parse("com.example:lib", "^2.19.0").unwrap();
        assert_eq!(mc.version, "2.19.0");
        let mc2 = MavenCoord::parse("com.example:lib", "~1.5.0").unwrap();
        assert_eq!(mc2.version, "1.5.0");
    }

    #[test]
    fn test_maven_coord_parse_invalid() {
        assert!(MavenCoord::parse("invalid", "1.0").is_err());
        assert!(MavenCoord::parse("a:b:c", "1.0").is_err());
    }

    #[test]
    fn test_maven_coord_keys() {
        let mc = MavenCoord::parse("org.example:lib", "1.0").unwrap();
        assert_eq!(mc.key(), "org.example:lib");
        assert_eq!(mc.versioned_key(), "org.example:lib:1.0");
    }

    #[test]
    fn test_maven_coord_urls() {
        let mc = MavenCoord::parse("org.example:lib", "1.0").unwrap();
        let repo = "https://repo1.maven.org/maven2";
        assert_eq!(mc.jar_url(repo), "https://repo1.maven.org/maven2/org/example/lib/1.0/lib-1.0.jar");
        assert_eq!(mc.pom_url(repo), "https://repo1.maven.org/maven2/org/example/lib/1.0/lib-1.0.pom");
    }

    #[test]
    fn test_maven_coord_paths() {
        let mc = MavenCoord::parse("org.example:lib", "1.0").unwrap();
        let cache = Path::new("/tmp/cache");
        assert_eq!(mc.jar_path(cache), PathBuf::from("/tmp/cache/org.example/lib/1.0/lib-1.0.jar"));
        assert_eq!(mc.pom_path(cache), PathBuf::from("/tmp/cache/org.example/lib/1.0/lib-1.0.pom"));
    }

    // --- Property resolution tests (Phase 1.4) ---

    #[test]
    fn test_resolve_properties_simple() {
        let mut props = HashMap::new();
        props.insert("spring.version".to_string(), "5.3.0".to_string());
        assert_eq!(resolve_properties("${spring.version}", &props), "5.3.0");
    }

    #[test]
    fn test_resolve_properties_no_placeholder() {
        let props = HashMap::new();
        assert_eq!(resolve_properties("plain-text", &props), "plain-text");
    }

    #[test]
    fn test_resolve_properties_transitive() {
        let mut props = HashMap::new();
        props.insert("base".to_string(), "1.0".to_string());
        props.insert("derived".to_string(), "${base}".to_string());
        // ${derived} -> ${base} -> 1.0
        assert_eq!(resolve_properties("${derived}", &props), "1.0");
    }

    #[test]
    fn test_resolve_properties_multiple() {
        let mut props = HashMap::new();
        props.insert("g".to_string(), "org.example".to_string());
        props.insert("v".to_string(), "2.0".to_string());
        assert_eq!(resolve_properties("${g}:lib:${v}", &props), "org.example:lib:2.0");
    }

    #[test]
    fn test_resolve_properties_unresolved() {
        let props = HashMap::new();
        assert_eq!(resolve_properties("${unknown}", &props), "${unknown}");
    }

    // --- POM property collection tests ---

    #[test]
    fn test_collect_pom_properties_basic() {
        let pom = r#"<?xml version="1.0"?>
<project>
    <groupId>org.example</groupId>
    <artifactId>my-app</artifactId>
    <version>1.0.0</version>
    <properties>
        <spring.version>5.3.0</spring.version>
        <java.version>17</java.version>
    </properties>
</project>"#;
        let doc = roxmltree::Document::parse(pom).unwrap();
        let props = collect_pom_properties(&doc);
        assert_eq!(props.get("spring.version").unwrap(), "5.3.0");
        assert_eq!(props.get("java.version").unwrap(), "17");
        assert_eq!(props.get("project.groupId").unwrap(), "org.example");
        assert_eq!(props.get("project.version").unwrap(), "1.0.0");
        assert_eq!(props.get("project.artifactId").unwrap(), "my-app");
    }

    #[test]
    fn test_collect_pom_properties_inherits_parent() {
        let pom = r#"<?xml version="1.0"?>
<project>
    <parent>
        <groupId>org.parent</groupId>
        <artifactId>parent-pom</artifactId>
        <version>2.0.0</version>
    </parent>
    <artifactId>child</artifactId>
</project>"#;
        let doc = roxmltree::Document::parse(pom).unwrap();
        let props = collect_pom_properties(&doc);
        // groupId and version inherited from parent
        assert_eq!(props.get("project.groupId").unwrap(), "org.parent");
        assert_eq!(props.get("project.version").unwrap(), "2.0.0");
        assert_eq!(props.get("parent.groupId").unwrap(), "org.parent");
        assert_eq!(props.get("parent.version").unwrap(), "2.0.0");
    }

    // --- Managed versions / BOM tests (Phase 1.1) ---

    #[test]
    fn test_collect_managed_versions_basic() {
        let pom = r#"<?xml version="1.0"?>
<project>
    <dependencyManagement>
        <dependencies>
            <dependency>
                <groupId>com.example</groupId>
                <artifactId>lib-a</artifactId>
                <version>1.0</version>
            </dependency>
            <dependency>
                <groupId>com.example</groupId>
                <artifactId>lib-b</artifactId>
                <version>2.0</version>
            </dependency>
        </dependencies>
    </dependencyManagement>
</project>"#;
        let doc = roxmltree::Document::parse(pom).unwrap();
        let props = HashMap::new();
        let client = reqwest::blocking::Client::new();
        let managed = collect_managed_versions_with_bom(&doc, &props, &client, Path::new("/tmp"), &[], 0);
        assert_eq!(managed.get("com.example:lib-a").unwrap(), "1.0");
        assert_eq!(managed.get("com.example:lib-b").unwrap(), "2.0");
    }

    #[test]
    fn test_collect_managed_versions_with_property_resolution() {
        let pom = r#"<?xml version="1.0"?>
<project>
    <properties>
        <lib.version>3.0</lib.version>
    </properties>
    <dependencyManagement>
        <dependencies>
            <dependency>
                <groupId>com.example</groupId>
                <artifactId>lib</artifactId>
                <version>${lib.version}</version>
            </dependency>
        </dependencies>
    </dependencyManagement>
</project>"#;
        let doc = roxmltree::Document::parse(pom).unwrap();
        let mut props = HashMap::new();
        props.insert("lib.version".to_string(), "3.0".to_string());
        let client = reqwest::blocking::Client::new();
        let managed = collect_managed_versions_with_bom(&doc, &props, &client, Path::new("/tmp"), &[], 0);
        assert_eq!(managed.get("com.example:lib").unwrap(), "3.0");
    }

    #[test]
    fn test_bom_depth_limit() {
        let pom = r#"<?xml version="1.0"?>
<project>
    <dependencyManagement>
        <dependencies>
            <dependency>
                <groupId>com.example</groupId>
                <artifactId>bom</artifactId>
                <version>1.0</version>
                <type>pom</type>
                <scope>import</scope>
            </dependency>
        </dependencies>
    </dependencyManagement>
</project>"#;
        let doc = roxmltree::Document::parse(pom).unwrap();
        let props = HashMap::new();
        let client = reqwest::blocking::Client::new();
        // At depth 11 (> 10 limit), should return empty
        let managed = collect_managed_versions_with_bom(&doc, &props, &client, Path::new("/tmp"), &[], 11);
        assert!(managed.is_empty());
    }

    // --- POM dependency parsing tests ---

    #[test]
    fn test_parse_pom_dependencies_basic() {
        let pom = r#"<?xml version="1.0"?>
<project>
    <dependencies>
        <dependency>
            <groupId>com.example</groupId>
            <artifactId>lib</artifactId>
            <version>1.0</version>
        </dependency>
    </dependencies>
</project>"#;
        let props = HashMap::new();
        let client = reqwest::blocking::Client::new();
        let deps = parse_pom_dependencies_with_props(pom, &props, &client, Path::new("/tmp"), &[]).unwrap();
        assert_eq!(deps.len(), 1);
        assert_eq!(deps[0].group_id, "com.example");
        assert_eq!(deps[0].artifact_id, "lib");
        assert_eq!(deps[0].version, "1.0");
    }

    #[test]
    fn test_parse_pom_skips_test_scope() {
        let pom = r#"<?xml version="1.0"?>
<project>
    <dependencies>
        <dependency>
            <groupId>junit</groupId>
            <artifactId>junit</artifactId>
            <version>4.13</version>
            <scope>test</scope>
        </dependency>
    </dependencies>
</project>"#;
        let props = HashMap::new();
        let client = reqwest::blocking::Client::new();
        let deps = parse_pom_dependencies_with_props(pom, &props, &client, Path::new("/tmp"), &[]).unwrap();
        assert!(deps.is_empty());
    }

    #[test]
    fn test_parse_pom_skips_optional() {
        let pom = r#"<?xml version="1.0"?>
<project>
    <dependencies>
        <dependency>
            <groupId>com.example</groupId>
            <artifactId>optional-lib</artifactId>
            <version>1.0</version>
            <optional>true</optional>
        </dependency>
    </dependencies>
</project>"#;
        let props = HashMap::new();
        let client = reqwest::blocking::Client::new();
        let deps = parse_pom_dependencies_with_props(pom, &props, &client, Path::new("/tmp"), &[]).unwrap();
        assert!(deps.is_empty());
    }

    #[test]
    fn test_parse_pom_skips_provided_scope() {
        let pom = r#"<?xml version="1.0"?>
<project>
    <dependencies>
        <dependency>
            <groupId>javax.servlet</groupId>
            <artifactId>servlet-api</artifactId>
            <version>3.0</version>
            <scope>provided</scope>
        </dependency>
    </dependencies>
</project>"#;
        let props = HashMap::new();
        let client = reqwest::blocking::Client::new();
        let deps = parse_pom_dependencies_with_props(pom, &props, &client, Path::new("/tmp"), &[]).unwrap();
        assert!(deps.is_empty());
    }

    #[test]
    fn test_parse_pom_uses_managed_version() {
        let pom = r#"<?xml version="1.0"?>
<project>
    <dependencyManagement>
        <dependencies>
            <dependency>
                <groupId>com.example</groupId>
                <artifactId>lib</artifactId>
                <version>2.0</version>
            </dependency>
        </dependencies>
    </dependencyManagement>
    <dependencies>
        <dependency>
            <groupId>com.example</groupId>
            <artifactId>lib</artifactId>
        </dependency>
    </dependencies>
</project>"#;
        let props = HashMap::new();
        let client = reqwest::blocking::Client::new();
        let deps = parse_pom_dependencies_with_props(pom, &props, &client, Path::new("/tmp"), &[]).unwrap();
        assert_eq!(deps.len(), 1);
        assert_eq!(deps[0].version, "2.0");
    }

    #[test]
    fn test_parse_pom_skips_dependency_management_section() {
        let pom = r#"<?xml version="1.0"?>
<project>
    <dependencyManagement>
        <dependencies>
            <dependency>
                <groupId>com.managed</groupId>
                <artifactId>lib</artifactId>
                <version>1.0</version>
            </dependency>
        </dependencies>
    </dependencyManagement>
</project>"#;
        let props = HashMap::new();
        let client = reqwest::blocking::Client::new();
        let deps = parse_pom_dependencies_with_props(pom, &props, &client, Path::new("/tmp"), &[]).unwrap();
        // dependencyManagement deps should not appear as direct dependencies
        assert!(deps.is_empty());
    }

    // --- Repo list tests ---

    #[test]
    fn test_build_repo_list_default() {
        let repos = build_repo_list(&[]);
        assert_eq!(repos.len(), 1);
        assert_eq!(repos[0], DEFAULT_REPO);
    }

    #[test]
    fn test_build_repo_list_custom() {
        let custom = vec!["https://custom.repo/maven".to_string()];
        let repos = build_repo_list(&custom);
        assert_eq!(repos.len(), 2);
        assert_eq!(repos[0], "https://custom.repo/maven");
        assert_eq!(repos[1], DEFAULT_REPO);
    }

    #[test]
    fn test_build_repo_list_no_duplicate_central() {
        let custom = vec![DEFAULT_REPO.to_string()];
        let repos = build_repo_list(&custom);
        assert_eq!(repos.len(), 1);
    }

    #[test]
    fn test_build_repo_list_trims_trailing_slash() {
        let custom = vec!["https://custom.repo/maven/".to_string()];
        let repos = build_repo_list(&custom);
        assert_eq!(repos[0], "https://custom.repo/maven");
    }

    // --- Lock file conflict detection tests (Phase 1.3) ---

    #[test]
    fn test_check_conflicts_no_conflicts() {
        let mut lock = LockFile::default();
        lock.dependencies.insert("com.example:lib:1.0".to_string(), LockedDependency { sha256: None, dependencies: None });
        let conflicts = check_conflicts(&lock);
        assert!(conflicts.is_empty());
    }

    #[test]
    fn test_check_conflicts_detects_multiple_versions() {
        let mut lock = LockFile::default();
        lock.dependencies.insert("com.example:lib:1.0".to_string(), LockedDependency { sha256: None, dependencies: None });
        lock.dependencies.insert("com.example:lib:2.0".to_string(), LockedDependency { sha256: None, dependencies: None });
        let conflicts = check_conflicts(&lock);
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].0, "com.example:lib");
        assert!(conflicts[0].1.contains(&"1.0".to_string()));
        assert!(conflicts[0].1.contains(&"2.0".to_string()));
    }

    // --- Lock-based resolution tests ---

    #[test]
    fn test_try_resolve_from_lock_empty() {
        let deps = BTreeMap::new();
        let lock = LockFile::default();
        let result = try_resolve_from_lock(&deps, Path::new("/tmp/cache"), &lock);
        // Empty lock returns None
        assert!(result.is_none());
    }
}
