use anyhow::{bail, Result};
use console::style;
use rayon::prelude::*;
use std::path::{Path, PathBuf};
use std::time::Instant;

use crate::compiler;
use crate::compiler::incremental;
use crate::compiler::javac;
use crate::config;
use crate::config::schema::YmConfig;
use crate::jvm;
use crate::resources;
use crate::scripts;
use crate::workspace::graph::WorkspaceGraph;

/// Custom output directory override (set via --output flag)
static OUTPUT_DIR_OVERRIDE: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// Verbose mode flag
static VERBOSE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Set the number of parallel compilation threads.
/// Configures rayon's global thread pool.
pub fn set_parallelism(threads: usize) {
    let threads = threads.max(1);
    let _ = rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .build_global();
}

/// Set a custom output directory for compiled classes.
pub fn set_output_dir(dir: &str) {
    let _ = OUTPUT_DIR_OVERRIDE.set(dir.to_string());
}

/// Enable verbose build output.
pub fn set_verbose(v: bool) {
    VERBOSE.store(v, std::sync::atomic::Ordering::Relaxed);
}

/// Check if verbose mode is enabled.
pub fn is_verbose() -> bool {
    VERBOSE.load(std::sync::atomic::Ordering::Relaxed)
}

/// Build with per-phase timing breakdown
pub fn execute_with_profile(_target: Option<String>, release: bool) -> Result<()> {
    let total_start = Instant::now();

    let (config_path, cfg) = config::load_or_find_config()?;
    let project = config::project_dir(&config_path);

    println!();
    println!("  {}", style("Build Profile").bold().underlined());
    println!();

    // Phase 1: Config loading (already done above)
    let config_time = total_start.elapsed();
    println!(
        "  {} Config loading                               {:>6}ms",
        style("■").dim(),
        config_time.as_millis()
    );

    // Phase 2: JDK check
    let jdk_start = Instant::now();
    ensure_jdk_for_config(&cfg)?;
    let jdk_time = jdk_start.elapsed();
    println!(
        "  {} JDK verification                             {:>6}ms",
        style("■").dim(),
        jdk_time.as_millis()
    );

    // Phase 3: Dependency resolution
    let dep_start = Instant::now();
    let jars = resolve_deps(&project, &cfg)?;
    let dep_time = dep_start.elapsed();
    println!(
        "  {} Dependency resolution ({} jars)            {:>6}ms",
        style("■").dim(),
        jars.len(),
        dep_time.as_millis()
    );

    // Phase 4: Compilation
    let compile_start = Instant::now();
    let result = compile_project(&project, &cfg, &jars)?;
    let compile_time = compile_start.elapsed();

    if !result.success {
        eprint!("{}", compiler::colorize_errors(&result.errors));
        bail!("Compilation failed");
    }

    println!(
        "  {} Compilation ({} files)                     {:>6}ms",
        style("■").dim(),
        result.files_compiled,
        compile_time.as_millis()
    );

    // Phase 5: Release JAR (if requested)
    if release {
        let jar_start = Instant::now();
        build_release_jar(&project, &cfg, &jars)?;
        let jar_time = jar_start.elapsed();
        println!(
            "  {} Release JAR packaging                        {:>6}ms",
            style("■").dim(),
            jar_time.as_millis()
        );
    }

    scripts::run_script(&cfg.scripts, &cfg.env, "postbuild", &project)?;

    let total = total_start.elapsed();
    println!();
    println!(
        "  {} Total                                        {:>6}ms",
        style("■").cyan().bold(),
        total.as_millis()
    );
    println!();

    Ok(())
}

pub fn execute(target: Option<String>, release: bool) -> Result<()> {
    let total_start = Instant::now();

    let (config_path, cfg) = config::load_or_find_config()?;
    let project = config::project_dir(&config_path);

    // Run prebuild script
    scripts::run_script(&cfg.scripts, &cfg.env, "prebuild", &project)?;

    // Ensure JDK is available
    ensure_jdk_for_config(&cfg)?;

    if cfg.workspaces.is_some() {
        let result = build_workspace(&project, &cfg, target.as_deref(), release);
        scripts::run_script(&cfg.scripts, &cfg.env, "postbuild", &project)?;
        print_total_time(total_start);
        return result;
    }

    // Single project mode
    let start = Instant::now();
    let jars = resolve_deps(&project, &cfg)?;
    let resolve_time = start.elapsed();

    let compile_start = Instant::now();
    let result = compile_project(&project, &cfg, &jars)?;
    let compile_time = compile_start.elapsed();

    if !result.success {
        eprint!("{}", compiler::colorize_errors(&result.errors));
        bail!("Compilation failed");
    }

    // In verbose mode, show all compiler output (including warnings)
    if is_verbose() && !result.errors.is_empty() {
        eprint!("{}", compiler::colorize_errors(&result.errors));
    }

    println!(
        "  {} Resolved dependencies                          {:>6}ms",
        style("✓").green(),
        resolve_time.as_millis()
    );

    if result.files_compiled == 0 {
        println!(
            "  {} {} is up to date",
            style("✓").green(),
            style(&cfg.name).bold()
        );
    } else {
        println!(
            "  {} Compiled {} ({} files)                         {:>6}ms",
            style("✓").green(),
            style(&cfg.name).bold(),
            result.files_compiled,
            compile_time.as_millis()
        );
    }

    // Build release fat JAR if requested
    if release {
        build_release_jar(&project, &cfg, &jars)?;
    }

    // Run postbuild script
    scripts::run_script(&cfg.scripts, &cfg.env, "postbuild", &project)?;

    print_total_time(total_start);
    Ok(())
}

