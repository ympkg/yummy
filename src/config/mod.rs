pub mod schema;

use anyhow::{Context, Result};
use schema::{Lockfile, YmConfig};
use std::path::{Path, PathBuf};

pub const CONFIG_FILE: &str = "ym.json";
pub const LOCKFILE_NAME: &str = "ym-lock.json";
pub const CACHE_DIR: &str = ".ym";
pub const OUTPUT_DIR: &str = "out";
pub const CLASSES_DIR: &str = "classes";
pub const TEST_CLASSES_DIR: &str = "test-classes";
pub const SOURCE_DIR: &str = "src";
pub const MAVEN_CACHE_DIR: &str = "maven";
pub const BUILD_CACHE_DIR: &str = "build-cache";
pub const POM_CACHE_DIR: &str = "pom-cache";

/// Search upward from `start` for an ym.json file
pub fn find_config(start: &Path) -> Option<PathBuf> {
    let mut dir = start.to_path_buf();
    loop {
        let config = dir.join(CONFIG_FILE);
        if config.exists() {
            return Some(config);
        }
        if !dir.pop() {
            return None;
        }
    }
}

/// Search upward for the workspace root (an ym.json with "workspaces" field)
pub fn find_workspace_root(start: &Path) -> Option<PathBuf> {
    let mut dir = start.to_path_buf();
    let mut last_with_workspaces = None;
    loop {
        let config_path = dir.join(CONFIG_FILE);
        if config_path.exists() {
            if let Ok(config) = load_config(&config_path) {
                if config.workspaces.is_some() {
                    last_with_workspaces = Some(dir.clone());
                }
            }
        }
        if !dir.pop() {
            break;
        }
    }
    last_with_workspaces
}

pub fn load_config(path: &Path) -> Result<YmConfig> {
    let content =
        std::fs::read_to_string(path).with_context(|| format!("Failed to read {}", path.display()))?;
    let config: YmConfig =
        serde_json::from_str(&content).with_context(|| format!("Failed to parse {}", path.display()))?;
    Ok(config)
}

pub fn save_config(path: &Path, config: &YmConfig) -> Result<()> {
    let content = serde_json::to_string_pretty(config)? + "\n";
    std::fs::write(path, content)?;
    Ok(())
}

/// Load ym-lock.json from project root (workspace root, not .ym/).
/// Returns Default if file doesn't exist (caller is expected to populate + save).
/// Legacy `.ym/resolved.json` is intentionally ignored — see ADR-016.
pub fn load_lockfile(project: &Path) -> Result<Lockfile> {
    let path = lockfile_path(project);
    if !path.exists() {
        return Ok(Lockfile::default());
    }
    let content = std::fs::read_to_string(&path)?;
    let lock: Lockfile = serde_json::from_str(&content)?;
    Ok(lock)
}

/// Load lockfile, invalidating dependencies if config has changed.
/// Returns empty deps + new hash when ym.json's dependency-relevant fields changed.
///
/// When `project` is a workspace child, the lockfile is owned by the workspace
/// root and indexed by the root cfg's fingerprint (which sums every module's
/// deps). A child cfg only knows its own slice, so its fingerprint is always
/// different — comparing them would falsely invalidate the lockfile on every
/// per-module resolve. We therefore skip the freshness check for workspace
/// children and trust the on-disk lockfile; the workspace root build path is
/// the sole writer of `config_hash`. See ADR-016 and the partnered guard in
/// [`save_lockfile`].
pub fn load_lockfile_checked(project: &Path, cfg: &YmConfig) -> Result<Lockfile> {
    let mut lock = load_lockfile(project)?;
    if is_workspace_child(project) {
        return Ok(lock);
    }
    let current_hash = cfg.dependency_fingerprint();
    if lock.config_hash != current_hash {
        lock.dependencies.clear();
        lock.config_hash = current_hash;
    }
    Ok(lock)
}

/// True when `project` lives under a workspace root that is a different
/// directory. Single projects and the workspace root itself return false.
fn is_workspace_child(project: &Path) -> bool {
    match find_workspace_root(project) {
        Some(ws_root) => ws_root != project,
        None => false,
    }
}

