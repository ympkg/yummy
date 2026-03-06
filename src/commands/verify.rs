use anyhow::{bail, Result};
use console::style;

use crate::compiler::incremental::hash_bytes;
use crate::config;
use crate::workspace::resolver::MavenCoord;

/// Verify integrity of all cached dependencies against lock file SHA-256 hashes.
pub fn execute() -> Result<()> {
    let (config_path, _cfg) = config::load_or_find_config()?;
    let project = config::project_dir(&config_path);

    let lock_path = project.join(config::LOCK_FILE);
    let lock = config::load_lock(&lock_path)?;

    if lock.dependencies.is_empty() {
        bail!("No lock file found. Run 'ym build' first.");
    }

    let cache = config::maven_cache_dir(&project);

    println!();
    println!(
        "  {} Verifying {} dependencies...",
        style("~").blue(),
        lock.dependencies.len()
    );

    let mut ok = 0;
    let mut mismatched = 0;
    let mut missing = 0;
    let mut no_hash = 0;

    for (key, locked) in &lock.dependencies {
        let parts: Vec<&str> = key.split(':').collect();
        if parts.len() != 3 {
            continue;
        }

        let coord = MavenCoord {
            group_id: parts[0].to_string(),
            artifact_id: parts[1].to_string(),
            version: parts[2].to_string(),
        };

        let jar_path = coord.jar_path(&cache);

        if !jar_path.exists() {
            missing += 1;
            println!(
                "  {} {} (missing from cache)",
                style("?").yellow(),
                key
            );
            continue;
        }

        match &locked.sha256 {
            Some(expected) => {
                let data = std::fs::read(&jar_path)?;
                let actual = hash_bytes(&data);
                if actual == *expected {
                    ok += 1;
                } else {
                    mismatched += 1;
                    println!(
                        "  {} {} SHA mismatch!",
                        style("✗").red().bold(),
                        style(key).red()
                    );
                    println!(
                        "    expected: {}",
                        style(&expected[..16]).dim()
                    );
                    println!(
                        "    actual:   {}",
                        style(&actual[..16]).dim()
                    );
                }
            }
            None => {
                no_hash += 1;
            }
        }
    }

    println!();
    if mismatched > 0 {
        println!(
            "  {} {} integrit{} mismatch! Run 'ym clean && ym build' to re-download.",
            style("✗").red().bold(),
            mismatched,
            if mismatched == 1 { "y" } else { "ies" }
        );
    } else {
        println!(
            "  {} All dependencies verified",
            style("✓").green().bold()
        );
    }

    if ok > 0 {
        println!(
            "    {} verified",
            style(format!("{} OK", ok)).green()
        );
    }
    if missing > 0 {
        println!(
            "    {} missing (run 'ym build' to download)",
            style(format!("{}", missing)).yellow()
        );
    }
    if no_hash > 0 {
        println!(
            "    {} without hash (run 'ym build' to compute)",
            style(format!("{}", no_hash)).dim()
        );
    }

    println!();

    if mismatched > 0 {
        bail!("Integrity verification failed");
    }

    Ok(())
}
