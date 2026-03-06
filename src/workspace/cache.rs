use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

const GRAPH_CACHE_FILE: &str = "graph.json";

/// Cached workspace graph for fast loading.
/// Invalidated when any ym.json file changes.
#[derive(Debug, Serialize, Deserialize)]
pub struct GraphCache {
    /// Timestamp of cache creation
    pub created_at: u64,
    /// Map of ym.json path -> modification time (for invalidation)
    pub config_mtimes: HashMap<String, u64>,
    /// Cached package info
    pub packages: Vec<CachedPackage>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct CachedPackage {
    pub name: String,
    pub path: String,
    pub workspace_dependencies: Vec<String>,
}

impl GraphCache {
    pub fn load(cache_dir: &Path) -> Option<Self> {
        let path = cache_dir.join(GRAPH_CACHE_FILE);
        let content = std::fs::read_to_string(&path).ok()?;
        let cache: GraphCache = serde_json::from_str(&content).ok()?;

        // Validate: check if any ym.json has changed
        for (config_path, cached_mtime) in &cache.config_mtimes {
            let current_mtime = file_mtime(Path::new(config_path)).unwrap_or(0);
            if current_mtime != *cached_mtime {
                return None; // Cache invalidated
            }
        }

        Some(cache)
    }

    pub fn save(&self, cache_dir: &Path) -> Result<()> {
        std::fs::create_dir_all(cache_dir)?;
        let path = cache_dir.join(GRAPH_CACHE_FILE);
        let content = serde_json::to_string(self)?;
        std::fs::write(path, content)?;
        Ok(())
    }

    pub fn build_from_workspace(
        workspace_root: &Path,
        packages: &[(String, PathBuf, Vec<String>)],
    ) -> Self {
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs();

        let mut config_mtimes = HashMap::new();
        let mut cached_packages = Vec::new();

        for (name, path, ws_deps) in packages {
            let config_path = path.join(crate::config::CONFIG_FILE);
            let mtime = file_mtime(&config_path).unwrap_or(0);
            config_mtimes.insert(config_path.to_string_lossy().to_string(), mtime);

            cached_packages.push(CachedPackage {
                name: name.clone(),
                path: path.to_string_lossy().to_string(),
                workspace_dependencies: ws_deps.clone(),
            });
        }

        // Also track root ym.json
        let root_config = workspace_root.join(crate::config::CONFIG_FILE);
        let root_mtime = file_mtime(&root_config).unwrap_or(0);
        config_mtimes.insert(root_config.to_string_lossy().to_string(), root_mtime);

        GraphCache {
            created_at: now,
            config_mtimes,
            packages: cached_packages,
        }
    }
}

fn file_mtime(path: &Path) -> Option<u64> {
    std::fs::metadata(path)
        .ok()?
        .modified()
        .ok()?
        .duration_since(SystemTime::UNIX_EPOCH)
        .ok()
        .map(|d| d.as_secs())
}
