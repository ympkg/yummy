use anyhow::{bail, Result};
use console::style;

use crate::config;

pub fn execute(target: Option<String>) -> Result<()> {
    let (config_path, cfg) = config::load_or_find_config()?;
    let project = config::project_dir(&config_path);

    // Build first
    super::build::execute(target.clone(), false)?;

    if cfg.workspaces.is_some() {
        let target = target.as_deref().unwrap_or_else(|| {
            eprintln!("  In workspace mode, specify a target: ym jar <module>");
            std::process::exit(1);
        });
        let ws = crate::workspace::graph::WorkspaceGraph::build(&project)?;
        let pkg = ws.get_package(target).unwrap();
        return create_jar(&pkg.path, &pkg.config);
    }

    create_jar(&project, &cfg)
}

fn create_jar(project: &std::path::Path, cfg: &config::schema::YmConfig) -> Result<()> {
    let out = config::output_classes_dir(project);
    if !out.exists() {
        bail!("No compiled classes found. Run 'ym build' first.");
    }

    let release_dir = project.join("out");
    std::fs::create_dir_all(&release_dir)?;

    let jar_name = format!(
        "{}-{}.jar",
        cfg.name,
        cfg.version.as_deref().unwrap_or("0.0.0")
    );
    let jar_path = release_dir.join(&jar_name);

    // Create manifest
    let manifest_dir = std::env::temp_dir().join("ym-jar-manifest");
    std::fs::create_dir_all(&manifest_dir)?;
    let manifest_path = manifest_dir.join("MANIFEST.MF");

    let mut manifest = String::from("Manifest-Version: 1.0\n");
    if let Some(ref main) = cfg.main {
        manifest.push_str(&format!("Main-Class: {}\n", main));
    }
    manifest.push('\n');
    std::fs::write(&manifest_path, &manifest)?;

    let status = std::process::Command::new("jar")
        .arg("cfm")
        .arg(&jar_path)
        .arg(&manifest_path)
        .arg("-C")
        .arg(&out)
        .arg(".")
        .status()?;

    let _ = std::fs::remove_dir_all(&manifest_dir);

    if !status.success() {
        bail!("Failed to create JAR");
    }

    // Show file size
    let size = std::fs::metadata(&jar_path)?.len();
    let size_str = if size > 1024 * 1024 {
        format!("{:.1} MB", size as f64 / (1024.0 * 1024.0))
    } else if size > 1024 {
        format!("{:.1} KB", size as f64 / 1024.0)
    } else {
        format!("{} B", size)
    };

    println!(
        "  {} Created {} ({})",
        style("✓").green(),
        style(jar_path.display()).bold(),
        style(size_str).dim()
    );

    Ok(())
}
