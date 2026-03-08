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
    /// Build the workspace graph by scanning all package.toml files.
    /// Uses a cached graph when available and all package.toml mtimes match.
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
                let ws_deps = pkg.config.workspace_module_deps();
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

        // Scan workspace patterns for package.toml files
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

        // Add all packages as nodes, validating name uniqueness
        for pkg in &packages {
            if let Some(&existing_idx) = name_to_index.get(&pkg.name) {
                let existing_node: &PackageNode = &graph[existing_idx];
                let existing_path = &existing_node.path;
                bail!(
                    "Duplicate module name '{}' found in:\n  - {}\n  - {}",
                    pkg.name,
                    existing_path.display(),
                    pkg.path.display()
                );
            }
            let idx = graph.add_node(pkg.clone());
            name_to_index.insert(pkg.name.clone(), idx);
        }

        // Add edges for workspace dependencies
        for pkg in &packages {
            let ws_deps = pkg.config.workspace_module_deps();
            let from = name_to_index[&pkg.name];
            for dep_name in &ws_deps {
                if let Some(&to) = name_to_index.get(dep_name) {
                    graph.add_edge(from, to, ());
                }
            }
        }

        // Cycle detection
        if let Err(cycle) = petgraph::algo::toposort(&graph, None) {
            let cycle_node = cycle.node_id();
            // Find cycle path using DFS
            let cycle_name = &graph[cycle_node].name;
            let mut path = Vec::new();
            let mut visited = HashSet::new();
            Self::find_cycle_path(&graph, cycle_node, cycle_node, &mut visited, &mut path);
            if path.is_empty() {
                bail!("Cycle detected involving module '{}'", cycle_name);
            } else {
                let path_str: Vec<&str> = path.iter().map(|idx| graph[*idx].name.as_str()).collect();
                bail!(
                    "Cycle detected: {} → {}",
                    path_str.join(" → "),
                    graph[cycle_node].name
                );
            }
        }

        Ok(WorkspaceGraph {
            graph,
            name_to_index,
        })
    }

    /// Find a cycle path using DFS from start node back to target.
    fn find_cycle_path(
        graph: &DiGraph<PackageNode, ()>,
        current: NodeIndex,
        target: NodeIndex,
        visited: &mut HashSet<NodeIndex>,
        path: &mut Vec<NodeIndex>,
    ) -> bool {
        path.push(current);
        visited.insert(current);

        for neighbor in graph.neighbors(current) {
            if neighbor == target && path.len() > 1 {
                return true;
            }
            if !visited.contains(&neighbor) {
                if Self::find_cycle_path(graph, neighbor, target, visited, path) {
                    return true;
                }
            }
        }

        path.pop();
        false
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
            let ws_deps = self.graph[idx].config.workspace_module_deps();
            let mut deg = 0;
            for dep_name in &ws_deps {
                if let Some(&dep_idx) = self.name_to_index.get(dep_name) {
                    if visited.contains(&dep_idx) {
                        deg += 1;
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

            for &idx in &visited {
                let ws_deps = self.graph[idx].config.workspace_module_deps();
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

    /// Get all package names in topological order (dependencies first).
    pub fn topological_order(&self) -> Vec<String> {
        match petgraph::algo::toposort(&self.graph, None) {
            Ok(sorted) => sorted.iter().map(|&idx| self.graph[idx].name.clone()).collect(),
            Err(_) => self.all_packages(), // fallback if cycle (shouldn't happen after build validation)
        }
    }

    /// Get packages grouped by topological levels.
    /// Level 0 = no workspace dependencies, level 1 = depends only on level 0, etc.
    /// Within each level, packages can safely run in parallel.
    pub fn topological_levels(&self) -> Vec<Vec<String>> {
        let topo = match petgraph::algo::toposort(&self.graph, None) {
            Ok(sorted) => sorted,
            Err(_) => return vec![self.all_packages()],
        };

        let mut level_of: HashMap<NodeIndex, usize> = HashMap::new();
        let mut max_level = 0usize;

        for &idx in &topo {
            let mut my_level = 0;
            for neighbor in self.graph.neighbors(idx) {
                if let Some(&dep_level) = level_of.get(&neighbor) {
                    my_level = my_level.max(dep_level + 1);
                }
            }
            level_of.insert(idx, my_level);
            max_level = max_level.max(my_level);
        }

        let mut levels: Vec<Vec<String>> = vec![Vec::new(); max_level + 1];
        for (&idx, &level) in &level_of {
            levels[level].push(self.graph[idx].name.clone());
        }
        levels
    }
}
