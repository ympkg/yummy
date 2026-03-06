use anyhow::{bail, Result};
use petgraph::graph::{DiGraph, NodeIndex};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};

use crate::config;
use crate::config::schema::YmConfig;

/// A node in the workspace dependency graph
#[derive(Debug, Clone)]
pub struct PackageNode {
    pub name: String,
    pub path: PathBuf,
    pub config: YmConfig,
}

/// Workspace dependency graph
pub struct WorkspaceGraph {
    pub graph: DiGraph<PackageNode, ()>,
    pub name_to_index: HashMap<String, NodeIndex>,
}

impl WorkspaceGraph {
    /// Build the workspace graph by scanning all package.json files.
    /// Uses a cached graph when available and all package.json mtimes match.
    pub fn build(workspace_root: &Path) -> Result<Self> {
        let cache_dir = workspace_root.join(config::CACHE_DIR);

        // Try loading from cache first
        if let Some(cached) = super::cache::GraphCache::load(&cache_dir) {
            if let Ok(ws) = Self::from_cache(&cached) {
                return Ok(ws);
            }
        }

        let ws = Self::build_fresh(workspace_root)?;

        // Save to cache for next time
        let cache_data: Vec<(String, PathBuf, Vec<String>)> = ws
            .name_to_index
            .keys()
            .filter_map(|name| {
                let pkg = ws.get_package(name)?;
                let ws_deps = pkg.config.workspace_dependencies.clone().unwrap_or_default();
                Some((name.clone(), pkg.path.clone(), ws_deps))
            })
            .collect();
        let graph_cache = super::cache::GraphCache::build_from_workspace(workspace_root, &cache_data);
        let _ = graph_cache.save(&cache_dir);

        Ok(ws)
    }

    /// Build the graph fresh from disk (no cache).
    fn build_fresh(workspace_root: &Path) -> Result<Self> {
        let root_config = config::load_config(&workspace_root.join(config::CONFIG_FILE))?;
        let patterns = root_config
            .workspaces
            .as_ref()
            .cloned()
            .unwrap_or_default();

        let mut graph = DiGraph::new();
        let mut name_to_index = HashMap::new();
        let mut packages = Vec::new();

        // Scan workspace patterns for package.json files
        for pattern in &patterns {
            let full_pattern = workspace_root.join(pattern).join(config::CONFIG_FILE);
            let pattern_str = full_pattern.to_string_lossy().to_string();

            for entry in glob::glob(&pattern_str).unwrap_or_else(|_| glob::glob("").unwrap()) {
                if let Ok(config_path) = entry {
                    if let Ok(cfg) = config::load_config(&config_path) {
                        let pkg_dir = config_path.parent().unwrap().to_path_buf();
                        let node = PackageNode {
                            name: cfg.name.clone(),
                            path: pkg_dir,
                            config: cfg,
                        };
                        packages.push(node);
                    }
                }
            }
        }

        // Add all packages as nodes
        for pkg in &packages {
            let idx = graph.add_node(pkg.clone());
            name_to_index.insert(pkg.name.clone(), idx);
        }

        // Add edges for workspace dependencies
        for pkg in &packages {
            if let Some(ref ws_deps) = pkg.config.workspace_dependencies {
                let from = name_to_index[&pkg.name];
                for dep_name in ws_deps {
                    if let Some(&to) = name_to_index.get(dep_name) {
                        graph.add_edge(from, to, ());
                    }
                }
            }
        }

        Ok(WorkspaceGraph {
            graph,
            name_to_index,
        })
    }

    /// Reconstruct the graph from a validated cache.
    fn from_cache(cached: &super::cache::GraphCache) -> Result<Self> {
        let mut graph = DiGraph::new();
        let mut name_to_index = HashMap::new();

        for cpkg in &cached.packages {
            let config_path = PathBuf::from(&cpkg.path).join(config::CONFIG_FILE);
            let cfg = config::load_config(&config_path)?;
            let node = PackageNode {
                name: cpkg.name.clone(),
                path: PathBuf::from(&cpkg.path),
                config: cfg,
            };
            let idx = graph.add_node(node);
            name_to_index.insert(cpkg.name.clone(), idx);
        }

        for cpkg in &cached.packages {
            if let Some(&from) = name_to_index.get(&cpkg.name) {
                for dep_name in &cpkg.workspace_dependencies {
                    if let Some(&to) = name_to_index.get(dep_name) {
                        graph.add_edge(from, to, ());
                    }
                }
            }
        }

        Ok(WorkspaceGraph {
            graph,
            name_to_index,
        })
    }

    /// Get the transitive dependency closure for a target package.
    /// Returns package names in topological order (dependencies first).
    pub fn transitive_closure(&self, target: &str) -> Result<Vec<String>> {
        let start = match self.name_to_index.get(target) {
            Some(&idx) => idx,
            None => bail!("Package '{}' not found in workspace", target),
        };

        let mut visited = HashSet::new();
        let mut queue = VecDeque::new();
        queue.push_back(start);
        visited.insert(start);

        while let Some(node) = queue.pop_front() {
            for neighbor in self.graph.neighbors(node) {
                if visited.insert(neighbor) {
                    queue.push_back(neighbor);
                }
            }
        }

        // Topological sort of the subgraph
        let mut result = Vec::new();
        let mut in_degree: HashMap<NodeIndex, usize> = HashMap::new();

        for &idx in &visited {
            let mut deg = 0;
            if let Some(ref ws_deps) = self.graph[idx].config.workspace_dependencies {
                for dep_name in ws_deps {
                    if let Some(&dep_idx) = self.name_to_index.get(dep_name) {
                        if visited.contains(&dep_idx) {
                            deg += 1;
                        }
                    }
                }
            }
            in_degree.insert(idx, deg);
        }

        let mut ready: VecDeque<NodeIndex> = in_degree
            .iter()
            .filter(|(_, deg)| **deg == 0)
            .map(|(idx, _)| *idx)
            .collect();

        while let Some(node) = ready.pop_front() {
            result.push(self.graph[node].name.clone());

            // Find nodes that depend on this one
            for &idx in &visited {
                if let Some(ref ws_deps) = self.graph[idx].config.workspace_dependencies {
                    if ws_deps.contains(&self.graph[node].name) {
                        if let Some(deg) = in_degree.get_mut(&idx) {
                            *deg -= 1;
                            if *deg == 0 {
                                ready.push_back(idx);
                            }
                        }
                    }
                }
            }
        }

        Ok(result)
    }

