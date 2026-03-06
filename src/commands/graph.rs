use anyhow::Result;
use console::style;
use std::collections::HashSet;

use crate::config;

pub fn execute(target: Option<String>, dot: bool, reverse: bool, max_depth: usize) -> Result<()> {
    let (config_path, cfg) = config::load_or_find_config()?;
    let project = config::project_dir(&config_path);

    if cfg.workspaces.is_some() {
        return graph_workspace(&project, target.as_deref(), dot);
    }

    let lock_path = project.join(config::LOCK_FILE);
    let lock = config::load_lock(&lock_path)?;

    let direct = cfg.dependencies.as_ref().cloned().unwrap_or_default();

    if reverse {
        return print_reverse_graph(&lock);
    }

    if dot {
        print_dot_maven(&cfg.name, &direct, &lock, max_depth);
    } else {
        print_text_maven(&cfg.name, &direct, &lock, max_depth);
    }

    Ok(())
}

/// Show reverse dependency graph: for each package, show who depends on it.
fn print_reverse_graph(lock: &config::schema::LockFile) -> Result<()> {
    // Build reverse index: package -> list of dependents
    let mut reverse: std::collections::BTreeMap<String, Vec<String>> = std::collections::BTreeMap::new();

    for (parent_key, locked) in &lock.dependencies {
        if let Some(ref deps) = locked.dependencies {
            for dep_key in deps {
                reverse
                    .entry(dep_key.clone())
                    .or_default()
                    .push(parent_key.clone());
            }
        }
    }

    println!();
    println!("  {} (who depends on what)", style("Reverse dependency graph").bold());
    println!();

    for (pkg, dependents) in &reverse {
        let parts: Vec<&str> = pkg.split(':').collect();
        let display = if parts.len() == 3 {
            format!("{}:{}", parts[0], parts[1])
        } else {
            pkg.clone()
        };
        println!(
            "  {} <- {}",
            style(&display).cyan().bold(),
            dependents
                .iter()
                .map(|d| {
                    let p: Vec<&str> = d.split(':').collect();
                    if p.len() == 3 {
                        format!("{}:{}", p[0], p[1])
                    } else {
                        d.clone()
                    }
                })
                .collect::<Vec<_>>()
                .join(", ")
        );
    }

    // Show packages no one depends on (roots)
    let all_keys: HashSet<&String> = lock.dependencies.keys().collect();
    let depended_on: HashSet<&String> = reverse.keys().collect();
    let roots: Vec<&&String> = all_keys.difference(&depended_on).collect();
    if !roots.is_empty() {
        println!();
        println!("  {} (no one depends on these):", style("Root packages").dim());
        for r in roots {
            println!("    {}", style(r).dim());
        }
    }

    println!();
    Ok(())
}

fn graph_workspace(root: &std::path::Path, target: Option<&str>, dot: bool) -> Result<()> {
    let ws = crate::workspace::graph::WorkspaceGraph::build(root)?;

    let packages = if let Some(target) = target {
        ws.transitive_closure(target)?
    } else {
        ws.all_packages()
    };

    if dot {
        println!("digraph workspace {{");
        println!("  rankdir=LR;");
        println!("  node [shape=box, style=filled, fillcolor=lightyellow];");
        for pkg_name in &packages {
            let pkg = ws.get_package(pkg_name).unwrap();
            let deps = pkg
                .config
                .workspace_dependencies
                .as_ref()
                .cloned()
                .unwrap_or_default();
            for dep in &deps {
                println!("  \"{}\" -> \"{}\";", pkg_name, dep);
            }
            if deps.is_empty() {
                println!("  \"{}\";", pkg_name);
            }
        }
        println!("}}");
    } else {
        println!();
        for pkg_name in &packages {
            let pkg = ws.get_package(pkg_name).unwrap();
            let deps = pkg
                .config
                .workspace_dependencies
                .as_ref()
                .cloned()
                .unwrap_or_default();
            if deps.is_empty() {
                println!(
                    "  {} (no workspace deps)",
                    style(pkg_name).bold()
                );
            } else {
                println!(
                    "  {} -> {}",
                    style(pkg_name).bold(),
                    deps.iter()
                        .map(|d| style(d).cyan().to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                );
            }
        }
        println!();
        println!(
            "  {} Use --dot to output Graphviz DOT format",
            style("tip").dim()
        );
        println!();
    }

    Ok(())
}

fn print_dot_maven(
    name: &str,
    direct: &std::collections::BTreeMap<String, String>,
    lock: &config::schema::LockFile,
    max_depth: usize,
) {
    println!("digraph dependencies {{");
    println!("  rankdir=LR;");
    println!("  \"{}\" [shape=box, style=filled, fillcolor=lightblue];", name);

    let mut visited = HashSet::new();

    for (coord, version) in direct {
        let key = format!("{}:{}", coord, version);
        println!("  \"{}\" -> \"{}\";", name, coord);
        print_dot_transitive(lock, &key, coord, &mut visited, 1, max_depth);
    }

    println!("}}");
}

fn print_dot_transitive(
    lock: &config::schema::LockFile,
    key: &str,
    display: &str,
    visited: &mut HashSet<String>,
    current_depth: usize,
    max_depth: usize,
) {
    if visited.contains(key) {
        return;
    }
    if max_depth > 0 && current_depth > max_depth {
        return;
    }
    visited.insert(key.to_string());

    if let Some(locked) = lock.dependencies.get(key) {
        if let Some(ref deps) = locked.dependencies {
            for dep_key in deps {
                let parts: Vec<&str> = dep_key.split(':').collect();
                let dep_display = if parts.len() == 3 {
                    format!("{}:{}", parts[0], parts[1])
                } else {
                    dep_key.clone()
                };
                println!("  \"{}\" -> \"{}\";", display, dep_display);
                print_dot_transitive(lock, dep_key, &dep_display, visited, current_depth + 1, max_depth);
            }
        }
    }
}

fn print_text_maven(
    name: &str,
    direct: &std::collections::BTreeMap<String, String>,
    lock: &config::schema::LockFile,
    max_depth: usize,
) {
    println!();
    println!("  {}", style(name).bold());

    let count = direct.len();
    for (i, (coord, version)) in direct.iter().enumerate() {
        let is_last = i == count - 1;
        let prefix = if is_last { "  └── " } else { "  ├── " };
        println!(
            "{}{} {}",
            prefix,
            style(coord).cyan(),
            style(version).dim()
        );

        if max_depth == 1 {
            continue;
        }

        let key = format!("{}:{}", coord, version);
        if let Some(locked) = lock.dependencies.get(&key) {
            if let Some(ref deps) = locked.dependencies {
                let child_prefix = if is_last { "      " } else { "  │   " };
                let dep_count = deps.len();
                for (j, dep_key) in deps.iter().enumerate() {
                    let dep_last = j == dep_count - 1;
                    let connector = if dep_last { "└── " } else { "├── " };
                    let parts: Vec<&str> = dep_key.split(':').collect();
                    let display = if parts.len() == 3 {
                        format!("{}:{} {}", parts[0], parts[1], style(parts[2]).dim())
                    } else {
                        dep_key.clone()
                    };
                    println!("{}{}{}", child_prefix, connector, display);
                }
            }
        }
    }

    println!();
    println!(
        "  {} Use --dot to output Graphviz DOT format",
        style("tip").dim()
    );
    println!();
}