/// Save lockfile to project root (workspace root) as ym-lock.json.
/// Stamps `ymc_version` and `generated_at` on every write.
/// Atomic via tmp+rename. Skips writing if content unchanged (preserves mtime).
///
/// **Workspace-child guard**: if `project` is a workspace child, this is a
/// no-op. The lockfile is workspace-root-scoped — its `config_hash` is the
/// root cfg's fingerprint and its `dependencies` is the union of every
/// module's resolution. A per-child resolve only knows its own slice and
/// would otherwise corrupt both fields (last-writer-wins). Callers that need
/// to refresh the lockfile must run from the workspace root (e.g. `ym install`
/// or `ymc build` with no module target). See ADR-016.
pub fn save_lockfile(project: &Path, lock: &Lockfile) -> Result<()> {
    if is_workspace_child(project) {
        return Ok(());
    }
    let mut lock = lock.clone();
    lock.ymc_version = env!("CARGO_PKG_VERSION").to_string();
    lock.generated_at = chrono::Utc::now().to_rfc3339();
    if lock.version_winner_strategy.is_empty() {
        lock.version_winner_strategy = "latest-wins".to_string();
    }
    if lock.lockfile_version == 0 {
        lock.lockfile_version = 1;
    }

    let path = lockfile_path(project);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let content = serde_json::to_string_pretty(&lock)? + "\n";
    // Skip if existing lock body matches modulo timestamp (avoid pointless mtime churn)
    if let Ok(existing) = std::fs::read_to_string(&path) {
        if let Ok(existing_lock) = serde_json::from_str::<Lockfile>(&existing) {
            if existing_lock.config_hash == lock.config_hash
                && existing_lock.dependencies == lock.dependencies
                && existing_lock.lockfile_version == lock.lockfile_version
                && existing_lock.version_winner_strategy == lock.version_winner_strategy
            {
                return Ok(());
            }
        }
    }
    let tmp_path = path.with_extension("json.tmp");
    std::fs::write(&tmp_path, &content)?;
    std::fs::rename(&tmp_path, &path)?;
    Ok(())
}

/// Load ym.json from the current directory or any parent
pub fn load_or_find_config() -> Result<(PathBuf, YmConfig)> {
    let cwd = std::env::current_dir()?;
    let config_path = find_config(&cwd).context("No ym.json found. Run 'ym init' to create one.")?;
    let config = load_config(&config_path)?;
    Ok((config_path, config))
}

pub fn project_dir(config_path: &Path) -> PathBuf {
    config_path.parent().unwrap().to_path_buf()
}

pub fn output_classes_dir(project: &Path) -> PathBuf {
    project.join(OUTPUT_DIR).join(CLASSES_DIR)
}

pub fn output_test_classes_dir(project: &Path) -> PathBuf {
    project.join(OUTPUT_DIR).join(TEST_CLASSES_DIR)
}

/// Get the source directory: prefers `src/main/java` (Maven convention),
/// falls back to `src/`.
pub fn source_dir(project: &Path) -> PathBuf {
    let maven_src = project.join("src").join("main").join("java");
    if maven_src.exists() {
        maven_src
    } else {
        project.join(SOURCE_DIR)
    }
}

/// Get the source directory respecting ym.json `sourceDir` override.
pub fn source_dir_for(project: &Path, cfg: &YmConfig) -> PathBuf {
    if let Some(ref custom) = cfg.source_dir {
        project.join(custom)
    } else {
        source_dir(project)
    }
}

/// Get the test source directory: prefers `src/test/java` (Maven convention),
/// falls back to `test/`.
pub fn test_dir(project: &Path) -> PathBuf {
    let maven_test = project.join("src").join("test").join("java");
    if maven_test.exists() {
        maven_test
    } else {
        project.join("test")
    }
}

/// Get the test directory respecting ym.json `testDir` override.
pub fn test_dir_for(project: &Path, cfg: &YmConfig) -> PathBuf {
    if let Some(ref custom) = cfg.test_dir {
        project.join(custom)
    } else {
        test_dir(project)
    }
}

/// Get the .ym cache directory (at workspace root or project root)
pub fn cache_dir(project: &Path) -> PathBuf {
    let root = find_workspace_root(project).unwrap_or_else(|| project.to_path_buf());
    root.join(CACHE_DIR)
}

pub fn maven_cache_dir() -> PathBuf {
    dirs::home_dir()
        .expect("Cannot determine home directory")
        .join(CACHE_DIR)
        .join(MAVEN_CACHE_DIR)
}

pub fn pom_cache_dir() -> PathBuf {
    dirs::home_dir()
        .expect("Cannot determine home directory")
        .join(CACHE_DIR)
        .join(POM_CACHE_DIR)
}

