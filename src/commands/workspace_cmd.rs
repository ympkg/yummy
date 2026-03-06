use anyhow::{Context, Result};
use console::style;

use crate::config;
use crate::workspace::graph::WorkspaceGraph;

pub fn list() -> Result<()> {
    let (config_path, cfg) = config::load_or_find_config()?;
    let project = config::project_dir(&config_path);

    if cfg.workspaces.is_none() {
        println!("  Not a workspace project");
        return Ok(());
    }

    let ws = WorkspaceGraph::build(&project)?;
    let mut packages: Vec<_> = ws.all_packages();
    packages.sort();

    println!();
    for name in &packages {
        let pkg = ws.get_package(name).unwrap();
        let version = pkg.config.version.as_deref().unwrap_or("-");
        let rel = pkg
            .path
            .strip_prefix(&project)
            .unwrap_or(&pkg.path)
            .to_string_lossy();
        println!(
            "  {} {} {}",
            style(name).bold(),
            style(version).dim(),
            style(format!("({})", rel)).dim()
        );
    }
    println!();
    println!("  {} packages total", packages.len());

    Ok(())
}

pub fn graph() -> Result<()> {
    let (config_path, cfg) = config::load_or_find_config()?;
    let project = config::project_dir(&config_path);

    if cfg.workspaces.is_none() {
        println!("  Not a workspace project");
        return Ok(());
    }

    let ws = WorkspaceGraph::build(&project)?;
    let packages = ws.all_packages();

    println!();
    for name in &packages {
        let pkg = ws.get_package(name).unwrap();
        let deps = pkg
            .config
            .workspace_dependencies
            .as_ref()
            .cloned()
            .unwrap_or_default();

        if deps.is_empty() {
            println!("  {} (no workspace deps)", style(name).bold());
        } else {
            println!("  {} -> {}", style(name).bold(), deps.join(", "));
        }
    }
    println!();

    Ok(())
}

/// Run a command in each workspace package
pub fn foreach(args: Vec<String>, parallel: bool) -> Result<()> {
    if args.is_empty() {
        anyhow::bail!("Usage: ym workspace foreach [--parallel] <command> [args...]");
    }

    let (config_path, cfg) = config::load_or_find_config()?;
    let project = config::project_dir(&config_path);

    if cfg.workspaces.is_none() {
        println!("  Not a workspace project");
        return Ok(());
    }

    let ws = WorkspaceGraph::build(&project)?;
    let mut packages: Vec<_> = ws.all_packages();
    packages.sort();

    let cmd_name = &args[0];
    let cmd_args = &args[1..];

    if parallel {
        return foreach_parallel(&ws, &packages, cmd_name, cmd_args);
    }

    let mut failures = Vec::new();

    for name in &packages {
        let pkg = ws.get_package(name).unwrap();
        println!(
            "  {} {} in {}",
            style("→").blue(),
            style(args.join(" ")).bold(),
            style(name).cyan()
        );

        let status = std::process::Command::new(cmd_name)
            .args(cmd_args)
            .current_dir(&pkg.path)
            .status();

        match status {
            Ok(s) if s.success() => {
                println!(
                    "  {} {} {}",
                    style("✓").green(),
                    name,
                    style("done").dim()
                );
            }
            Ok(s) => {
                let code = s.code().unwrap_or(-1);
                println!(
                    "  {} {} exited with code {}",
                    style("✗").red(),
                    name,
                    code
                );
                failures.push(name.clone());
            }
            Err(e) => {
                println!(
                    "  {} {} failed: {}",
                    style("✗").red(),
                    name,
                    e
                );
                failures.push(name.clone());
            }
        }
        println!();
    }

    print_summary(&packages, &failures);
    Ok(())
}

fn foreach_parallel(
    ws: &WorkspaceGraph,
    packages: &[String],
    cmd_name: &str,
    cmd_args: &[String],
) -> Result<()> {
    use std::sync::Mutex;

    println!(
        "  {} Running '{}' in {} packages in parallel...",
        style("→").blue(),
        style(format!("{} {}", cmd_name, cmd_args.join(" "))).bold(),
        packages.len()
    );
    println!();

    let failures = Mutex::new(Vec::new());

    // Use rayon for parallel execution
    rayon::scope(|s| {
        for name in packages {
            let failures = &failures;
            s.spawn(move |_| {
                let pkg = ws.get_package(name).unwrap();
                let output = std::process::Command::new(cmd_name)
                    .args(cmd_args)
                    .current_dir(&pkg.path)
                    .output();

                match output {
                    Ok(o) if o.status.success() => {
                        println!("  {} {}", style("✓").green(), name);
                    }
                    Ok(o) => {
                        let stderr = String::from_utf8_lossy(&o.stderr);
                        println!(
                            "  {} {} (exit {}): {}",
                            style("✗").red(),
                            name,
                            o.status.code().unwrap_or(-1),
                            stderr.lines().next().unwrap_or("")
                        );
                        failures.lock().unwrap().push(name.clone());
                    }
                    Err(e) => {
                        println!("  {} {} failed: {}", style("✗").red(), name, e);
                        failures.lock().unwrap().push(name.clone());
                    }
                }
            });
        }
    });

    let failures = failures.into_inner().unwrap();
    print_summary(packages, &failures);
    Ok(())
}

