use anyhow::Result;
use console::style;

use crate::config;

/// List cache contents and sizes
pub fn list() -> Result<()> {
    let cwd = std::env::current_dir()?;
    let cache = config::cache_dir(&cwd);

    if !cache.exists() {
        println!("  No cache directory found.");
        return Ok(());
    }

    println!();
    println!("  {} {}", style("Cache directory:").bold(), cache.display());
    println!();

    // Maven cache
    let maven_cache = cache.join("cache").join("maven");
    if maven_cache.exists() {
        let (size, count) = dir_stats(&maven_cache);
        println!(
            "  {} Maven artifacts  {} ({} files)",
            style("●").blue(),
            style(format_size(size)).bold(),
            count
        );
    }

    // Fingerprints
    let fp_dir = cache.join("fingerprints");
    if fp_dir.exists() {
        let (size, count) = dir_stats(&fp_dir);
        println!(
            "  {} Fingerprints     {} ({} files)",
            style("●").blue(),
            style(format_size(size)).bold(),
            count
        );
    }

    // Total
    let (total, total_count) = dir_stats(&cache);
    println!();
    println!(
        "  Total: {} ({} files)",
        style(format_size(total)).bold(),
        total_count
    );
    println!();

    Ok(())
}

/// Clean the cache (maven artifacts + fingerprints)
pub fn clean(maven_only: bool) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let cache = config::cache_dir(&cwd);

    if !cache.exists() {
        println!("  No cache to clean.");
        return Ok(());
    }

    if maven_only {
        let maven_cache = cache.join("cache").join("maven");
        if maven_cache.exists() {
            let (size, _) = dir_stats(&maven_cache);
            std::fs::remove_dir_all(&maven_cache)?;
            println!(
                "  {} Removed Maven cache ({})",
                style("✓").green(),
                format_size(size)
            );
        }
    } else {
        let (size, _) = dir_stats(&cache);
        std::fs::remove_dir_all(&cache)?;
        println!(
            "  {} Removed all caches ({})",
            style("✓").green(),
            format_size(size)
        );
    }

    Ok(())
}

fn dir_stats(path: &std::path::Path) -> (u64, usize) {
    let mut size = 0u64;
    let mut count = 0usize;
    for entry in walkdir::WalkDir::new(path).into_iter().filter_map(|e| e.ok()) {
        if entry.file_type().is_file() {
            if let Ok(meta) = entry.metadata() {
                size += meta.len();
                count += 1;
            }
        }
    }
    (size, count)
}

fn format_size(bytes: u64) -> String {
    if bytes >= 1_073_741_824 {
        format!("{:.1} GB", bytes as f64 / 1_073_741_824.0)
    } else if bytes >= 1_048_576 {
        format!("{:.1} MB", bytes as f64 / 1_048_576.0)
    } else if bytes >= 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{} B", bytes)
    }
}
