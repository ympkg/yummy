use anyhow::Result;
use console::style;
use std::path::{Path, PathBuf};

use crate::config;
use crate::workspace::graph::WorkspaceGraph;

pub fn execute(target: Option<String>) -> Result<()> {
    let (config_path, cfg) = config::load_or_find_config()?;
    let project = config::project_dir(&config_path);

    if cfg.workspaces.is_some() {
        let ws = WorkspaceGraph::build(&project)?;

        let packages = if let Some(ref target) = target {
            ws.transitive_closure(target)?
        } else {
            let mut all = ws.all_packages();
            all.sort();
            all
        };

        generate_workspace_vscode(&project, &packages, &ws)?;

        println!(
            "  {} Generated VSCode workspace ({} modules)",
            style("✓").green(),
            packages.len()
        );
    } else {
        generate_single_vscode(&project, &cfg)?;

        println!(
            "  {} Generated VSCode settings for {}",
            style("✓").green(),
            style(&cfg.name).bold()
        );
    }

    println!("  Open this directory in VSCode to get started.");
    Ok(())
}

fn generate_single_vscode(project: &Path, cfg: &config::schema::YmConfig) -> Result<()> {
    let vscode_dir = project.join(".vscode");
    std::fs::create_dir_all(&vscode_dir)?;

    let jars = super::build::resolve_deps(project, cfg)?;
    let source_paths = detect_source_paths(project);
    let test_paths = detect_test_paths(project);
    let java_version = cfg.target.as_deref().unwrap_or("21");

    let mut lib_entries: Vec<String> = jars
        .iter()
        .map(|jar| format!("    \"{}\"", jar.to_string_lossy()))
        .collect();
    lib_entries.sort();
    lib_entries.dedup();

    let settings = format_settings(&source_paths, &test_paths, &lib_entries, java_version);
    std::fs::write(vscode_dir.join("settings.json"), settings)?;

    Ok(())
}

fn generate_workspace_vscode(
    root: &Path,
    packages: &[String],
    ws: &WorkspaceGraph,
) -> Result<()> {
    let vscode_dir = root.join(".vscode");
    std::fs::create_dir_all(&vscode_dir)?;

    let root_cfg = &ws.get_package(packages.first().unwrap_or(&String::new()))
        .map(|p| &p.config);
    let java_version = root_cfg
        .and_then(|c| c.target.as_deref())
        .unwrap_or("21");

    let mut all_source_paths = Vec::new();
    let mut all_test_paths = Vec::new();
    let mut all_jars: Vec<PathBuf> = Vec::new();

    for pkg_name in packages {
        let pkg = ws.get_package(pkg_name).unwrap();
        let rel_path = pkg.path.strip_prefix(root)
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|_| pkg.path.to_string_lossy().to_string());

        // Source paths relative to root
        for sp in detect_source_paths(&pkg.path) {
            let full = format!("{}/{}", rel_path, sp);
            all_source_paths.push(full);
        }
        for tp in detect_test_paths(&pkg.path) {
            let full = format!("{}/{}", rel_path, tp);
            all_test_paths.push(full);
        }

        // Resolve deps
        let jars = super::build::resolve_deps(&pkg.path, &pkg.config)?;
        all_jars.extend(jars);

        // Add workspace module output dirs to classpath
        for dep in pkg.config.workspace_module_deps() {
            if let Some(dep_pkg) = ws.get_package(&dep) {
                let classes_dir = dep_pkg.path.join("out").join("classes");
                all_jars.push(classes_dir);
            }
        }
    }

    all_jars.sort();
    all_jars.dedup();

    let mut lib_entries: Vec<String> = all_jars
        .iter()
        .map(|jar| format!("    \"{}\"", jar.to_string_lossy()))
        .collect();
    lib_entries.sort();
    lib_entries.dedup();

    let settings = format_settings(&all_source_paths, &all_test_paths, &lib_entries, java_version);
    std::fs::write(vscode_dir.join("settings.json"), settings)?;

    Ok(())
}

fn format_settings(
    source_paths: &[String],
    test_paths: &[String],
    lib_entries: &[String],
    java_version: &str,
) -> String {
    let mut all_sources: Vec<String> = source_paths
        .iter()
        .chain(test_paths.iter())
        .map(|s| format!("    \"{}\"", s))
        .collect();
    all_sources.sort();

    format!(
        r#"{{
  "java.project.sourcePaths": [
{}
  ],
  "java.project.outputPath": "out/classes",
  "java.project.referencedLibraries": [
{}
  ],
  "java.configuration.updateBuildConfiguration": "automatic",
  "java.compile.nullAnalysis.mode": "disabled",
  "java.jdt.ls.java.home": "",
  "java.configuration.runtimes": [
    {{
      "name": "JavaSE-{}",
      "default": true
    }}
  ]
}}
"#,
        all_sources.join(",\n"),
        lib_entries.join(",\n"),
        java_version,
    )
}

fn detect_source_paths(project: &Path) -> Vec<String> {
    let maven_src = project.join("src").join("main").join("java");
    if maven_src.exists() {
        let mut paths = vec!["src/main/java".to_string()];
        let maven_res = project.join("src").join("main").join("resources");
        if maven_res.exists() {
            paths.push("src/main/resources".to_string());
        }
        paths
    } else if project.join("src").exists() {
        vec!["src".to_string()]
    } else {
        vec!["src".to_string()]
    }
}

fn detect_test_paths(project: &Path) -> Vec<String> {
    let maven_test = project.join("src").join("test").join("java");
    if maven_test.exists() {
        let mut paths = vec!["src/test/java".to_string()];
        let maven_test_res = project.join("src").join("test").join("resources");
        if maven_test_res.exists() {
            paths.push("src/test/resources".to_string());
        }
        paths
    } else if project.join("test").exists() {
        vec!["test".to_string()]
    } else {
        vec![]
    }
}