fn print_summary(packages: &[String], failures: &[String]) {
    if failures.is_empty() {
        println!(
            "  {} All {} packages succeeded",
            style("✓").green().bold(),
            packages.len()
        );
    } else {
        println!(
            "  {} {}/{} packages failed: {}",
            style("✗").red().bold(),
            failures.len(),
            packages.len(),
            failures.join(", ")
        );
    }
}

/// Show workspace summary info
pub fn info() -> Result<()> {
    let (config_path, cfg) = config::load_or_find_config()?;
    let project = config::project_dir(&config_path);

    if cfg.workspaces.is_none() {
        println!("  Not a workspace project");
        return Ok(());
    }

    let ws = WorkspaceGraph::build(&project)?;
    let packages = ws.all_packages();

    let mut total_sources = 0usize;
    let mut total_maven_deps = 0usize;
    let mut total_ws_deps = 0usize;
    let mut has_main = 0usize;

    for name in &packages {
        let pkg = ws.get_package(name).unwrap();
        let src = config::source_dir(&pkg.path);
        if src.exists() {
            total_sources += walkdir::WalkDir::new(&src)
                .into_iter()
                .filter_map(|e| e.ok())
                .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("java"))
                .count();
        }
        total_maven_deps += pkg.config.dependencies.as_ref().map(|d| d.len()).unwrap_or(0);
        total_ws_deps += pkg.config.workspace_dependencies.as_ref().map(|d| d.len()).unwrap_or(0);
        if pkg.config.main.is_some() {
            has_main += 1;
        }
    }

    println!();
    println!("  {}", style("Workspace Summary").bold().underlined());
    println!();
    println!("  {} {}", style("name").dim(), style(&cfg.name).bold());
    if let Some(ref v) = cfg.version {
        println!("  {} {}", style("version").dim(), v);
    }
    if let Some(ref j) = cfg.target {
        println!("  {} {}", style("target").dim(), j);
    }
    println!("  {} {}", style("packages").dim(), packages.len());
    println!("  {} {} apps ({} with main class)", style("apps").dim(), has_main, has_main);
    println!("  {} {} libs (no main class)", style("libs").dim(), packages.len() - has_main);
    println!("  {} {} .java files", style("sources").dim(), total_sources);
    println!("  {} {} maven deps (across all packages)", style("maven").dim(), total_maven_deps);
    println!("  {} {} workspace dep edges", style("ws deps").dim(), total_ws_deps);

    // Check for graph stats
    let edge_count = ws.graph.edge_count();
    let node_count = ws.graph.node_count();
    println!("  {} {} nodes, {} edges", style("graph").dim(), node_count, edge_count);

    println!();
    Ok(())
}

/// Show full dependency details for a specific module
pub fn focus(target: &str) -> Result<()> {
    let (config_path, cfg) = config::load_or_find_config()?;
    let project = config::project_dir(&config_path);

    if cfg.workspaces.is_none() {
        println!("  Not a workspace project");
        return Ok(());
    }

    let ws = WorkspaceGraph::build(&project)?;
    let pkg = ws.get_package(target)
        .ok_or_else(|| anyhow::anyhow!("Package '{}' not found", target))?;

    println!();
    println!("  {}", style(format!("Package: {}", target)).bold().underlined());
    println!();

    // Basic info
    println!("  {} {}", style("path").dim(), pkg.path.display());
    if let Some(ref v) = pkg.config.version {
        println!("  {} {}", style("version").dim(), v);
    }
    if let Some(ref m) = pkg.config.main {
        println!("  {} {}", style("main").dim(), m);
    }

    // Workspace dependencies
    if let Some(ref ws_deps) = pkg.config.workspace_dependencies {
        if !ws_deps.is_empty() {
            println!();
            println!("  {} ({}):", style("Workspace Dependencies").cyan().bold(), ws_deps.len());
            for dep in ws_deps {
                println!("    {} {}", style("→").blue(), dep);
            }
        }
    }

    // Maven dependencies
    if let Some(ref deps) = pkg.config.dependencies {
        if !deps.is_empty() {
            println!();
            println!("  {} ({}):", style("Maven Dependencies").cyan().bold(), deps.len());
            for (coord, version) in deps {
                println!("    {} {} {}", style("·").dim(), style(coord).cyan(), style(version).dim());
            }
        }
    }

    // Dev dependencies
    if let Some(ref dev_deps) = pkg.config.dev_dependencies {
        if !dev_deps.is_empty() {
            println!();
            println!("  {} ({}):", style("Dev Dependencies").cyan().bold(), dev_deps.len());
            for (coord, version) in dev_deps {
                println!("    {} {} {}", style("·").dim(), coord, style(version).dim());
            }
        }
    }

    // Transitive workspace closure
    let closure = ws.transitive_closure(target)?;
    if closure.len() > 1 {
        println!();
        println!("  {} ({} packages):", style("Transitive Closure").cyan().bold(), closure.len());
        for name in &closure {
            let marker = if name == target { "★" } else { "·" };
            println!("    {} {}", style(marker).dim(), name);
        }
    }

    // Source file count
    let src = config::source_dir(&pkg.path);
    if src.exists() {
        let count = walkdir::WalkDir::new(&src)
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("java"))
            .count();
        println!();
        println!("  {} {} .java files", style("sources").dim(), count);
    }

    println!();
    Ok(())
}

