use anyhow::{bail, Result};
use console::style;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::config;
use crate::hotreload;
use crate::scripts;
use crate::watcher::FileWatcher;
use crate::workspace::graph::WorkspaceGraph;

pub fn execute_with_options(target: Option<String>, no_reload: bool, program_args: Vec<String>) -> Result<()> {
    let (config_path, cfg) = config::load_or_find_config()?;
    let project = config::project_dir(&config_path);

    // Ensure JDK is available
    super::build::ensure_jdk_for_config(&cfg)?;

    // Run predev script
    scripts::run_script(&cfg.scripts, &cfg.env, "predev", &project)?;

    if cfg.workspaces.is_some() {
        let target = target.as_deref().unwrap_or_else(|| {
            eprintln!("  In workspace mode, specify a target: ym dev <module>");
            std::process::exit(1);
        });
        return dev_workspace(&project, target);
    }

    let display_name = target.as_deref().unwrap_or(&cfg.name);
    println!();
    println!("  {} {}", style("ym dev").bold(), display_name);
    println!();

    // Resolve dependencies
    let start = Instant::now();
    let jars = super::build::resolve_deps(&project, &cfg)?;
    let dep_count = jars.len();
    let resolve_time = start.elapsed();

    let ws_count = cfg
        .workspace_dependencies
        .as_ref()
        .map(|d| d.len())
        .unwrap_or(0);

    println!(
        "  {} Resolved dependencies ({} workspace + {} maven)     {:>4}ms",
        style("✓").green(),
        ws_count,
        dep_count,
        resolve_time.as_millis()
    );

    // Initial compile
    let compile_start = Instant::now();
    let result = super::build::compile_project(&project, &cfg, &jars)?;
    let compile_time = compile_start.elapsed();

    if !result.success {
        eprint!("{}", crate::compiler::colorize_errors(&result.errors));
        bail!("Compilation failed");
    }

    println!(
        "  {} Compiled {} ({} files)                         {:>6}ms",
        style("✓").green(),
        style(&cfg.name).bold(),
        result.files_compiled,
        compile_time.as_millis()
    );

    // Find main class
    let main_class = super::run::resolve_main_class(&cfg, &project, target.as_deref())?;

    // Build classpath
    let out_dir = config::output_classes_dir(&project);
    let mut classpath = vec![out_dir.clone()];
    classpath.extend(jars.clone());

    let mut jvm_args: Vec<String> = cfg.jvm_args.clone().unwrap_or_default();

    // Enable enhanced class redefinition on JBR (DCEVM built-in)
    if let Ok(java_home) = std::env::var("JAVA_HOME") {
        let home_lower = java_home.to_lowercase();
        if home_lower.contains("jbr") || home_lower.contains("jetbrains") {
            if !jvm_args.iter().any(|a| a.contains("AllowEnhancedClassRedefinition")) {
                jvm_args.push("-XX:+AllowEnhancedClassRedefinition".to_string());
            }
        }
    }

    // Try to attach hot reload agent
    let hot_reload_enabled = !no_reload
        && cfg
            .hot_reload
            .as_ref()
            .and_then(|h| h.enabled)
            .unwrap_or(true);

    let agent_port = if hot_reload_enabled {
        if let Some(agent_jar) = hotreload::find_agent_jar() {
            let port = hotreload::find_free_port()?;
            let agent_args = hotreload::agent_jvm_args(&agent_jar, port);
            jvm_args.extend(agent_args);
            println!(
                "  {} Hot reload agent on port {}",
                style("✓").green(),
                port
            );
            Some(port)
        } else {
            None
        }
    } else {
        None
    };

    // Start the Java process
    let run_start = Instant::now();
    let mut child = super::run::start_java_process_with_args(&main_class, &classpath, &jvm_args, &program_args)?;
    let run_time = run_start.elapsed();

    println!(
        "  {} Started {}                  {:>4.1}s",
        style("✓").green(),
        style(&main_class).bold(),
        run_time.as_secs_f64()
    );
    println!();

    // Set up file watcher
    let src_dir = config::source_dir(&project);
    let watch_extensions = cfg
        .hot_reload
        .as_ref()
        .and_then(|h| h.watch_extensions.clone())
        .unwrap_or_else(|| vec![".java".to_string()]);

    let file_count = count_source_files(&src_dir, &watch_extensions);

    println!(
        "  Watching {} source files for changes...",
        style(file_count).cyan()
    );
    println!();

    let watcher = FileWatcher::new(&[src_dir], watch_extensions)?;

    dev_watch_loop(watcher, &mut child, &main_class, &classpath, &jvm_args, &program_args, &project, &cfg, &jars, agent_port)
}

