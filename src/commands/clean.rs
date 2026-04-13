use anyhow::Result;
use console::style;

use crate::config;

/// Clean build outputs and incremental-compilation fingerprints.
/// Invoked internally by `ymc build --clean`; user-facing dependency-cache
/// cleanup lives in `cache_clean` and is exposed as `ym cache clean`.
pub fn execute() -> Result<()> {
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

    // Incremental compiler fingerprints must be cleared when out/ is deleted,
    // otherwise the incremental compiler sees stale fingerprints + empty
    // out/classes and incorrectly returns UpToDate without restoring from cache.
    let cache = config::cache_dir(&project);
    if cache.exists() {
        if let Ok(entries) = std::fs::read_dir(&cache) {
            for entry in entries.flatten() {
                let name = entry.file_name();
                let name_str = name.to_string_lossy();
                if name_str.starts_with("workspace-build-fingerprint-")
                    || name_str.starts_with("workspace-module-fps-")
                {
                    let _ = std::fs::remove_file(entry.path());
                }
            }
        }
        let fp_dir = cache.join("fingerprints");
        if fp_dir.exists() {
            let _ = std::fs::remove_dir_all(&fp_dir);
        }
    }

    println!("  {} Clean complete", style("✓").green());
    Ok(())
}
