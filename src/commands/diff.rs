use anyhow::Result;
use console::style;

use crate::compiler::incremental::Fingerprints;
use crate::config;

pub fn execute(target: Option<String>) -> Result<()> {
    let (config_path, cfg) = config::load_or_find_config()?;
    let project = config::project_dir(&config_path);

    if cfg.workspaces.is_some() {
        return diff_workspace(&project, target.as_deref());
    }

    diff_single(&project, &cfg)
}

fn diff_single(
    project: &std::path::Path,
    cfg: &config::schema::YmConfig,
) -> Result<()> {
    let cache = config::cache_dir(project);
    let out = config::output_classes_dir(project);
    let src = config::source_dir_for(project, cfg);

    // Derive the fingerprint dir the same way incremental.rs does
    let fp_dir = {
        let hash = crate::compiler::incremental::hash_bytes(out.to_string_lossy().as_bytes());
        cache.join("fingerprints").join(&hash[..16])
    };

    let fingerprints = Fingerprints::load(&fp_dir);
    let (changed, all_files) = fingerprints.get_changed_files(&[src.clone()])?;

    if changed.is_empty() {
        println!(
            "  {} No changes since last build ({} files tracked)",
            style("✓").green(),
            all_files.len()
        );
        return Ok(());
    }

    println!();
    for file in &changed {
        let rel = file
            .strip_prefix(&src)
            .unwrap_or(file);
        println!("  {} {}", style("M").yellow().bold(), rel.display());
    }

    // Check for new files (not in fingerprints)
    let tracked_count = all_files.len() - changed.len();
    println!();
    println!(
        "  {} {} changed, {} unchanged, {} total",
        style("■").cyan(),
        changed.len(),
        tracked_count,
        all_files.len()
    );

    // Check for deleted .class files (stale outputs)
    if out.exists() {
        let mut stale = 0;
        for entry in walkdir::WalkDir::new(&out) {
            let entry = entry?;
            if entry.path().extension().and_then(|e| e.to_str()) != Some("class") {
                continue;
            }
            let rel = entry.path().strip_prefix(&out)?;
            let java_rel = rel.to_string_lossy().replace(".class", ".java");
            let src_path = src.join(&*java_rel);
            if !src_path.exists() {
                // Check nested classes (Foo$Bar.class)
                let base = java_rel.split('$').next().unwrap_or(&java_rel);
                let base_path = src.join(base);
                if !base_path.exists() {
                    stale += 1;
                }
            }
        }
        if stale > 0 {
            println!(
                "  {} {} stale .class files (consider 'ym rebuild')",
                style("!").yellow(),
                stale
            );
        }
    }

    println!();
    Ok(())
}

fn diff_workspace(root: &std::path::Path, target: Option<&str>) -> Result<()> {
    let ws = crate::workspace::graph::WorkspaceGraph::build(root)?;

    let packages = if let Some(target) = target {
        ws.transitive_closure(target)?
    } else {
        ws.all_packages()
    };

    let mut total_changed = 0;
    let mut total_files = 0;

    for pkg_name in &packages {
        let pkg = ws.get_package(pkg_name).unwrap();
        let cache = config::cache_dir(&pkg.path);
        let out = config::output_classes_dir(&pkg.path);
        let src = config::source_dir(&pkg.path);

        let fp_dir = {
            let hash = crate::compiler::incremental::hash_bytes(out.to_string_lossy().as_bytes());
            cache.join("fingerprints").join(&hash[..16])
        };

        let fingerprints = Fingerprints::load(&fp_dir);
        let (changed, all_files) = fingerprints.get_changed_files(&[src.clone()])?;

        total_changed += changed.len();
        total_files += all_files.len();

        if !changed.is_empty() {
            println!(
                "  {} {} ({} changed)",
                style("M").yellow().bold(),
                style(pkg_name).bold(),
                changed.len()
            );
            for file in &changed {
                let rel = file.strip_prefix(&src).unwrap_or(file);
                println!("    {}", style(rel.display()).dim());
            }
        }
    }

    if total_changed == 0 {
        println!(
            "  {} No changes across {} packages ({} files)",
            style("✓").green(),
            packages.len(),
            total_files
        );
    } else {
        println!();
        println!(
            "  {} {} changed files across {} packages",
            style("■").cyan(),
            total_changed,
            packages.len()
        );
    }

    Ok(())
}
