use anyhow::Result;
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

/// Run a command in each workspace package
pub fn foreach(args: Vec<String>, parallel: bool, jobs: Option<usize>, keep_going: bool) -> Result<()> {
    if args.is_empty() {
        anyhow::bail!("Usage: ym workspace foreach [--parallel] -- <command> [args...]");
    }

    let (config_path, cfg) = config::load_or_find_config()?;
    let project = config::project_dir(&config_path);

    if cfg.workspaces.is_none() {
        println!("  Not a workspace project");
        return Ok(());
    }

    let ws = WorkspaceGraph::build(&project)?;
    // Use topological order for consistent dependency-first execution
    let packages = ws.topological_order();

    let cmd_name = &args[0];
    let cmd_args = &args[1..];

    if parallel {
        if let Some(n) = jobs {
            let _ = rayon::ThreadPoolBuilder::new()
                .num_threads(n.max(1))
                .build_global();
        }
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
                if !keep_going {
                    anyhow::bail!(
                        "Command failed in '{}' with exit code {}. Use --keep-going to continue.",
                        name,
                        code
                    );
                }
                failures.push(name.clone());
            }
            Err(e) => {
                println!(
                    "  {} {} failed: {}",
                    style("✗").red(),
                    name,
                    e
                );
                if !keep_going {
                    anyhow::bail!("Command failed in '{}': {}", name, e);
                }
                failures.push(name.clone());
            }
        }
        println!();
    }

    print_summary(&packages, &failures);
    if !failures.is_empty() {
        anyhow::bail!("{} package(s) failed", failures.len());
    }
    Ok(())
}

fn foreach_parallel(
    ws: &WorkspaceGraph,
    packages: &[String],
    cmd_name: &str,
    cmd_args: &[String],
) -> Result<()> {
    use std::collections::HashSet;
    use std::sync::Mutex;

    println!(
        "  {} Running '{}' in {} packages in parallel (by topological level)...",
        style("→").blue(),
        style(format!("{} {}", cmd_name, cmd_args.join(" "))).bold(),
        packages.len()
    );
    println!();

    let pkg_set: HashSet<&String> = packages.iter().collect();
    let levels = ws.topological_levels();
    let all_failures = Mutex::new(Vec::new());

    for level in &levels {
        // Filter to only packages in the requested set
        let level_pkgs: Vec<&String> = level.iter().filter(|n| pkg_set.contains(n)).collect();
        if level_pkgs.is_empty() {
            continue;
        }

        rayon::scope(|s| {
            for name in &level_pkgs {
                let all_failures = &all_failures;
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
                            all_failures.lock().unwrap().push((*name).clone());
                        }
                        Err(e) => {
                            println!("  {} {} failed: {}", style("✗").red(), name, e);
                            all_failures.lock().unwrap().push((*name).clone());
                        }
                    }
                });
            }
        });
    }

    let failures = all_failures.into_inner().unwrap();
    print_summary(packages, &failures);
    if !failures.is_empty() {
        anyhow::bail!("{} package(s) failed", failures.len());
    }
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
