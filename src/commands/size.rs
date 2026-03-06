use anyhow::Result;
use console::style;

use crate::config;

pub fn execute(target: Option<String>) -> Result<()> {
    let (config_path, cfg) = config::load_or_find_config()?;
    let project = config::project_dir(&config_path);

    if cfg.workspaces.is_some() {
        return size_workspace(&project, target.as_deref());
    }

    size_single(&project, &cfg)
}

fn size_single(project: &std::path::Path, cfg: &config::schema::YmConfig) -> Result<()> {
    println!();
    println!("  {}", style(&cfg.name).bold());
    println!();

    // Source files
    let src = config::source_dir_for(project, cfg);
    if src.exists() {
        let (file_count, total_bytes) = count_dir(&src, Some("java"));
        println!(
            "  {} {} source files ({})",
            style("src").dim(),
            file_count,
            format_size(total_bytes)
        );
    }

    // Test files
    let test = config::test_dir_for(project, cfg);
    if test.exists() {
        let (file_count, total_bytes) = count_dir(&test, Some("java"));
        println!(
            "  {} {} test files ({})",
            style("test").dim(),
            file_count,
            format_size(total_bytes)
        );
    }

    // Compiled classes
    let out = config::output_classes_dir(project);
    if out.exists() {
        let (file_count, total_bytes) = count_dir(&out, Some("class"));
        let (_, total_all) = count_dir(&out, None);
        println!(
            "  {} {} class files ({})",
            style("classes").dim(),
            file_count,
            format_size(total_bytes)
        );
        if total_all > total_bytes {
            println!(
                "  {} {} total output (including resources)",
                style("output").dim(),
                format_size(total_all)
            );
        }
    }

    // JAR artifacts in out/
    let out_dir = project.join("out");
    if out_dir.exists() {
        let (jar_count, jar_bytes) = count_dir(&out_dir, Some("jar"));
        if jar_count > 0 {
            println!(
                "  {} {} JAR file(s) ({})",
                style("jars").dim(),
                jar_count,
                format_size(jar_bytes)
            );
        }
    }

    // Dependencies
    let lock_path = project.join(config::LOCK_FILE);
    if lock_path.exists() {
        if let Ok(lock) = config::load_lock(&lock_path) {
            let cache = config::maven_cache_dir(project);
            let mut dep_size: u64 = 0;
            let mut dep_count: usize = 0;
            for key in lock.dependencies.keys() {
                let parts: Vec<&str> = key.split(':').collect();
                if parts.len() == 3 {
                    let jar = cache
                        .join(parts[0])
                        .join(parts[1])
                        .join(parts[2])
                        .join(format!("{}-{}.jar", parts[1], parts[2]));
                    if jar.exists() {
                        if let Ok(meta) = std::fs::metadata(&jar) {
                            dep_size += meta.len();
                            dep_count += 1;
                        }
                    }
                }
            }
            if dep_count > 0 {
                println!(
                    "  {} {} dependency JARs ({})",
                    style("deps").dim(),
                    dep_count,
                    format_size(dep_size)
                );
            }
        }
    }

    // Cache
    let cache_dir = config::cache_dir(project);
    if cache_dir.exists() {
        let (_, cache_total) = count_dir(&cache_dir, None);
        println!(
            "  {} {} total cache",
            style("cache").dim(),
            format_size(cache_total)
        );
    }

    println!();
    Ok(())
}

fn size_workspace(root: &std::path::Path, target: Option<&str>) -> Result<()> {
    let ws = crate::workspace::graph::WorkspaceGraph::build(root)?;

    let packages = if let Some(target) = target {
        ws.transitive_closure(target)?
    } else {
        ws.all_packages()
    };

    println!();
    let mut total_src = 0u64;
    let mut total_classes = 0u64;

    for pkg_name in &packages {
        let pkg = ws.get_package(pkg_name).unwrap();
        let src = config::source_dir(&pkg.path);
        let out = config::output_classes_dir(&pkg.path);

        let (src_count, src_bytes) = if src.exists() {
            count_dir(&src, Some("java"))
        } else {
            (0, 0)
        };

        let (class_count, class_bytes) = if out.exists() {
            count_dir(&out, Some("class"))
        } else {
            (0, 0)
        };

        total_src += src_bytes;
        total_classes += class_bytes;

        println!(
            "  {} {} src ({}) / {} classes ({})",
            style(pkg_name).bold(),
            src_count,
            format_size(src_bytes),
            class_count,
            format_size(class_bytes)
        );
    }

    println!();
    println!(
        "  {} total: {} source, {} compiled",
        style("■").cyan(),
        format_size(total_src),
        format_size(total_classes)
    );
    println!();

    Ok(())
}

fn count_dir(dir: &std::path::Path, ext_filter: Option<&str>) -> (usize, u64) {
    let mut count = 0usize;
    let mut total = 0u64;

    if let Ok(entries) = walkdir::WalkDir::new(dir).into_iter().collect::<Result<Vec<_>, _>>() {
        for entry in entries {
            if !entry.file_type().is_file() {
                continue;
            }
            if let Some(ext) = ext_filter {
                if entry.path().extension().and_then(|e| e.to_str()) != Some(ext) {
                    continue;
                }
            }
            if let Ok(meta) = entry.metadata() {
                total += meta.len();
                count += 1;
            }
        }
    }

    (count, total)
}

fn format_size(bytes: u64) -> String {
    if bytes >= 1024 * 1024 * 1024 {
        format!("{:.1} GB", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
    } else if bytes >= 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    } else if bytes >= 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{} B", bytes)
    }
}
