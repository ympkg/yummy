use anyhow::Result;
use console::style;

use crate::config;

pub fn execute(json: bool) -> Result<()> {
    let (config_path, cfg) = config::load_or_find_config()?;
    let project = config::project_dir(&config_path);

    let src = config::source_dir(&project);
    let dep_count = cfg.dependencies.as_ref().map(|d| d.len()).unwrap_or(0);
    let maven_count = cfg.maven_dependencies().len();
    let ws_module_count = cfg.workspace_module_deps().len();
    let source_count = count_files_with_ext(&src, "java");
    let out = config::output_classes_dir(&project);
    let class_count = count_files_with_ext(&out, "class");
    let is_workspace = cfg.workspaces.is_some();

    let ym_version = env!("CARGO_PKG_VERSION");
    let java_version = detect_java_version();
    let java_home = std::env::var("JAVA_HOME").unwrap_or_else(|_| "(not set)".to_string());
    let os_info = format!("{} {}", std::env::consts::OS, std::env::consts::ARCH);

    if json {
        println!("{{");
        println!("  \"name\": \"{}\",", cfg.name);
        println!("  \"version\": \"{}\",", cfg.version.as_deref().unwrap_or(""));
        println!("  \"groupId\": \"{}\",", cfg.group_id);
        println!("  \"target\": \"{}\",", cfg.target.as_deref().unwrap_or(""));
        if let Some(ref m) = cfg.main {
            println!("  \"main\": \"{}\",", m);
        }
        println!("  \"workspace\": {},", is_workspace);
        println!("  \"dependencies\": {},", dep_count);
        println!("  \"mavenDependencies\": {},", maven_count);
        // Count by scope for JSON
        let mut json_scopes: std::collections::BTreeMap<&str, usize> = std::collections::BTreeMap::new();
        if let Some(ref deps) = cfg.dependencies {
            for (coord, value) in deps {
                if crate::config::schema::is_maven_dep(coord) && !value.is_workspace() {
                    *json_scopes.entry(value.scope()).or_insert(0) += 1;
                }
            }
        }
        print!("  \"dependenciesByScope\": {{");
        let scope_entries: Vec<String> = json_scopes.iter().map(|(k, v)| format!("\"{}\": {}", k, v)).collect();
        print!("{}", scope_entries.join(", "));
        println!("}},");
        println!("  \"workspaceModuleDeps\": {},", ws_module_count);
        println!("  \"sourceFiles\": {},", source_count);
        println!("  \"classFiles\": {},", class_count);
        println!("  \"ymVersion\": \"{}\",", ym_version);
        println!("  \"javaVersion\": \"{}\",", java_version);
        println!("  \"javaHome\": \"{}\",", java_home);
        println!("  \"os\": \"{}\",", os_info);
        println!("  \"configPath\": \"{}\"", config_path.display());
        println!("}}");
        return Ok(());
    }

    println!();
    println!("  {} {}", style("name").dim(), style(&cfg.name).bold());

    if let Some(ref v) = cfg.version {
        println!("  {} {}", style("version").dim(), v);
    }

    println!("  {} {}", style("groupId").dim(), cfg.group_id);

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

    // Count dependencies by scope
    let mut scope_counts: std::collections::BTreeMap<&str, usize> = std::collections::BTreeMap::new();
    if let Some(ref deps) = cfg.dependencies {
        for (coord, value) in deps {
            if crate::config::schema::is_maven_dep(coord) && !value.is_workspace() {
                *scope_counts.entry(value.scope()).or_insert(0) += 1;
            }
        }
    }
    let scope_summary: Vec<String> = scope_counts
        .iter()
        .map(|(scope, count)| format!("{} {}", count, scope))
        .collect();
    let scope_str = if scope_summary.is_empty() {
        String::new()
    } else {
        format!(" ({})", scope_summary.join(", "))
    };

    println!(
        "  {} {} dependencies ({} maven{}, {} workspace modules)",
        style("deps").dim(),
        dep_count,
        maven_count,
        scope_str,
        ws_module_count
    );

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

    println!("  {} {}", style("config").dim(), config_path.display());

    if source_count > 0 {
        println!("  {} {} .java files", style("sources").dim(), source_count);
    }

    if class_count > 0 {
        println!("  {} {} .class files", style("compiled").dim(), class_count);
    }

    println!();
    println!("  {} {}", style("ym").dim(), ym_version);
    println!("  {} {}", style("java").dim(), java_version);
    println!("  {} {}", style("JAVA_HOME").dim(), java_home);
    println!("  {} {}", style("os").dim(), os_info);

    println!();
    Ok(())
}

fn detect_java_version() -> String {
    match std::process::Command::new("java").arg("-version").output() {
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            stderr.lines().next().unwrap_or("unknown").trim().to_string()
        }
        Err(_) => "not found".to_string(),
    }
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
