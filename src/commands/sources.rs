use anyhow::Result;
use console::style;

use crate::config;

pub fn execute() -> Result<()> {
    let (config_path, _cfg) = config::load_or_find_config()?;
    let project = config::project_dir(&config_path);

    let lock_path = project.join(config::LOCK_FILE);
    let lock = config::load_lock(&lock_path)?;
    let cache = config::maven_cache_dir(&project);

    if lock.dependencies.is_empty() {
        println!("  No dependencies. Run 'ym build' first.");
        return Ok(());
    }

    let client = reqwest::blocking::Client::builder()
        .user_agent("ym/0.1.0")
        .timeout(std::time::Duration::from_secs(15))
        .build()?;

    let mut downloaded = 0;
    let mut skipped = 0;
    let mut failed = 0;
    let total = lock.dependencies.len();

    for (i, key) in lock.dependencies.keys().enumerate() {
        let parts: Vec<&str> = key.split(':').collect();
        if parts.len() != 3 {
            continue;
        }
        let (group, artifact, version) = (parts[0], parts[1], parts[2]);

        let sources_jar = cache
            .join(group)
            .join(artifact)
            .join(version)
            .join(format!("{}-{}-sources.jar", artifact, version));

        if sources_jar.exists() {
            skipped += 1;
            continue;
        }

        let url = format!(
            "https://repo1.maven.org/maven2/{}/{}/{}/{}-{}-sources.jar",
            group.replace('.', "/"),
            artifact,
            version,
            artifact,
            version
        );

        eprint!(
            "\r  [{}/{}] Downloading sources for {}:{}...",
            i + 1,
            total,
            group,
            artifact
        );

        match client.get(&url).send() {
            Ok(resp) if resp.status().is_success() => {
                if let Ok(bytes) = resp.bytes() {
                    if let Some(parent) = sources_jar.parent() {
                        let _ = std::fs::create_dir_all(parent);
                    }
                    let _ = std::fs::write(&sources_jar, &bytes);
                    downloaded += 1;
                }
            }
            _ => {
                failed += 1;
            }
        }
    }

    // Clear progress line
    eprint!("\r{}\r", " ".repeat(80));

    if downloaded > 0 {
        println!(
            "  {} Downloaded {} source JAR(s)",
            style("✓").green(),
            downloaded
        );
    }
    if skipped > 0 {
        println!(
            "  {} {} already cached",
            style("✓").green(),
            skipped
        );
    }
    if failed > 0 {
        println!(
            "  {} {} not available (some libs don't publish sources)",
            style("!").yellow(),
            failed
        );
    }

    Ok(())
}
