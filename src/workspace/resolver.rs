use anyhow::{bail, Context, Result};
use console::style;
use rayon::prelude::*;
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, atomic::{AtomicUsize, Ordering}};
use tempfile::NamedTempFile;

use crate::config::schema::{Lockfile, ResolvedDependency};

fn format_bytes(bytes: u64) -> String {
    if bytes >= 1_073_741_824 {
        format!("{:.1} GB", bytes as f64 / 1_073_741_824.0)
    } else if bytes >= 1_048_576 {
        format!("{:.1} MB", bytes as f64 / 1_048_576.0)
    } else if bytes >= 1024 {
        format!("{:.0} KB", bytes as f64 / 1024.0)
    } else {
        format!("{} B", bytes)
    }
}

fn format_eta(secs: u64) -> String {
    if secs == 0 { return String::new(); }
    if secs >= 60 {
        format!("ETA {}m{}s", secs / 60, secs % 60)
    } else {
        format!("ETA {}s", secs)
    }
}

/// Update progress: updates spinner message when active, otherwise raw eprint.
/// In quiet mode: static line output (no animation), only called at key milestones.
fn resolver_progress(msg: &str) {
    if crate::SPINNER_ACTIVE.load(std::sync::atomic::Ordering::Relaxed) {
        crate::set_spinner_msg(msg);
    } else if crate::is_progress_quiet() {
        eprintln!("{} {}", style(format!("{:>12}", "Resolving")).green().bold(), msg);
    } else {
        eprint!("\r{} {}   ", style(format!("{:>12}", "Resolving")).green().bold(), msg);
    }
}

/// Check if a pom-only marker exists and is still valid (within 7-day TTL).
fn is_pom_only_cached(jar_path: &Path) -> bool {
    let marker = jar_path.with_extension("jar.pom-only");
    match std::fs::read_to_string(&marker) {
        Ok(content) => {
            let created: u64 = content.trim().parse().unwrap_or(0);
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            now.saturating_sub(created) < 7 * 24 * 3600
        }
        Err(_) => false,
    }
}

/// Current platform classifier for native JAR detection (e.g. "linux-x86_64").
fn platform_classifier() -> &'static str {
    if cfg!(target_os = "linux") && cfg!(target_arch = "x86_64") {
        "linux-x86_64"
    } else if cfg!(target_os = "linux") && cfg!(target_arch = "aarch64") {
        "linux-arm64"
    } else if cfg!(target_os = "macos") && cfg!(target_arch = "x86_64") {
        "macosx-x86_64"
    } else if cfg!(target_os = "macos") && cfg!(target_arch = "aarch64") {
        "macosx-arm64"
    } else if cfg!(target_os = "windows") && cfg!(target_arch = "x86_64") {
        "windows-x86_64"
    } else {
        ""
    }
}

/// For each resolved JAR, check if a platform-specific classifier JAR exists
/// in the same directory and append it to the list.
fn append_native_jars(jars: &mut Vec<PathBuf>) {
    let classifier = platform_classifier();
    if classifier.is_empty() { return; }

    let mut extras = Vec::new();
    for jar in jars.iter() {
        if let Some(name) = jar.file_name().and_then(|n| n.to_str()) {
            if let Some(stem) = name.strip_suffix(".jar") {
                // Skip JARs that already have a classifier suffix
                if stem.ends_with(classifier) { continue; }
                let native_name = format!("{}-{}.jar", stem, classifier);
                let native_jar = jar.with_file_name(&native_name);
                if native_jar.exists() {
                    extras.push(native_jar);
                }
            }
        }
    }
    jars.extend(extras);
}

/// A registry entry with optional scope routing
#[derive(Clone, Debug)]
pub struct RegistryEntry {
    pub url: String,
    pub scope: Option<String>,
    pub username: Option<String>,
    pub password: Option<String>,
}

