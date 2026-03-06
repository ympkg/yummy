use anyhow::Result;
use console::style;

use crate::config;

pub fn execute() -> Result<()> {
    println!();

    // ym version
    println!(
        "  {} {}",
        style("ym").dim(),
        env!("CARGO_PKG_VERSION")
    );

    // Java version
    match std::process::Command::new("java").arg("-version").output() {
        Ok(output) => {
            let version_str = String::from_utf8_lossy(&output.stderr);
            if let Some(first_line) = version_str.lines().next() {
                println!("  {} {}", style("java").dim(), first_line.trim());
            }
        }
        Err(_) => {
            println!(
                "  {} {}",
                style("java").dim(),
                style("not found").red()
            );
        }
    }

    // javac version
    match std::process::Command::new("javac").arg("-version").output() {
        Ok(output) => {
            let out = String::from_utf8_lossy(&output.stdout);
            let err = String::from_utf8_lossy(&output.stderr);
            let version_str = if out.trim().is_empty() { err } else { out };
            println!(
                "  {} {}",
                style("javac").dim(),
                version_str.trim()
            );
        }
        Err(_) => {
            println!(
                "  {} {}",
                style("javac").dim(),
                style("not found").red()
            );
        }
    }

    // JAVA_HOME
    match std::env::var("JAVA_HOME") {
        Ok(home) => println!("  {} {}", style("JAVA_HOME").dim(), home),
        Err(_) => println!(
            "  {} {}",
            style("JAVA_HOME").dim(),
            style("not set").yellow()
        ),
    }

    // OS
    println!(
        "  {} {} {}",
        style("os").dim(),
        std::env::consts::OS,
        std::env::consts::ARCH
    );

    // Working directory
    if let Ok(cwd) = std::env::current_dir() {
        println!("  {} {}", style("cwd").dim(), cwd.display());
    }

    // ym.json info
    if let Ok((config_path, cfg)) = config::load_or_find_config() {
        let project = config::project_dir(&config_path);
        println!("  {} {}", style("config").dim(), config_path.display());
        println!("  {} {}", style("project").dim(), cfg.name);

        if let Some(ref java) = cfg.target {
            println!("  {} {}", style("target java").dim(), java);
        }

        let engine = cfg
            .compiler
            .as_ref()
            .and_then(|c| c.engine.as_deref())
            .unwrap_or("javac");
        println!("  {} {}", style("compiler").dim(), engine);

        // Cache info
        let cache = config::cache_dir(&project);
        if cache.exists() {
            println!("  {} {}", style("cache").dim(), cache.display());
        }

        // Lock file
        let lock_path = project.join(config::LOCK_FILE);
        if lock_path.exists() {
            if let Ok(lock) = config::load_lock(&lock_path) {
                println!(
                    "  {} {} locked dependencies",
                    style("lock").dim(),
                    lock.dependencies.len()
                );
            }
        }
    }

    println!();
    Ok(())
}
