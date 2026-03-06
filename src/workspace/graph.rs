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
    /// Build the workspace graph by scanning all ym.json files
    pub fn build(workspace_root: &Path) -> Result<Self> {
        let root_config = config::load_config(&workspace_root.join(config::CONFIG_FILE))?;
        let patterns = root_config
            .workspaces
            .as_ref()
            .cloned()
            .unwrap_or_default();

        let mut graph = DiGraph::new();
        let mut name_to_index = HashMap::new();
        let mut packages = Vec::new();

        // Scan workspace patterns for ym.json files
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
