use anyhow::Result;
use std::path::PathBuf;

use crate::config;
use crate::workspace::graph::WorkspaceGraph;

pub fn execute(target: Option<String>) -> Result<()> {
    let (config_path, cfg) = config::load_or_find_config()?;
    let project = config::project_dir(&config_path);

    if cfg.workspaces.is_some() {
        let target = target.as_deref().unwrap_or_else(|| {
            eprintln!("  In workspace mode, specify a target: ym classpath <module>");
            std::process::exit(1);
        });
        return print_workspace_classpath(&project, target);
    }

    let jars = super::build::resolve_deps(&project, &cfg)?;
    let out = config::output_classes_dir(&project);
    let mut classpath = vec![out];
    classpath.extend(jars);

    print_classpath(&classpath);
    Ok(())
}

fn print_workspace_classpath(root: &std::path::Path, target: &str) -> Result<()> {
    let ws = WorkspaceGraph::build(root)?;
    let packages = ws.transitive_closure(target)?;

    let mut classpath: Vec<PathBuf> = Vec::new();
    for pkg_name in &packages {
        let pkg = ws.get_package(pkg_name).unwrap();
        classpath.push(config::output_classes_dir(&pkg.path));
        let jars = super::build::resolve_deps(&pkg.path, &pkg.config)?;
        classpath.extend(jars);
    }

    print_classpath(&classpath);
    Ok(())
}

fn print_classpath(classpath: &[PathBuf]) {
    let sep = if cfg!(windows) { ";" } else { ":" };
    let cp = classpath
        .iter()
        .map(|p| p.to_string_lossy().to_string())
        .collect::<Vec<_>>()
        .join(sep);
    println!("{}", cp);
}
