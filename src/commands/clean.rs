use anyhow::Result;
use console::style;

use crate::config;

pub fn execute(all: bool, yes: bool) -> Result<()> {
    let (config_path, cfg) = config::load_or_find_config()?;
    let project = config::project_dir(&config_path);

    // Always clean out/ directory
    let out_dir = project.join(config::OUTPUT_DIR);
    if out_dir.exists() {
        std::fs::remove_dir_all(&out_dir)?;
        println!("  {} Removed {}", style("✓").green(), out_dir.display());
    }

    // In workspace root, also clean all modules' out/ directories
    if cfg.workspaces.is_some() {
        if let Ok(ws) = crate::workspace::graph::WorkspaceGraph::build(&project) {
            for name in ws.all_packages() {
                if let Some(pkg) = ws.get_package(&name) {
                    let pkg_out = pkg.path.join(config::OUTPUT_DIR);
                    if pkg_out.exists() {
                        let _ = std::fs::remove_dir_all(&pkg_out);
                        println!("  {} Removed {}", style("✓").green(), pkg_out.display());
                    }
                }
            }
        }
    }

    // Remove workspace build fingerprints
    let cache = config::cache_dir(&project);
    if cache.exists() {
        if let Ok(entries) = std::fs::read_dir(&cache) {
            for entry in entries.flatten() {
                let name = entry.file_name();
                if name.to_string_lossy().starts_with("workspace-build-fingerprint-") {
                    let _ = std::fs::remove_file(entry.path());
                }
            }
        }
    }

    if all {
        // Confirm before deleting dependency cache
        if !yes {
            let maven_cache = config::maven_cache_dir(&project);
            if maven_cache.exists() {
                let size = dir_size(&maven_cache);
                let confirm = dialoguer::Confirm::new()
                    .with_prompt(format!(
                        "  Delete Maven dependency cache ({:.1} MB)?",
                        size as f64 / 1_048_576.0
                    ))
                    .default(false)
                    .interact()?;
                if !confirm {
                    println!("  {} Skipped cache deletion", style("!").yellow());
                    println!("  {} Clean complete", style("✓").green());
                    return Ok(());
                }
            }
        }

        // Remove .ym/cache/ (Maven dependency cache)
        let maven_cache = config::maven_cache_dir(&project);
        if maven_cache.exists() {
            let size = dir_size(&maven_cache);
            std::fs::remove_dir_all(&maven_cache)?;
            println!(
                "  {} Removed Maven cache ({:.1} MB)",
                style("✓").green(),
                size as f64 / 1_048_576.0
            );
        }

        // Remove resolved cache
        let resolved_path = config::cache_dir(&project).join(config::RESOLVED_FILE);
        if resolved_path.exists() {
            std::fs::remove_file(&resolved_path)?;
            println!("  {} Removed resolved cache", style("✓").green());
        }
    }

    println!("  {} Clean complete", style("✓").green());
    Ok(())
}

fn dir_size(path: &std::path::Path) -> u64 {
    walkdir::WalkDir::new(path)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .filter_map(|e| e.metadata().ok())
        .map(|m| m.len())
        .sum()
}