fn print_total_time(start: Instant) {
    let elapsed = start.elapsed();
    let ms = elapsed.as_millis();
    if ms > 1000 {
        println!(
            "\n  {} Done in {:.1}s",
            style("⚡").cyan(),
            elapsed.as_secs_f64()
        );
    } else {
        println!(
            "\n  {} Done in {}ms",
            style("⚡").cyan(),
            ms
        );
    }
}

fn build_workspace(root: &Path, _cfg: &YmConfig, target: Option<&str>, release: bool) -> Result<()> {
    let ws = WorkspaceGraph::build(root)?;

    let packages = if let Some(target) = target {
        ws.transitive_closure(target)?
    } else {
        // Build all in topological order
        let mut all = Vec::new();
        for name in ws.all_packages() {
            if !all.contains(&name) {
                let closure = ws.transitive_closure(&name)?;
                for pkg in closure {
                    if !all.contains(&pkg) {
                        all.push(pkg);
                    }
                }
            }
        }
        all
    };

    // Group into topological levels for parallel compilation
    let levels = compute_parallel_levels(&packages, &ws);

    let mut workspace_classpath: Vec<PathBuf> = Vec::new();

    for level in &levels {
        if level.len() == 1 {
            // Single package: compile normally
            let pkg_name = &level[0];
            let pkg = ws.get_package(pkg_name).unwrap();
            let start = Instant::now();

            let jars = resolve_deps(&pkg.path, &pkg.config)?;
            let mut classpath = jars;
            classpath.extend(workspace_classpath.clone());

            let result = compile_project(&pkg.path, &pkg.config, &classpath)?;
            let elapsed = start.elapsed();

            if !result.success {
                eprintln!("{}", result.errors);
                bail!("Compilation of '{}' failed", pkg_name);
            }

            print_compile_result(pkg_name, &result, elapsed);
            workspace_classpath.push(config::output_classes_dir(&pkg.path));
        } else {
            // Multiple packages at same level: compile in parallel with rayon
            let start = Instant::now();
            let cp_snapshot = workspace_classpath.clone();

            let results: Vec<(String, Result<compiler::CompileResult>)> = level
                .par_iter()
                .map(|pkg_name| {
                    let pkg = ws.get_package(pkg_name).unwrap();
                    let jars = resolve_deps(&pkg.path, &pkg.config).unwrap_or_default();
                    let mut classpath = jars;
                    classpath.extend(cp_snapshot.clone());
                    let result = compile_project(&pkg.path, &pkg.config, &classpath);
                    (pkg_name.clone(), result)
                })
                .collect();

            let elapsed = start.elapsed();

            for (pkg_name, result) in &results {
                match result {
                    Ok(r) if r.success => {
                        print_compile_result(pkg_name, r, elapsed);
                    }
                    Ok(r) => {
                        eprint!("{}", compiler::colorize_errors(&r.errors));
                        bail!("Compilation of '{}' failed", pkg_name);
                    }
                    Err(e) => bail!("Compilation of '{}' failed: {}", pkg_name, e),
                }
            }

            for pkg_name in level {
                let pkg = ws.get_package(pkg_name).unwrap();
                workspace_classpath.push(config::output_classes_dir(&pkg.path));
            }
        }
    }

    // Build release JAR for the target package if requested
    if release {
        if let Some(target) = target {
            let pkg = ws.get_package(target).unwrap();
            // Collect all workspace dep classes + maven jars
            let mut all_deps = Vec::new();
            for pkg_name in &packages {
                let p = ws.get_package(pkg_name).unwrap();
                if pkg_name != target {
                    // Include classes from workspace deps
                    all_deps.push(config::output_classes_dir(&p.path));
                }
                let dep_jars = resolve_deps(&p.path, &p.config)?;
                all_deps.extend(dep_jars);
            }
            build_release_jar(&pkg.path, &pkg.config, &all_deps)?;
        } else {
            println!(
                "  {} --release in workspace mode requires a target: ym build --release <module>",
                style("!").yellow()
            );
        }
    }

    Ok(())
}

