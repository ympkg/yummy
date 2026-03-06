use anyhow::Result;
use console::style;
use std::time::Instant;

use crate::config;

pub fn execute(target: Option<String>, release: bool) -> Result<()> {
    let start = Instant::now();

    let (config_path, _cfg) = config::load_or_find_config()?;
    let project = config::project_dir(&config_path);

    // Clean output directory
    let out_dir = project.join(config::OUTPUT_DIR);
    if out_dir.exists() {
        std::fs::remove_dir_all(&out_dir)?;
        println!(
            "  {} Cleaned {}",
            style("✓").green(),
            style(out_dir.display()).dim()
        );
    }

    // Clean fingerprint cache
    let cache = config::cache_dir(&project);
    let fp_dir = cache.join("fingerprints");
    if fp_dir.exists() {
        std::fs::remove_dir_all(&fp_dir)?;
        println!(
            "  {} Cleaned fingerprint cache",
            style("✓").green()
        );
    }

    // Rebuild
    println!();
    super::build::execute(target, release)?;

    let elapsed = start.elapsed();
    if elapsed.as_millis() > 1000 {
        println!(
            "\n  {} Full rebuild in {:.1}s",
            style("⚡").cyan(),
            elapsed.as_secs_f64()
        );
    } else {
        println!(
            "\n  {} Full rebuild in {}ms",
            style("⚡").cyan(),
            elapsed.as_millis()
        );
    }

    Ok(())
}
