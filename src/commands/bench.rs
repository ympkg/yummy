use anyhow::{bail, Result};
use console::style;
use std::path::PathBuf;

use crate::config;

/// JMH Maven coordinates
const JMH_GROUP: &str = "org.openjdk.jmh";
const JMH_CORE: &str = "jmh-core";
const JMH_ANNPROCESS: &str = "jmh-generator-annprocess";
const JMH_VERSION: &str = "1.37";

pub fn execute(target: Option<String>, filter: Option<String>) -> Result<()> {
    let (config_path, cfg) = config::load_or_find_config()?;
    let project = config::project_dir(&config_path);

    if cfg.workspaces.is_some() {
        let target = target.as_deref().unwrap_or_else(|| {
            eprintln!("  In workspace mode, specify a target: ym bench <module>");
            std::process::exit(1);
        });
        return bench_workspace(&project, target, filter.as_deref());
    }

    // Resolve project deps + JMH deps
    let mut jars = super::build::resolve_deps(&project, &cfg)?;
    let jmh_jars = ensure_jmh_jars(&project)?;
    jars.extend(jmh_jars);

    // Compile with JMH on classpath (including annotation processor)
    let result = super::build::compile_project(&project, &cfg, &jars)?;
    if !result.success {
        eprint!("{}", crate::compiler::colorize_errors(&result.errors));
        bail!("Compilation failed");
    }

    println!(
        "  {} Compiled {} ({} files)",
        style("✓").green(),
        style(&cfg.name).bold(),
        result.files_compiled
    );

    // Run JMH
    let out = config::output_classes_dir(&project);
    let mut classpath = vec![out];
    classpath.extend(jars);

    run_jmh(&classpath, &cfg.jvm_args.clone().unwrap_or_default(), filter.as_deref())
}

fn bench_workspace(root: &std::path::Path, target: &str, filter: Option<&str>) -> Result<()> {
    // Build the target first
    super::build::execute(Some(target.to_string()), false)?;

    let ws = crate::workspace::graph::WorkspaceGraph::build(root)?;
    let packages = ws.transitive_closure(target)?;

    let mut classpath: Vec<PathBuf> = Vec::new();
    for pkg_name in &packages {
        let pkg = ws.get_package(pkg_name).unwrap();
        classpath.push(config::output_classes_dir(&pkg.path));
        let jars = super::build::resolve_deps(&pkg.path, &pkg.config)?;
        classpath.extend(jars);
    }

    // Add JMH jars
    let jmh_jars = ensure_jmh_jars(root)?;
    classpath.extend(jmh_jars);

    let target_pkg = ws.get_package(target).unwrap();
    let jvm_args = target_pkg.config.jvm_args.clone().unwrap_or_default();

    run_jmh(&classpath, &jvm_args, filter)
}

fn run_jmh(classpath: &[PathBuf], jvm_args: &[String], filter: Option<&str>) -> Result<()> {
    let sep = if cfg!(windows) { ";" } else { ":" };
    let cp = classpath
        .iter()
        .map(|p| p.to_string_lossy().to_string())
        .collect::<Vec<_>>()
        .join(sep);

    println!(
        "  {} Running JMH benchmarks...",
        style("▶").cyan()
    );
    println!();

    let mut cmd = std::process::Command::new("java");
    cmd.args(jvm_args)
        .arg("-cp")
        .arg(&cp)
        .arg("org.openjdk.jmh.Main");

    if let Some(f) = filter {
        cmd.arg(f);
    }

    let status = cmd.status()?;

    if !status.success() {
        bail!("JMH benchmark failed");
    }

    Ok(())
}

/// Ensure JMH jars are in the Maven cache, download if needed
fn ensure_jmh_jars(project: &std::path::Path) -> Result<Vec<PathBuf>> {
    let cache = config::maven_cache_dir(project);

    let artifacts = [
        (JMH_GROUP, JMH_CORE, JMH_VERSION),
        (JMH_GROUP, JMH_ANNPROCESS, JMH_VERSION),
    ];

    let mut jars = Vec::new();

    for (group, artifact, version) in &artifacts {
        let jar_dir = cache.join(group).join(artifact).join(version);
        let jar_path = jar_dir.join(format!("{}-{}.jar", artifact, version));

        if !jar_path.exists() {
            std::fs::create_dir_all(&jar_dir)?;
            let url = format!(
                "https://repo1.maven.org/maven2/{}/{}/{}/{}-{}.jar",
                group.replace('.', "/"),
                artifact,
                version,
                artifact,
                version
            );

            println!(
                "  {} Downloading {}:{}:{}",
                style("↓").blue(),
                group,
                artifact,
                version
            );

            let client = reqwest::blocking::Client::builder()
                .user_agent("ym/0.1.0")
                .timeout(std::time::Duration::from_secs(60))
                .build()?;

            let resp = client.get(&url).send()?;
            if !resp.status().is_success() {
                bail!(
                    "Failed to download JMH jar: {} (HTTP {})",
                    url,
                    resp.status()
                );
            }

            let bytes = resp.bytes()?;
            std::fs::write(&jar_path, &bytes)?;
        }

        jars.push(jar_path);
    }

    Ok(jars)
}
