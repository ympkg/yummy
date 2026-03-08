use anyhow::{bail, Result};
use console::style;
use rayon::prelude::*;
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::config::schema::{ResolvedCache, ResolvedDependency};

/// A registry entry with optional scope routing
#[derive(Clone, Debug)]
pub struct RegistryEntry {
    pub url: String,
    pub scope: Option<String>,
}

/// A parsed Maven coordinate
#[derive(Clone)]
pub struct MavenCoord {
    pub group_id: String,
    pub artifact_id: String,
    pub version: String,
    /// Optional classifier (e.g. "natives-linux", "sources", "javadoc")
    pub classifier: Option<String>,
    /// POM-level exclusions declared for this dependency ("groupId:artifactId")
    pub exclusions: Vec<String>,
    /// Effective scope after propagation (compile > provided > runtime > test)
    pub scope: Option<String>,
}

impl MavenCoord {
    pub fn parse(coord: &str, version: &str) -> Result<Self> {
        let parts: Vec<&str> = coord.split(':').collect();
        if parts.len() < 2 || parts.len() > 3 {
            bail!(
                "Invalid dependency coordinate: '{}'. Expected format: groupId:artifactId[:classifier]",
                coord
            );
        }
        // Strip npm-style version prefixes: ^2.19.0 → 2.19.0, ~1.5.0 → 1.5.0
        let clean_version = version.trim_start_matches('^').trim_start_matches('~');
        let classifier = if parts.len() == 3 && !parts[2].is_empty() {
            Some(parts[2].to_string())
        } else {
            None
        };
        Ok(MavenCoord {
            group_id: parts[0].to_string(),
            artifact_id: parts[1].to_string(),
            version: clean_version.to_string(),
            classifier,
            exclusions: Vec::new(),
            scope: None,
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

    pub fn is_snapshot(&self) -> bool {
        self.version.ends_with("-SNAPSHOT")
    }

    pub fn jar_url(&self, repo: &str) -> String {
        let classifier_suffix = self.classifier.as_ref()
            .map(|c| format!("-{}", c))
            .unwrap_or_default();
        format!(
            "{}/{}/{}/{}/{}-{}{}.jar",
            repo,
            self.group_path(),
            self.artifact_id,
            self.version,
            self.artifact_id,
            self.version,
            classifier_suffix
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

    /// For SNAPSHOT versions, resolve the timestamped artifact URL from maven-metadata.xml.
    fn snapshot_jar_url(&self, repo: &str, timestamp: &str, build_number: &str) -> String {
        let base_version = self.version.trim_end_matches("-SNAPSHOT");
        let classifier_suffix = self.classifier.as_ref()
            .map(|c| format!("-{}", c))
            .unwrap_or_default();
        format!(
            "{}/{}/{}/{}/{}-{}-{}-{}{}.jar",
            repo,
            self.group_path(),
            self.artifact_id,
            self.version,
            self.artifact_id,
            base_version,
            timestamp,
            build_number,
            classifier_suffix
        )
    }

    fn snapshot_pom_url(&self, repo: &str, timestamp: &str, build_number: &str) -> String {
        let base_version = self.version.trim_end_matches("-SNAPSHOT");
        format!(
            "{}/{}/{}/{}/{}-{}-{}-{}.pom",
            repo,
            self.group_path(),
            self.artifact_id,
            self.version,
            self.artifact_id,
            base_version,
            timestamp,
            build_number
        )
    }

    fn metadata_url(&self, repo: &str) -> String {
        format!(
            "{}/{}/{}/{}/maven-metadata.xml",
            repo,
            self.group_path(),
            self.artifact_id,
            self.version
        )
    }

    pub fn jar_path(&self, cache: &Path) -> PathBuf {
        let classifier_suffix = self.classifier.as_ref()
            .map(|c| format!("-{}", c))
            .unwrap_or_default();
        cache
            .join(&self.group_id)
            .join(&self.artifact_id)
            .join(&self.version)
            .join(format!("{}-{}{}.jar", self.artifact_id, self.version, classifier_suffix))
    }

    pub fn pom_path(&self, cache: &Path) -> PathBuf {
        cache
            .join(&self.group_id)
            .join(&self.artifact_id)
            .join(&self.version)
            .join(format!("{}-{}.pom", self.artifact_id, self.version))
    }
}

/// Scope strength: lower number = stronger scope.
fn scope_strength(scope: &str) -> u8 {
    match scope {
        "compile" => 0,
        "provided" => 1,
        "runtime" => 2,
        "test" => 3,
        _ => 0, // unknown defaults to compile
    }
}

/// Return the stronger (lower number) of two scopes.
fn stronger_scope(a: &str, b: &str) -> String {
    if scope_strength(a) <= scope_strength(b) { a.to_string() } else { b.to_string() }
}

/// Return the weaker (higher number) of two scopes.
fn weaker_scope(a: &str, b: &str) -> String {
    if scope_strength(a) >= scope_strength(b) {
        a.to_string()
    } else {
        b.to_string()
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
    lock: &mut ResolvedCache,
) -> Result<Vec<PathBuf>> {
    resolve_and_download_full(dependencies, cache_dir, lock, &[], &[])
}

/// Full resolve with repos, exclusions, and resolutions.
pub fn resolve_and_download_full(
    dependencies: &BTreeMap<String, String>,
    cache_dir: &Path,
    lock: &mut ResolvedCache,
    registries: &[RegistryEntry],
    exclusions: &[String],
) -> Result<Vec<PathBuf>> {
    resolve_and_download_with_resolutions(dependencies, cache_dir, lock, registries, exclusions, &BTreeMap::new())
}

/// Full resolve with repos, exclusions, and resolutions that override all versions.
pub fn resolve_and_download_with_resolutions(
    dependencies: &BTreeMap<String, String>,
    cache_dir: &Path,
    lock: &mut ResolvedCache,
    registries: &[RegistryEntry],
    exclusions: &[String],
    resolutions: &BTreeMap<String, String>,
) -> Result<Vec<PathBuf>> {
    resolve_and_download_with_scopes(
        dependencies, cache_dir, lock, registries, exclusions, resolutions, &HashMap::new(),
    )
}

/// Full resolve with scope propagation for transitive dependencies.
/// `dep_scopes` maps "groupId:artifactId" to its direct dependency scope.
pub fn resolve_and_download_with_scopes(
    dependencies: &BTreeMap<String, String>,
    cache_dir: &Path,
    lock: &mut ResolvedCache,
    registries: &[RegistryEntry],
    exclusions: &[String],
    resolutions: &BTreeMap<String, String>,
    dep_scopes: &HashMap<String, String>,
) -> Result<Vec<PathBuf>> {
    let exclusion_set: HashSet<String> = exclusions.iter().cloned().collect();
    // Fast path: try to resolve entirely from lock file + local cache
    if resolutions.is_empty() {
        if let Some(jars) = try_resolve_from_lock(dependencies, cache_dir, lock) {
            return Ok(jars);
        }
    } else {
        // With resolutions, also check fast path but with resolved deps
        let mut resolved_deps = dependencies.clone();
        for (k, v) in resolutions {
            if resolved_deps.contains_key(k) {
                resolved_deps.insert(k.clone(), v.clone());
            }
        }
        if let Some(jars) = try_resolve_from_lock(&resolved_deps, cache_dir, lock) {
            return Ok(jars);
        }
    }

    // Slow path: resolve from network
    // Phase 1: resolve the full dependency graph (BFS), collecting coordinates to download
    let mut visited = HashSet::new();
    let mut queue = VecDeque::new();
    let mut ordered_keys = Vec::new(); // track order for jar collection

    let client = reqwest::blocking::Client::builder()
        .user_agent(concat!("ym/", env!("CARGO_PKG_VERSION")))
        .timeout(std::time::Duration::from_secs(30))
        .build()?;

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
    // Accumulated POM-level exclusions per versioned_key (propagated from parent)
    let mut accumulated_exclusions: HashMap<String, HashSet<String>> = HashMap::new();
    // Scope tracking: maps versioned_key -> effective scope
    // Scope strength: compile(0) > provided(1) > runtime(2) > test(3)
    let mut scope_map: HashMap<String, String> = HashMap::new();

    // Initialize direct dependencies at depth 0
    for (coord, version) in dependencies {
        let mut mc = MavenCoord::parse(coord, version)?;
        let ga_key = format!("{}:{}", mc.group_id, mc.artifact_id);
        let direct_scope = dep_scopes.get(&ga_key).cloned().unwrap_or_else(|| "compile".to_string());
        mc.scope = Some(direct_scope.clone());
        scope_map.insert(mc.versioned_key(), direct_scope);
        depth_map.insert(mc.versioned_key(), 0);
        resolved_versions.insert(ga_key, (0, mc.version.clone()));
        queue.push_back(mc);
    }

    while !queue.is_empty() {
        // Drain the current level for batched processing
        let mut current_level: Vec<MavenCoord> = Vec::new();
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
                    continue;
                }
            }

            visited.insert(key.clone());
            ordered_keys.push(key.clone());
            resolved_versions.entry(ga_key).or_insert((current_depth, coord.version.clone()));

            // Queue JAR download if not cached
            let jar_path = coord.jar_path(cache_dir);
            if !jar_path.exists() {
                coords_to_download.push((key, coord.clone()));
            } else {
                // Still need to record the key for lock file
                let _ = &key;
            }

            current_level.push(coord);
        }

        if current_level.is_empty() {
            break;
        }

        // Resolve transitive deps for this level (parallel if > 1 item and POMs not cached)
        let level_results: Vec<(MavenCoord, Vec<MavenCoord>)> = if current_level.len() > 1 {
            current_level
                .par_iter()
                .map(|coord| {
                    let transitive = resolve_transitive_cached(
                        &client, coord, cache_dir, registries, Some(&pom_cache),
                    ).unwrap_or_default();
                    (coord.clone(), transitive)
                })
                .collect()
        } else {
            current_level
                .iter()
                .map(|coord| {
                    let transitive = resolve_transitive_cached(
                        &client, coord, cache_dir, registries, Some(&pom_cache),
                    ).unwrap_or_default();
                    (coord.clone(), transitive)
                })
                .collect()
        };

        for (coord, transitive) in level_results {
            let key = coord.versioned_key();
            let current_depth = depth_map.get(&key).copied().unwrap_or(0);

            // Get accumulated exclusions for this coord (from its parent)
            let coord_excl = accumulated_exclusions
                .get(&key)
                .cloned()
                .unwrap_or_default();

            // Apply user-level exclusions AND accumulated POM exclusions
            let transitive: Vec<MavenCoord> = transitive
                .into_iter()
                .filter(|dep| {
                    let dep_key = format!("{}:{}", dep.group_id, dep.artifact_id);
                    !exclusion_set.contains(&dep_key) && !coord_excl.contains(&dep_key)
                })
                .collect();

            let dep_keys: Vec<String> = transitive.iter().map(|c| c.versioned_key()).collect();
            let parent_scope = scope_map.get(&key).cloned().unwrap_or_else(|| "compile".to_string());
            dep_map.insert(key, dep_keys);

            let child_depth = current_depth + 1;

            for mut dep in transitive {
                // Apply resolutions: override transitive dep version if matched
                let ga_key = format!("{}:{}", dep.group_id, dep.artifact_id);
                if let Some(forced_version) = resolutions.get(&ga_key) {
                    dep.version = forced_version.clone();
                }
                let dep_vk = dep.versioned_key();
                depth_map.entry(dep_vk.clone()).or_insert(child_depth);

                // Scope propagation: child inherits parent's scope, narrowed by POM scope.
                // If POM declares this dep as "runtime", and parent is "compile" → child is "runtime".
                // If parent is "runtime" and POM dep is "compile" → child stays "runtime".
                // Rule: take the weaker (higher number) of parent scope and POM scope.
                let pom_scope = dep.scope.as_deref().unwrap_or("compile");
                let effective_scope = weaker_scope(&parent_scope, pom_scope);

                // If this GA was already seen via another path, keep the stronger scope
                if let Some(existing) = scope_map.get(&dep_vk) {
                    let merged = stronger_scope(existing, &effective_scope);
                    scope_map.insert(dep_vk.clone(), merged.to_string());
                } else {
                    scope_map.insert(dep_vk.clone(), effective_scope.to_string());
                }
                dep.scope = Some(scope_map.get(&dep_vk).cloned().unwrap_or_else(|| "compile".to_string()));

                // Propagate exclusions: parent's accumulated + this dep's own POM-level exclusions
                let mut child_excl = coord_excl.clone();
                for e in &dep.exclusions {
                    child_excl.insert(e.clone());
                }
                if !child_excl.is_empty() {
                    accumulated_exclusions
                        .entry(dep_vk)
                        .or_default()
                        .extend(child_excl);
                }

                queue.push_back(dep);
            }
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
                let hash_result = if coord.is_snapshot() {
                    // For SNAPSHOT, try timestamped version from maven-metadata.xml first
                    if let Some((ts, bn)) = resolve_snapshot_version(&client, &coord, registries) {
                        download_from_repos(&client, &coord, &jar_path, registries, |c, r| c.snapshot_jar_url(r, &ts, &bn))
                    } else {
                        // Fallback: plain SNAPSHOT naming (some repos use this)
                        download_from_repos(&client, &coord, &jar_path, registries, |c, r| c.jar_url(r))
                    }
                } else {
                    download_from_repos(&client, &coord, &jar_path, registries, |c, r| c.jar_url(r))
                };
                // Try GPG signature verification if JAR downloaded successfully
                if hash_result.is_ok() {
                    verify_gpg_signature(&client, &coord, &jar_path, registries);
                }
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

        // Print GPG verification summary (if any failures)
        print_gpg_summary();

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
            classifier: None,
            exclusions: Vec::new(),
            scope: None,
        };
        all_jars.push(coord.jar_path(cache_dir));

        let dep_keys = dep_map.remove(key).unwrap_or_default();
        let effective_scope = scope_map.get(key).cloned();
        // Insert lock entry if not already present (download phase may have set sha)
        lock.dependencies.entry(key.clone()).or_insert(ResolvedDependency {
            sha256: None,
            dependencies: if dep_keys.is_empty() {
                None
            } else {
                Some(dep_keys)
            },
            scope: effective_scope,
        });
    }

    Ok(all_jars)
}

/// Resolve SNAPSHOT version: fetch maven-metadata.xml to find the latest timestamped version.
/// Returns (timestamp, buildNumber) or None if the SNAPSHOT uses plain naming.
fn resolve_snapshot_version(
    client: &reqwest::blocking::Client,
    coord: &MavenCoord,
    registries: &[RegistryEntry],
) -> Option<(String, String)> {
    let repos = repos_for_group_id(registries, &coord.group_id);
    for repo in &repos {
        let url = coord.metadata_url(repo);

        let mut request = client.get(&url);
        if let Some((username, password)) = load_credentials_for_url(&url) {
            request = request.basic_auth(username, Some(password));
        }

        if let Ok(response) = request.send() {
            if response.status().is_success() {
                if let Ok(text) = response.text() {
                    // Parse <snapshot><timestamp>...</timestamp><buildNumber>...</buildNumber></snapshot>
                    if let (Some(ts), Some(bn)) = (
                        extract_xml_text(&text, "timestamp"),
                        extract_xml_text(&text, "buildNumber"),
                    ) {
                        return Some((ts, bn));
                    }
                }
            }
        }
    }
    None
}

/// Try to resolve all dependencies from lock file without network access.
/// Returns None if any dep is missing from lock or cache.
fn try_resolve_from_lock(
    dependencies: &BTreeMap<String, String>,
    cache_dir: &Path,
    lock: &ResolvedCache,
) -> Option<Vec<PathBuf>> {
    if lock.dependencies.is_empty() {
        return None;
    }

    // If any dependency is a SNAPSHOT, skip fast path (need to check for updates)
    for version in dependencies.values() {
        if version.ends_with("-SNAPSHOT") {
            return None;
        }
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
                        classifier: None,
                        exclusions: Vec::new(),
                        scope: None,
                    });
                }
            }
        }
    }

    Some(all_jars)
}