fn dev_workspace(root: &std::path::Path, target: &str) -> Result<()> {
    println!();
    println!("  {} {}", style("ym dev").bold(), target);
    println!();

    let ws = WorkspaceGraph::build(root)?;
    let packages = ws.transitive_closure(target)?;

    // Build all packages
    let start = Instant::now();
    super::build::execute(Some(target.to_string()), false)?;
    let _build_time = start.elapsed();

    // Build full classpath
    let mut classpath = Vec::new();
    let mut watch_dirs = Vec::new();
    let mut all_jars = Vec::new();

    for pkg_name in &packages {
        let pkg = ws.get_package(pkg_name).unwrap();
        classpath.push(config::output_classes_dir(&pkg.path));
        let jars = super::build::resolve_deps(&pkg.path, &pkg.config)?;
        all_jars.extend(jars);

        let src = config::source_dir(&pkg.path);
        if src.exists() {
            watch_dirs.push(src);
        }
    }
    classpath.extend(all_jars);

    let target_pkg = ws.get_package(target).unwrap();
    let main_class = super::run::resolve_main_class(&target_pkg.config, &target_pkg.path, None)?;
    let jvm_args = target_pkg.config.jvm_args.clone().unwrap_or_default();

    // Start Java process
    let run_start = Instant::now();
    let mut child = super::run::start_java_process(&main_class, &classpath, &jvm_args)?;
    let run_time = run_start.elapsed();

    println!(
        "  {} Started {}                  {:>4.1}s",
        style("✓").green(),
        style(&main_class).bold(),
        run_time.as_secs_f64()
    );
    println!();

    let watch_extensions = vec![".java".to_string()];
    let file_count: usize = watch_dirs
        .iter()
        .map(|d| count_source_files(d, &watch_extensions))
        .sum();

    println!(
        "  Watching {} source files for changes...",
        style(file_count).cyan()
    );
    println!();

    let watcher = FileWatcher::new(&watch_dirs, watch_extensions)?;

    // Recompile all workspace packages on change
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

        // Rebuild all packages in the closure
        let compile_start = Instant::now();
        let build_ok = super::build::execute(Some(target.to_string()), false).is_ok();
        let compile_time = compile_start.elapsed();

        if build_ok {
            let _ = child.kill();
            let _ = child.wait();
            child = super::run::start_java_process(&main_class, &classpath, &jvm_args)?;

            println!(
                "  {} Recompiled ({}ms) -> Restarted {}",
                style(&now).dim(),
                compile_time.as_millis(),
                style("✓").green()
            );
        } else {
            eprintln!(
                "  {} Compilation failed ({}ms)",
                style(&now).dim(),
                compile_time.as_millis()
            );
        }
    }

    println!();
    println!("  Stopping...");
    let _ = child.kill();
    let _ = child.wait();

    Ok(())
}

fn dev_watch_loop(
    watcher: FileWatcher,
    child: &mut std::process::Child,
    main_class: &str,
    classpath: &[std::path::PathBuf],
    jvm_args: &[String],
    program_args: &[String],
    project: &std::path::Path,
    cfg: &config::schema::YmConfig,
    jars: &[std::path::PathBuf],
    agent_port: Option<u16>,
) -> Result<()> {
    let running = Arc::new(AtomicBool::new(true));
    let r = running.clone();
    ctrlc::set_handler(move || {
        r.store(false, Ordering::SeqCst);
    })?;

    let agent_client = agent_port.map(hotreload::AgentClient::new);

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

        let compile_start = Instant::now();
        let result = super::build::compile_project(project, cfg, jars)?;
        let compile_time = compile_start.elapsed();

        if !result.success {
            eprintln!(
                "  {} Compilation failed ({}ms)",
                style(&now).dim(),
                compile_time.as_millis()
            );
            eprint!("{}", crate::compiler::colorize_errors(&result.errors));
            continue;
        }

        // Try hot reload via agent (only if process is still running)
        let process_alive = child.try_wait().ok().flatten().is_none();
        if process_alive {
            if let Some(ref client) = agent_client {
                let class_names = extract_class_names(&changed, project);
                if !class_names.is_empty() {
                    let out_dir = config::output_classes_dir(project);
                    match client.reload(&out_dir, &class_names) {
                        Ok(reload_result) if reload_result.success => {
                            println!(
                                "  {} Compiled {} file(s) ({}ms) -> {} {}",
                                style(&now).dim(),
                                result.files_compiled,
                                compile_time.as_millis(),
                                reload_result.strategy,
                                style("✓").green()
                            );
                            continue;
                        }
                        Ok(reload_result) => {
                            eprintln!(
                                "  {} Hot reload failed: {} (falling back to restart)",
                                style("!").yellow(),
                                reload_result.error.as_deref().unwrap_or("unknown")
                            );
                        }
                        Err(e) => {
                            eprintln!(
                                "  {} Agent unreachable: {} (falling back to restart)",
                                style("!").yellow(),
                                e
                            );
                        }
                    }
                }
            }
        }

        // Fall back to restart
        let _ = child.kill();
        let _ = child.wait();

        *child = super::run::start_java_process_with_args(main_class, classpath, jvm_args, program_args)?;

        println!(
            "  {} Compiled {} file(s) ({}ms) -> Restarted {}",
            style(&now).dim(),
            result.files_compiled,
            compile_time.as_millis(),
            style("✓").green()
        );
    }

    println!();
    println!("  Stopping...");
    let _ = child.kill();
    let _ = child.wait();

    Ok(())
}

/// Extract Java class names from changed file paths.
fn extract_class_names(changed_files: &[std::path::PathBuf], project: &std::path::Path) -> Vec<String> {
    let src_dir = config::source_dir(project);
    changed_files
        .iter()
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("java"))
        .filter_map(|p| {
            p.strip_prefix(&src_dir).ok().map(|rel| {
                rel.to_string_lossy()
                    .replace('/', ".")
                    .replace('\\', ".")
                    .trim_end_matches(".java")
                    .to_string()
            })
        })
        .collect()
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

fn count_source_files(dir: &std::path::Path, extensions: &[String]) -> usize {
    if !dir.exists() {
        return 0;
    }
    walkdir::WalkDir::new(dir)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| {
            if let Some(ext) = e.path().extension().and_then(|e| e.to_str()) {
                let dot_ext = format!(".{}", ext);
                extensions.iter().any(|x| x == &dot_ext || x == ext)
            } else {
                false
            }
        })
        .count()
}