    /// Get the PackageNode for a given name
    pub fn get_package(&self, name: &str) -> Option<&PackageNode> {
        self.name_to_index
            .get(name)
            .map(|&idx| &self.graph[idx])
    }

    /// Get all package names
    pub fn all_packages(&self) -> Vec<String> {
        self.name_to_index.keys().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_graph_cache_roundtrip() {
        let tmpdir = std::env::temp_dir().join("ym-graph-test");
        let _ = std::fs::remove_dir_all(&tmpdir);
        std::fs::create_dir_all(&tmpdir).unwrap();

        // Create a minimal workspace root
        let root_config = r#"{"name":"root","workspaces":["packages/*"]}"#;
        std::fs::write(tmpdir.join("package.json"), root_config).unwrap();

        // Create a package
        let pkg_dir = tmpdir.join("packages").join("my-lib");
        std::fs::create_dir_all(&pkg_dir).unwrap();
        std::fs::write(pkg_dir.join("package.json"), r#"{"name":"my-lib"}"#).unwrap();

        // Build fresh
        let ws = WorkspaceGraph::build(&tmpdir).unwrap();
        assert_eq!(ws.all_packages().len(), 1);
        assert!(ws.get_package("my-lib").is_some());

        // Build again - should use cache
        let ws2 = WorkspaceGraph::build(&tmpdir).unwrap();
        assert_eq!(ws2.all_packages().len(), 1);
        assert!(ws2.get_package("my-lib").is_some());

        let _ = std::fs::remove_dir_all(&tmpdir);
    }

    #[test]
    fn test_graph_cache_invalidates_on_change() {
        let tmpdir = std::env::temp_dir().join("ym-graph-cache-inv-test");
        let _ = std::fs::remove_dir_all(&tmpdir);
        std::fs::create_dir_all(&tmpdir).unwrap();

        let root_config = r#"{"name":"root","workspaces":["packages/*"]}"#;
        std::fs::write(tmpdir.join("package.json"), root_config).unwrap();

        let pkg_dir = tmpdir.join("packages").join("lib-a");
        std::fs::create_dir_all(&pkg_dir).unwrap();
        std::fs::write(pkg_dir.join("package.json"), r#"{"name":"lib-a"}"#).unwrap();

        // Build and cache
        let ws = WorkspaceGraph::build(&tmpdir).unwrap();
        assert_eq!(ws.all_packages().len(), 1);

        // Add another package -> cache should be invalidated
        let pkg_dir_b = tmpdir.join("packages").join("lib-b");
        std::fs::create_dir_all(&pkg_dir_b).unwrap();
        std::fs::write(pkg_dir_b.join("package.json"), r#"{"name":"lib-b"}"#).unwrap();

        // Touch root to invalidate
        std::fs::write(tmpdir.join("package.json"), root_config).unwrap();

        let ws2 = WorkspaceGraph::build(&tmpdir).unwrap();
        assert_eq!(ws2.all_packages().len(), 2);

        let _ = std::fs::remove_dir_all(&tmpdir);
    }

    #[test]
    fn test_transitive_closure_ordering() {
        let tmpdir = std::env::temp_dir().join("ym-topo-test");
        let _ = std::fs::remove_dir_all(&tmpdir);
        std::fs::create_dir_all(&tmpdir).unwrap();

        std::fs::write(tmpdir.join("package.json"), r#"{"name":"root","workspaces":["packages/*"]}"#).unwrap();

        // A depends on B, B depends on C
        for (name, deps) in &[("lib-a", r#"["lib-b"]"#), ("lib-b", r#"["lib-c"]"#), ("lib-c", "null")] {
            let dir = tmpdir.join("packages").join(name);
            std::fs::create_dir_all(&dir).unwrap();
            let ws_deps = if *deps == "null" {
                "".to_string()
            } else {
                format!(r#","workspaceDependencies":{}"#, deps)
            };
            std::fs::write(dir.join("package.json"), format!(r#"{{"name":"{}"{}}}"#, name, ws_deps)).unwrap();
        }

        let ws = WorkspaceGraph::build(&tmpdir).unwrap();
        let closure = ws.transitive_closure("lib-a").unwrap();
        assert_eq!(closure.len(), 3);
        // C must come before B, B before A
        let pos_c = closure.iter().position(|n| n == "lib-c").unwrap();
        let pos_b = closure.iter().position(|n| n == "lib-b").unwrap();
        let pos_a = closure.iter().position(|n| n == "lib-a").unwrap();
        assert!(pos_c < pos_b);
        assert!(pos_b < pos_a);

        let _ = std::fs::remove_dir_all(&tmpdir);
    }
}
