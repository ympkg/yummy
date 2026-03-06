use anyhow::Result;
use console::style;
use sha2::{Digest, Sha256};

use crate::config;

pub fn execute(target: Option<String>) -> Result<()> {
    let (config_path, cfg) = config::load_or_find_config()?;
    let project = config::project_dir(&config_path);

    if cfg.workspaces.is_some() {
        return hash_workspace(&project, target.as_deref());
    }

    let hash = compute_project_hash(&project, &cfg)?;
    println!("{}", hash);
    Ok(())
}

fn hash_workspace(root: &std::path::Path, target: Option<&str>) -> Result<()> {
    let ws = crate::workspace::graph::WorkspaceGraph::build(root)?;

    let packages = if let Some(target) = target {
        ws.transitive_closure(target)?
    } else {
        ws.all_packages()
    };

    let mut combined = Sha256::new();

    for pkg_name in &packages {
        let pkg = ws.get_package(pkg_name).unwrap();
        let hash = compute_project_hash(&pkg.path, &pkg.config)?;
        combined.update(hash.as_bytes());

        eprintln!(
            "  {} {} {}",
            style("■").dim(),
            style(pkg_name).bold(),
            style(&hash[..16]).dim()
        );
    }

    let final_hash = format!("{:x}", combined.finalize());
    println!("{}", final_hash);
    Ok(())
}

fn compute_project_hash(
    project: &std::path::Path,
    cfg: &config::schema::YmConfig,
) -> Result<String> {
    let mut hasher = Sha256::new();

    // Hash ym.json contents
    let config_path = project.join(config::CONFIG_FILE);
    if config_path.exists() {
        let content = std::fs::read(&config_path)?;
        hasher.update(&content);
    }

    // Hash all source files (sorted for determinism)
    let src = config::source_dir_for(project, cfg);
    if src.exists() {
        hash_dir_into(&src, &mut hasher)?;
    }

    // Hash test files
    let test = config::test_dir_for(project, cfg);
    if test.exists() {
        hash_dir_into(&test, &mut hasher)?;
    }

    // Hash resource files
    let resources = project.join("src").join("main").join("resources");
    if resources.exists() {
        hash_dir_into(&resources, &mut hasher)?;
    }

    // Hash lock file
    let lock_path = project.join(config::LOCK_FILE);
    if lock_path.exists() {
        let content = std::fs::read(&lock_path)?;
        hasher.update(&content);
    }

    Ok(format!("{:x}", hasher.finalize()))
}

fn hash_dir_into(dir: &std::path::Path, hasher: &mut Sha256) -> Result<()> {
    let mut paths: Vec<_> = walkdir::WalkDir::new(dir)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .map(|e| e.path().to_path_buf())
        .collect();

    // Sort for deterministic ordering
    paths.sort();

    for path in paths {
        // Include relative path in hash for rename detection
        if let Ok(rel) = path.strip_prefix(dir) {
            hasher.update(rel.to_string_lossy().as_bytes());
        }
        let content = std::fs::read(&path)?;
        hasher.update(&content);
    }

    Ok(())
}