/// In-memory cache for parsed POM transitive dependencies.
/// Key: groupId:artifactId:version, Value: list of dependency coords with exclusions.
struct PomCache {
    entries: Mutex<HashMap<String, Vec<(String, String, String, Vec<String>)>>>,
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
                .map(|(g, a, ver, excl)| MavenCoord {
                    group_id: g.clone(),
                    artifact_id: a.clone(),
                    version: ver.clone(),
                    classifier: None,
                    exclusions: excl.clone(),
                    scope: None,
                })
                .collect()
        })
    }

    fn insert(&self, key: &str, deps: &[MavenCoord]) {
        let mut entries = self.entries.lock().unwrap();
        entries.insert(
            key.to_string(),
            deps.iter()
                .map(|d| (d.group_id.clone(), d.artifact_id.clone(), d.version.clone(), d.exclusions.clone()))
                .collect(),
        );
    }
}

fn resolve_transitive_cached(
    client: &reqwest::blocking::Client,
    coord: &MavenCoord,
    cache_dir: &Path,
    registries: &[RegistryEntry],
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
            // Try new format with exclusions first, then fallback to old format
            if let Ok(cached_deps) = serde_json::from_str::<Vec<(String, String, String, Vec<String>)>>(&content) {
                let deps: Vec<MavenCoord> = cached_deps
                    .iter()
                    .map(|(g, a, v, excl)| MavenCoord {
                        group_id: g.clone(),
                        artifact_id: a.clone(),
                        version: v.clone(),
                        classifier: None,
                        exclusions: excl.clone(),
                        scope: None,
                    })
                    .collect();
                if let Some(cache) = pom_cache {
                    cache.insert(&cache_key, &deps);
                }
                return Ok(deps);
            }
            // Fallback: old format without exclusions — delete stale cache so it gets regenerated
            if serde_json::from_str::<Vec<(String, String, String)>>(&content).is_ok() {
                let _ = std::fs::remove_file(&pom_cache_file);
            }
        }
    }

    let pom_path = coord.pom_path(cache_dir);

    // For SNAPSHOT, always re-download POM (may have changed)
    if !pom_path.exists() || coord.is_snapshot() {
        let pom_result = if coord.is_snapshot() {
            if let Some((ts, bn)) = resolve_snapshot_version(client, coord, registries) {
                download_from_repos(client, coord, &pom_path, registries, |c, r| c.snapshot_pom_url(r, &ts, &bn))
            } else {
                download_from_repos(client, coord, &pom_path, registries, |c, r| c.pom_url(r))
            }
        } else {
            download_from_repos(client, coord, &pom_path, registries, |c, r| c.pom_url(r))
        };
        if pom_result.is_err() {
            return Ok(vec![]); // POM not found is non-fatal
        }
    }

    let pom_content = std::fs::read_to_string(&pom_path)?;

    // Collect parent POM properties (unlimited depth with cycle detection)
    let mut all_properties = HashMap::new();
    let mut visited_poms = HashSet::new();
    resolve_parent_properties(client, &pom_content, cache_dir, registries, &mut all_properties, 0, &mut visited_poms)?;

    let deps = parse_pom_dependencies_with_props(&pom_content, &all_properties, client, cache_dir, registries)?;

    // Write to disk cache (with exclusions)
    if let Some(parent) = pom_cache_file.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let serializable: Vec<(String, String, String, Vec<String>)> = deps
        .iter()
        .map(|d| (d.group_id.clone(), d.artifact_id.clone(), d.version.clone(), d.exclusions.clone()))
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
    registries: &[RegistryEntry],
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
                    classifier: None,
                    exclusions: Vec::new(),
                    scope: None,
                };
                let parent_pom_path = parent_coord.pom_path(cache_dir);
                if !parent_pom_path.exists() {
                    let _ = download_from_repos(client, &parent_coord, &parent_pom_path, registries, |c, r| c.pom_url(r));
                }
                if parent_pom_path.exists() {
                    let parent_content = std::fs::read_to_string(&parent_pom_path)?;
                    // Recurse into grandparent first (so child overrides parent)
                    resolve_parent_properties(client, &parent_content, cache_dir, registries, properties, depth + 1, visited_poms)?;
                    // Then merge parent properties
                    let parent_doc = roxmltree::Document::parse(&parent_content)?;
                    let parent_props = collect_pom_properties(&parent_doc);
                    for (k, v) in parent_props {
                        properties.entry(k).or_insert(v);
                    }
                    // Parent managed versions (including BOM imports)
                    let managed = collect_managed_versions_with_bom(
                        &parent_doc, properties, client, cache_dir, registries, 0,
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
    registries: &[RegistryEntry],
) -> Result<Vec<MavenCoord>> {
    let doc = roxmltree::Document::parse(pom)?;

    // Merge local properties with inherited
    let mut properties = extra_properties.clone();
    let local_props = collect_pom_properties(&doc);
    for (k, v) in local_props {
        properties.insert(k, v);
    }

    let managed = collect_managed_versions_with_bom(
        &doc, &properties, client, cache_dir, registries, 0,
    );

    let mut deps = Vec::new();

    for node in doc.descendants() {
        if node.tag_name().name() != "dependencies" {
            continue;
        }
        // Only accept <dependencies> that are direct children of <project> or <profile>.
        // Skip <dependencies> inside <build>, <plugins>, <plugin>, <reporting>,
        // <dependencyManagement>, etc.
        if let Some(parent) = node.parent() {
            let parent_name = parent.tag_name().name();
            match parent_name {
                "project" | "profile" => {} // valid project-level dependencies
                _ => continue, // skip plugin deps, dependencyManagement, etc.
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
            let mut dep_exclusions = Vec::new();

            for child in dep.children() {
                match child.tag_name().name() {
                    "groupId" => group_id = child.text().map(|s| s.to_string()),
                    "artifactId" => artifact_id = child.text().map(|s| s.to_string()),
                    "version" => version = child.text().map(|s| s.to_string()),
                    "scope" => scope = child.text().map(|s| s.to_string()),
                    "optional" => optional = child.text() == Some("true"),
                    "exclusions" => {
                        for excl in child.children() {
                            if excl.tag_name().name() != "exclusion" {
                                continue;
                            }
                            let mut eg = None;
                            let mut ea = None;
                            for ec in excl.children() {
                                match ec.tag_name().name() {
                                    "groupId" => eg = ec.text().map(|s| s.to_string()),
                                    "artifactId" => ea = ec.text().map(|s| s.to_string()),
                                    _ => {}
                                }
                            }
                            if let (Some(g), Some(a)) = (eg, ea) {
                                dep_exclusions.push(format!("{}:{}", g, a));
                            }
                        }
                    }
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
                            classifier: None,
                            exclusions: dep_exclusions,
                            scope: scope.clone(),
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
    registries: &[RegistryEntry],
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
                                    classifier: None,
                                    exclusions: Vec::new(),
                                    scope: None,
                                };
                                let bom_pom_path = bom_coord.pom_path(cache_dir);
                                if !bom_pom_path.exists() {
                                    let _ = download_from_repos(
                                        client, &bom_coord, &bom_pom_path, registries,
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
                                                client, &bom_content, cache_dir, registries,
                                                &mut bom_props, 0, &mut bom_visited,
                                            );

                                            // Recursively collect managed versions from BOM
                                            let bom_managed = collect_managed_versions_with_bom(
                                                &bom_doc, &bom_props, client, cache_dir, registries,
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
/// Build the list of repo URLs to try for a specific dependency's groupId.
///
/// Scope routing per spec:
/// 1. If groupId matches a registry's `scope` → only that registry (no fallback)
/// 2. No scope match → try registries without scope in order, then Maven Central
/// 3. Maven Central is always fallback unless dependency was scope-routed
fn repos_for_group_id(registries: &[RegistryEntry], group_id: &str) -> Vec<String> {
    // Step 1: Check for scope-matched registries
    for entry in registries {
        if let Some(ref scope_pattern) = entry.scope {
            if matches_scope(group_id, scope_pattern) {
                return vec![entry.url.trim_end_matches('/').to_string()];
            }
        }
    }

    // Step 2: No scope match — collect unscoped registries + Maven Central
    let mut repos: Vec<String> = registries
        .iter()
        .filter(|e| e.scope.is_none())
        .map(|e| e.url.trim_end_matches('/').to_string())
        .collect();
    let central = DEFAULT_REPO.to_string();
    if !repos.contains(&central) {
        repos.push(central);
    }
    repos
}

/// Check if a groupId matches a scope pattern.
/// Pattern "com.mycompany.*" matches groupId starting with "com.mycompany."
/// Pattern "com.mycompany" matches exactly "com.mycompany"
fn matches_scope(group_id: &str, pattern: &str) -> bool {
    if let Some(prefix) = pattern.strip_suffix(".*") {
        group_id == prefix || group_id.starts_with(&format!("{}.", prefix))
    } else {
        group_id == pattern
    }
}

/// Try to download an artifact from scope-routed repos, stopping at the first success.
/// Returns the SHA-256 hash of the downloaded file.
fn download_from_repos(
    client: &reqwest::blocking::Client,
    coord: &MavenCoord,
    path: &Path,
    registries: &[RegistryEntry],
    url_fn: impl Fn(&MavenCoord, &str) -> String,
) -> Result<String> {
    let repos = repos_for_group_id(registries, &coord.group_id);
    let mut last_err = None;
    for repo in &repos {
        let url = url_fn(coord, repo);
        match download_file(client, &url, path) {
            Ok(hash) => return Ok(hash),
            Err(e) => last_err = Some(e),
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow::anyhow!("No repositories configured")))
}

/// Download a file and return its SHA-256 hash.
/// Retries up to 3 times with exponential backoff (1s → 2s → 4s).
fn download_file(client: &reqwest::blocking::Client, url: &str, path: &Path) -> Result<String> {
    let max_retries = 3;
    let mut last_err = None;

    for attempt in 0..max_retries {
        if attempt > 0 {
            let delay = std::time::Duration::from_secs(1 << attempt); // 2s, 4s
            std::thread::sleep(delay);
        }

        let mut request = client.get(url);

        // Apply credentials if available for this URL
        if let Some((username, password)) = load_credentials_for_url(url) {
            request = request.basic_auth(username, Some(password));
        }

        match request.send() {
            Ok(response) => {
                if response.status().is_success() {
                    if let Some(parent) = path.parent() {
                        std::fs::create_dir_all(parent)?;
                    }
                    let bytes = response.bytes()?;
                    let hash = crate::compiler::incremental::hash_bytes(&bytes);
                    std::fs::write(path, &bytes)?;
                    return Ok(hash);
                }
                // 404 means artifact doesn't exist in this repo, no retry
                if response.status().as_u16() == 404 {
                    bail!("HTTP 404 for {}", url);
                }
                last_err = Some(anyhow::anyhow!("HTTP {} for {}", response.status(), url));
            }
            Err(e) => {
                last_err = Some(anyhow::anyhow!("Request failed for {}: {}", url, e));
            }
        }
    }

    Err(last_err.unwrap_or_else(|| anyhow::anyhow!("Download failed for {}", url)))
}

/// Counter for GPG verification failures (to avoid spamming warnings).
static GPG_FAIL_COUNT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
static GPG_WARNED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Try to verify GPG signature for a downloaded JAR.
/// Downloads the .jar.asc file and runs `gpg --verify`.
/// On failure, counts silently; summary printed after download phase.
fn verify_gpg_signature(
    client: &reqwest::blocking::Client,
    coord: &MavenCoord,
    jar_path: &Path,
    registries: &[RegistryEntry],
) {
    let asc_path = jar_path.with_extension("jar.asc");

    // Try to download the .asc signature file
    let asc_result = download_from_repos(client, coord, &asc_path, registries, |c, r| {
        format!("{}.asc", c.jar_url(r))
    });

    if asc_result.is_err() {
        // No .asc file available — common for non-Central repos, skip silently
        return;
    }

    // Check if gpg is available
    let gpg_check = std::process::Command::new("gpg")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();

    if gpg_check.is_err() || !gpg_check.unwrap().success() {
        // gpg not installed, skip verification
        let _ = std::fs::remove_file(&asc_path);
        return;
    }

    // Run gpg --verify
    let status = std::process::Command::new("gpg")
        .arg("--batch")
        .arg("--verify")
        .arg(&asc_path)
        .arg(jar_path)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();

    match status {
        Ok(s) if s.success() => {
            // Signature valid — clean up .asc file
            let _ = std::fs::remove_file(&asc_path);
        }
        _ => {
            GPG_FAIL_COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let _ = std::fs::remove_file(&asc_path);
        }
    }
}

/// Print summary of GPG verification failures (called after download phase).
fn print_gpg_summary() {
    let count = GPG_FAIL_COUNT.swap(0, std::sync::atomic::Ordering::Relaxed);
    if count > 0 && !GPG_WARNED.swap(true, std::sync::atomic::Ordering::Relaxed) {
        eprintln!(
            "  {} {} artifact(s) failed GPG signature verification (missing public keys?)",
            console::style("⚠").yellow(),
            count
        );
    }
}

/// Extract text content of a simple XML tag (e.g. `<timestamp>20240101.120000</timestamp>`).
fn extract_xml_text(xml: &str, tag: &str) -> Option<String> {
    let open = format!("<{}>", tag);
    let close = format!("</{}>", tag);
    let start = xml.find(&open)? + open.len();
    let end = xml[start..].find(&close)? + start;
    Some(xml[start..end].trim().to_string())
}

/// Load credentials for the given URL.
/// Priority: env vars > credentials.json file.
/// Env vars: YM_REGISTRY_USERNAME + YM_REGISTRY_PASSWORD, or YM_REGISTRY_TOKEN (Bearer).
/// File format: { "https://maven.example.com": { "username": "...", "password": "..." } }
fn load_credentials_for_url(url: &str) -> Option<(String, String)> {
    // 1. Check environment variables (highest priority)
    if let (Ok(username), Ok(password)) = (
        std::env::var("YM_REGISTRY_USERNAME"),
        std::env::var("YM_REGISTRY_PASSWORD"),
    ) {
        return Some((username, password));
    }
    // Bearer token as username with empty password (reqwest basic_auth encodes it)
    if let Ok(token) = std::env::var("YM_REGISTRY_TOKEN") {
        return Some((token, String::new()));
    }

    // 2. Check credentials.json file
    let home = std::env::var("HOME").ok()?;
    let creds_path = PathBuf::from(home).join(".ym").join("credentials.json");
    let content = std::fs::read_to_string(&creds_path).ok()?;
    let creds: std::collections::BTreeMap<String, serde_json::Value> =
        serde_json::from_str(&content).ok()?;

    let normalized = url.trim_end_matches('/');

    for (registry_url, value) in &creds {
        let reg_normalized = registry_url.trim_end_matches('/');
        if normalized.starts_with(reg_normalized) {
            // Support both {"username","password"} and {"token"} formats
            if let Some(token) = value.get("token").and_then(|t| t.as_str()) {
                return Some((token.to_string(), String::new()));
            }
            let username = value.get("username")?.as_str()?.to_string();
            let password = value.get("password")?.as_str()?.to_string();
            return Some((username, password));
        }
    }

    None
}

/// Check for dependency version conflicts in the resolved dependency set.
/// Returns a list of (groupId:artifactId, [versions]) for artifacts with multiple versions.
pub fn check_conflicts(lock: &ResolvedCache) -> Vec<(String, Vec<String>)> {
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
#[allow(dead_code)]
pub fn resolve_workspace_deps(
    all_module_deps: &[(String, BTreeMap<String, String>)],
    cache_dir: &Path,
    lock: &mut ResolvedCache,
    registries: &[RegistryEntry],
    exclusions: &[String],
) -> Result<HashMap<String, Vec<PathBuf>>> {
    resolve_workspace_deps_with_resolutions(all_module_deps, cache_dir, lock, registries, exclusions, &Default::default())
}

#[allow(clippy::too_many_arguments)]
pub fn resolve_workspace_deps_with_resolutions(
    all_module_deps: &[(String, BTreeMap<String, String>)],
    cache_dir: &Path,
    lock: &mut ResolvedCache,
    registries: &[RegistryEntry],
    exclusions: &[String],
    resolutions: &BTreeMap<String, String>,
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
    let _all_jars = resolve_and_download_with_resolutions(&merged_deps, cache_dir, lock, registries, exclusions, resolutions)?;

    // 3. Build a versioned key lookup: groupId:artifactId -> versioned key (g:a:v)
    //    using resolved_versions from the lock file
    let mut ga_to_versioned: HashMap<String, String> = HashMap::new();
    for key in lock.dependencies.keys() {
        let parts: Vec<&str> = key.split(':').collect();
        if parts.len() == 3 {
            let ga = format!("{}:{}", parts[0], parts[1]);
            ga_to_versioned.entry(ga).or_insert(key.clone());
        }
    }

    // 4. Per module: walk the lock file graph to collect only relevant jars
    let mut per_module = HashMap::new();
    for (name, deps) in all_module_deps {
        let mut module_jars = Vec::new();
        let mut visited_keys = HashSet::new();
        let mut queue = VecDeque::new();

        // Seed with this module's direct deps
        for (coord, version) in deps {
            let mc = MavenCoord::parse(coord, version);
            if let Ok(mc) = mc {
                let vk = mc.versioned_key();
                // Try exact match first, then GA lookup
                if lock.dependencies.contains_key(&vk) {
                    queue.push_back(vk);
                } else if let Some(resolved_key) = ga_to_versioned.get(coord) {
                    queue.push_back(resolved_key.clone());
                }
            }
        }

        // BFS through lock file dep graph
        while let Some(key) = queue.pop_front() {
            if !visited_keys.insert(key.clone()) {
                continue;
            }
            let parts: Vec<&str> = key.split(':').collect();
            if parts.len() == 3 {
                let coord = MavenCoord {
                    group_id: parts[0].to_string(),
                    artifact_id: parts[1].to_string(),
                    version: parts[2].to_string(),
                    classifier: None,
                    exclusions: Vec::new(),
                    scope: None,
                };
                let jar = coord.jar_path(cache_dir);
                if jar.exists() {
                    module_jars.push(jar);
                }
            }
            // Add transitive deps from lock
            if let Some(locked) = lock.dependencies.get(&key) {
                if let Some(ref dep_keys) = locked.dependencies {
                    for dk in dep_keys {
                        if !visited_keys.contains(dk) {
                            queue.push_back(dk.clone());
                        }
                    }
                }
            }
        }

        per_module.insert(name.clone(), module_jars);
    }

    Ok(per_module)
}

/// Search Maven Central for an artifact by keyword
pub fn search_maven(query: &str) -> Result<Vec<(String, String, String)>> {
    let client = reqwest::blocking::Client::builder()
        .user_agent(concat!("ym/", env!("CARGO_PKG_VERSION")))
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
        .user_agent(concat!("ym/", env!("CARGO_PKG_VERSION")))
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
        assert!(MavenCoord::parse("a:b:c:d", "1.0").is_err()); // too many parts
    }

    #[test]
    fn test_maven_coord_parse_classifier() {
        let mc = MavenCoord::parse("org.lwjgl:lwjgl:natives-linux", "3.3.3").unwrap();
        assert_eq!(mc.group_id, "org.lwjgl");
        assert_eq!(mc.artifact_id, "lwjgl");
        assert_eq!(mc.version, "3.3.3");
        assert_eq!(mc.classifier.as_deref(), Some("natives-linux"));
        assert!(mc.jar_url("https://repo1.maven.org/maven2").contains("lwjgl-3.3.3-natives-linux.jar"));
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

    // --- Scope routing tests ---

    #[test]
    fn test_repos_for_group_id_no_registries() {
        let repos = repos_for_group_id(&[], "com.example");
        assert_eq!(repos.len(), 1);
        assert_eq!(repos[0], DEFAULT_REPO);
    }

    #[test]
    fn test_repos_for_group_id_unscoped_registry() {
        let entries = vec![
            RegistryEntry { url: "https://custom.repo/maven".into(), scope: None },
        ];
        let repos = repos_for_group_id(&entries, "com.example");
        assert_eq!(repos.len(), 2);
        assert_eq!(repos[0], "https://custom.repo/maven");
        assert_eq!(repos[1], DEFAULT_REPO);
    }

    #[test]
    fn test_repos_for_group_id_scope_match() {
        let entries = vec![
            RegistryEntry { url: "https://private.repo/maven".into(), scope: Some("com.mycompany.*".into()) },
            RegistryEntry { url: "https://other.repo/maven".into(), scope: None },
        ];
        // Matching groupId → only scoped repo, no fallback
        let repos = repos_for_group_id(&entries, "com.mycompany.core");
        assert_eq!(repos.len(), 1);
        assert_eq!(repos[0], "https://private.repo/maven");
        // Non-matching groupId → unscoped repos + Central
        let repos = repos_for_group_id(&entries, "org.apache.commons");
        assert_eq!(repos.len(), 2);
        assert_eq!(repos[0], "https://other.repo/maven");
        assert_eq!(repos[1], DEFAULT_REPO);
    }

    #[test]
    fn test_repos_for_group_id_no_duplicate_central() {
        let entries = vec![
            RegistryEntry { url: DEFAULT_REPO.into(), scope: None },
        ];
        let repos = repos_for_group_id(&entries, "com.example");
        assert_eq!(repos.len(), 1);
    }

    #[test]
    fn test_repos_for_group_id_trims_trailing_slash() {
        let entries = vec![
            RegistryEntry { url: "https://custom.repo/maven/".into(), scope: None },
        ];
        let repos = repos_for_group_id(&entries, "com.example");
        assert_eq!(repos[0], "https://custom.repo/maven");
    }

    #[test]
    fn test_matches_scope_wildcard() {
        assert!(matches_scope("com.mycompany.core", "com.mycompany.*"));
        assert!(matches_scope("com.mycompany.core.utils", "com.mycompany.*"));
        assert!(matches_scope("com.mycompany", "com.mycompany.*"));
        assert!(!matches_scope("com.mycompanyextras", "com.mycompany.*"));
        assert!(!matches_scope("org.apache", "com.mycompany.*"));
    }

    #[test]
    fn test_matches_scope_exact() {
        assert!(matches_scope("com.mycompany", "com.mycompany"));
        assert!(!matches_scope("com.mycompany.core", "com.mycompany"));
    }

    // --- Lock file conflict detection tests (Phase 1.3) ---

    #[test]
    fn test_check_conflicts_no_conflicts() {
        let mut lock = ResolvedCache::default();
        lock.dependencies.insert("com.example:lib:1.0".to_string(), ResolvedDependency { sha256: None, dependencies: None, scope: None });
        let conflicts = check_conflicts(&lock);
        assert!(conflicts.is_empty());
    }

    #[test]
    fn test_check_conflicts_detects_multiple_versions() {
        let mut lock = ResolvedCache::default();
        lock.dependencies.insert("com.example:lib:1.0".to_string(), ResolvedDependency { sha256: None, dependencies: None, scope: None });
        lock.dependencies.insert("com.example:lib:2.0".to_string(), ResolvedDependency { sha256: None, dependencies: None, scope: None });
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
        let lock = ResolvedCache::default();
        let result = try_resolve_from_lock(&deps, Path::new("/tmp/cache"), &lock);
        // Empty lock returns None
        assert!(result.is_none());
    }

    // --- POM Cache tests ---

    #[test]
    fn test_pom_cache_insert_and_get() {
        let cache = PomCache::new();
        let deps = vec![
            MavenCoord { group_id: "com.example".into(), artifact_id: "lib".into(), version: "1.0".into(), classifier: None, exclusions: Vec::new(), scope: None },
        ];
        cache.insert("com.example:parent:1.0", &deps);
        let cached = cache.get("com.example:parent:1.0");
        assert!(cached.is_some());
        let cached = cached.unwrap();
        assert_eq!(cached.len(), 1);
        assert_eq!(cached[0].group_id, "com.example");
    }

    #[test]
    fn test_pom_cache_miss() {
        let cache = PomCache::new();
        assert!(cache.get("nonexistent:key:1.0").is_none());
    }

    // --- Workspace dep resolution tests ---

    #[test]
    fn test_resolve_workspace_deps_empty() {
        let module_deps: Vec<(String, BTreeMap<String, String>)> = vec![
            ("mod-a".into(), BTreeMap::new()),
        ];
        let mut lock = ResolvedCache::default();
        let result = resolve_workspace_deps(
            &module_deps, Path::new("/tmp/cache"), &mut lock, &[], &[],
        ).unwrap();
        assert_eq!(result.get("mod-a").unwrap().len(), 0);
    }

    // --- MavenCoord clone test ---

    #[test]
    fn test_maven_coord_clone() {
        let mc = MavenCoord::parse("org.example:lib", "1.0").unwrap();
        let mc2 = mc.clone();
        assert_eq!(mc.group_id, mc2.group_id);
        assert_eq!(mc.artifact_id, mc2.artifact_id);
        assert_eq!(mc.version, mc2.version);
    }

    // --- resolve_properties edge cases ---

    #[test]
    fn test_resolve_properties_deeply_nested() {
        let mut props = HashMap::new();
        props.insert("a".to_string(), "${b}".to_string());
        props.insert("b".to_string(), "${c}".to_string());
        props.insert("c".to_string(), "${d}".to_string());
        props.insert("d".to_string(), "final_value".to_string());
        // 4 levels of indirection
        assert_eq!(resolve_properties("${a}", &props), "final_value");
    }

    #[test]
    fn test_resolve_properties_circular_stops() {
        let mut props = HashMap::new();
        props.insert("a".to_string(), "${b}".to_string());
        props.insert("b".to_string(), "${a}".to_string());
        // Circular: should stop after 10 iterations, result will still contain ${...}
        let result = resolve_properties("${a}", &props);
        // Should not hang, and the result alternates between ${a} and ${b}
        assert!(result.contains("${"));
    }

    // --- Import scope skipping in regular deps ---

    #[test]
    fn test_parse_pom_skips_import_scope() {
        let pom = r#"<?xml version="1.0"?>
<project>
    <dependencies>
        <dependency>
            <groupId>org.springframework.boot</groupId>
            <artifactId>spring-boot-dependencies</artifactId>
            <version>3.2.0</version>
            <scope>import</scope>
        </dependency>
    </dependencies>
</project>"#;
        let props = HashMap::new();
        let client = reqwest::blocking::Client::new();
        let deps = parse_pom_dependencies_with_props(pom, &props, &client, Path::new("/tmp"), &[]).unwrap();
        // import scope deps should be skipped in regular dependencies section
        assert!(deps.is_empty());
    }

    // --- Managed version with parent-inherited version ---

    #[test]
    fn test_parse_pom_uses_parent_managed_version() {
        let pom = r#"<?xml version="1.0"?>
<project>
    <dependencies>
        <dependency>
            <groupId>com.example</groupId>
            <artifactId>lib</artifactId>
        </dependency>
    </dependencies>
</project>"#;
        // Simulate parent providing managed version
        let mut props = HashMap::new();
        props.insert("managed:com.example:lib".to_string(), "3.0".to_string());
        let client = reqwest::blocking::Client::new();
        let deps = parse_pom_dependencies_with_props(pom, &props, &client, Path::new("/tmp"), &[]).unwrap();
        assert_eq!(deps.len(), 1);
        assert_eq!(deps[0].version, "3.0");
    }

    // --- Multiple dependencies test ---

    #[test]
    fn test_parse_pom_multiple_deps() {
        let pom = r#"<?xml version="1.0"?>
<project>
    <dependencies>
        <dependency>
            <groupId>com.a</groupId>
            <artifactId>lib-a</artifactId>
            <version>1.0</version>
        </dependency>
        <dependency>
            <groupId>com.b</groupId>
            <artifactId>lib-b</artifactId>
            <version>2.0</version>
        </dependency>
        <dependency>
            <groupId>com.c</groupId>
            <artifactId>lib-c</artifactId>
            <version>3.0</version>
            <scope>runtime</scope>
        </dependency>
    </dependencies>
</project>"#;
        let props = HashMap::new();
        let client = reqwest::blocking::Client::new();
        let deps = parse_pom_dependencies_with_props(pom, &props, &client, Path::new("/tmp"), &[]).unwrap();
        // runtime scope is included (not test/provided/system/import)
        assert_eq!(deps.len(), 3);
    }
}