/// Clean all workspace module outputs
pub fn clean_all() -> Result<()> {
    let (config_path, cfg) = config::load_or_find_config()?;
    let project = config::project_dir(&config_path);

    if cfg.workspaces.is_none() {
        println!("  Not a workspace project");
        return Ok(());
    }

    let ws = WorkspaceGraph::build(&project)?;
    let packages = ws.all_packages();
    let mut cleaned = 0;

    for name in &packages {
        let pkg = ws.get_package(name).unwrap();
        let out_dir = pkg.path.join(config::OUTPUT_DIR);
        if out_dir.exists() {
            std::fs::remove_dir_all(&out_dir)?;
            cleaned += 1;
        }
    }

    // Also clean root out/ and .ym/
    let root_out = project.join(config::OUTPUT_DIR);
    if root_out.exists() {
        std::fs::remove_dir_all(&root_out)?;
    }
    let cache = config::cache_dir(&project);
    if cache.exists() {
        std::fs::remove_dir_all(&cache)?;
    }

    println!(
        "  {} Cleaned {} package(s) + root cache",
        style("✓").green(),
        cleaned
    );

    Ok(())
}

/// Show which packages are affected by changes to a module (reverse transitive closure)
pub fn impact(target: &str) -> Result<()> {
    let (config_path, cfg) = config::load_or_find_config()?;
    let project = config::project_dir(&config_path);

    if cfg.workspaces.is_none() {
        println!("  Not a workspace project");
        return Ok(());
    }

    let ws = WorkspaceGraph::build(&project)?;

    // Verify target exists
    if ws.get_package(target).is_none() {
        anyhow::bail!("Package '{}' not found in workspace", target);
    }

    // Build reverse dependency map
    let all = ws.all_packages();
    let mut reverse: std::collections::HashMap<String, Vec<String>> = std::collections::HashMap::new();
    for name in &all {
        let pkg = ws.get_package(name).unwrap();
        if let Some(ref deps) = pkg.config.workspace_dependencies {
            for dep in deps {
                reverse.entry(dep.clone()).or_default().push(name.clone());
            }
        }
    }

    // BFS from target through reverse edges
    let mut affected = Vec::new();
    let mut visited = std::collections::HashSet::new();
    let mut queue = std::collections::VecDeque::new();
    visited.insert(target.to_string());
    queue.push_back(target.to_string());

    while let Some(node) = queue.pop_front() {
        if node != target {
            affected.push(node.clone());
        }
        if let Some(dependents) = reverse.get(&node) {
            for dep in dependents {
                if visited.insert(dep.clone()) {
                    queue.push_back(dep.clone());
                }
            }
        }
    }

    println!();
    if affected.is_empty() {
        println!(
            "  {} No other packages depend on {}",
            style("✓").green(),
            style(target).bold()
        );
    } else {
        println!(
            "  Changes to {} affect {} package(s):",
            style(target).bold(),
            affected.len()
        );
        println!();
        affected.sort();
        for name in &affected {
            // Show direct or transitive
            let is_direct = reverse.get(target).map(|d| d.contains(name)).unwrap_or(false);
            let label = if is_direct { "direct" } else { "transitive" };
            println!(
                "  {} {} {}",
                style("~").yellow(),
                style(name).cyan(),
                style(format!("({})", label)).dim()
            );
        }
    }
    println!();

    Ok(())
}

pub fn changed() -> Result<()> {
    let (config_path, cfg) = config::load_or_find_config()?;
    let project = config::project_dir(&config_path);

    if cfg.workspaces.is_none() {
        println!("  Not a workspace project");
        return Ok(());
    }

    // Use git to find changed files
    let output = std::process::Command::new("git")
        .args(["diff", "--name-only", "HEAD"])
        .current_dir(&project)
        .output()
        .context("Failed to run git. Is this a git repository?")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        println!("  {}: {}", style("git error").red(), stderr.trim());
        return Ok(());
    }

    let changed_files = String::from_utf8_lossy(&output.stdout);
    let ws = WorkspaceGraph::build(&project)?;

    let mut changed_packages = std::collections::HashSet::new();

    for file in changed_files.lines() {
        let file_path = project.join(file);
        for name in ws.all_packages() {
            let pkg = ws.get_package(&name).unwrap();
            if file_path.starts_with(&pkg.path) {
                changed_packages.insert(name);
                break;
            }
        }
    }

    if changed_packages.is_empty() {
        println!("  No packages changed since last commit");
    } else {
        println!();
        let mut sorted: Vec<_> = changed_packages.into_iter().collect();
        sorted.sort();
        for name in &sorted {
            println!("  {} {}", style("~").yellow(), name);
        }
        println!();
        println!("  {} package(s) changed", sorted.len());
    }

    Ok(())
}