/// Group packages into levels where packages in the same level have no
/// mutual dependencies and can be compiled in parallel.
fn compute_parallel_levels(topo_sorted: &[String], ws: &WorkspaceGraph) -> Vec<Vec<String>> {
    let mut levels: Vec<Vec<String>> = Vec::new();
    let mut assigned: std::collections::HashMap<String, usize> = std::collections::HashMap::new();

    for pkg_name in topo_sorted {
        let pkg = ws.get_package(pkg_name).unwrap();
        let ws_deps = pkg
            .config
            .workspace_dependencies
            .as_ref()
            .cloned()
            .unwrap_or_default();

        // This package's level is max(dep levels) + 1
        let level = ws_deps
            .iter()
            .filter_map(|dep| assigned.get(dep))
            .max()
            .map(|l| l + 1)
            .unwrap_or(0);

        assigned.insert(pkg_name.clone(), level);

        while levels.len() <= level {
            levels.push(Vec::new());
        }
        levels[level].push(pkg_name.clone());
    }

    levels
}

fn print_compile_result(name: &str, result: &compiler::CompileResult, elapsed: std::time::Duration) {
    if result.files_compiled == 0 {
        println!(
            "  {} {} is up to date",
            style("✓").green(),
            style(name).bold()
        );
    } else {
        println!(
            "  {} Compiled {} ({} files)                     {:>6}ms",
            style("✓").green(),
            style(name).bold(),
            result.files_compiled,
            elapsed.as_millis()
        );
    }
}

/// Build a fat/executable JAR containing all classes and dependencies.
fn build_release_jar(project: &Path, cfg: &YmConfig, jars: &[PathBuf]) -> Result<()> {
    let out = config::output_classes_dir(project);
    let release_dir = project.join("out");
    std::fs::create_dir_all(&release_dir)?;

    let jar_name = format!("{}-{}.jar", cfg.name, cfg.version.as_deref().unwrap_or("0.0.0"));
    let jar_path = release_dir.join(&jar_name);

    // Create a staging directory for the fat JAR contents
    let staging = project.join("out").join(".release-staging");
    if staging.exists() {
        std::fs::remove_dir_all(&staging)?;
    }
    std::fs::create_dir_all(&staging)?;

    // Copy compiled classes
    copy_dir_recursive(&out, &staging)?;

    // Extract dependency JARs / copy dependency class directories into staging
    for dep in jars {
        if !dep.exists() {
            continue;
        }
        if dep.is_dir() {
            // Copy classes from workspace dependency output dirs
            copy_dir_recursive(dep, &staging)?;
        } else {
            // Extract JAR
            let _ = std::process::Command::new("jar")
                .arg("xf")
                .arg(dep)
                .current_dir(&staging)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
        }
    }

    // Remove META-INF/MANIFEST.MF from extracted deps (we'll create our own)
    let _ = std::fs::remove_file(staging.join("META-INF").join("MANIFEST.MF"));

    // Create manifest
    let manifest_dir = staging.join("META-INF");
    std::fs::create_dir_all(&manifest_dir)?;
    let mut manifest = String::from("Manifest-Version: 1.0\n");
    if let Some(ref main) = cfg.main {
        manifest.push_str(&format!("Main-Class: {}\n", main));
    }
    manifest.push('\n');
    std::fs::write(manifest_dir.join("MANIFEST.MF"), &manifest)?;

    // Create the JAR
    let status = std::process::Command::new("jar")
        .arg("cfm")
        .arg(&jar_path)
        .arg(manifest_dir.join("MANIFEST.MF"))
        .arg("-C")
        .arg(&staging)
        .arg(".")
        .status()?;

    // Clean up staging
    let _ = std::fs::remove_dir_all(&staging);

    if !status.success() {
        bail!("Failed to create release JAR");
    }

    println!(
        "  {} Created release JAR: {}",
        style("✓").green(),
        style(jar_path.display()).bold()
    );

    Ok(())
}

fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<()> {
    for entry in walkdir::WalkDir::new(src) {
        let entry = entry?;
        let rel = entry.path().strip_prefix(src)?;
        let target = dst.join(rel);
        if entry.file_type().is_dir() {
            std::fs::create_dir_all(&target)?;
        } else {
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}

pub fn resolve_deps(project: &Path, cfg: &YmConfig) -> Result<Vec<PathBuf>> {
    let mut deps = cfg.dependencies.as_ref().cloned().unwrap_or_default();
    let mut registries: Vec<String> = Vec::new();

    // Inherit dependencies from workspace root if inside a workspace
    if let Some(ws_root) = config::find_workspace_root(project) {
        if ws_root != project {
            let root_config_path = ws_root.join(config::CONFIG_FILE);
            if let Ok(root_cfg) = config::load_config(&root_config_path) {
                if let Some(root_deps) = root_cfg.dependencies {
                    // Root deps are inherited but child deps take precedence
                    for (k, v) in root_deps {
                        deps.entry(k).or_insert(v);
                    }
                }
                // Apply version resolutions from root
                if let Some(resolutions) = root_cfg.resolutions {
                    for (k, v) in resolutions {
                        if deps.contains_key(&k) {
                            deps.insert(k, v);
                        }
                    }
                }
                // Collect registries from root
                if let Some(regs) = root_cfg.registries {
                    registries.extend(regs.values().cloned());
                }
            }
        }
    }

    // Collect registries from current config (override root)
    if let Some(regs) = &cfg.registries {
        for url in regs.values() {
            if !registries.contains(url) {
                registries.insert(0, url.clone()); // Current config repos have priority
            }
        }
    }

    if deps.is_empty() {
        return Ok(vec![]);
    }

    let cache = config::maven_cache_dir(project);
    let lock_path = project.join(config::LOCK_FILE);
    let mut lock = config::load_lock(&lock_path)?;

    let exclusions = cfg.exclusions.as_ref().cloned().unwrap_or_default();

    let jars = crate::workspace::resolver::resolve_and_download_full(
        &deps, &cache, &mut lock, &registries, &exclusions,
    )?;
    config::save_lock(&lock_path, &lock)?;

    // Check for dependency version conflicts
    let conflicts = crate::workspace::resolver::check_conflicts(&lock);
    if !conflicts.is_empty() {
        for (ga, versions) in &conflicts {
            eprintln!(
                "  {} Version conflict: {} has versions: {}",
                console::style("!").yellow(),
                console::style(ga).bold(),
                versions.join(", ")
            );
        }
        eprintln!(
            "  {} Use \"resolutions\" in ym.json to pin a specific version",
            console::style("→").dim()
        );
    }

    Ok(jars)
}

pub fn compile_project(
    project: &Path,
    cfg: &YmConfig,
    classpath: &[PathBuf],
) -> Result<compiler::CompileResult> {
    let src = config::source_dir_for(project, cfg);
    let out = if let Some(custom) = OUTPUT_DIR_OVERRIDE.get() {
        PathBuf::from(custom)
    } else {
        config::output_classes_dir(project)
    };
    let cache = config::cache_dir(project);

    let encoding = cfg.compiler.as_ref().and_then(|c| c.encoding.clone());

    // Resolve annotation processor JARs
    let ap_jars = resolve_annotation_processors(project, cfg)?;

    let lint = cfg.compiler.as_ref().and_then(|c| c.lint.clone()).unwrap_or_default();
    let extra_args = cfg.compiler.as_ref().and_then(|c| c.args.clone()).unwrap_or_default();

    let compile_cfg = compiler::CompileConfig {
        source_dirs: vec![src.clone()],
        output_dir: out.clone(),
        classpath: classpath.to_vec(),
        java_version: cfg.target.clone(),
        encoding,
        annotation_processors: ap_jars,
        lint,
        extra_args,
    };

    // Copy resource files (non-.java) from src to output
    resources::copy_resources(&src, &out)?;

    // Also copy from src/main/resources if it exists
    let resources_dir = project.join("src").join("main").join("resources");
    if resources_dir.exists() {
        resources::copy_resources(&resources_dir, &out)?;
    }

    // Determine compiler engine
    let engine = compiler::CompilerEngine::from_config(
        cfg.compiler.as_ref().and_then(|c| c.engine.as_deref()),
    );

    // Use incremental compilation
    incremental::incremental_compile(&compile_cfg, &cache, &engine)
}

/// Resolve annotation processor JARs from config.
/// Processors are specified as "groupId:artifactId" in compiler.annotationProcessors.
/// They are resolved from the same Maven dependencies.
fn resolve_annotation_processors(project: &Path, cfg: &YmConfig) -> Result<Vec<PathBuf>> {
    let ap_coords = match cfg.compiler.as_ref().and_then(|c| c.annotation_processors.as_ref()) {
        Some(coords) if !coords.is_empty() => coords,
        _ => return Ok(vec![]),
    };

    let deps = cfg.dependencies.as_ref().cloned().unwrap_or_default();
    let cache = config::maven_cache_dir(project);
    let mut jars = Vec::new();

    for coord in ap_coords {
        // Find matching version from dependencies
        if let Some(version) = deps.get(coord) {
            let mc = crate::workspace::resolver::MavenCoord::parse(coord, version)?;
            let jar = mc.jar_path(&cache);
            if jar.exists() {
                jars.push(jar);
            }
        }
    }

    Ok(jars)
}

/// Ensure the JDK is available based on config. Sets JAVA_HOME if a JDK was downloaded.
pub fn ensure_jdk_for_config(cfg: &YmConfig) -> Result<()> {
    let version = cfg
        .jvm
        .as_ref()
        .and_then(|j| j.version.clone())
        .or_else(|| cfg.target.clone())
        .unwrap_or_else(|| "21".to_string());

    let vendor = cfg.jvm.as_ref().and_then(|j| j.vendor.as_deref());
    let auto_download = cfg
        .jvm
        .as_ref()
        .and_then(|j| j.auto_download)
        .unwrap_or(false);

    let java_home = jvm::ensure_jdk(&version, vendor, auto_download)?;

    // If we found/downloaded a real JAVA_HOME, set it for child processes
    if java_home != Path::new("system") && java_home.exists() {
        // SAFETY: ym is single-threaded at this point in the build lifecycle
        unsafe {
            std::env::set_var("JAVA_HOME", &java_home);
            // Also prepend to PATH so javac/java from this JDK are used
            let bin_dir = java_home.join("bin");
            if let Ok(current_path) = std::env::var("PATH") {
                let sep = if cfg!(windows) { ";" } else { ":" };
                std::env::set_var(
                    "PATH",
                    format!("{}{}{}", bin_dir.display(), sep, current_path),
                );
            }
        }
    }

    Ok(())
}

/// Watch for file changes and rebuild automatically
pub fn watch_loop(target: Option<String>, release: bool) -> Result<()> {
    use crate::watcher::FileWatcher;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    let (config_path, cfg) = config::load_or_find_config()?;
    let project = config::project_dir(&config_path);

    let src_dir = config::source_dir(&project);
    let mut watch_dirs = vec![];
    if src_dir.exists() {
        watch_dirs.push(src_dir);
    }

    // Also watch workspace source dirs if applicable
    if cfg.workspaces.is_some() {
        if let Ok(ws) = crate::workspace::graph::WorkspaceGraph::build(&project) {
            for name in ws.all_packages() {
                let pkg = ws.get_package(&name).unwrap();
                let pkg_src = config::source_dir(&pkg.path);
                if pkg_src.exists() {
                    watch_dirs.push(pkg_src);
                }
            }
        }
    }

    if watch_dirs.is_empty() {
        println!("  No source directories to watch.");
        return Ok(());
    }

    let extensions = vec![".java".to_string()];
    let watcher = FileWatcher::new(&watch_dirs, extensions)?;

    println!();
    println!(
        "  Watching for changes... (press Ctrl+C to stop)",
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

        for path in &changed {
            if let Some(name) = path.file_name() {
                println!(
                    "  {} Changed: {}",
                    console::style("↻").blue(),
                    console::style(name.to_string_lossy()).yellow()
                );
            }
        }

        let start = std::time::Instant::now();
        match execute(target.clone(), release) {
            Ok(()) => {
                println!(
                    "  {} Rebuilt in {}ms",
                    console::style("✓").green(),
                    start.elapsed().as_millis()
                );
            }
            Err(e) => {
                eprintln!(
                    "  {} Build failed: {}",
                    console::style("✗").red(),
                    e
                );
            }
        }
        println!();
    }

    println!("  Stopped watching.");
    Ok(())
}

/// Full compilation (ignoring incremental cache), for use by other commands
#[allow(dead_code)]
pub fn compile_project_full(
    project: &Path,
    cfg: &YmConfig,
    classpath: &[PathBuf],
) -> Result<compiler::CompileResult> {
    let src = config::source_dir_for(project, cfg);
    let out = config::output_classes_dir(project);

    let encoding = cfg.compiler.as_ref().and_then(|c| c.encoding.clone());

    let compile_cfg = compiler::CompileConfig {
        source_dirs: vec![src],
        output_dir: out,
        classpath: classpath.to_vec(),
        java_version: cfg.target.clone(),
        encoding,
        annotation_processors: vec![],
        lint: vec![],
        extra_args: vec![],
    };

    javac::compile(&compile_cfg)
}
