pub mod schema;

use anyhow::{Context, Result};
use schema::{LockFile, YmConfig};
use std::path::{Path, PathBuf};

pub const CONFIG_FILE: &str = "package.json";
pub const LOCK_FILE: &str = "package-lock.json";
pub const CACHE_DIR: &str = ".ym";
pub const OUTPUT_DIR: &str = "out";
pub const CLASSES_DIR: &str = "classes";
pub const SOURCE_DIR: &str = "src";

/// Search upward from `start` for a package.json file
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

/// Search upward for the workspace root (a package.json with "workspaces" field)
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
    let content = serde_json::to_string_pretty(config)?;
    std::fs::write(path, content + "\n")?;
    Ok(())
}

pub fn load_lock(path: &Path) -> Result<LockFile> {
    if !path.exists() {
        return Ok(LockFile::default());
    }
    let content = std::fs::read_to_string(path)?;
    let lock: LockFile = serde_json::from_str(&content)?;
    Ok(lock)
}

pub fn save_lock(path: &Path, lock: &LockFile) -> Result<()> {
    let content = serde_json::to_string_pretty(lock)?;
    std::fs::write(path, content + "\n")?;
    Ok(())
}

/// Load package.json from the current directory or any parent
pub fn load_or_find_config() -> Result<(PathBuf, YmConfig)> {
    let cwd = std::env::current_dir()?;
    let config_path = find_config(&cwd).context("No package.json found. Run 'ym init' to create one.")?;
    let config = load_config(&config_path)?;
    Ok((config_path, config))
}

pub fn project_dir(config_path: &Path) -> PathBuf {
    config_path.parent().unwrap().to_path_buf()
}

pub fn output_classes_dir(project: &Path) -> PathBuf {
    project.join(OUTPUT_DIR).join(CLASSES_DIR)
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

/// Get the source directory respecting package.json `sourceDir` override.
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

/// Get the test directory respecting package.json `testDir` override.
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

pub fn maven_cache_dir(project: &Path) -> PathBuf {
    cache_dir(project).join("cache").join("maven")
}