/// Lockfile lives at the workspace root (sibling of ym.json), not in `.ym/`.
pub fn lockfile_path(project: &Path) -> PathBuf {
    let root = find_workspace_root(project).unwrap_or_else(|| project.to_path_buf());
    root.join(LOCKFILE_NAME)
}

/// Missing path returns 0 rather than erroring so callers (clean, doctor)
/// can skip existence pre-checks.
pub fn dir_size(path: &Path) -> u64 {
    walkdir::WalkDir::new(path)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .filter_map(|e| e.metadata().ok())
        .map(|m| m.len())
        .sum()
}

pub fn format_size(bytes: u64) -> String {
    if bytes >= 1_073_741_824 {
        format!("{:.1} GB", bytes as f64 / 1_073_741_824.0)
    } else if bytes >= 1_048_576 {
        format!("{:.1} MB", bytes as f64 / 1_048_576.0)
    } else if bytes >= 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{} B", bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use schema::ResolvedDependency;
    use std::collections::BTreeMap;
    use std::fs;

    fn write_workspace(root: &Path) {
        fs::write(
            root.join(CONFIG_FILE),
            r#"{"name":"root","groupId":"com.example","workspaces":["child"],"dependencies":{"@google/guava":"33.4.0"},"scopeMapping":{"@google/guava":"com.google.guava:guava"}}"#,
        ).unwrap();
        let child = root.join("child");
        fs::create_dir_all(&child).unwrap();
        fs::write(
            child.join(CONFIG_FILE),
            r#"{"name":"child","groupId":"com.example","dependencies":{"@google/guava":{"workspace":true}}}"#,
        ).unwrap();
    }

    fn make_lock(hash: &str, deps: &[&str]) -> Lockfile {
        let mut lock = Lockfile::default();
        lock.config_hash = hash.to_string();
        lock.lockfile_version = 1;
        lock.version_winner_strategy = "latest-wins".to_string();
        let mut map = BTreeMap::new();
        for gav in deps {
            map.insert(gav.to_string(), ResolvedDependency::default());
        }
        lock.dependencies = map;
        lock
    }

    #[test]
    fn save_lockfile_is_noop_for_workspace_child() {
        // A per-module resolve from a workspace child must not overwrite the
        // workspace lockfile — otherwise child cfg's fingerprint (only its own
        // deps) would replace the root cfg's fingerprint (the full set).
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write_workspace(root);

        // Seed a "good" lockfile written from the workspace root.
        let good = make_lock("root_hash_aaaa", &["com.google.guava:guava:33.4.0"]);
        save_lockfile(root, &good).unwrap();
        let before = fs::read_to_string(lockfile_path(root)).unwrap();

        // A child write tries to clobber with a wrong hash + truncated deps.
        let child = root.join("child");
        let bad = make_lock("child_hash_xxxx", &[]);
        save_lockfile(&child, &bad).unwrap();

        let after = fs::read_to_string(lockfile_path(&child)).unwrap();
        assert_eq!(
            before, after,
            "workspace-child save_lockfile must be a no-op; lockfile changed"
        );
    }

    #[test]
    fn load_lockfile_checked_keeps_deps_for_workspace_child() {
        // Comparing the lockfile's root-fingerprint against a child cfg's own
        // fingerprint would always mismatch and falsely clear deps. The guard
        // makes load_lockfile_checked behave like load_lockfile for children.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write_workspace(root);

        let good = make_lock("root_hash_aaaa", &["com.google.guava:guava:33.4.0"]);
        save_lockfile(root, &good).unwrap();

        let child_path = root.join("child");
        let child_cfg = load_config(&child_path.join(CONFIG_FILE)).unwrap();
        let loaded = load_lockfile_checked(&child_path, &child_cfg).unwrap();
        assert_eq!(loaded.config_hash, "root_hash_aaaa");
        assert_eq!(loaded.dependencies.len(), 1);
    }

    #[test]
    fn save_lockfile_still_writes_for_workspace_root() {
        // Sanity check the happy path: writes from the workspace root itself
        // (not a child) must still go through.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write_workspace(root);

        let lock = make_lock("root_hash_bbbb", &["com.google.guava:guava:33.4.0"]);
        save_lockfile(root, &lock).unwrap();

        let written: Lockfile = serde_json::from_str(
            &fs::read_to_string(lockfile_path(root)).unwrap(),
        ).unwrap();
        assert_eq!(written.config_hash, "root_hash_bbbb");
        assert_eq!(written.dependencies.len(), 1);
    }
}