/// A parsed Maven coordinate
#[derive(Clone, Debug)]
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

    /// Parse a versioned key like "g:a:v" or "g:a:v:classifier" back into a MavenCoord.
    pub fn from_versioned_key(key: &str) -> Option<MavenCoord> {
        let parts: Vec<&str> = key.split(':').collect();
        if parts.len() >= 3 {
            Some(MavenCoord {
                group_id: parts[0].to_string(),
                artifact_id: parts[1].to_string(),
                version: parts[2].to_string(),
                classifier: parts.get(3).map(|s| s.to_string()),
                exclusions: Vec::new(),
                scope: None,
            })
        } else {
            None
        }
    }

    #[allow(dead_code)]
    pub fn key(&self) -> String {
        format!("{}:{}", self.group_id, self.artifact_id)
    }

    pub fn versioned_key(&self) -> String {
        match &self.classifier {
            Some(c) => format!("{}:{}:{}:{}", self.group_id, self.artifact_id, self.version, c),
            None => format!("{}:{}:{}", self.group_id, self.artifact_id, self.version),
        }
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

/// Compare two Maven version strings. Returns -1, 0, or 1.
/// Splits base version by '.', qualifier by '-'.
/// Release (no qualifier) > any qualifier (Maven convention).
/// e.g., 4.0.3 > 4.0.3-5 > 4.0.3-4
pub(crate) fn version_compare(a: &str, b: &str) -> i32 {
    // Split into base and qualifier: "4.0.3-5" → ("4.0.3", Some("5"))
    let (base_a, qual_a) = if let Some(pos) = a.find('-') {
        (&a[..pos], Some(&a[pos + 1..]))
    } else {
        (a, None)
    };
    let (base_b, qual_b) = if let Some(pos) = b.find('-') {
        (&b[..pos], Some(&b[pos + 1..]))
    } else {
        (b, None)
    };

    // Compare base versions numerically
    let parse_base = |s: &str| -> Vec<i64> {
        s.split('.').map(|seg| seg.parse::<i64>().unwrap_or(0)).collect()
    };
    let va = parse_base(base_a);
    let vb = parse_base(base_b);
    let len = va.len().max(vb.len());
    for i in 0..len {
        let sa = va.get(i).copied().unwrap_or(0);
        let sb = vb.get(i).copied().unwrap_or(0);
        if sa < sb { return -1; }
        if sa > sb { return 1; }
    }

    // Same base version — compare qualifiers
    // Release (None) > any qualifier (Some)
    match (qual_a, qual_b) {
        (None, None) => 0,
        (None, Some(_)) => 1,   // release > qualifier
        (Some(_), None) => -1,  // qualifier < release
        (Some(qa), Some(qb)) => {
            // Compare qualifier segments: "5" vs "8", or "beta.1" vs "beta.2"
            let parse_qual = |s: &str| -> Vec<i64> {
                s.split(|c: char| c == '.' || c == '-')
                    .map(|seg| seg.parse::<i64>().unwrap_or(0))
                    .collect()
            };
            let vqa = parse_qual(qa);
            let vqb = parse_qual(qb);
            let qlen = vqa.len().max(vqb.len());
            for i in 0..qlen {
                let sa = vqa.get(i).copied().unwrap_or(0);
                let sb = vqb.get(i).copied().unwrap_or(0);
                if sa < sb { return -1; }
                if sa > sb { return 1; }
            }
            0
        }
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
const MAVEN_CENTRAL_ALT: &str = "https://repo.maven.apache.org/maven2";

/// Known immutable Maven registries. Dependencies served from these URLs
/// skip remote sha1 validation in the fast path — Maven Central release
/// artifacts are immutable by convention, so validation is pure overhead.
/// Private registries (Reposilite/Nexus/Artifactory) are NOT in this list
/// because they commonly allow republishing.
const IMMUTABLE_REGISTRIES: &[&str] = &[DEFAULT_REPO, MAVEN_CENTRAL_ALT];

fn is_immutable_registry(url: &str) -> bool {
    // IMMUTABLE_REGISTRIES entries carry no trailing slash.
    IMMUTABLE_REGISTRIES.contains(&url.trim_end_matches('/'))
}

/// Build a rayon thread pool sized for I/O-bound network work. Higher
/// thread count than `num_cpus` because each thread spends most of its
/// time blocked on HTTP. Falls back to rayon's default pool if the custom
/// one cannot be built; panics only if the default also fails.
fn build_io_pool(num_threads: usize) -> rayon::ThreadPool {
    rayon::ThreadPoolBuilder::new()
        .num_threads(num_threads)
        .build()
        .unwrap_or_else(|_| {
            rayon::ThreadPoolBuilder::new()
                .build()
                .expect("rayon default thread pool failed to build")
        })
}

/// Resolve all dependencies (including transitive) and download JARs.
/// Returns list of JAR paths.
///
/// Fast path: if the lock file already contains all requested deps and
/// every JAR is in the local cache, no HTTP requests are made.
pub fn resolve_and_download(
    dependencies: &BTreeMap<String, String>,
    cache_dir: &Path,
    lock: &mut Lockfile,
) -> Result<Vec<PathBuf>> {
    resolve_and_download_full(dependencies, cache_dir, lock, &[], &[])
}

/// Full resolve with repos, exclusions, and resolutions.
pub fn resolve_and_download_full(
    dependencies: &BTreeMap<String, String>,
    cache_dir: &Path,
    lock: &mut Lockfile,
    registries: &[RegistryEntry],
    exclusions: &[String],
) -> Result<Vec<PathBuf>> {
    resolve_and_download_with_resolutions(dependencies, cache_dir, lock, registries, exclusions, &BTreeMap::new())
}

/// Full resolve with repos, exclusions, and resolutions that override all versions.
pub fn resolve_and_download_with_resolutions(
    dependencies: &BTreeMap<String, String>,
    cache_dir: &Path,
    lock: &mut Lockfile,
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
    lock: &mut Lockfile,
    registries: &[RegistryEntry],
    exclusions: &[String],
    resolutions: &BTreeMap<String, String>,
    dep_scopes: &HashMap<String, String>,
) -> Result<Vec<PathBuf>> {
    resolve_inner(dependencies, cache_dir, lock, registries, exclusions, resolutions, &BTreeMap::new(), dep_scopes, true)
}

/// Full resolve with constraints (BOM managed versions, "at least this version" semantics).
pub fn resolve_and_download_with_constraints(
    dependencies: &BTreeMap<String, String>,
    cache_dir: &Path,
    lock: &mut Lockfile,
    registries: &[RegistryEntry],
    exclusions: &[String],
    resolutions: &BTreeMap<String, String>,
    constraints: &BTreeMap<String, String>,
    dep_scopes: &HashMap<String, String>,
) -> Result<Vec<PathBuf>> {
    resolve_inner(dependencies, cache_dir, lock, registries, exclusions, resolutions, constraints, dep_scopes, true)
}

/// Resolve dependency graph and return expected JAR paths without downloading.
/// Used by `ym idea --json` to avoid blocking on network I/O.
pub fn resolve_no_download(
    dependencies: &BTreeMap<String, String>,
    cache_dir: &Path,
    lock: &mut Lockfile,
    registries: &[RegistryEntry],
    exclusions: &[String],
    resolutions: &BTreeMap<String, String>,
    dep_scopes: &HashMap<String, String>,
) -> Result<Vec<PathBuf>> {
    resolve_inner(dependencies, cache_dir, lock, registries, exclusions, resolutions, &BTreeMap::new(), dep_scopes, false)
}

/// Core resolver.
/// - `resolutions`: forced version overrides (always win, like enforcedPlatform)
/// - `constraints`: BOM managed versions ("at least this version", like platform())
///   Only applies to deps already in the tree. Higher transitive version wins over constraint.
fn resolve_inner(
    dependencies: &BTreeMap<String, String>,
    cache_dir: &Path,
    lock: &mut Lockfile,
    registries: &[RegistryEntry],
    exclusions: &[String],
    resolutions: &BTreeMap<String, String>,
    constraints: &BTreeMap<String, String>,
    dep_scopes: &HashMap<String, String>,
    download: bool,
) -> Result<Vec<PathBuf>> {
    let exclusion_set: HashSet<String> = exclusions.iter().cloned().collect();
    // Fast path: try to resolve entirely from lock file + local cache.
    // Sha1 remote validation only runs when network is allowed (i.e. the
    // caller would otherwise download on miss). `resolve_no_download`
    // callers (e.g. `ym idea --json`) skip validation for zero-network use.
    if resolutions.is_empty() {
        if let Some(jars) = try_resolve_from_lock(dependencies, cache_dir, lock, &exclusion_set, resolutions, registries, download)? {
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
        if let Some(jars) = try_resolve_from_lock(&resolved_deps, cache_dir, lock, &exclusion_set, resolutions, registries, download)? {
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
        .connect_timeout(std::time::Duration::from_secs(30))
        .build()?;

    // Resolve transitive graph with latest-wins strategy (Gradle convention, see ADR-016).
    // Each `groupId:artifactId` resolves to the highest version encountered across all paths.
    let mut coords_to_download: Vec<(String, MavenCoord)> = Vec::new();
    let mut dep_map: BTreeMap<String, Vec<String>> = BTreeMap::new();
    // Maps groupId:artifactId -> highest version seen so far. Used to skip
    // BFS exploration of versions that can never win.
    let mut resolved_versions: HashMap<String, String> = HashMap::new();
    // ADR-022: GAs that came from `ym.json` direct dependencies. Their version
    // is what the user explicitly declared (possibly via scope alias, `${var}`
    // interpolation, or `{ workspace = true }` inheritance) and must NEVER be
    // changed by an imported BOM `constraint`. `resolutions` still wins (it's
    // the user actively forcing an override, like Gradle `enforcedPlatform`).
    let mut explicit_deps: HashSet<String> = HashSet::new();
    // Track BFS depth per queued item (used for diagnostics, not for arbitration)
    let mut depth_map: HashMap<String, usize> = HashMap::new();

    // In-memory POM cache to avoid re-parsing the same POM
    let pom_cache = PomCache::new();
    // ADR-021: collect every POM fetch/parse failure across all par_iter
    // workers so we can fail-loud with the full list at end-of-BFS instead
    // of silently swallowing them into empty dep lists (the root of the
    // "80 vs 84 jars" non-determinism).
    let pom_failures: std::sync::Arc<Mutex<Vec<(MavenCoord, String)>>> =
        std::sync::Arc::new(Mutex::new(Vec::new()));
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
        resolved_versions.insert(ga_key.clone(), mc.version.clone());
        // ADR-022: mark this GA as explicit so BOM constraint application
        // below leaves its version alone.
        explicit_deps.insert(ga_key);
        queue.push_back(mc);
    }

    let resolved_count = AtomicUsize::new(0);
    let show_resolve_progress = !crate::is_json_quiet() && !crate::RESOLVER_QUIET.load(Ordering::Relaxed);


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

            // Latest-wins (Gradle): if a strictly higher version of this GA was
            // already explored, skip the lower version — it can never win.
            // Equal versions (same GAV) are caught by `visited.contains` above.
            if let Some(resolved_ver) = resolved_versions.get(&ga_key) {
                if version_compare(&coord.version, resolved_ver) < 0 {
                    continue;
                }
            }

            visited.insert(key.clone());
            ordered_keys.push(key.clone());
            // Update high-water mark — newer/higher version takes the spot.
            resolved_versions
                .entry(ga_key)
                .and_modify(|v| {
                    if version_compare(&coord.version, v) > 0 {
                        *v = coord.version.clone();
                    }
                })
                .or_insert_with(|| coord.version.clone());

            // Queue JAR download if not cached and not known pom-only
            let jar_path = coord.jar_path(cache_dir);
            if !jar_path.exists() && !is_pom_only_cached(&jar_path) {
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

        // Resolve transitive deps for this level using dedicated 32-thread pool
        let pom_failures_ref = &pom_failures;
        let level_results: Vec<(MavenCoord, Vec<MavenCoord>)> = network_io_pool().install(|| {
            current_level
                .par_iter()
                .map(|coord| {
                    // ADR-021: do not swallow errors into Vec::new() — that
                    // turns a transient registry hiccup into a permanently
                    // missing transitive subtree. Record the failure, return
                    // empty deps so other parallel workers can still progress,
                    // and let the BFS end-of-loop check fail-loud with the
                    // full list.
                    let transitive = match resolve_transitive_cached(
                        &client, coord, cache_dir, registries, Some(&pom_cache),
                    ) {
                        Ok(t) => t,
                        Err(e) => {
                            // `{:#}` unfolds the full anyhow chain; `to_string()` would drop the inner causes ADR-021/023 stacked.
                            pom_failures_ref
                                .lock()
                                .unwrap()
                                .push((coord.clone(), format!("{:#}", e)));
                            Vec::new()
                        }
                    };
                    let n = resolved_count.fetch_add(1, Ordering::Relaxed) + 1;
                    let interval = if crate::is_progress_quiet() { 100 } else { 20 };
                    if show_resolve_progress && n % interval == 0 {
                        resolver_progress(&format!("dependency graph ({} artifacts)...", n));
                    }
                    (coord.clone(), transitive)
                })
                .collect()
        });
        // Clear progress line after level completes (only needed without spinner)
        if show_resolve_progress && resolved_count.load(Ordering::Relaxed) >= 20 && !crate::SPINNER_ACTIVE.load(Ordering::Relaxed) {
            eprint!("\r{}\r", " ".repeat(60));
        }

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
                let ga_key = format!("{}:{}", dep.group_id, dep.artifact_id);

                // Apply resolutions: forced override (like enforcedPlatform).
                // Resolutions win even over explicit deps — they are the user
                // actively forcing an override.
                if let Some(forced_version) = resolutions.get(&ga_key) {
                    dep.version = forced_version.clone();
                }
                // ADR-022: Apply BOM constraints ONLY to non-explicit GAs.
                // A GA the user declared directly in `ym.json` keeps its
                // explicit version — BOM constraint is a hint for transitive
                // resolution, not authority over the user's pin.
                else if !explicit_deps.contains(&ga_key) {
                    if let Some(constraint_version) = constraints.get(&ga_key) {
                        // "at least this version" (like Gradle platform()).
                        // Only upgrade, never downgrade. Higher transitive
                        // version wins over BOM-managed version.
                        if version_compare(&dep.version, constraint_version) < 0 {
                            dep.version = constraint_version.clone();
                        }
                    }
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

    // ADR-021 + ADR-022: BFS finished — partition POM fetch/parse failures
    // into "fatal" and "stale-noise" buckets, then fail-loud only on the fatal
    // bucket. A failure whose `coord.version` is strictly lower than the GA's
    // current winner in `resolved_versions` cannot enter the final classpath
    // (latest-wins arbitration already chose a higher version), so its POM
    // failure is noise that would mask real errors and block the build for
    // no reason. Print those as warnings instead.
    {
        let failures = pom_failures.lock().unwrap();
        if !failures.is_empty() {
            let mut fatal: Vec<&(MavenCoord, String)> = Vec::new();
            let mut stale: Vec<&(MavenCoord, String)> = Vec::new();
            for entry in failures.iter() {
                let (coord, _reason) = entry;
                let ga_key = format!("{}:{}", coord.group_id, coord.artifact_id);
                match resolved_versions.get(&ga_key) {
                    Some(winner) if version_compare(winner, &coord.version) > 0 => {
                        stale.push(entry);
                    }
                    _ => fatal.push(entry),
                }
            }
            for (coord, reason) in &stale {
                let ga_key = format!("{}:{}", coord.group_id, coord.artifact_id);
                let winner = resolved_versions.get(&ga_key).map(|s| s.as_str()).unwrap_or("?");
                eprintln!(
                    "{} stale POM fetch failed (superseded by {}): {}:{}:{} — {}",
                    style(format!("{:>12}", "warning")).yellow().bold(),
                    winner,
                    coord.group_id, coord.artifact_id, coord.version,
                    reason
                );
            }
            if !fatal.is_empty() {
                let mut msg = String::from("\n  ✗ Dependency resolution failed — POM fetch/parse errors:\n\n");
                for (coord, reason) in &fatal {
                    msg.push_str(&format!(
                        "    - {}:{}:{}\n        reason: {}\n",
                        coord.group_id, coord.artifact_id, coord.version, reason
                    ));
                }
                msg.push_str("\n  Possible causes:\n");
                msg.push_str("    - registry temporarily unreachable (retry; check network and credentials)\n");
                msg.push_str("    - artifact does not exist at that version (typo in ym.json?)\n");
                msg.push_str("    - cache corruption (run `ym cache clean -y` to retry from scratch)\n");
                return Err(anyhow::anyhow!(msg));
            }
        }
    }

    // Phase 2: parallel JAR downloads (skipped in resolve-only mode)
    if download && !coords_to_download.is_empty() {
        let total = coords_to_download.len();
        let is_tty = console::Term::stdout().is_term();
        let show_progress = !crate::is_json_quiet() && is_tty;
        let completed = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        // Shared download progress: total bytes expected, bytes downloaded so far
        let dl_total_bytes = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        let dl_done_bytes = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        let dl_start = std::time::Instant::now();
        // Throttle eprint updates to avoid flickering from concurrent threads
        let last_print_ms = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        let pending = std::sync::Arc::new(Mutex::new(std::collections::BTreeSet::<String>::new()));

        if !crate::is_json_quiet() {
            let first = coords_to_download.first()
                .map(|(_, c)| format!("{}:{}:{}", c.group_id, c.artifact_id, c.version))
                .unwrap_or_default();
            if crate::SPINNER_ACTIVE.load(Ordering::Relaxed) {
                crate::set_spinner_msg(format!("Downloading 0/{} {}", total, first));
            } else if is_tty {
                eprint!(
                    "{} 0/{} {}",
                    style(format!("{:>12}", "Downloading")).green().bold(), total, first
                );
            } else {
                let dep_word = if total == 1 { "dependency" } else { "dependencies" };
                println!(
                    "{} {} {}...",
                    style(format!("{:>12}", "Downloading")).green().bold(), total, dep_word
                );
            }
        }

        let download_results: Vec<(String, Result<String>)> = network_io_pool().install(|| {
            coords_to_download
            .into_par_iter()
            .map(|(key, coord)| {
                let short_name = format!("{}:{}:{}", coord.group_id, coord.artifact_id, coord.version);
                if show_progress {
                    pending.lock().unwrap().insert(short_name.clone());
                }
                let jar_path = coord.jar_path(cache_dir);

                // Build progress callback for byte-level tracking
                let dl_total_ref = dl_total_bytes.clone();
                let dl_done_ref = dl_done_bytes.clone();
                let completed_ref = completed.clone();
                let pending_ref = pending.clone();
                let last_print_ref = last_print_ms.clone();
                let dl_start_ref = dl_start;
                let name_for_cb = short_name.clone();
                let progress_cb: Option<Box<dyn Fn(u64, u64) + Send + Sync>> = if show_progress {
                    Some(Box::new(move |content_len: u64, chunk_bytes: u64| {
                        // First call per download: register total size
                        if chunk_bytes == 0 {
                            dl_total_ref.fetch_add(content_len, Ordering::Relaxed);
                            return;
                        }
                        let done = dl_done_ref.fetch_add(chunk_bytes, Ordering::Relaxed) + chunk_bytes;
                        // Throttle: only update display every 100ms
                        let now_ms = dl_start_ref.elapsed().as_millis() as u64;
                        let prev = last_print_ref.load(Ordering::Relaxed);
                        if now_ms.saturating_sub(prev) < 100 {
                            return;
                        }
                        if last_print_ref.compare_exchange(prev, now_ms, Ordering::Relaxed, Ordering::Relaxed).is_err() {
                            return; // another thread won the race, skip
                        }
                        let total_b = dl_total_ref.load(Ordering::Relaxed);
                        let elapsed = dl_start_ref.elapsed().as_secs_f64();
                        let speed = if elapsed > 0.1 { done as f64 / elapsed } else { 0.0 };
                        let eta = if speed > 0.0 && total_b > done { ((total_b - done) as f64 / speed) as u64 } else { 0 };
                        let done_count = completed_ref.load(Ordering::Relaxed);
                        let current = pending_ref.lock().unwrap().iter().next().cloned().unwrap_or_else(|| name_for_cb.clone());
                        if crate::SPINNER_ACTIVE.load(Ordering::Relaxed) {
                            crate::set_spinner_msg(format!(
                                "Downloading {}/{}  {}    {}/{}  {}/s  {}",
                                done_count, total, current,
                                format_bytes(done), format_bytes(total_b),
                                format_bytes(speed as u64), format_eta(eta),
                            ));
                        } else {
                            eprint!(
                                "\r{} {}/{}  {}    {}/{}  {}/s  {}{}",
                                style(format!("{:>12}", "Downloading")).green().bold(),
                                done_count, total,
                                current,
                                format_bytes(done), format_bytes(total_b),
                                format_bytes(speed as u64),
                                format_eta(eta),
                                " ".repeat(10)
                            );
                        }
                    }))
                } else {
                    None
                };

                let hash_result = if coord.is_snapshot() {
                    if let Some((ts, bn)) = resolve_snapshot_version(&client, &coord, registries) {
                        download_from_repos(&client, &coord, &jar_path, registries, progress_cb.as_deref(), |c, r| c.snapshot_jar_url(r, &ts, &bn))
                    } else {
                        download_from_repos(&client, &coord, &jar_path, registries, progress_cb.as_deref(), |c, r| c.jar_url(r))
                    }
                } else {
                    download_from_repos(&client, &coord, &jar_path, registries, progress_cb.as_deref(), |c, r| c.jar_url(r))
                };
                if hash_result.is_ok() {
                    verify_gpg_signature(&client, &coord, &jar_path, registries);
                }
                // Mark completed
                if show_progress {
                    let done_count = completed.fetch_add(1, Ordering::Relaxed) + 1;
                    let mut pend = pending.lock().unwrap();
                    pend.remove(&short_name);
                    let current = pend.iter().next().cloned().unwrap_or_default();
                    let done = dl_done_bytes.load(Ordering::Relaxed);
                    let total_b = dl_total_bytes.load(Ordering::Relaxed);
                    let elapsed = dl_start.elapsed().as_secs_f64();
                    let speed = if elapsed > 0.1 { done as f64 / elapsed } else { 0.0 };
                    let eta = if speed > 0.0 && total_b > done { ((total_b - done) as f64 / speed) as u64 } else { 0 };
                    if crate::SPINNER_ACTIVE.load(Ordering::Relaxed) {
                        crate::set_spinner_msg(format!(
                            "Downloading {}/{}  {}    {}/{}  {}/s  {}",
                            done_count, total, current,
                            format_bytes(done), format_bytes(total_b),
                            format_bytes(speed as u64), format_eta(eta),
                        ));
                    } else {
                        eprint!(
                            "\r{} {}/{}  {}    {}/{}  {}/s  {}{}",
                            style(format!("{:>12}", "Downloading")).green().bold(),
                            done_count, total,
                            current,
                            format_bytes(done), format_bytes(total_b),
                            format_bytes(speed as u64),
                            format_eta(eta),
                            " ".repeat(5)
                        );
                    }
                }
                (key, hash_result)
            })
            .collect()
        });

        // Clear progress line (not needed when spinner handles it)
        if !crate::is_json_quiet() && is_tty && !crate::SPINNER_ACTIVE.load(Ordering::Relaxed) {
            eprint!("\r{}\r", " ".repeat(60));
        }

        // Print final summary (skip when spinner is active — build.rs prints its own summary)
        if !crate::is_json_quiet() && !crate::SPINNER_ACTIVE.load(Ordering::Relaxed) {
            let ok_count = download_results.iter().filter(|(_, r)| r.is_ok()).count();
            let done_word = if ok_count == 1 { "dependency" } else { "dependencies" };
            println!(
                "{} {} {}",
                style(format!("{:>12}", "Downloaded")).green().bold(), ok_count, done_word
            );
        }

        // Print only failures (defer if spinner active to avoid visual conflict)
        if !crate::SPINNER_ACTIVE.load(Ordering::Relaxed) {
            for (key, result) in &download_results {
                if let Err(e) = result {
                    let msg = e.to_string();
                    if !msg.contains("HTTP 404") {
                        eprintln!("{} {} — {}", style(format!("{:>12}", "error")).red().bold(), key, e);
                    }
                }
            }
            print_gpg_summary();
        }

        // Record hashes in lock; collect failures
        let mut failures = Vec::new();
        for (key, hash_result) in download_results {
            match hash_result {
                Ok(hash) => {
                    if let Some(entry) = lock.dependencies.get_mut(&key) {
                        entry.sha256 = Some(hash);
                    }
                }
                Err(e) => {
                    let msg = e.to_string();
                    if msg.contains("HTTP 404") {
                        // POM-only artifact (e.g. kotlin-stdlib-common merged into kotlin-stdlib in Kotlin 2.x).
                        // POM exists but JAR doesn't — skip gracefully.
                        // Create pom-only marker so future runs skip download attempt.
                        if let Some(coord) = MavenCoord::from_versioned_key(&key) {
                            let jar_path = coord.jar_path(cache_dir);
                            let marker = jar_path.with_extension("jar.pom-only");
                            if let Some(parent) = marker.parent() {
                                let _ = std::fs::create_dir_all(parent);
                            }
                            let timestamp = std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .unwrap_or_default()
                                .as_secs()
                                .to_string();
                            let _ = std::fs::write(&marker, timestamp);
                        }
                        eprintln!("{} {} (pom-only, no JAR — skipped)", style(format!("{:>12}", "Skipping")).yellow().bold(), key);
                    } else {
                        failures.push(format!("{}: {}", key, e));
                    }
                }
            }
        }
        if !failures.is_empty() {
            bail!("Failed to download {} artifact(s):\n  {}", failures.len(), failures.join("\n  "));
        }
    }

    // Phase 3: build lock entries and collect JAR paths.
    // Latest-wins: pick the highest version per groupId:artifactId, then write only
    // those winner entries to the lock with transitive references normalized to winners
    // (so every reference inside the lock resolves to a present entry).
    let mut ga_winners: HashMap<String, (MavenCoord, String)> = HashMap::new(); // GA → (coord, winner_key)
    let mut ga_order: Vec<String> = Vec::new();
    for key in &ordered_keys {
        let Some(coord) = MavenCoord::from_versioned_key(key) else {
            continue;
        };
        let ga = format!("{}:{}", coord.group_id, coord.artifact_id);
        if let Some((existing, _)) = ga_winners.get(&ga) {
            if version_compare(&coord.version, &existing.version) > 0 {
                ga_winners.insert(ga, (coord, key.clone()));
            }
        } else {
            ga_order.push(ga.clone());
            ga_winners.insert(ga, (coord, key.clone()));
        }
    }

    // Insert lock entries only for winner versions; rewrite transitive GAV references
    // so they point at winners (avoids dangling references in the lock).
    for ga in &ga_order {
        let Some((_, winner_key)) = ga_winners.get(ga) else { continue };
        let raw_deps = dep_map.remove(winner_key).unwrap_or_default();
        let normalized_deps: Vec<String> = raw_deps
            .into_iter()
            .map(|gav| {
                let parts: Vec<&str> = gav.splitn(3, ':').collect();
                if parts.len() == 3 {
                    let dep_ga = format!("{}:{}", parts[0], parts[1]);
                    if let Some((_, winner_gav)) = ga_winners.get(&dep_ga) {
                        return winner_gav.clone();
                    }
                }
                gav
            })
            .collect();
        let effective_scope = scope_map.get(winner_key).cloned();
        lock.dependencies.entry(winner_key.clone()).or_insert(ResolvedDependency {
            sha256: None,
            dependencies: if normalized_deps.is_empty() {
                None
            } else {
                Some(normalized_deps)
            },
            scope: effective_scope,
        });
    }

    let mut all_jars = Vec::new();
    for ga in &ga_order {
        let Some((coord, _)) = ga_winners.get(ga) else { continue };
        let jar_path = coord.jar_path(cache_dir);
        if jar_path.exists() {
            all_jars.push(jar_path);
        }
    }

    append_native_jars(&mut all_jars);
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
        let url = coord.metadata_url(&repo.url);

        let mut request = client.get(&url);
        if let (Some(u), Some(p)) = (&repo.username, &repo.password) {
            request = request.basic_auth(u, Some(p));
        } else if let Some((username, password)) = load_credentials_for_url(&url) {
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

/// Try to resolve all dependencies from lock file without (usually) hitting
/// the network. When `validate_sha1` is true, non-whitelist registries are
/// probed with `.jar.sha1` to catch republished artifacts (ADR-017): on a
/// real mismatch the stale derivatives are purged and the caller re-resolves
/// cache-cold, except under `--frozen-lockfile` where it fails loud instead.
/// Network errors degrade to cache-only. The source registry for each dep is
/// computed from `registries` via the same scope-routing rules used during
/// the original download — no per-dep registry information is stored in lock.
/// Returns `Ok(None)` if any dep is missing from lock or cache.
fn try_resolve_from_lock(
    dependencies: &BTreeMap<String, String>,
    cache_dir: &Path,
    lock: &Lockfile,
    exclusion_set: &HashSet<String>,
    resolutions: &BTreeMap<String, String>,
    registries: &[RegistryEntry],
    validate_sha1: bool,
) -> Result<Option<Vec<PathBuf>>> {
    if lock.dependencies.is_empty() {
        return Ok(None);
    }

    // If any dependency is a SNAPSHOT, skip fast path (need to check for updates)
    for version in dependencies.values() {
        if version.ends_with("-SNAPSHOT") {
            return Ok(None);
        }
    }

    // Deduplicate by groupId:artifactId — keep the highest version for each GA.
    let mut ga_winners: HashMap<String, MavenCoord> = HashMap::new();
    let mut ga_order: Vec<String> = Vec::new();
    let mut visited = HashSet::new();
    let mut queue = VecDeque::new();

    for (coord, version) in dependencies {
        let mc = match MavenCoord::parse(coord, version) {
            Ok(mc) => mc,
            Err(_) => return Ok(None),
        };
        queue.push_back(mc);
    }

    while let Some(coord) = queue.pop_front() {
        let ga_key = format!("{}:{}", coord.group_id, coord.artifact_id);
        if exclusion_set.contains(&ga_key) {
            continue;
        }

        let key = coord.versioned_key();
        if visited.contains(&key) {
            continue;
        }
        visited.insert(key.clone());

        // Check JAR exists in cache (or is known pom-only)
        let jar_path = coord.jar_path(cache_dir);
        let pom_only = is_pom_only_cached(&jar_path);
        if !jar_path.exists() && !pom_only {
            return Ok(None); // Cache miss
        }

        // Check lock file entry exists
        let locked = match lock.dependencies.get(&key) {
            Some(locked) => locked,
            None => return Ok(None),
        };

        if !pom_only {
            // Track highest version per GA
            if let Some(existing) = ga_winners.get(&ga_key) {
                if version_compare(&coord.version, &existing.version) > 0 {
                    ga_winners.insert(ga_key.clone(), coord.clone());
                }
            } else {
                ga_order.push(ga_key.clone());
                ga_winners.insert(ga_key.clone(), coord.clone());
            }
        }
        if let Some(ref dep_keys) = locked.dependencies {
            for dep_key in dep_keys {
                if let Some(mut mc) = MavenCoord::from_versioned_key(dep_key) {
                    // Apply resolutions to transitive deps (same as slow path)
                    let ga = format!("{}:{}", mc.group_id, mc.artifact_id);
                    if let Some(forced_version) = resolutions.get(&ga) {
                        mc.version = forced_version.clone();
                    }
                    queue.push_back(mc);
                }
            }
        }
    }

    // Remote sha1 validation: catch republished artifacts on mutable
    // registries (private registries routinely violate Maven's immutable-
    // release convention). The source registry for each dep is recomputed
    // from `registries` via scope routing — the same logic that picked it
    // at download time. Whitelist (Maven Central) entries skip validation.
    //
    // `ga_winners` only contains non-pom-only coords (see the `if !pom_only`
    // guard upstream), so pom-only artifacts are not validated here — a
    // known minor gap; pom-only artifacts are rare and rarely republished.
    if validate_sha1 {
        let validation_targets: Vec<ValidationTarget> = ga_winners
            .values()
            .filter_map(|coord| {
                let repos = repos_for_group_id(registries, &coord.group_id);
                let repo = repos.first()?;
                if is_immutable_registry(&repo.url) {
                    return None;
                }
                Some(ValidationTarget {
                    coord: coord.clone(),
                    registry_url: repo.url.clone(),
                })
            })
            .collect();

        // A non-empty result means one or more cached artifacts no longer
        // match the registry — the release version was republished (ADR-017).
        let republished = validate_sha1_remote(&validation_targets, cache_dir);
        if !republished.is_empty() {
            if crate::commands::build::is_frozen_lockfile() {
                // ADR-017 fix ③: frozen mode must not silently re-resolve a
                // stale lock. Fail loud and point at the fix.
                let mut msg = String::from(
                    "ym-lock.json is out of date — upstream release(s) were republished:",
                );
                for coord in &republished {
                    msg.push_str(&format!("\n  ✗ {}", coord.versioned_key()));
                }
                msg.push_str(
                    "\n\n  A release version was overwritten on a mutable registry, so the\n  \
                     locked dependency graph no longer matches reality.\n  \
                     Run `ym install` to re-resolve, then commit the updated ym-lock.json.",
                );
                bail!("{}", msg);
            }
            // ADR-017 fix ②: purge every republished artifact's cached
            // derivatives (raw `.pom` / `.jar` / parsed pom-cache json) so the
            // slow path re-resolves them cache-cold instead of reading the
            // stale `.pom` straight back. The transitive closure is rebuilt
            // by the slow path's full BFS.
            for coord in &republished {
                purge_artifact_cache(coord, cache_dir);
            }
            if !crate::is_progress_quiet() {
                eprintln!(
                    "{} dependency cache out of date (sha1 mismatch), re-resolving",
                    console::style(format!("{:>12}", "info")).cyan().bold()
                );
            }
            return Ok(None);
        }
    }

    let mut all_jars = Vec::new();
    for ga in &ga_order {
        if let Some(coord) = ga_winners.get(ga) {
            let jar_path = coord.jar_path(cache_dir);
            if jar_path.exists() {
                all_jars.push(jar_path);
            }
        }
    }

    append_native_jars(&mut all_jars);
    Ok(Some(all_jars))
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

/// On-disk POM cache entry (`~/.ym/pom-cache/{g}/{a}/{v}.json`).
///
/// `pom_sha256` binds the parsed dependency list to the exact raw `.pom` it
/// was derived from. A republished release (same GAV, new `.pom`) makes the
/// hashes diverge, so the stale entry is rejected instead of silently
/// feeding an outdated dependency graph into the build (ADR-017).
///
/// `schema_version` lets the resolver invalidate old cache entries when the
/// semantics of `resolved` change without the raw `.pom` changing. v2 (ADR-021)
/// introduced parent-POM `<dependencies>` inheritance: `resolved` now contains
/// the child's own deps **plus** the union of every ancestor POM's project-level
/// `<dependencies>` block (Maven inheritance). v1 entries (missing
/// `schema_version`) fail to deserialize as v2 and are dropped + regenerated.
#[derive(serde::Serialize, serde::Deserialize)]
struct PomCacheEntry {
    schema_version: u8,
    pom_sha256: String,
    resolved: Vec<(String, String, String, Vec<String>)>,
}

/// Bump when the semantics of `PomCacheEntry.resolved` change.
/// v2: ADR-021 — resolved includes parent-POM dependency inheritance.
const POM_CACHE_SCHEMA_VERSION: u8 = 2;

/// Remove every cached derivative of one artifact-version — raw `.pom`,
/// `.jar`, and the parsed pom-cache entry. Called when a republished release
/// is detected (ADR-017 fix ②) so the slow path re-resolves it cache-cold
/// rather than reading the stale `.pom` straight back.
fn purge_artifact_cache(coord: &MavenCoord, cache_dir: &Path) {
    let _ = std::fs::remove_file(coord.jar_path(cache_dir));
    let _ = std::fs::remove_file(coord.pom_path(cache_dir));
    let pom_cache_file = crate::config::pom_cache_dir()
        .join(&coord.group_id)
        .join(&coord.artifact_id)
        .join(format!("{}.json", coord.version));
    let _ = std::fs::remove_file(pom_cache_file);
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

    // Check on-disk POM cache (~/.ym/pom-cache/)
    let pom_cache_dir = crate::config::pom_cache_dir();
    let pom_cache_file = pom_cache_dir
        .join(&coord.group_id)
        .join(&coord.artifact_id)
        .join(format!("{}.json", coord.version));

    let pom_path = coord.pom_path(cache_dir);

    // On-disk POM cache is content-addressed (ADR-017): the entry records the
    // sha256 of the raw `.pom` it was parsed from and is trusted only while
    // the local `.pom` still hashes to that value. A republished release
    // (same GAV, new `.pom`) therefore cannot be served stale, and old-format
    // entries (bare arrays) fail to parse as `PomCacheEntry` and regenerate.
    if pom_cache_file.exists() {
        if let Ok(content) = std::fs::read_to_string(&pom_cache_file) {
            let fresh = serde_json::from_str::<PomCacheEntry>(&content)
                .ok()
                .filter(|entry| entry.schema_version == POM_CACHE_SCHEMA_VERSION)
                .filter(|entry| {
                    compute_sha256_file(&pom_path)
                        .map(|local| local == entry.pom_sha256)
                        .unwrap_or(false)
                });
            if let Some(entry) = fresh {
                let deps: Vec<MavenCoord> = entry
                    .resolved
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
            // Stale (sha mismatch / `.pom` missing) or old format — drop it.
            let _ = std::fs::remove_file(&pom_cache_file);
        }
    }

    // L3 (ADR-024): completeness > existence — a stale corrupt POM from a previous race / pre-ADR-023 install must not satisfy the .exists() check below.
    if !coord.is_snapshot() {
        invalidate_corrupt_pom(&pom_path);
    }

    // For SNAPSHOT, always re-download POM (may have changed)
    if !pom_path.exists() || coord.is_snapshot() {
        let pom_result = if coord.is_snapshot() {
            if let Some((ts, bn)) = resolve_snapshot_version(client, coord, registries) {
                download_from_repos(client, coord, &pom_path, registries, None, |c, r| c.snapshot_pom_url(r, &ts, &bn))
            } else {
                download_from_repos(client, coord, &pom_path, registries, None, |c, r| c.pom_url(r))
            }
        } else {
            download_from_repos(client, coord, &pom_path, registries, None, |c, r| c.pom_url(r))
        };
        if let Err(e) = pom_result {
            // ADR-021: Silent-swallow on POM fetch error is the root cause of
            // intermittent "Resolving 80 jars vs 84 jars" non-determinism — a
            // transient registry hiccup makes a whole BFS subtree disappear
            // without any user-visible signal, and "lucky" cache states from
            // unrelated builds can mask the loss until they invalidate.
            // Fail-loud propagates the error; the BFS driver collects all
            // failures and reports them together so users see every missing
            // dep in one shot, not one at a time on retry.
            return Err(anyhow::anyhow!(
                "POM fetch failed for {}:{}:{} — {}",
                coord.group_id, coord.artifact_id, coord.version, e
            ));
        }
    }

    let pom_content = std::fs::read_to_string(&pom_path)
        .with_context(|| format!("read raw POM at {}", pom_path.display()))?;

    // ADR-023: every parse error from helpers below carries an inner context
    // (e.g. "parse POM body in …"); we tack on the GAV + path at this outer
    // layer so the BFS driver sees a self-contained error in `pom_failures`.
    let outer_ctx = || format!(
        "resolve transitive for {}:{}:{} (POM at {})",
        coord.group_id, coord.artifact_id, coord.version, pom_path.display()
    );

    // Collect parent POM properties (unlimited depth with cycle detection)
    let mut all_properties = HashMap::new();
    let mut visited_poms = HashSet::new();
    resolve_parent_properties(client, &pom_content, cache_dir, registries, &mut all_properties, 0, &mut visited_poms)
        .with_context(outer_ctx)?;

    let mut deps = parse_pom_dependencies_with_props(&pom_content, &all_properties, client, cache_dir, registries)
        .with_context(outer_ctx)?;

    // ADR-021: Merge parent POM `<dependencies>` blocks into the child's
    // transitive set (Maven inheritance). Without this, projects whose deps
    // live in a parent POM (e.g. AWS SDK `services` parent ships
    // sdk-core/auth/regions/http-client-spi) silently lose those jars from the
    // classpath, since `parse_pom_dependencies_with_props` only reads the
    // current POM's `<dependencies>`. The previous (P1-era) implementation
    // assumed all parents only carry `<properties>` + `<dependencyManagement>`,
    // which is wrong for any monorepo that uses parent-level deps.
    //
    // Merge rule: child-declared deps win on identical `groupId:artifactId`
    // (Maven override semantics — the closer ancestor's declaration wins).
    let mut visited_parent_deps = HashSet::new();
    let parent_deps = collect_parent_dependencies(
        client,
        &pom_content,
        cache_dir,
        registries,
        &all_properties,
        0,
        &mut visited_parent_deps,
    ).with_context(outer_ctx)?;
    let mut existing_ga: HashSet<String> = deps
        .iter()
        .map(|d| format!("{}:{}", d.group_id, d.artifact_id))
        .collect();
    for pd in parent_deps {
        let ga = format!("{}:{}", pd.group_id, pd.artifact_id);
        if existing_ga.insert(ga) {
            deps.push(pd);
        }
    }

    // Write content-addressed disk cache (ADR-017): bind the parsed result to
    // the sha256 of the `.pom` it came from so a later republish invalidates it.
    if let Some(parent) = pom_cache_file.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(pom_sha256) = compute_sha256_file(&pom_path) {
        let entry = PomCacheEntry {
            schema_version: POM_CACHE_SCHEMA_VERSION,
            pom_sha256,
            resolved: deps
                .iter()
                .map(|d| (d.group_id.clone(), d.artifact_id.clone(), d.version.clone(), d.exclusions.clone()))
                .collect(),
        };
        let _ = std::fs::write(&pom_cache_file, serde_json::to_string(&entry).unwrap_or_default());
    }

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

    // ADR-023: parse errors must carry context — roxmltree's `NoRootNode`
    // displays as "the document does not have a root node" with no path or
    // GAV. The outer wrapping at the entry point (e.g. `resolve_transitive_cached`)
    // adds the GAV; this layer adds which POM body we were trying to parse.
    let doc = roxmltree::Document::parse(pom_content)
        .with_context(|| "parse POM body in resolve_parent_properties (current POM)")?;

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
                invalidate_corrupt_pom(&parent_pom_path); // L3 (ADR-024)
                if !parent_pom_path.exists() {
                    let _ = download_from_repos(client, &parent_coord, &parent_pom_path, registries, None, |c, r| c.pom_url(r));
                }
                if parent_pom_path.exists() {
                    let parent_content = std::fs::read_to_string(&parent_pom_path)?;
                    // Recurse into grandparent first (so child overrides parent)
                    resolve_parent_properties(client, &parent_content, cache_dir, registries, properties, depth + 1, visited_poms)?;
                    // Then merge parent properties
                    let parent_doc = roxmltree::Document::parse(&parent_content)
                        .with_context(|| format!("parse parent POM at {}", parent_pom_path.display()))?;
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
    let doc = roxmltree::Document::parse(pom)
        .with_context(|| "parse POM body in parse_pom_dependencies_with_props")?;

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

/// ADR-021: Walk the `<parent>` chain and union every ancestor POM's
/// project-level `<dependencies>` block (Maven inheritance).
///
/// The caller already runs `resolve_parent_properties` first, so
/// `extra_properties` contains the full transitive property + `managed:G:A`
/// map for the chain. We can therefore reuse `parse_pom_dependencies_with_props`
/// on each ancestor and trust it to filter scope=test/provided/system,
/// optional=true and import-scope deps the same way it does for the child.
///
/// **De-duplication**: returned list is keyed by `groupId:artifactId` with
/// closer ancestors winning. `resolve_transitive_cached` then merges the
/// child's own deps on top with the same rule, giving final precedence:
/// child > parent > grandparent > root.
///
/// **Depth limit + cycle detection** match `resolve_parent_properties`:
/// 20 levels max, visited set bails on cycles.
///
/// **Why a separate function from `resolve_parent_properties`**: that one is
/// about merging values *into* a shared mutable map; this one collects deps
/// *out* to a returned list. Different signatures, different semantics,
/// reusing the same walker would force a 3-output return type and obscure
/// the merge rules.
fn collect_parent_dependencies(
    client: &reqwest::blocking::Client,
    pom_content: &str,
    cache_dir: &Path,
    registries: &[RegistryEntry],
    extra_properties: &HashMap<String, String>,
    depth: u8,
    visited: &mut HashSet<String>,
) -> Result<Vec<MavenCoord>> {
    if depth > 20 {
        return Ok(vec![]);
    }
    let doc = roxmltree::Document::parse(pom_content)
        .with_context(|| "parse POM body in collect_parent_dependencies")?;
    let mut deps: Vec<MavenCoord> = Vec::new();
    let mut seen_ga: HashSet<String> = HashSet::new();

    for node in doc.root_element().children() {
        if node.tag_name().name() != "parent" {
            continue;
        }
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
            if visited.contains(&parent_key) {
                break;
            }
            visited.insert(parent_key);

            let parent_coord = MavenCoord {
                group_id: g.to_string(),
                artifact_id: a.to_string(),
                version: v.to_string(),
                classifier: None,
                exclusions: Vec::new(),
                scope: None,
            };
            let parent_pom_path = parent_coord.pom_path(cache_dir);
            invalidate_corrupt_pom(&parent_pom_path); // L3 (ADR-024)
            if !parent_pom_path.exists() {
                let _ = download_from_repos(
                    client,
                    &parent_coord,
                    &parent_pom_path,
                    registries,
                    None,
                    |c, r| c.pom_url(r),
                );
            }
            if parent_pom_path.exists() {
                if let Ok(parent_content) = std::fs::read_to_string(&parent_pom_path) {
                    if let Ok(parent_own_deps) = parse_pom_dependencies_with_props(
                        &parent_content,
                        extra_properties,
                        client,
                        cache_dir,
                        registries,
                    ) {
                        for d in parent_own_deps {
                            let ga = format!("{}:{}", d.group_id, d.artifact_id);
                            if seen_ga.insert(ga) {
                                deps.push(d);
                            }
                        }
                    }
                    if let Ok(grandparent_deps) = collect_parent_dependencies(
                        client,
                        &parent_content,
                        cache_dir,
                        registries,
                        extra_properties,
                        depth + 1,
                        visited,
                    ) {
                        for gd in grandparent_deps {
                            let ga = format!("{}:{}", gd.group_id, gd.artifact_id);
                            if seen_ga.insert(ga) {
                                deps.push(gd);
                            }
                        }
                    }
                }
            }
        }
        break;
    }

    Ok(deps)
}

/// Collect <properties> from POM, including built-in Maven properties.
pub fn collect_pom_properties(doc: &roxmltree::Document) -> HashMap<String, String> {
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

    // Collect explicit <properties> — only from project root, skip profiles/plugins
    for node in root.children() {
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
pub fn collect_managed_versions_with_bom(
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
                                        client, &bom_coord, &bom_pom_path, registries, None,
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
fn repos_for_group_id(registries: &[RegistryEntry], group_id: &str) -> Vec<RegistryEntry> {
    // Step 1: Check for scope-matched registries
    for entry in registries {
        if let Some(ref scope_pattern) = entry.scope {
            if matches_scope(group_id, scope_pattern) {
                return vec![RegistryEntry {
                    url: entry.url.trim_end_matches('/').to_string(),
                    scope: entry.scope.clone(),
                    username: entry.username.clone(),
                    password: entry.password.clone(),
                }];
            }
        }
    }

    // Step 2: No scope match — collect unscoped registries + Maven Central
    let mut repos: Vec<RegistryEntry> = registries
        .iter()
        .filter(|e| e.scope.is_none())
        .map(|e| RegistryEntry {
            url: e.url.trim_end_matches('/').to_string(),
            scope: None,
            username: e.username.clone(),
            password: e.password.clone(),
        })
        .collect();
    let central = DEFAULT_REPO.to_string();
    if !repos.iter().any(|r| r.url == central) {
        repos.push(RegistryEntry {
            url: central,
            scope: None,
            username: None,
            password: None,
        });
    }
    repos
}

/// Check if a groupId matches a scope pattern.
/// Supports comma-separated patterns: "com.mycompany.*,sh.yummy.*"
/// Pattern "com.mycompany.*" matches groupId starting with "com.mycompany."
/// Pattern "com.mycompany" matches exactly "com.mycompany"
fn matches_scope(group_id: &str, pattern: &str) -> bool {
    pattern.split(',').any(|p| {
        let p = p.trim();
        if let Some(prefix) = p.strip_suffix(".*") {
            group_id == prefix || group_id.starts_with(&format!("{}.", prefix))
        } else {
            group_id == p
        }
    })
}

/// Try to download an artifact from scope-routed repos, stopping at the first success.
/// Returns the SHA-256 hash of the downloaded file.
fn download_from_repos(
    client: &reqwest::blocking::Client,
    coord: &MavenCoord,
    path: &Path,
    registries: &[RegistryEntry],
    progress: Option<&(dyn Fn(u64, u64) + Send + Sync)>,
    url_fn: impl Fn(&MavenCoord, &str) -> String,
) -> Result<String> {
    let repos = repos_for_group_id(registries, &coord.group_id);
    if repos.is_empty() {
        bail!(
            "download failed for {}:{}:{}: no repositories configured",
            coord.group_id, coord.artifact_id, coord.version
        );
    }
    // ADR-023: accumulate per-repo failures so the terminal error names the
    // GAV and the inner download_file message (URL / status / bytes /
    // category / hint) for every repo attempted, not just the last one.
    let mut repo_errors: Vec<String> = Vec::new();
    for repo in &repos {
        let url = url_fn(coord, &repo.url);
        let creds = match (&repo.username, &repo.password) {
            (Some(u), Some(p)) => Some((u.as_str(), p.as_str())),
            _ => None,
        };
        match download_file(client, &url, path, progress, creds) {
            Ok(hash) => return Ok(hash),
            Err(e) => repo_errors.push(e.to_string()),
        }
    }
    let mut msg = format!(
        "download failed for {}:{}:{} across {} repositor{}",
        coord.group_id, coord.artifact_id, coord.version,
        repo_errors.len(),
        if repo_errors.len() == 1 { "y" } else { "ies" }
    );
    for (i, e) in repo_errors.iter().enumerate() {
        let indented = e.replace('\n', "\n    ");
        msg.push_str(&format!("\n  repo #{}: {}", i + 1, indented));
    }
    Err(anyhow::anyhow!(msg))
}

/// A dep queued for sha1 validation.
struct ValidationTarget {
    coord: MavenCoord,
    registry_url: String,
}

/// Compute the SHA-1 hex digest of a file in streaming fashion.
fn compute_sha1_file(path: &Path) -> Result<String> {
    use sha1::{Digest, Sha1};
    use std::io::Read;

    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha1::new();
    let mut buf = [0u8; 65536];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

/// Compute the SHA-256 hex digest of a file in streaming fashion.
/// Used to content-address the on-disk POM cache (ADR-017).
fn compute_sha256_file(path: &Path) -> Result<String> {
    use sha2::{Digest, Sha256};
    use std::io::Read;

    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 65536];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

/// Fetch a `.sha1` sidecar file from a Maven registry.
/// Returns `Ok(Some(hash))` on success, `Ok(None)` for 404 (no sidecar),
/// `Err(_)` for network / protocol failures.
fn fetch_remote_sha1(
    client: &reqwest::blocking::Client,
    url: &str,
    creds: Option<&(String, String)>,
) -> Result<Option<String>> {
    let mut request = client.get(url);
    if let Some((username, password)) = creds {
        // Matches `download_file`: token format is `(token, "")` and works
        // with both Basic and GitHub Packages auth.
        request = request.basic_auth(username, Some(password));
    }
    let response = request.send()?;
    if response.status().as_u16() == 404 {
        return Ok(None);
    }
    if !response.status().is_success() {
        bail!("HTTP {} for {}", response.status(), url);
    }
    let text = response.text()?;
    // Maven .sha1 format: just the hex hash, or `hash  filename`.
    let hash = text
        .split_whitespace()
        .next()
        .ok_or_else(|| anyhow::anyhow!("empty sha1 response from {}", url))?;
    Ok(Some(hash.to_lowercase()))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Sha1CheckResult {
    Match,
    Mismatch,
    NetworkError,
}

fn check_remote_sha1(
    file_path: &Path,
    sha1_url: &str,
    client: &reqwest::blocking::Client,
    creds: Option<&(String, String)>,
) -> Sha1CheckResult {
    // A local read/hash failure (corrupted file) is a real cache fault.
    // Report `Mismatch` so the slow path re-downloads and overwrites it.
    let local_sha1 = match compute_sha1_file(file_path) {
        Ok(s) => s,
        Err(_) => return Sha1CheckResult::Mismatch,
    };
    match fetch_remote_sha1(client, sha1_url, creds) {
        Ok(None) => Sha1CheckResult::Match, // No sidecar — trust cache
        Ok(Some(remote)) => {
            if local_sha1 == remote {
                Sha1CheckResult::Match
            } else {
                Sha1CheckResult::Mismatch
            }
        }
        Err(_) => Sha1CheckResult::NetworkError,
    }
}

// Shared resources for network-bound resolution. Re-creating these on
// every `resolve_inner` call would cost ~10ms of pool/TLS init per
// invocation — painful in `ymc dev` watch loops where every file change
// re-resolves. Lazy statics amortize the cost to once per process.
//
// The three phases (sha1 validation → POM BFS → JAR download) run
// sequentially within a single `resolve_inner`, so one shared pool suffices
// — keeping them separate would just pin 2× the thread stacks for no
// concurrency gain.
const NETWORK_POOL_THREADS: usize = 32;
const SHA1_VALIDATION_CONNECT_MS: u64 = 800;
const SHA1_VALIDATION_REQUEST_MS: u64 = 1000;

static NETWORK_IO_POOL: std::sync::OnceLock<rayon::ThreadPool> =
    std::sync::OnceLock::new();
static SHA1_VALIDATION_CLIENT: std::sync::OnceLock<reqwest::blocking::Client> =
    std::sync::OnceLock::new();

fn network_io_pool() -> &'static rayon::ThreadPool {
    NETWORK_IO_POOL.get_or_init(|| build_io_pool(NETWORK_POOL_THREADS))
}

fn sha1_validation_client() -> &'static reqwest::blocking::Client {
    SHA1_VALIDATION_CLIENT.get_or_init(|| {
        // `.sha1` files are ~40 bytes; a 1s per-request timeout is generous.
        // reqwest follows redirects by default (seen on some Nexus setups).
        reqwest::blocking::Client::builder()
            .user_agent(concat!("ym/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(std::time::Duration::from_millis(SHA1_VALIDATION_CONNECT_MS))
            .timeout(std::time::Duration::from_millis(SHA1_VALIDATION_REQUEST_MS))
            .build()
            .unwrap_or_else(|_| reqwest::blocking::Client::new())
    })
}

/// Check one target's JAR and POM sha1 against the registry. A missing
/// local JAR or POM is reported as `Mismatch` by `check_remote_sha1` so the
/// slow path re-downloads it.
fn check_target(
    t: &ValidationTarget,
    cache_dir: &Path,
    client: &reqwest::blocking::Client,
    creds: Option<&(String, String)>,
) -> Sha1CheckResult {
    let jar_url = format!("{}.sha1", t.coord.jar_url(&t.registry_url));
    match check_remote_sha1(&t.coord.jar_path(cache_dir), &jar_url, client, creds) {
        Sha1CheckResult::Match => {}
        other => return other,
    }

    let pom_url = format!("{}.sha1", t.coord.pom_url(&t.registry_url));
    check_remote_sha1(&t.coord.pom_path(cache_dir), &pom_url, client, creds)
}

/// Validate locally cached JARs and POMs against registry-provided
/// `.sha1` files.
///
/// Returns the coordinates whose cached content no longer matches the
/// registry — i.e. release versions that were republished (ADR-017). An
/// empty result means the fast path can safely proceed: either everything
/// matched, or the registry was unreachable (offline graceful degradation).
fn validate_sha1_remote(targets: &[ValidationTarget], cache_dir: &Path) -> Vec<MavenCoord> {
    use rayon::prelude::*;

    if targets.is_empty() {
        return Vec::new();
    }

    let client = sha1_validation_client();
    let pool = network_io_pool();

    // Resolve credentials once per unique registry URL. Typical cardinality
    // is 1–3, so a linear-scan Vec is both faster and simpler than a hasher.
    let mut creds_by_registry: Vec<(&str, Option<(String, String)>)> = Vec::new();
    for t in targets {
        if !creds_by_registry.iter().any(|(u, _)| *u == t.registry_url) {
            creds_by_registry.push((
                t.registry_url.as_str(),
                load_credentials_for_url(&t.registry_url),
            ));
        }
    }
    let lookup_creds = |registry_url: &str| -> Option<&(String, String)> {
        creds_by_registry
            .iter()
            .find(|(u, _)| *u == registry_url)
            .and_then(|(_, c)| c.as_ref())
    };

    // Every target is checked (no early abort): the caller must purge each
    // republished artifact (ADR-017 fix ②), and missing one would leave a
    // torn cache. Each thread accumulates `(mismatched_coords, net_errors)`.
    let (mismatched, network_errors) = pool.install(|| {
        targets
            .par_iter()
            .fold(
                || (Vec::<MavenCoord>::new(), 0usize),
                |(mut m, n), t| {
                    let creds = lookup_creds(&t.registry_url);
                    match check_target(t, cache_dir, client, creds) {
                        Sha1CheckResult::Match => (m, n),
                        Sha1CheckResult::Mismatch => {
                            m.push(t.coord.clone());
                            (m, n)
                        }
                        Sha1CheckResult::NetworkError => (m, n + 1),
                    }
                },
            )
            .reduce(
                || (Vec::new(), 0usize),
                |(mut m1, n1), (m2, n2)| {
                    m1.extend(m2);
                    (m1, n1 + n2)
                },
            )
    });

    if !mismatched.is_empty() {
        return mismatched;
    }

    if network_errors > 0 && !crate::is_progress_quiet() {
        let total = targets.len();
        if network_errors == total {
            eprintln!(
                "  {} offline mode, using cache (may be stale)",
                console::style("warning").yellow()
            );
        } else {
            eprintln!(
                "  {} sha1 validation incomplete ({}/{} unreachable), using cache",
                console::style("warning").yellow(),
                network_errors,
                total
            );
        }
    }

    Vec::new()
}

/// Download a file and return its SHA-256 hash.
/// Retries up to 3 times with exponential backoff (1s → 2s → 4s).
/// `progress`: optional callback — first call `(content_length, 0)` to register size,
/// then `(0, chunk_bytes)` for each chunk read.
/// Outcome of one download attempt, used to build a self-contained failure
/// message after all retries are exhausted (ADR-023: error must contain
/// URL / HTTP status / body bytes / category / hint).
struct AttemptOutcome {
    http_status: Option<u16>,
    body_bytes: Option<u64>,
    category: String,
}

/// Validate that a downloaded POM body parses as a Maven POM. Returns the
/// error category string on failure (ADR-023). HTTP 200 + non-empty body is
/// not sufficient: CDN edge nodes occasionally return truncated bodies with
/// HTTP 200, and `roxmltree` reports those as `NoRootNode` deep in the BFS
/// without any context. Catch them at write time instead.
fn verify_pom_body(body: &[u8]) -> std::result::Result<(), String> {
    if body.is_empty() {
        return Err("empty body".to_string());
    }
    let text = match std::str::from_utf8(body) {
        Ok(s) => s,
        Err(e) => return Err(format!("invalid UTF-8 in POM body: {}", e)),
    };
    let doc = match roxmltree::Document::parse(text) {
        Ok(d) => d,
        Err(e) => return Err(format!("parse error: {}", e)),
    };
    let root_name = doc.root_element().tag_name().name();
    if root_name != "project" {
        return Err(format!("unexpected root element <{}> (expected <project>)", root_name));
    }
    Ok(())
}

/// L3 (ADR-024): unlink any cached POM that fails integrity check so the caller's subsequent `.exists()` check triggers a refetch ("完整性 > 存在性").
fn invalidate_corrupt_pom(pom_path: &Path) {
    if !pom_path.exists() {
        return;
    }
    let valid = std::fs::read(pom_path)
        .map(|b| verify_pom_body(&b).is_ok())
        .unwrap_or(false);
    if !valid {
        let _ = std::fs::remove_file(pom_path);
    }
}

fn download_file(
    client: &reqwest::blocking::Client,
    url: &str,
    path: &Path,
    progress: Option<&(dyn Fn(u64, u64) + Send + Sync)>,
    inline_creds: Option<(&str, &str)>,
) -> Result<String> {
    let max_retries = 3;
    // ADR-023: track per-attempt outcome so the terminal error is self-contained.
    let mut last_outcome: Option<AttemptOutcome> = None;
    let is_pom = path.extension().and_then(|s| s.to_str()) == Some("pom");

    for attempt in 0..max_retries {
        if attempt > 0 {
            let delay = std::time::Duration::from_secs(1 << attempt); // 2s, 4s
            std::thread::sleep(delay);
        }

        let mut request = client.get(url);

        // Apply credentials: prefer inline (from registry config), fallback to credentials.json
        if let Some((username, password)) = inline_creds {
            request = request.basic_auth(username, Some(password));
        } else if let Some((username, password)) = load_credentials_for_url(url) {
            request = request.basic_auth(username, Some(password));
        }

        match request.send() {
            Ok(response) => {
                if response.status().is_success() {
                    let status = response.status().as_u16();
                    if let Some(parent) = path.parent() {
                        std::fs::create_dir_all(parent)?;
                    }
                    let content_len = response.content_length().unwrap_or(0);
                    if let Some(cb) = &progress {
                        cb(content_len, 0); // register total size
                    }
                    // L1 (ADR-024): NamedTempFile gives a per-call unique path + RAII unlink — two BFS workers fetching the same GAV can no longer truncate each other's in-flight bytes via a shared `.part`.
                    let parent_dir = path.parent().unwrap_or_else(|| Path::new("."));
                    let mut tmp_file = match NamedTempFile::new_in(parent_dir) {
                        Ok(t) => t,
                        Err(e) => {
                            last_outcome = Some(AttemptOutcome {
                                http_status: Some(status),
                                body_bytes: None,
                                category: format!("tempfile create failed: {}", e),
                            });
                            continue;
                        }
                    };

                    let stream_result: Result<(String, u64)> = (|| {
                        use sha2::{Digest, Sha256};
                        use std::io::{Read, Write};
                        let mut reader = response;
                        let file = tmp_file.as_file_mut();
                        let mut hasher = Sha256::new();
                        let mut buf = [0u8; 65536];
                        let mut bytes_written: u64 = 0;
                        loop {
                            let n = reader.read(&mut buf)?;
                            if n == 0 { break; }
                            hasher.update(&buf[..n]);
                            file.write_all(&buf[..n])?;
                            bytes_written += n as u64;
                            if let Some(cb) = &progress {
                                cb(0, n as u64);
                            }
                        }
                        // L4 (ADR-024): CDN-truncated HTTP 200 — cross-check stream length so we retry instead of caching a parseable-but-incomplete POM.
                        if content_len > 0 && bytes_written != content_len {
                            anyhow::bail!(
                                "Content-Length advertised {} bytes but read {} bytes (truncated mid-stream)",
                                content_len, bytes_written
                            );
                        }
                        // L6 (ADR-024): fsync before persist so a host crash never leaves a zero-page .pom that downstream reads as "no root node".
                        file.sync_all()?;
                        Ok((format!("{:x}", hasher.finalize()), bytes_written))
                    })();
                    match stream_result {
                        Ok((hash, bytes_written)) => {
                            // ADR-023: parse-before-persist for raw POM. HTTP
                            // 200 + streamed body is not enough — also see L4
                            // above for length-based truncation detection.
                            if is_pom {
                                match std::fs::read(tmp_file.path()) {
                                    Ok(body) => match verify_pom_body(&body) {
                                        Ok(()) => {
                                            tmp_file.persist(path).map_err(|e| {
                                                anyhow::anyhow!("persist tempfile to {}: {}", path.display(), e.error)
                                            })?;
                                            return Ok(hash);
                                        }
                                        Err(reason) => {
                                            // tmp_file drop unlinks the bad tempfile
                                            last_outcome = Some(AttemptOutcome {
                                                http_status: Some(status),
                                                body_bytes: Some(bytes_written),
                                                category: format!("truncated body ({})", reason),
                                            });
                                            continue; // retry
                                        }
                                    },
                                    Err(e) => {
                                        last_outcome = Some(AttemptOutcome {
                                            http_status: Some(status),
                                            body_bytes: Some(bytes_written),
                                            category: format!("read tmp failed: {}", e),
                                        });
                                        continue;
                                    }
                                }
                            }
                            // Non-POM (e.g. jar): trust the stream.
                            tmp_file.persist(path).map_err(|e| {
                                anyhow::anyhow!("persist tempfile to {}: {}", path.display(), e.error)
                            })?;
                            return Ok(hash);
                        }
                        Err(e) => {
                            // tmp_file drop unlinks the partial tempfile
                            last_outcome = Some(AttemptOutcome {
                                http_status: Some(status),
                                body_bytes: None,
                                category: format!("download stream failed: {}", e),
                            });
                        }
                    }
                } else if response.status().as_u16() == 404 {
                    // 404 means artifact doesn't exist in this repo, no retry
                    bail!("HTTP 404 for {}", url);
                } else {
                    last_outcome = Some(AttemptOutcome {
                        http_status: Some(response.status().as_u16()),
                        body_bytes: None,
                        category: format!("HTTP {}", response.status()),
                    });
                }
            }
            Err(e) => {
                last_outcome = Some(AttemptOutcome {
                    http_status: None,
                    body_bytes: None,
                    category: format!("request failed: {}", e),
                });
            }
        }
    }

    // ADR-023: build a self-contained error message containing URL, last
    // attempt HTTP status, body bytes, retry count, category, and a hint.
    let outcome = last_outcome.unwrap_or(AttemptOutcome {
        http_status: None,
        body_bytes: None,
        category: "unknown failure".to_string(),
    });
    let status_str = outcome.http_status.map(|s| format!("HTTP {}", s)).unwrap_or_else(|| "no response".to_string());
    let bytes_str = outcome.body_bytes.map(|b| format!("{} bytes", b)).unwrap_or_else(|| "n/a".to_string());
    let hint = if outcome.category.starts_with("truncated body") {
        "\n    hint: registry may be returning incomplete responses (CDN edge node? proxy?)"
    } else if outcome.http_status.map(|s| s >= 500).unwrap_or(false) {
        "\n    hint: registry returned 5xx — temporary outage, retry later"
    } else if outcome.category.starts_with("request failed") {
        "\n    hint: network/DNS issue or registry unreachable; check credentials and connectivity"
    } else {
        ""
    };
    Err(anyhow::anyhow!(
        "download failed after {} retries\n    URL: {}\n    last attempt: {}, {}\n    failure category: {}{}",
        max_retries, url, status_str, bytes_str, outcome.category, hint
    ))
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
    // Skip GPG verification in quiet/CI mode — avoids unnecessary .asc downloads
    if crate::is_progress_quiet() {
        return;
    }
    let asc_path = jar_path.with_extension("jar.asc");

    // Try to download the .asc signature file
    let asc_result = download_from_repos(client, coord, &asc_path, registries, None, |c, r| {
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
            "{} {} artifact(s) failed GPG signature verification (missing public keys?)",
            console::style(format!("{:>12}", "warning")).yellow().bold(),
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

/// Load credentials for the given URL from `~/.ym/credentials.json`.
/// Use `ym login` to store credentials.
/// File format: { "https://maven.example.com": { "username": "...", "password": "..." } }
fn load_credentials_for_url(url: &str) -> Option<(String, String)> {
    let creds_path = crate::home_dir().join(".ym").join("credentials.json");
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
pub fn check_conflicts(lock: &Lockfile) -> Vec<(String, Vec<String>)> {
    let mut versions_map: BTreeMap<String, Vec<String>> = BTreeMap::new();

    for key in lock.dependencies.keys() {
        if let Some(mc) = MavenCoord::from_versioned_key(key) {
            // Only track conflicts for base artifacts (not classifier variants)
            if mc.classifier.is_none() {
                let ga = format!("{}:{}", mc.group_id, mc.artifact_id);
                versions_map
                    .entry(ga)
                    .or_default()
                    .push(mc.version);
            }
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
    lock: &mut Lockfile,
    registries: &[RegistryEntry],
    exclusions: &[String],
) -> Result<HashMap<String, Vec<PathBuf>>> {
    resolve_workspace_deps_with_resolutions(all_module_deps, cache_dir, lock, registries, exclusions, &Default::default())
}

#[allow(clippy::too_many_arguments)]
pub fn resolve_workspace_deps_with_resolutions(
    all_module_deps: &[(String, BTreeMap<String, String>)],
    cache_dir: &Path,
    lock: &mut Lockfile,
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

    // 3+4. Distribute resolved JARs to per-module sets
    if !crate::is_json_quiet() && console::Term::stderr().is_term() {
        resolver_progress("Distributing dependencies...");
    }
    Ok(distribute_jars_per_module(all_module_deps, cache_dir, lock))
}

/// Distribute resolved JARs to per-module sets by BFS through the lock file graph.
fn distribute_jars_per_module(
    all_module_deps: &[(String, BTreeMap<String, String>)],
    cache_dir: &Path,
    lock: &Lockfile,
) -> HashMap<String, Vec<PathBuf>> {
    // Build GA → versioned key lookup from lock file
    let mut ga_to_versioned: HashMap<String, String> = HashMap::new();
    for key in lock.dependencies.keys() {
        if let Some(mc) = MavenCoord::from_versioned_key(key) {
            let ga = format!("{}:{}", mc.group_id, mc.artifact_id);
            ga_to_versioned.entry(ga).or_insert(key.clone());
        }
    }

    let mut per_module = HashMap::new();
    for (name, deps) in all_module_deps {
        let mut module_jars = Vec::new();
        let mut visited_keys = HashSet::new();
        let mut queue = VecDeque::new();

        // Seed with this module's direct deps
        for (coord, version) in deps {
            if let Ok(mc) = MavenCoord::parse(coord, version) {
                let vk = mc.versioned_key();
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
            if let Some(coord) = MavenCoord::from_versioned_key(&key) {
                let jar = coord.jar_path(cache_dir);
                if jar.exists() {
                    module_jars.push(jar);
                } else {
                    // Fall back to resolved version JAR
                    let ga = format!("{}:{}", coord.group_id, coord.artifact_id);
                    if let Some(resolved_key) = ga_to_versioned.get(&ga) {
                        if let Some(rc) = MavenCoord::from_versioned_key(resolved_key) {
                            let rjar = rc.jar_path(cache_dir);
                            if rjar.exists() {
                                module_jars.push(rjar);
                            }
                        }
                    }
                }
            }
            // Add transitive deps from lock
            let locked_entry = lock.dependencies.get(&key).or_else(|| {
                MavenCoord::from_versioned_key(&key).and_then(|mc| {
                    let ga = format!("{}:{}", mc.group_id, mc.artifact_id);
                    ga_to_versioned.get(&ga).and_then(|resolved_key| lock.dependencies.get(resolved_key))
                })
            });
            if let Some(locked) = locked_entry {
                if let Some(ref dep_keys) = locked.dependencies {
                    for dk in dep_keys {
                        if !visited_keys.contains(dk) {
                            queue.push_back(dk.clone());
                        }
                    }
                }
            }
        }

        append_native_jars(&mut module_jars);
        per_module.insert(name.clone(), module_jars);
    }

    per_module
}

/// Workspace-level no-download variant: resolve merged deps from lock + local cache only.
/// Used by `idea --json` to avoid network I/O.
pub fn resolve_workspace_deps_no_download(
    all_module_deps: &[(String, BTreeMap<String, String>)],
    cache_dir: &Path,
    lock: &mut Lockfile,
    registries: &[RegistryEntry],
    exclusions: &[String],
    resolutions: &BTreeMap<String, String>,
) -> Result<HashMap<String, Vec<PathBuf>>> {
    let mut merged_deps = BTreeMap::new();
    for (_name, deps) in all_module_deps {
        for (coord, version) in deps {
            merged_deps.entry(coord.clone()).or_insert(version.clone());
        }
    }

    if merged_deps.is_empty() {
        return Ok(all_module_deps.iter().map(|(name, _)| (name.clone(), vec![])).collect());
    }

    // Resolve without downloading (lock + local cache only)
    let _all_jars = resolve_no_download(
        &merged_deps, cache_dir, lock, registries, exclusions, resolutions, &HashMap::new(),
    )?;

    Ok(distribute_jars_per_module(all_module_deps, cache_dir, lock))
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

    // Try 1: Solr search API (fast, but index may lag for new packages)
    let url = format!(
        "https://search.maven.org/solrsearch/select?q=g:\"{}\" AND a:\"{}\"&rows=1&wt=json",
        group_id, artifact_id
    );

    if let Ok(response) = client.get(&url).send() {
        if let Ok(body) = response.json::<serde_json::Value>() {
            if let Some(docs) = body["response"]["docs"].as_array() {
                if let Some(doc) = docs.first() {
                    if let Some(v) = doc["latestVersion"].as_str() {
                        return Ok(v.to_string());
                    }
                }
            }
        }
    }

    // Try 2: Direct maven-metadata.xml (reliable, works for all published packages)
    let metadata_url = format!(
        "https://repo1.maven.org/maven2/{}/{}/maven-metadata.xml",
        group_id.replace('.', "/"),
        artifact_id
    );

    if let Ok(response) = client.get(&metadata_url).send() {
        if response.status().is_success() {
            if let Ok(text) = response.text() {
                // Parse <release>version</release> or last <version> in <versions>
                if let Some(v) = extract_xml_value(&text, "release") {
                    return Ok(v);
                }
                // Fallback: get last <version> entry
                let mut last_version = None;
                for line in text.lines() {
                    let trimmed = line.trim();
                    if trimmed.starts_with("<version>") && trimmed.ends_with("</version>") {
                        last_version = Some(
                            trimmed
                                .strip_prefix("<version>")
                                .unwrap()
                                .strip_suffix("</version>")
                                .unwrap()
                                .to_string(),
                        );
                    }
                }
                if let Some(v) = last_version {
                    return Ok(v);
                }
            }
        }
    }

    bail!("Could not find {}:{} on Maven Central", group_id, artifact_id)
}

fn extract_xml_value(text: &str, tag: &str) -> Option<String> {
    let open = format!("<{}>", tag);
    let close = format!("</{}>", tag);
    let start = text.find(&open)? + open.len();
    let end = text[start..].find(&close)? + start;
    let val = text[start..end].trim();
    if val.is_empty() { None } else { Some(val.to_string()) }
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
        assert_eq!(repos[0].url, DEFAULT_REPO);
    }

    #[test]
    fn test_repos_for_group_id_unscoped_registry() {
        let entries = vec![
            RegistryEntry { url: "https://custom.repo/maven".into(), scope: None, username: None, password: None },
        ];
        let repos = repos_for_group_id(&entries, "com.example");
        assert_eq!(repos.len(), 2);
        assert_eq!(repos[0].url, "https://custom.repo/maven");
        assert_eq!(repos[1].url, DEFAULT_REPO);
    }

    #[test]
    fn test_repos_for_group_id_scope_match() {
        let entries = vec![
            RegistryEntry { url: "https://private.repo/maven".into(), scope: Some("com.mycompany.*".into()), username: None, password: None },
            RegistryEntry { url: "https://other.repo/maven".into(), scope: None, username: None, password: None },
        ];
        // Matching groupId → only scoped repo, no fallback
        let repos = repos_for_group_id(&entries, "com.mycompany.core");
        assert_eq!(repos.len(), 1);
        assert_eq!(repos[0].url, "https://private.repo/maven");
        // Non-matching groupId → unscoped repos + Central
        let repos = repos_for_group_id(&entries, "org.apache.commons");
        assert_eq!(repos.len(), 2);
        assert_eq!(repos[0].url, "https://other.repo/maven");
        assert_eq!(repos[1].url, DEFAULT_REPO);
    }

    #[test]
    fn test_repos_for_group_id_no_duplicate_central() {
        let entries = vec![
            RegistryEntry { url: DEFAULT_REPO.into(), scope: None, username: None, password: None },
        ];
        let repos = repos_for_group_id(&entries, "com.example");
        assert_eq!(repos.len(), 1);
    }

    #[test]
    fn test_repos_for_group_id_trims_trailing_slash() {
        let entries = vec![
            RegistryEntry { url: "https://custom.repo/maven/".into(), scope: None, username: None, password: None },
        ];
        let repos = repos_for_group_id(&entries, "com.example");
        assert_eq!(repos[0].url, "https://custom.repo/maven");
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
        let mut lock = Lockfile::default();
        lock.dependencies.insert("com.example:lib:1.0".to_string(), ResolvedDependency::default());
        let conflicts = check_conflicts(&lock);
        assert!(conflicts.is_empty());
    }

    #[test]
    fn test_check_conflicts_detects_multiple_versions() {
        let mut lock = Lockfile::default();
        lock.dependencies.insert("com.example:lib:1.0".to_string(), ResolvedDependency::default());
        lock.dependencies.insert("com.example:lib:2.0".to_string(), ResolvedDependency::default());
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
        let lock = Lockfile::default();
        let result = try_resolve_from_lock(&deps, Path::new("/tmp/cache"), &lock, &HashSet::new(), &BTreeMap::new(), &[], false);
        // Empty lock returns Ok(None)
        assert!(result.unwrap().is_none());
    }

    #[test]
    fn test_compute_sha256_file() {
        let dir = std::env::temp_dir().join(format!("ym_sha256_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("x.txt");
        std::fs::write(&f, b"hello").unwrap();
        assert_eq!(
            compute_sha256_file(&f).unwrap(),
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_pom_cache_entry_roundtrip() {
        // ADR-017 + ADR-021: content-addressed pom-cache format round-trips.
        let entry = PomCacheEntry {
            schema_version: POM_CACHE_SCHEMA_VERSION,
            pom_sha256: "abc123".to_string(),
            resolved: vec![("g".into(), "a".into(), "1.0".into(), vec!["x:y".into()])],
        };
        let json = serde_json::to_string(&entry).unwrap();
        let back: PomCacheEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(back.schema_version, POM_CACHE_SCHEMA_VERSION);
        assert_eq!(back.pom_sha256, "abc123");
        assert_eq!(back.resolved.len(), 1);
        // Old bare-array format must NOT parse as PomCacheEntry — forces a
        // one-time regeneration instead of being trusted blind.
        let old = r#"[["g","a","1.0",[]]]"#;
        assert!(serde_json::from_str::<PomCacheEntry>(old).is_err());
    }

    #[test]
    fn test_validate_sha1_remote_empty() {
        // No targets → nothing to validate → empty result (fast path proceeds).
        assert!(validate_sha1_remote(&[], Path::new("/tmp/cache")).is_empty());
    }

    #[test]
    fn test_purge_artifact_cache() {
        // ADR-017 fix ②: a republished artifact's jar + pom are removed so the
        // slow path re-resolves it cache-cold.
        let dir = std::env::temp_dir().join(format!("ym_purge_test_{}", std::process::id()));
        let coord = MavenCoord::parse("com.example:purge-lib", "9.9.9").unwrap();
        let jar = coord.jar_path(&dir);
        let pom = coord.pom_path(&dir);
        std::fs::create_dir_all(jar.parent().unwrap()).unwrap();
        std::fs::write(&jar, b"jar").unwrap();
        std::fs::write(&pom, b"pom").unwrap();
        assert!(jar.exists() && pom.exists());
        purge_artifact_cache(&coord, &dir);
        assert!(!jar.exists(), "jar should be purged");
        assert!(!pom.exists(), "pom should be purged");
        let _ = std::fs::remove_dir_all(&dir);
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
        let mut lock = Lockfile::default();
        let result = resolve_workspace_deps(
            &module_deps, Path::new("/tmp/cache"), &mut lock, &[], &[],
        ).unwrap();
        assert_eq!(result.get("mod-a").unwrap().len(), 0);
    }

    // --- version_compare tests (latest-wins arbitration support, ADR-016) ---

    #[test]
    fn test_version_compare_basic_ordering() {
        assert!(version_compare("1.0", "2.0") < 0);
        assert!(version_compare("2.0", "1.0") > 0);
        assert_eq!(version_compare("1.0", "1.0"), 0);
    }

    #[test]
    fn test_version_compare_semver_not_lexical() {
        // SemVer: 1.10.0 must be greater than 1.9.0 (lexical would say opposite)
        assert!(version_compare("1.10.0", "1.9.0") > 0);
        assert!(version_compare("2.10", "2.2") > 0);
        assert!(version_compare("4.0.10", "4.0.3") > 0);
    }

    #[test]
    fn test_version_compare_qualifier_release_wins() {
        // Maven convention: release (no qualifier) > any qualifier
        assert!(version_compare("4.0.3", "4.0.3-5") > 0);
        assert!(version_compare("1.0", "1.0-SNAPSHOT") > 0);
    }

    #[test]
    fn test_version_compare_uneven_segment_count() {
        // 1.0 should equal 1.0.0 (missing segments treated as 0)
        assert_eq!(version_compare("1.0", "1.0.0"), 0);
        assert!(version_compare("1.0.1", "1.0") > 0);
        assert!(version_compare("2", "1.99.99") > 0);
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

    // --- ADR-021: parent POM <dependencies> inheritance ---

    #[test]
    fn test_pom_cache_entry_v1_schema_rejected() {
        // v1 (pre-ADR-021) cache entry has no `schema_version` field. Loading
        // it as the v2 struct must fail deserialization so the disk cache
        // self-heals to v2 on next read.
        let v1_json = r#"{"pom_sha256":"abc","resolved":[]}"#;
        let result: std::result::Result<PomCacheEntry, _> = serde_json::from_str(v1_json);
        assert!(result.is_err(), "v1 schema must fail to deserialize as v2");
    }

    #[test]
    fn test_pom_cache_entry_v2_schema_roundtrip() {
        let entry = PomCacheEntry {
            schema_version: POM_CACHE_SCHEMA_VERSION,
            pom_sha256: "deadbeef".to_string(),
            resolved: vec![("g".to_string(), "a".to_string(), "1.0".to_string(), vec![])],
        };
        let json = serde_json::to_string(&entry).unwrap();
        assert!(json.contains("\"schema_version\":2"), "v2 must persist schema_version=2");
        let parsed: PomCacheEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.schema_version, 2);
        assert_eq!(parsed.pom_sha256, "deadbeef");
        assert_eq!(parsed.resolved.len(), 1);
    }

    fn write_pom_to_cache(cache_dir: &Path, g: &str, a: &str, v: &str, body: &str) {
        let coord = MavenCoord {
            group_id: g.to_string(),
            artifact_id: a.to_string(),
            version: v.to_string(),
            classifier: None,
            exclusions: Vec::new(),
            scope: None,
        };
        let pom = coord.pom_path(cache_dir);
        std::fs::create_dir_all(pom.parent().unwrap()).unwrap();
        std::fs::write(&pom, body).unwrap();
    }

    #[test]
    fn test_collect_parent_dependencies_inherits_parent_block() {
        // The AWS SDK scenario: a child POM (ec2) declares almost nothing,
        // but its parent (services) ships sdk-core/auth/regions/http-client-spi
        // as direct compile-scope deps. Pre-ADR-021 ym silently dropped these.
        let cache_dir = std::env::temp_dir()
            .join(format!("ym_p021_inherit_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&cache_dir);
        std::fs::create_dir_all(&cache_dir).unwrap();

        let parent_pom = r#"<?xml version="1.0"?>
<project>
    <groupId>com.example</groupId>
    <artifactId>services</artifactId>
    <version>1.0</version>
    <dependencies>
        <dependency>
            <groupId>com.lib</groupId>
            <artifactId>sdk-core</artifactId>
            <version>2.0</version>
        </dependency>
        <dependency>
            <groupId>com.lib</groupId>
            <artifactId>auth</artifactId>
            <version>2.0</version>
        </dependency>
    </dependencies>
</project>"#;
        write_pom_to_cache(&cache_dir, "com.example", "services", "1.0", parent_pom);

        let child_pom = r#"<?xml version="1.0"?>
<project>
    <parent>
        <groupId>com.example</groupId>
        <artifactId>services</artifactId>
        <version>1.0</version>
    </parent>
    <artifactId>ec2</artifactId>
    <dependencies>
        <dependency>
            <groupId>com.lib</groupId>
            <artifactId>protocol-core</artifactId>
            <version>2.0</version>
        </dependency>
    </dependencies>
</project>"#;

        let props = HashMap::new();
        let client = reqwest::blocking::Client::new();
        let mut visited = HashSet::new();
        let parent_deps = collect_parent_dependencies(
            &client, child_pom, &cache_dir, &[], &props, 0, &mut visited,
        ).unwrap();

        let gas: Vec<String> = parent_deps.iter()
            .map(|d| format!("{}:{}", d.group_id, d.artifact_id))
            .collect();
        assert!(gas.contains(&"com.lib:sdk-core".to_string()),
            "parent's sdk-core must be inherited; got {:?}", gas);
        assert!(gas.contains(&"com.lib:auth".to_string()),
            "parent's auth must be inherited; got {:?}", gas);
        // child's own protocol-core is NOT returned by this fn (the caller
        // merges child deps on top); only ancestor deps are here.
        assert!(!gas.contains(&"com.lib:protocol-core".to_string()),
            "child's own deps should not be in parent-chain returns");

        let _ = std::fs::remove_dir_all(&cache_dir);
    }

    #[test]
    fn test_collect_parent_dependencies_closer_parent_wins_over_grandparent() {
        // Same G:A in both parent and grandparent — closer ancestor's version
        // wins. Maven inheritance: deeper ancestors don't override nearer ones.
        let cache_dir = std::env::temp_dir()
            .join(format!("ym_p021_precedence_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&cache_dir);
        std::fs::create_dir_all(&cache_dir).unwrap();

        let grandparent_pom = r#"<?xml version="1.0"?>
<project>
    <groupId>com.example</groupId>
    <artifactId>grandparent</artifactId>
    <version>1.0</version>
    <dependencies>
        <dependency>
            <groupId>com.lib</groupId>
            <artifactId>auth</artifactId>
            <version>1.0</version>
        </dependency>
    </dependencies>
</project>"#;
        write_pom_to_cache(&cache_dir, "com.example", "grandparent", "1.0", grandparent_pom);

        let parent_pom = r#"<?xml version="1.0"?>
<project>
    <parent>
        <groupId>com.example</groupId>
        <artifactId>grandparent</artifactId>
        <version>1.0</version>
    </parent>
    <artifactId>parent</artifactId>
    <dependencies>
        <dependency>
            <groupId>com.lib</groupId>
            <artifactId>auth</artifactId>
            <version>2.0</version>
        </dependency>
        <dependency>
            <groupId>com.lib</groupId>
            <artifactId>utils</artifactId>
            <version>2.0</version>
        </dependency>
    </dependencies>
</project>"#;
        write_pom_to_cache(&cache_dir, "com.example", "parent", "1.0", parent_pom);

        let child_pom = r#"<?xml version="1.0"?>
<project>
    <parent>
        <groupId>com.example</groupId>
        <artifactId>parent</artifactId>
        <version>1.0</version>
    </parent>
    <artifactId>child</artifactId>
</project>"#;

        let props = HashMap::new();
        let client = reqwest::blocking::Client::new();
        let mut visited = HashSet::new();
        let parent_deps = collect_parent_dependencies(
            &client, child_pom, &cache_dir, &[], &props, 0, &mut visited,
        ).unwrap();

        let auth = parent_deps.iter().find(|d| d.artifact_id == "auth")
            .expect("auth must appear");
        assert_eq!(auth.version, "2.0",
            "closer parent (2.0) must win over grandparent (1.0)");
        assert!(parent_deps.iter().any(|d| d.artifact_id == "utils"),
            "grandparent-only dep not declared by closer parent should still merge");

        let _ = std::fs::remove_dir_all(&cache_dir);
    }

    #[test]
    fn test_collect_parent_dependencies_filters_test_provided_optional() {
        // The reused parse_pom_dependencies_with_props already drops these,
        // but pin it down so a future refactor that bypasses the reuse
        // doesn't regress filtering on parent deps.
        let cache_dir = std::env::temp_dir()
            .join(format!("ym_p021_filter_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&cache_dir);
        std::fs::create_dir_all(&cache_dir).unwrap();

        let parent_pom = r#"<?xml version="1.0"?>
<project>
    <groupId>com.example</groupId>
    <artifactId>parent</artifactId>
    <version>1.0</version>
    <dependencies>
        <dependency>
            <groupId>com.lib</groupId>
            <artifactId>compile-dep</artifactId>
            <version>1.0</version>
        </dependency>
        <dependency>
            <groupId>com.lib</groupId>
            <artifactId>test-dep</artifactId>
            <version>1.0</version>
            <scope>test</scope>
        </dependency>
        <dependency>
            <groupId>com.lib</groupId>
            <artifactId>provided-dep</artifactId>
            <version>1.0</version>
            <scope>provided</scope>
        </dependency>
        <dependency>
            <groupId>com.lib</groupId>
            <artifactId>optional-dep</artifactId>
            <version>1.0</version>
            <optional>true</optional>
        </dependency>
    </dependencies>
</project>"#;
        write_pom_to_cache(&cache_dir, "com.example", "parent", "1.0", parent_pom);

        let child_pom = r#"<?xml version="1.0"?>
<project>
    <parent>
        <groupId>com.example</groupId>
        <artifactId>parent</artifactId>
        <version>1.0</version>
    </parent>
    <artifactId>child</artifactId>
</project>"#;

        let props = HashMap::new();
        let client = reqwest::blocking::Client::new();
        let mut visited = HashSet::new();
        let parent_deps = collect_parent_dependencies(
            &client, child_pom, &cache_dir, &[], &props, 0, &mut visited,
        ).unwrap();

        let ids: Vec<String> = parent_deps.iter().map(|d| d.artifact_id.clone()).collect();
        assert!(ids.contains(&"compile-dep".to_string()), "compile dep must inherit");
        assert!(!ids.contains(&"test-dep".to_string()), "test-scope must be filtered");
        assert!(!ids.contains(&"provided-dep".to_string()), "provided-scope must be filtered");
        assert!(!ids.contains(&"optional-dep".to_string()), "optional=true must be filtered");

        let _ = std::fs::remove_dir_all(&cache_dir);
    }

    #[test]
    fn test_resolve_transitive_cached_merges_child_and_parent_deps() {
        // End-to-end equivalent of the AWS SDK ec2/services scenario:
        // child (ec2) declares protocol-core; parent (services) declares
        // auth + sdk-core. The merged direct-deps list returned by
        // resolve_transitive_cached must contain BOTH the child's own and
        // the parent's inherited deps. Pre-ADR-021 ym returned only the
        // child's, silently dropping the 4 jars (auth/sdk-core/regions/
        // http-client-spi) needed at compile time.
        let cache_dir = std::env::temp_dir()
            .join(format!("ym_p021_e2e_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&cache_dir);
        std::fs::create_dir_all(&cache_dir).unwrap();

        let child_pom = r#"<?xml version="1.0"?>
<project>
    <parent>
        <groupId>com.example</groupId>
        <artifactId>services</artifactId>
        <version>1.0</version>
    </parent>
    <artifactId>ec2</artifactId>
    <version>1.0</version>
    <dependencies>
        <dependency>
            <groupId>com.lib</groupId>
            <artifactId>protocol-core</artifactId>
            <version>1.0</version>
        </dependency>
    </dependencies>
</project>"#;
        write_pom_to_cache(&cache_dir, "com.example", "ec2", "1.0", child_pom);

        let parent_pom = r#"<?xml version="1.0"?>
<project>
    <groupId>com.example</groupId>
    <artifactId>services</artifactId>
    <version>1.0</version>
    <dependencies>
        <dependency>
            <groupId>com.lib</groupId>
            <artifactId>auth</artifactId>
            <version>1.0</version>
        </dependency>
        <dependency>
            <groupId>com.lib</groupId>
            <artifactId>sdk-core</artifactId>
            <version>1.0</version>
        </dependency>
    </dependencies>
</project>"#;
        write_pom_to_cache(&cache_dir, "com.example", "services", "1.0", parent_pom);

        let coord = MavenCoord {
            group_id: "com.example".into(),
            artifact_id: "ec2".into(),
            version: "1.0".into(),
            classifier: None,
            exclusions: Vec::new(),
            scope: None,
        };
        let client = reqwest::blocking::Client::new();
        let deps = resolve_transitive_cached(&client, &coord, &cache_dir, &[], None).unwrap();

        let gas: Vec<String> = deps.iter()
            .map(|d| format!("{}:{}", d.group_id, d.artifact_id))
            .collect();
        assert!(gas.contains(&"com.lib:protocol-core".to_string()),
            "child's own dep must be present; got {:?}", gas);
        assert!(gas.contains(&"com.lib:auth".to_string()),
            "parent's auth must be merged into child deps; got {:?}", gas);
        assert!(gas.contains(&"com.lib:sdk-core".to_string()),
            "parent's sdk-core must be merged into child deps; got {:?}", gas);

        let _ = std::fs::remove_dir_all(&cache_dir);
    }

    #[test]
    fn test_resolve_transitive_cached_child_overrides_parent_same_ga() {
        // When the same groupId:artifactId appears in both child and parent,
        // the child's declaration wins (Maven override semantics).
        let cache_dir = std::env::temp_dir()
            .join(format!("ym_p021_override_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&cache_dir);
        std::fs::create_dir_all(&cache_dir).unwrap();

        let child_pom = r#"<?xml version="1.0"?>
<project>
    <parent>
        <groupId>com.example</groupId>
        <artifactId>p</artifactId>
        <version>1.0</version>
    </parent>
    <artifactId>c</artifactId>
    <version>1.0</version>
    <dependencies>
        <dependency>
            <groupId>com.lib</groupId>
            <artifactId>auth</artifactId>
            <version>3.0</version>
        </dependency>
    </dependencies>
</project>"#;
        write_pom_to_cache(&cache_dir, "com.example", "c", "1.0", child_pom);

        let parent_pom = r#"<?xml version="1.0"?>
<project>
    <groupId>com.example</groupId>
    <artifactId>p</artifactId>
    <version>1.0</version>
    <dependencies>
        <dependency>
            <groupId>com.lib</groupId>
            <artifactId>auth</artifactId>
            <version>1.0</version>
        </dependency>
    </dependencies>
</project>"#;
        write_pom_to_cache(&cache_dir, "com.example", "p", "1.0", parent_pom);

        let coord = MavenCoord {
            group_id: "com.example".into(), artifact_id: "c".into(),
            version: "1.0".into(), classifier: None,
            exclusions: Vec::new(), scope: None,
        };
        let client = reqwest::blocking::Client::new();
        let deps = resolve_transitive_cached(&client, &coord, &cache_dir, &[], None).unwrap();

        let auth = deps.iter().find(|d| d.artifact_id == "auth")
            .expect("auth must appear");
        assert_eq!(auth.version, "3.0",
            "child's auth (3.0) must win over parent's auth (1.0); deduplicated on G:A");
        assert_eq!(deps.iter().filter(|d| d.artifact_id == "auth").count(), 1,
            "auth must appear exactly once after dedup");

        let _ = std::fs::remove_dir_all(&cache_dir);
    }

    #[test]
    fn test_collect_parent_dependencies_no_parent_returns_empty() {
        // A root POM (no <parent> block) must safely return [] — the function
        // is called unconditionally for every coord in the BFS.
        let pom = r#"<?xml version="1.0"?>
<project>
    <groupId>com.example</groupId>
    <artifactId>root</artifactId>
    <version>1.0</version>
    <dependencies>
        <dependency>
            <groupId>com.lib</groupId>
            <artifactId>x</artifactId>
            <version>1.0</version>
        </dependency>
    </dependencies>
</project>"#;
        let props = HashMap::new();
        let client = reqwest::blocking::Client::new();
        let mut visited = HashSet::new();
        let deps = collect_parent_dependencies(
            &client, pom, Path::new("/tmp"), &[], &props, 0, &mut visited,
        ).unwrap();
        assert!(deps.is_empty(), "no parent → no inherited deps");
    }

    // --- ADR-023: raw POM body verification ---

    #[test]
    fn test_verify_pom_body_rejects_empty() {
        // Empty body = unmistakable truncation. Must fail before write.
        let err = verify_pom_body(b"").unwrap_err();
        assert!(err.contains("empty"), "got: {}", err);
    }

    #[test]
    fn test_verify_pom_body_rejects_non_xml() {
        // Body that is not XML at all (CDN returned an HTML error page, etc.).
        assert!(verify_pom_body(b"not xml at all").is_err());
    }

    #[test]
    fn test_verify_pom_body_rejects_wrong_root_element() {
        // XML parses but root is not <project> — likely an error page or
        // unrelated XML proxied through.
        let body = br#"<?xml version="1.0"?><html><body>oops</body></html>"#;
        let err = verify_pom_body(body).unwrap_err();
        assert!(err.contains("unexpected root"), "got: {}", err);
    }

    #[test]
    fn test_verify_pom_body_accepts_valid_project_root() {
        let body = br#"<?xml version="1.0"?>
<project>
    <groupId>com.example</groupId>
    <artifactId>lib</artifactId>
    <version>1.0</version>
</project>"#;
        verify_pom_body(body).expect("valid POM body must pass");
    }

    // --- ADR-022: explicit dep is shielded from BOM constraint ---

    #[test]
    fn test_resolve_inner_explicit_dep_unchanged_by_constraint() {
        // ADR-022 main invariant: a GA declared directly in ym.json keeps
        // its declared version, even when an imported BOM constraint would
        // upgrade it. This is the user-pin-wins contract.
        let cache_dir = std::env::temp_dir()
            .join(format!("ym_adr022_explicit_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&cache_dir);
        std::fs::create_dir_all(&cache_dir).unwrap();

        let explicit_pom = r#"<?xml version="1.0"?>
<project>
    <groupId>com.example</groupId>
    <artifactId>explicit-dep</artifactId>
    <version>1.0</version>
</project>"#;
        write_pom_to_cache(&cache_dir, "com.example", "explicit-dep", "1.0", explicit_pom);

        let mut deps = BTreeMap::new();
        deps.insert("com.example:explicit-dep".to_string(), "1.0".to_string());

        // BOM tries to upgrade explicit dep to 5.0 — must be IGNORED.
        let mut constraints = BTreeMap::new();
        constraints.insert("com.example:explicit-dep".to_string(), "5.0".to_string());

        let mut lock = Lockfile::default();
        let result = resolve_inner(
            &deps,
            &cache_dir,
            &mut lock,
            &[],
            &[],
            &BTreeMap::new(),
            &constraints,
            &HashMap::new(),
            false,
        );
        let _ = std::fs::remove_dir_all(&cache_dir);
        result.expect("resolve_inner should succeed");

        let keys: Vec<String> = lock.dependencies.keys().cloned().collect();
        assert!(
            keys.contains(&"com.example:explicit-dep:1.0".to_string()),
            "explicit pin must keep version 1.0; got {:?}", keys
        );
        assert!(
            !keys.contains(&"com.example:explicit-dep:5.0".to_string()),
            "BOM constraint 5.0 must NOT override explicit 1.0; got {:?}", keys
        );
    }

    #[test]
    fn test_resolve_inner_transitive_dep_upgraded_by_constraint() {
        // ADR-022 regression test: BOM constraint still works on non-explicit
        // (transitive) GAs. Locks down that the explicit-guard didn't break
        // the existing platform()-style upgrade behavior for transitive deps.
        let cache_dir = std::env::temp_dir()
            .join(format!("ym_adr022_trans_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&cache_dir);
        std::fs::create_dir_all(&cache_dir).unwrap();

        // Direct dep: host:1.0 brings nested:2.0 transitively.
        let host_pom = r#"<?xml version="1.0"?>
<project>
    <groupId>com.example</groupId>
    <artifactId>host</artifactId>
    <version>1.0</version>
    <dependencies>
        <dependency>
            <groupId>com.lib</groupId>
            <artifactId>nested</artifactId>
            <version>2.0</version>
        </dependency>
    </dependencies>
</project>"#;
        write_pom_to_cache(&cache_dir, "com.example", "host", "1.0", host_pom);

        // BOM upgrades nested → 5.0. Since nested is NOT explicit, the upgrade
        // must take effect.
        let nested_5_pom = r#"<?xml version="1.0"?>
<project>
    <groupId>com.lib</groupId>
    <artifactId>nested</artifactId>
    <version>5.0</version>
</project>"#;
        write_pom_to_cache(&cache_dir, "com.lib", "nested", "5.0", nested_5_pom);

        let mut deps = BTreeMap::new();
        deps.insert("com.example:host".to_string(), "1.0".to_string());

        let mut constraints = BTreeMap::new();
        constraints.insert("com.lib:nested".to_string(), "5.0".to_string());

        let mut lock = Lockfile::default();
        let result = resolve_inner(
            &deps,
            &cache_dir,
            &mut lock,
            &[],
            &[],
            &BTreeMap::new(),
            &constraints,
            &HashMap::new(),
            false,
        );
        let _ = std::fs::remove_dir_all(&cache_dir);
        result.expect("resolve_inner should succeed");

        let keys: Vec<String> = lock.dependencies.keys().cloned().collect();
        assert!(
            keys.contains(&"com.lib:nested:5.0".to_string()),
            "BOM constraint must upgrade non-explicit transitive; got {:?}", keys
        );
        assert!(
            !keys.contains(&"com.lib:nested:2.0".to_string()),
            "transitive version 2.0 must not win after BOM upgrade; got {:?}", keys
        );
    }

    // Guards resolver.rs:~637 — `to_string()` drops inner causes; `{:#}` walks the full anyhow chain ADR-021/023 stacked.
    #[test]
    fn anyhow_alt_display_unfolds_full_chain_but_plain_display_does_not() {
        use anyhow::{anyhow, Context};

        let err: anyhow::Error = Err::<(), _>(anyhow!("the document does not have a root node"))
            .context("parse POM body in parse_pom_dependencies_with_props")
            .context("resolve transitive for software.amazon.awssdk:sdk-core:2.44.4 (POM at /tmp/x.pom)")
            .unwrap_err();

        let plain = err.to_string();
        assert!(plain.contains("resolve transitive for"), "outer ctx missing: {}", plain);
        assert!(
            !plain.contains("the document does not have a root node"),
            "plain Display leaked leaf cause unexpectedly — anyhow changed behavior? got: {}",
            plain
        );
        assert!(
            !plain.contains("parse POM body"),
            "plain Display leaked inner ctx unexpectedly: {}",
            plain
        );

        let alt = format!("{:#}", err);
        assert!(alt.contains("resolve transitive for"), "outer ctx missing: {}", alt);
        assert!(alt.contains("parse POM body"), "inner ctx missing: {}", alt);
        assert!(
            alt.contains("the document does not have a root node"),
            "leaf cause missing: {}",
            alt
        );
    }

    // L3 (ADR-024) — stale corrupt cache files must not satisfy a downstream `.exists()` hit ("完整性 > 存在性").
    #[test]
    fn invalidate_corrupt_pom_unlinks_empty_file() {
        let dir = std::env::temp_dir().join(format!("ym_inv_empty_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let pom = dir.join("a.pom");
        std::fs::write(&pom, b"").unwrap();
        invalidate_corrupt_pom(&pom);
        let removed = !pom.exists();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(removed, "empty POM must be unlinked");
    }

    #[test]
    fn invalidate_corrupt_pom_unlinks_truncated_xml() {
        let dir = std::env::temp_dir().join(format!("ym_inv_trunc_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let pom = dir.join("a.pom");
        std::fs::write(&pom, b"<?xml version=\"1.0\"?>\n<project>incomplete").unwrap();
        invalidate_corrupt_pom(&pom);
        let removed = !pom.exists();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(removed, "truncated POM must be unlinked");
    }

    #[test]
    fn invalidate_corrupt_pom_keeps_valid_pom() {
        let dir = std::env::temp_dir().join(format!("ym_inv_valid_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let pom = dir.join("a.pom");
        let body = br#"<?xml version="1.0"?>
<project>
  <modelVersion>4.0.0</modelVersion>
  <groupId>x</groupId>
  <artifactId>y</artifactId>
  <version>1.0</version>
</project>"#;
        std::fs::write(&pom, body).unwrap();
        invalidate_corrupt_pom(&pom);
        let kept = pom.exists();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(kept, "valid POM must not be unlinked");
    }

    #[test]
    fn invalidate_corrupt_pom_handles_missing_file_gracefully() {
        let dir = std::env::temp_dir().join(format!("ym_inv_miss_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let pom = dir.join("nonexistent.pom");
        invalidate_corrupt_pom(&pom); // must not panic
        let _ = std::fs::remove_dir_all(&dir);
    }

    // L1 (ADR-024) — legacy fixed `.part` tempfile let two BFS workers truncate each other's in-flight bytes; NamedTempFile::new_in must hand out a unique path per call.
    #[test]
    fn namedtempfile_new_in_returns_unique_paths_under_concurrency() {
        use std::sync::{Arc, Mutex};
        use std::thread;

        let dir = std::env::temp_dir().join(format!("ym_tmpf_unique_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        let paths: Arc<Mutex<Vec<PathBuf>>> = Arc::new(Mutex::new(Vec::new()));
        let mut handles = vec![];
        for _ in 0..8 {
            let dir = dir.clone();
            let paths = paths.clone();
            handles.push(thread::spawn(move || {
                // Hold the tempfile alive across the join barrier so its
                // path stays allocated (mirrors what download_file does
                // between create and persist).
                let t = NamedTempFile::new_in(&dir).expect("NamedTempFile::new_in");
                paths.lock().unwrap().push(t.path().to_path_buf());
                std::thread::sleep(std::time::Duration::from_millis(20));
                drop(t);
            }));
        }
        for h in handles { h.join().unwrap(); }

        let paths = paths.lock().unwrap().clone();
        let _ = std::fs::remove_dir_all(&dir);

        let unique: std::collections::HashSet<&PathBuf> = paths.iter().collect();
        assert_eq!(
            unique.len(), paths.len(),
            "concurrent NamedTempFile::new_in paths collided — race fix broken: {:?}",
            paths
        );
    }

    // L1 integration (ADR-024) — 8 concurrent download_file calls on same URL+path must converge on a single valid POM without leaving `.part.*` orphans.
    #[test]
    fn download_file_concurrent_same_path_no_corruption() {
        use std::sync::Arc;
        use std::thread;

        let pom_body: &[u8] = br#"<?xml version="1.0"?>
<project>
  <modelVersion>4.0.0</modelVersion>
  <groupId>com.example</groupId>
  <artifactId>race</artifactId>
  <version>1.0</version>
</project>"#;

        let mut server = mockito::Server::new();
        let mock = server.mock("GET", "/com/example/race/1.0/race-1.0.pom")
            .with_status(200)
            .with_header("content-type", "application/xml")
            .with_body(pom_body)
            .expect_at_least(1)
            .create();

        let dir = std::env::temp_dir().join(format!("ym_dl_race_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("race-1.0.pom");
        let url = format!("{}/com/example/race/1.0/race-1.0.pom", server.url());

        let client = Arc::new(reqwest::blocking::Client::new());
        let mut handles = vec![];
        for _ in 0..8 {
            let client = client.clone();
            let url = url.clone();
            let path = path.clone();
            handles.push(thread::spawn(move || {
                download_file(&client, &url, &path, None, None)
            }));
        }
        let mut errors = vec![];
        for h in handles {
            if let Err(e) = h.join().unwrap() {
                errors.push(e.to_string());
            }
        }

        let final_body = std::fs::read(&path).ok();
        let leftovers: Vec<PathBuf> = std::fs::read_dir(&dir).unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p != &path)
            .collect();
        let _ = std::fs::remove_dir_all(&dir);
        mock.assert();

        assert!(errors.is_empty(), "no concurrent download must error, got: {:?}", errors);
        assert_eq!(final_body.as_deref(), Some(pom_body), "final POM must equal the source — race must not corrupt content");
        assert!(leftovers.is_empty(), "no `.part.*` orphans after races, found: {:?}", leftovers);
    }

    // L4 integration (ADR-024) — when a server advertises Content-Length but truncates the body, download_file must NOT persist the partial .pom. Uses a raw TcpListener because mockito's hyper backend refuses to ship a mismatched response (asserts server-side). hyper *client* will error first, but the contract under test is "no partial .pom lands in cache" — that contract is what defends us against future protocol-violating proxies.
    #[test]
    fn download_file_does_not_persist_pom_when_server_truncates() {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        use std::thread;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();

        let server_thread = thread::spawn(move || {
            // Serve up to 3 attempts so download_file's retry loop can drain.
            for _ in 0..3 {
                let Ok((mut stream, _)) = listener.accept() else { return };
                let mut buf = [0u8; 4096];
                let _ = stream.read(&mut buf);
                let response: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\nContent-Type: application/xml\r\n\r\n<short/>";
                let _ = stream.write_all(response);
                let _ = stream.shutdown(std::net::Shutdown::Both);
            }
        });

        let dir = std::env::temp_dir().join(format!("ym_dl_truncate_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("trunc.pom");
        let url = format!("http://127.0.0.1:{}/trunc.pom", port);

        let client = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(5))
            .build()
            .unwrap();
        let result = download_file(&client, &url, &path, None, None);

        let pom_exists = path.exists();
        let part_orphans: Vec<PathBuf> = std::fs::read_dir(&dir).unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p != &path)
            .collect();
        let _ = std::fs::remove_dir_all(&dir);
        let _ = server_thread.join();

        assert!(result.is_err(), "download must fail when server advertises Content-Length 100 but ships 8 bytes");
        assert!(!pom_exists, "no .pom must land in cache when length mismatches — got persisted file");
        assert!(part_orphans.is_empty(), "no tempfile orphans must leak, found: {:?}", part_orphans);
    }

    // L3 integration (ADR-024) — resolve_transitive_cached must detect a pre-existing corrupt POM in cache, unlink it, refetch via the registry, and return valid deps.
    #[test]
    fn resolve_transitive_cached_recovers_from_corrupt_cache() {
        let pom_body: &[u8] = br#"<?xml version="1.0"?>
<project>
  <modelVersion>4.0.0</modelVersion>
  <groupId>com.example</groupId>
  <artifactId>self-heal</artifactId>
  <version>1.0</version>
  <dependencies>
    <dependency>
      <groupId>com.example</groupId>
      <artifactId>downstream</artifactId>
      <version>2.0</version>
    </dependency>
  </dependencies>
</project>"#;

        let mut server = mockito::Server::new();
        let mock = server.mock("GET", "/com/example/self-heal/1.0/self-heal-1.0.pom")
            .with_status(200)
            .with_body(pom_body)
            .expect_at_least(1)
            .create();

        let cache_dir = std::env::temp_dir().join(format!("ym_l3_self_heal_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&cache_dir);
        std::fs::create_dir_all(&cache_dir).unwrap();

        // Plant a corrupt POM exactly where resolve_transitive_cached will look.
        let coord = MavenCoord::parse("com.example:self-heal", "1.0").unwrap();
        let pom_path = coord.pom_path(&cache_dir);
        std::fs::create_dir_all(pom_path.parent().unwrap()).unwrap();
        std::fs::write(&pom_path, b"<truncated").unwrap();
        let corrupt_size = std::fs::metadata(&pom_path).unwrap().len();
        assert_eq!(corrupt_size, "<truncated".len() as u64, "test fixture must be a corrupt POM");

        let registries = vec![RegistryEntry {
            url: server.url(),
            scope: None,
            username: None,
            password: None,
        }];
        let client = reqwest::blocking::Client::new();

        let deps = resolve_transitive_cached(&client, &coord, &cache_dir, &registries, None)
            .expect("must self-heal corrupt cache and resolve deps");

        let final_body = std::fs::read(&pom_path).unwrap();
        let _ = std::fs::remove_dir_all(&cache_dir);

        mock.assert();
        assert_eq!(final_body, pom_body, "corrupt POM in cache must be replaced by registry body");
        assert_eq!(deps.len(), 1, "must return the single transitive dep, got: {:?}", deps);
        assert_eq!(deps[0].artifact_id, "downstream");
        assert_eq!(deps[0].version, "2.0");
    }

    // ───────────────────────── ADR-022: pom_failures high-water-mark pruning ─────────────────────────
    //
    // The BFS records every POM fetch/parse failure into `pom_failures`, then partitions them
    // against the latest-wins winner before fail-loud: a failure whose version is strictly BELOW
    // the resolved winner cannot enter the classpath, so it's noise (warn, don't block); a failure
    // at the winner version is fatal. These drive a mock registry (mockito, already in dev-deps),
    // mirroring `resolve_transitive_cached_recovers_from_corrupt_cache`.
    //
    // Layering note: BFS line ~585 skips any coord whose version is below the current high-water
    // mark, so a stale version only lands in `pom_failures` if it is fetched BEFORE the winner is
    // seen. The fixture therefore introduces shared:1.0 at depth 1 (direct dep of host) and
    // shared:2.0 at depth 2 (via mid), guaranteeing 1.0 is fetched (and 404s) before 2.0 wins.

    /// ADR-022 fix ②: a stale POM failure superseded by a higher winner is pruned to a warning,
    /// not propagated to fail-loud. The build succeeds and the winner is locked.
    #[test]
    fn resolve_inner_prunes_stale_pom_failure_superseded_by_winner() {
        let host_pom = r#"<?xml version="1.0"?>
<project>
  <modelVersion>4.0.0</modelVersion>
  <groupId>com.ymtest022</groupId>
  <artifactId>host</artifactId>
  <version>1.0</version>
  <dependencies>
    <dependency><groupId>com.ymtest022</groupId><artifactId>shared</artifactId><version>1.0</version></dependency>
    <dependency><groupId>com.ymtest022</groupId><artifactId>mid</artifactId><version>1.0</version></dependency>
  </dependencies>
</project>"#;
        let mid_pom = r#"<?xml version="1.0"?>
<project>
  <modelVersion>4.0.0</modelVersion>
  <groupId>com.ymtest022</groupId>
  <artifactId>mid</artifactId>
  <version>1.0</version>
  <dependencies>
    <dependency><groupId>com.ymtest022</groupId><artifactId>shared</artifactId><version>2.0</version></dependency>
  </dependencies>
</project>"#;
        let shared2_pom = r#"<?xml version="1.0"?>
<project>
  <modelVersion>4.0.0</modelVersion>
  <groupId>com.ymtest022</groupId>
  <artifactId>shared</artifactId>
  <version>2.0</version>
</project>"#;

        let mut server = mockito::Server::new();
        let _h = server.mock("GET", "/com/ymtest022/host/1.0/host-1.0.pom")
            .with_status(200).with_body(host_pom).create();
        let _m = server.mock("GET", "/com/ymtest022/mid/1.0/mid-1.0.pom")
            .with_status(200).with_body(mid_pom).create();
        let _s2 = server.mock("GET", "/com/ymtest022/shared/2.0/shared-2.0.pom")
            .with_status(200).with_body(shared2_pom).create();
        // The stale loser 404s — must be pruned, not fatal.
        let _s1 = server.mock("GET", "/com/ymtest022/shared/1.0/shared-1.0.pom")
            .with_status(404).create();

        let cache_dir = std::env::temp_dir().join(format!("ym_adr022_stale_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&cache_dir);
        std::fs::create_dir_all(&cache_dir).unwrap();

        let mut deps = BTreeMap::new();
        deps.insert("com.ymtest022:host".to_string(), "1.0".to_string());
        let registries = vec![RegistryEntry { url: server.url(), scope: None, username: None, password: None }];

        let mut lock = Lockfile::default();
        let result = resolve_inner(
            &deps, &cache_dir, &mut lock, &registries, &[],
            &BTreeMap::new(), &BTreeMap::new(), &HashMap::new(), false,
        );
        let keys: Vec<String> = lock.dependencies.keys().cloned().collect();
        let _ = std::fs::remove_dir_all(&cache_dir);

        result.expect("stale POM failure (1.0) superseded by winner (2.0) must be pruned, not fatal");
        assert!(keys.iter().any(|k| k == "com.ymtest022:shared:2.0"),
            "winner shared:2.0 must be locked; got {:?}", keys);
        assert!(!keys.iter().any(|k| k == "com.ymtest022:shared:1.0"),
            "stale shared:1.0 must NOT be locked; got {:?}", keys);
    }

    /// ADR-022 / ADR-021: a POM failure AT the winner version (no higher version supersedes it)
    /// is fatal — resolve_inner fails loud, naming the missing GAV.
    #[test]
    fn resolve_inner_fatal_when_winner_pom_fetch_fails() {
        let mut server = mockito::Server::new();
        let _m = server.mock("GET", "/com/ymtest022/missing/1.0/missing-1.0.pom")
            .with_status(404).create();

        let cache_dir = std::env::temp_dir().join(format!("ym_adr022_fatal_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&cache_dir);
        std::fs::create_dir_all(&cache_dir).unwrap();

        let mut deps = BTreeMap::new();
        deps.insert("com.ymtest022:missing".to_string(), "1.0".to_string());
        let registries = vec![RegistryEntry { url: server.url(), scope: None, username: None, password: None }];

        let mut lock = Lockfile::default();
        let result = resolve_inner(
            &deps, &cache_dir, &mut lock, &registries, &[],
            &BTreeMap::new(), &BTreeMap::new(), &HashMap::new(), false,
        );
        let _ = std::fs::remove_dir_all(&cache_dir);

        let err = result.expect_err("winner-version POM fetch failure must be fatal (fail-loud)");
        let msg = err.to_string();
        assert!(msg.contains("missing"), "fail-loud error must name the missing artifact, got:\n{}", msg);
        assert!(msg.contains("resolution failed") || msg.contains("POM fetch"),
            "error must indicate dependency resolution failure, got:\n{}", msg);
    }
}
