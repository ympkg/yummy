//! `ym install` — sync ym-lock.json with ym.json without compiling.
//!
//! Resolves all workspace dependencies, downloads JARs to the local Maven cache
//! and writes a complete ym-lock.json. This is the lockfile-only counterpart to
//! `ymc build` and the way to sync after hand-editing ym.json (analogous to
//! `npm install` / `yarn install`, `cargo generate-lockfile`).
//!
//! Before this command existed, `ymc build` (no target, no --frozen-lockfile)
//! was the only way to refresh the lockfile, which forced a full workspace
//! compilation on every dependency edit. See ADR-016 for the lockfile model.
use std::path::Path;
use std::time::Instant;

use anyhow::Result;
use console::style;

use crate::config;
use crate::config::schema::YmConfig;
use crate::workspace::graph::WorkspaceGraph;
use crate::workspace::resolver;

pub fn execute() -> Result<()> {
    let total_start = Instant::now();
    let (config_path, cfg) = config::load_or_find_config()?;
    let project = config::project_dir(&config_path);

    // If invoked from a workspace child, redirect to the workspace root so that
    // we sync the full workspace lockfile (not just this module). Running install
    // on a child would otherwise overwrite ym-lock.json with only the child's
    // deps, corrupting the workspace lock.
    let (root_project, root_cfg) = resolve_install_root(&project, cfg)?;

    if root_cfg.workspaces.is_some() {
        install_workspace(&root_project, &root_cfg)?;
    } else {
        install_single(&root_project, &root_cfg)?;
    }

    let elapsed = total_start.elapsed();
    let time = if elapsed.as_millis() > 1000 {
        format!("{:.2}s", elapsed.as_secs_f64())
    } else {
        format!("{}ms", elapsed.as_millis())
    };
    println!(
        "{} install in {}",
        style(format!("{:>12}", "Finished")).green().bold(),
        time,
    );
    Ok(())
}

fn resolve_install_root(project: &Path, cfg: YmConfig) -> Result<(std::path::PathBuf, YmConfig)> {
    if cfg.workspaces.is_some() {
        return Ok((project.to_path_buf(), cfg));
    }
    if let Some(ws_root) = config::find_workspace_root(project) {
        if ws_root != project {
            let root_config_path = ws_root.join(config::CONFIG_FILE);
            let root_cfg = config::load_config(&root_config_path)?;
            if root_cfg.workspaces.is_some() {
                return Ok((ws_root, root_cfg));
            }
        }
    }
    Ok((project.to_path_buf(), cfg))
}

/// Workspace install: replicates `build_workspace`'s dep-resolution phase
/// (own_module_deps → transitive closure propagation → workspace-wide resolve
/// + lock save) without any compilation, packaging or scripts.
fn install_workspace(root: &Path, root_cfg: &YmConfig) -> Result<()> {
    eprintln!("  Scanning workspace...");
    let ws = WorkspaceGraph::build(root)?;
    let mut packages = ws.all_packages();
    packages.sort();

    if packages.is_empty() {
        eprintln!(
            "{} no workspace modules found",
            style(format!("{:>12}", "warning")).yellow().bold(),
        );
        return Ok(());
    }

    eprintln!("  Scanning workspace ({} modules)...", packages.len());

    // Mirror build_workspace: validate workspace dep declarations before resolving.
    for name in &packages {
        let pkg = ws.get_package(name).unwrap();
        let errors = pkg.config.validate_workspace_deps(root_cfg);
        if !errors.is_empty() {
            for e in &errors {
                eprintln!(
                    "{} {}: {}",
                    style(format!("{:>12}", "error")).red().bold(),
                    name,
                    e,
                );
            }
            anyhow::bail!("Invalid workspace dependency declarations in '{}'", name);
        }
    }

    let dep_start = Instant::now();

    let own_module_deps: std::collections::HashMap<String, std::collections::BTreeMap<String, String>> = packages
        .iter()
        .map(|name| {
            let pkg = ws.get_package(name).unwrap();
            let mut deps = pkg.config.maven_dependencies_with_root(root_cfg);
            for (k, v) in root_cfg.resolved_resolutions(root_cfg) {
                if deps.contains_key(&k) {
                    deps.insert(k, v);
                }
            }
            (name.clone(), deps)
        })
        .collect();

    let closure_cache: std::collections::HashMap<String, Vec<String>> = packages
        .iter()
        .map(|name| (name.clone(), ws.transitive_closure(name).unwrap_or_default()))
        .collect();

    let all_module_deps: Vec<(String, std::collections::BTreeMap<String, String>)> = packages
        .iter()
        .map(|name| {
            let mut deps = own_module_deps.get(name).cloned().unwrap_or_default();
            if let Some(closure) = closure_cache.get(name) {
                for ws_dep in closure {
                    if ws_dep != name {
                        if let Some(ws_dep_deps) = own_module_deps.get(ws_dep) {
                            for (k, v) in ws_dep_deps {
                                deps.entry(k.clone()).or_insert(v.clone());
                            }
                        }
                    }
                }
            }
            (name.clone(), deps)
        })
        .collect();

    let unique_artifacts: usize = {
        let mut set = std::collections::BTreeSet::new();
        for (_, deps) in &all_module_deps {
            set.extend(deps.keys().cloned());
        }
        set.len()
    };
    eprintln!(
        "  Resolving dependencies ({} modules, {} artifacts)...",
        packages.len(),
        unique_artifacts,
    );

    let cache = config::maven_cache_dir();
    let mut resolved = config::load_lockfile_checked(root, root_cfg)?;
    let registries = root_cfg.registry_entries();
    let mut exclusions = root_cfg.exclusions.as_ref().cloned().unwrap_or_default();
    exclusions.extend(root_cfg.per_dependency_exclusions());
    exclusions.extend(root_cfg.resolved_exclusions());
    let resolutions = root_cfg.resolved_resolutions(root_cfg);

    let _per_module_jars = resolver::resolve_workspace_deps_with_resolutions(
        &all_module_deps,
        &cache,
        &mut resolved,
        &registries,
        &exclusions,
        &resolutions,
    )?;

    config::save_lockfile(root, &resolved)?;
    let dep_time = dep_start.elapsed();
    let total_jars = resolved.dependencies.len();
    println!(
        "{} dependencies ({} jars) {:>25}ms",
        style(format!("{:>12}", "Resolving")).green().bold(),
        total_jars,
        dep_time.as_millis(),
    );

    let conflicts = resolver::check_conflicts(&resolved);
    if !conflicts.is_empty() {
        for (ga, versions) in &conflicts {
            eprintln!(
                "{} version conflict: {} has versions: {}",
                style(format!("{:>12}", "warning")).yellow().bold(),
                style(ga).bold(),
                versions.join(", "),
            );
        }
        eprintln!("             Use [resolutions] in ym.json to pin a specific version");
    }

    Ok(())
}

/// Single-project install: delegate to the persisting variant
/// `build::resolve_and_persist_deps`. `install` is a declarative lockfile
/// writer (ADR-020); workspace children are already redirected to the root
/// path by [`resolve_install_root`] above before reaching here.
fn install_single(project: &Path, cfg: &YmConfig) -> Result<()> {
    let dep_start = Instant::now();
    let jars = super::build::resolve_and_persist_deps(project, cfg)?;
    let dep_time = dep_start.elapsed();
    println!(
        "{} dependencies ({} jars) {:>25}ms",
        style(format!("{:>12}", "Resolving")).green().bold(),
        jars.len(),
        dep_time.as_millis(),
    );
    Ok(())
}
