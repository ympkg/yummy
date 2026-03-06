use anyhow::Result;
use console::style;

use crate::config;

pub fn execute(all: bool) -> Result<()> {
    let (config_path, _cfg) = config::load_or_find_config()?;
    let project = config::project_dir(&config_path);

    let out_dir = project.join(config::OUTPUT_DIR);
    if out_dir.exists() {
        std::fs::remove_dir_all(&out_dir)?;
        println!("  {} Removed {}", style("✓").green(), out_dir.display());
    }

    let cache = config::cache_dir(&project);
    if cache.exists() {
        std::fs::remove_dir_all(&cache)?;
        println!("  {} Removed {}", style("✓").green(), cache.display());
    }

    if all {
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

        // Remove lock file
        let lock_path = project.join(config::LOCK_FILE);
        if lock_path.exists() {
            std::fs::remove_file(&lock_path)?;
            println!("  {} Removed {}", style("✓").green(), config::LOCK_FILE);
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
