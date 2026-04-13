pub mod schema;

use anyhow::{Context, Result};
use schema::{ResolvedCache, YmConfig};
use std::path::{Path, PathBuf};

pub const CONFIG_FILE: &str = "ym.json";
pub const CACHE_DIR: &str = ".ym";
pub const OUTPUT_DIR: &str = "out";
pub const CLASSES_DIR: &str = "classes";
pub const TEST_CLASSES_DIR: &str = "test-classes";
pub const SOURCE_DIR: &str = "src";
pub const RESOLVED_FILE: &str = "resolved.json";
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

/// Load the resolved dependency cache from .ym/resolved.json
pub fn load_resolved_cache(project: &Path) -> Result<ResolvedCache> {
    let path = resolved_cache_path(project);
    if !path.exists() {
        return Ok(ResolvedCache::default());
    }
    let content = std::fs::read_to_string(&path)?;
    let cache: ResolvedCache = serde_json::from_str(&content)?;
    Ok(cache)
}

/// Load resolved cache, invalidating if config has changed.
/// Returns empty cache if dependency-relevant fields (dependencies, resolutions,
/// exclusions, registries) have changed since last resolve.
pub fn load_resolved_cache_checked(project: &Path, cfg: &YmConfig) -> Result<ResolvedCache> {
    let mut cache = load_resolved_cache(project)?;
    let current_hash = cfg.dependency_fingerprint();
    if cache.config_hash.as_deref() != Some(&current_hash) {
        // Config changed — invalidate
        cache.dependencies.clear();
        cache.config_hash = Some(current_hash);
    }
    Ok(cache)
}

/// Save the resolved dependency cache to .ym/resolved.json
/// Skips writing if the content is unchanged (preserves mtime for fingerprinting).
pub fn save_resolved_cache(project: &Path, cache: &ResolvedCache) -> Result<()> {
    // One call to `cache_dir` (via `resolved_cache_path`) — the enclosing
    // directory is derived from the file path for symmetry with `load`.
    let path = resolved_cache_path(project);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let content = serde_json::to_string_pretty(cache)? + "\n";
    // Only write if content has changed, to preserve mtime for build fingerprinting
    if let Ok(existing) = std::fs::read_to_string(&path) {
        if existing == content {
            return Ok(());
        }
    }
    // Atomic write: tmp + rename to avoid corruption on interrupt
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

pub fn resolved_cache_path(project: &Path) -> PathBuf {
    cache_dir(project).join(RESOLVED_FILE)
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
