use anyhow::Result;
use console::style;

use crate::config;

pub fn execute(json: bool) -> Result<()> {
    let (config_path, cfg) = config::load_or_find_config()?;
    let project = config::project_dir(&config_path);

    let src = config::source_dir(&project);
    let dep_count = cfg.dependencies.as_ref().map(|d| d.len()).unwrap_or(0);
    let dev_count = cfg.dev_dependencies.as_ref().map(|d| d.len()).unwrap_or(0);
    let ws_dep_count = cfg.workspace_dependencies.as_ref().map(|d| d.len()).unwrap_or(0);
    let source_count = count_files_with_ext(&src, "java");
    let out = config::output_classes_dir(&project);
    let class_count = count_files_with_ext(&out, "class");
    let engine = cfg.compiler.as_ref().and_then(|c| c.engine.as_deref()).unwrap_or("javac");
    let is_workspace = cfg.workspaces.is_some();

    if json {
        println!("{{");
        println!("  \"name\": \"{}\",", cfg.name);
        println!("  \"version\": \"{}\",", cfg.version.as_deref().unwrap_or(""));
        println!("  \"target\": \"{}\",", cfg.target.as_deref().unwrap_or(""));
        if let Some(ref m) = cfg.main {
            println!("  \"main\": \"{}\",", m);
        }
        println!("  \"compiler\": \"{}\",", engine);
        println!("  \"workspace\": {},", is_workspace);
        println!("  \"dependencies\": {},", dep_count);
        println!("  \"devDependencies\": {},", dev_count);
        println!("  \"workspaceDependencies\": {},", ws_dep_count);
        println!("  \"sourceFiles\": {},", source_count);
        println!("  \"classFiles\": {},", class_count);
        println!("  \"configPath\": \"{}\"", config_path.display());
        println!("}}");
        return Ok(());
    }

    println!();
    println!("  {} {}", style("name").dim(), style(&cfg.name).bold());

    if let Some(ref v) = cfg.version {
        println!("  {} {}", style("version").dim(), v);
    }

    if let Some(ref j) = cfg.target {
        println!("  {} {}", style("target").dim(), j);
    }

    if let Some(ref m) = cfg.main {
        println!("  {} {}", style("main").dim(), m);
    }

    println!(
        "  {} {}",
        style("source").dim(),
        src.strip_prefix(&project).unwrap_or(&src).display()
    );

    println!(
        "  {} {} dependencies, {} devDependencies",
        style("deps").dim(),
        dep_count,
        dev_count
    );

    if ws_dep_count > 0 {
        println!(
            "  {} {} workspace dependencies",
            style("ws").dim(),
            ws_dep_count
        );
    }

    if is_workspace {
        println!(
            "  {} {} (workspace root)",
            style("type").dim(),
            style("workspace").cyan()
        );
        if let Some(ref patterns) = cfg.workspaces {
            println!("  {} {}", style("patterns").dim(), patterns.join(", "));
        }
    }

    println!("  {} {}", style("compiler").dim(), engine);
    println!("  {} {}", style("config").dim(), config_path.display());

    if source_count > 0 {
        println!("  {} {} .java files", style("sources").dim(), source_count);
    }

    if class_count > 0 {
        println!("  {} {} .class files", style("compiled").dim(), class_count);
    }

    println!();
    Ok(())
}

fn count_files_with_ext(dir: &std::path::Path, ext: &str) -> usize {
    if !dir.exists() {
        return 0;
    }
    walkdir::WalkDir::new(dir)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some(ext))
        .count()
}
