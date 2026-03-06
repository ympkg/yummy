use anyhow::{bail, Result};
use console::style;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crate::config;
use crate::watcher::FileWatcher;

/// Watch for file changes and run a command.
/// Example: `ym watch -- ym build`
pub fn execute(args: Vec<String>, ext: String) -> Result<()> {
    if args.is_empty() {
        bail!("Usage: ym watch [--ext .java,.xml] -- <command> [args...]");
    }

    let (config_path, _cfg) = config::load_or_find_config()?;
    let project = config::project_dir(&config_path);

    let src = config::source_dir(&project);
    let mut watch_dirs = vec![];
    if src.exists() {
        watch_dirs.push(src);
    }

    let test_dir = config::test_dir(&project);
    if test_dir.exists() {
        watch_dirs.push(test_dir);
    }

    // Also watch resources
    let res_dir = project.join("src").join("main").join("resources");
    if res_dir.exists() {
        watch_dirs.push(res_dir);
    }

    if watch_dirs.is_empty() {
        watch_dirs.push(project.clone());
    }

    let extensions: Vec<String> = ext
        .split(',')
        .map(|e| {
            let e = e.trim();
            if e.starts_with('.') {
                e.to_string()
            } else {
                format!(".{}", e)
            }
        })
        .collect();

    let watcher = FileWatcher::new(&watch_dirs, extensions.clone())?;

    let cmd_name = &args[0];
    let cmd_args = &args[1..];

    println!();
    println!(
        "  {} Watching for {} changes...",
        style("~").blue(),
        extensions.join(", ")
    );
    println!(
        "  {} Will run: {} {}",
        style("→").dim(),
        cmd_name,
        cmd_args.join(" ")
    );
    println!();

    let running = Arc::new(AtomicBool::new(true));
    let r = running.clone();
    ctrlc::set_handler(move || {
        r.store(false, Ordering::SeqCst);
    })?;

    while running.load(Ordering::SeqCst) {
        let changed = watcher.wait_for_changes(Duration::from_millis(100));

        if !running.load(Ordering::SeqCst) {
            break;
        }

        if changed.is_empty() {
            continue;
        }

        let now = chrono_time();
        for path in &changed {
            if let Some(name) = path.file_name() {
                println!(
                    "  {} Changed: {}",
                    style(&now).dim(),
                    style(name.to_string_lossy()).yellow()
                );
            }
        }

        let start = std::time::Instant::now();
        let status = std::process::Command::new(cmd_name)
            .args(cmd_args)
            .current_dir(&project)
            .status();

        let elapsed = start.elapsed();

        match status {
            Ok(s) if s.success() => {
                println!(
                    "  {} Command succeeded ({}ms)",
                    style("✓").green(),
                    elapsed.as_millis()
                );
            }
            Ok(s) => {
                println!(
                    "  {} Command exited with code {} ({}ms)",
                    style("✗").red(),
                    s.code().unwrap_or(-1),
                    elapsed.as_millis()
                );
            }
            Err(e) => {
                println!(
                    "  {} Command failed: {}",
                    style("✗").red(),
                    e
                );
            }
        }
        println!();
    }

    println!("  Stopped watching.");
    Ok(())
}

fn chrono_time() -> String {
    use std::time::SystemTime;
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap();
    let secs = now.as_secs() % 86400;
    let hours = secs / 3600;
    let minutes = (secs % 3600) / 60;
    let seconds = secs % 60;
    format!("[{:02}:{:02}:{:02}]", hours, minutes, seconds)
}
