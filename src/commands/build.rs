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

/// Strict mode flag (warnings as errors)
static STRICT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Set the number of parallel compilation threads.
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

/// Enable strict mode (treat warnings as errors).
pub fn set_strict(v: bool) {
    STRICT.store(v, std::sync::atomic::Ordering::Relaxed);
}

/// Check if strict mode is enabled.
pub fn is_strict() -> bool {
    STRICT.load(std::sync::atomic::Ordering::Relaxed)
}

/// Build with per-phase timing breakdown
pub fn execute_with_profile(_target: Option<String>) -> Result<()> {
    let total_start = Instant::now();

    let (config_path, cfg) = config::load_or_find_config()?;
    let project = config::project_dir(&config_path);

    println!();
    println!("  {}", style("Build Profile").bold().underlined());
    println!();

    let config_time = total_start.elapsed();
    println!(
        "  {} config loading                               {:>6}ms",
        style("·").dim(),
        config_time.as_millis()
    );

    let jdk_start = Instant::now();
    ensure_jdk_for_config(&cfg)?;
    let jdk_time = jdk_start.elapsed();
    println!(
        "  {} JDK verification                             {:>6}ms",
        style("·").dim(),
        jdk_time.as_millis()
    );

    let dep_start = Instant::now();
    let _all_jars = resolve_deps(&project, &cfg)?;
    let compile_jars = resolve_deps_with_scopes(&project, &cfg, &["compile", "provided"])?;
    let dep_time = dep_start.elapsed();
    println!(
        "  {} dependency resolution ({} jars)            {:>6}ms",
        style("·").dim(),
        compile_jars.len(),
        dep_time.as_millis()
    );

    let compile_start = Instant::now();
    let result = compile_project(&project, &cfg, &compile_jars)?;
    let compile_time = compile_start.elapsed();

    if !result.success {
        eprint!("{}", compiler::colorize_errors(&result.errors));
        bail!("Compilation failed");
    }

    println!(
        "  {} compilation ({} files)                     {:>6}ms",
        style("·").dim(),
        result.files_compiled,
        compile_time.as_millis()
    );

    if cfg.main.is_some() {
        let jar_start = Instant::now();
        let runtime_jars = resolve_deps_with_scopes(&project, &cfg, &["compile", "runtime"])?;
        build_release_jar(&project, &cfg, &runtime_jars, None)?;
        let jar_time = jar_start.elapsed();
        println!(
            "  {} JAR packaging                                {:>6}ms",
            style("·").dim(),
            jar_time.as_millis()
        );
    }

    scripts::run_script(&cfg, "postbuild", &project)?;

    let total = total_start.elapsed();
    println!();
    println!(
        "  {} total                                        {:>6}ms",
        style("✓").green().bold(),
        total.as_millis()
    );
    println!();

    Ok(())
}

/// Compile only (no JAR packaging). Used by dev/test commands.
pub fn compile_only(target: Option<String>) -> Result<()> {
    build_impl(target, false, false)
}

pub fn execute(target: Option<String>) -> Result<()> {
    build_impl(target, true, false)
}

pub fn execute_keep_going(target: Option<String>) -> Result<()> {
    build_impl(target, true, true)
}

fn build_impl(target: Option<String>, package: bool, keep_going: bool) -> Result<()> {
    let total_start = Instant::now();

    let (config_path, cfg) = config::load_or_find_config()?;
    let project = config::project_dir(&config_path);

    super::idea::auto_sync_idea(&project, &cfg);

    // Run prebuild script
    scripts::run_script(&cfg, "prebuild", &project)?;

    // Ensure JDK is available
    ensure_jdk_for_config(&cfg)?;

    if cfg.workspaces.is_some() {
        let result = build_workspace(&project, &cfg, target.as_deref(), package, keep_going);
        scripts::run_script(&cfg, "postbuild", &project)?;
        print_total_time(total_start);
        return result;
    }

    // Single project mode
    let start = Instant::now();
    // Resolve all deps to populate cache
    let _all_jars = resolve_deps(&project, &cfg)?;
    // Compilation classpath: compile + provided (exclude runtime and test)
    let compile_jars = resolve_deps_with_scopes(&project, &cfg, &["compile", "provided"])?;
    let resolve_time = start.elapsed();

    let compile_start = Instant::now();
    let result = compile_project(&project, &cfg, &compile_jars)?;
    let compile_time = compile_start.elapsed();

    if !result.success {
        eprint!("{}", compiler::colorize_errors(&result.errors));
        bail!("Compilation failed");
    }

    if is_verbose() && !result.errors.is_empty() {
        eprint!("{}", compiler::colorize_errors(&result.errors));
    }

    println!(
        "  {} resolved dependencies {:>38}ms",
        style("✓").green(),
        resolve_time.as_millis()
    );

    let out_dir = config::output_classes_dir(&project);
    print_compile_result(&cfg.name, &result, compile_time, &out_dir);

    if package && cfg.main.is_some() {
        // Fat JAR: compile + runtime (exclude provided and test)
        let runtime_jars = resolve_deps_with_scopes(&project, &cfg, &["compile", "runtime"])?;
        build_release_jar(&project, &cfg, &runtime_jars, None)?;
    }

    scripts::run_script(&cfg, "postbuild", &project)?;

    print_total_time(total_start);
    Ok(())
}

fn print_total_time(start: Instant) {
    let elapsed = start.elapsed();
    let ms = elapsed.as_millis();
    if ms > 1000 {
        println!(
            "\n  {} built in {:.2}s",
            style("✓").green(),
            elapsed.as_secs_f64()
        );
    } else {
        println!(
            "\n  {} built in {}ms",
            style("✓").green(),
            ms
        );
    }
}

fn build_workspace(root: &Path, root_cfg: &YmConfig, target: Option<&str>, package: bool, keep_going: bool) -> Result<()> {
    let ws = WorkspaceGraph::build(root)?;

    let packages = if let Some(target) = target {
        ws.transitive_closure(target)?
    } else {
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

    // Validate workspace dependency declarations
    for name in &packages {
        let pkg = ws.get_package(name).unwrap();
        let errors = pkg.config.validate_workspace_deps(root_cfg);
        if !errors.is_empty() {
            for e in &errors {
                eprintln!("  {} {}: {}", console::style("✗").red(), name, e);
            }
            anyhow::bail!("Invalid workspace dependency declarations in '{}'", name);
        }
    }

    // Workspace-level dependency resolution
    let dep_start = Instant::now();

    let all_module_deps: Vec<(String, std::collections::BTreeMap<String, String>)> = packages
        .iter()
        .map(|name| {
            let pkg = ws.get_package(name).unwrap();
            // Resolve { workspace = true } Maven deps from root, don't auto-inherit
            let mut deps = pkg.config.maven_dependencies_with_root(root_cfg);
            // Apply root resolutions
            if let Some(ref resolutions) = root_cfg.resolutions {
                for (k, v) in resolutions {
                    if deps.contains_key(k) {
                        deps.insert(k.clone(), v.clone());
                    }
                }
            }
            (name.clone(), deps)
        })
        .collect();

    let cache = config::maven_cache_dir(root);
    let mut resolved = config::load_resolved_cache_checked(root, root_cfg)?;
    let registries = root_cfg.registry_entries();
    let exclusions = root_cfg.exclusions.as_ref().cloned().unwrap_or_default();

    let resolutions = root_cfg.resolutions.as_ref().cloned().unwrap_or_default();
    let per_module_jars = crate::workspace::resolver::resolve_workspace_deps_with_resolutions(
        &all_module_deps, &cache, &mut resolved, &registries, &exclusions, &resolutions,
    )?;
    config::save_resolved_cache(root, &resolved)?;

    let dep_time = dep_start.elapsed();
    let total_jars: usize = per_module_jars.values().next().map(|v| v.len()).unwrap_or(0);
    println!(
        "  {} resolved workspace dependencies ({} jars) {:>11}ms",
        style("✓").green(),
        total_jars,
        dep_time.as_millis()
    );

    // Group into topological levels for parallel compilation
    let levels = compute_parallel_levels(&packages, &ws);

    let mut workspace_classpath: Vec<PathBuf> = Vec::new();
    let mut failed_modules: Vec<String> = Vec::new();

    for level in &levels {
        if level.len() == 1 {
            let pkg_name = &level[0];

            // Skip modules that depend on a failed module
            if keep_going && has_failed_dependency(pkg_name, &failed_modules, &ws) {
                failed_modules.push(pkg_name.clone());
                println!(
                    "{} {} (depends on failed module)",
                    style(format!("{:>12}", "Skipping")).yellow().bold(),
                    pkg_name
                );
                continue;
            }

            let pkg = ws.get_package(pkg_name).unwrap();
            let start = Instant::now();

            let jars = per_module_jars.get(pkg_name).cloned().unwrap_or_default();
            let mut classpath = jars;
            classpath.extend(workspace_classpath.clone());

            let result = compile_project(&pkg.path, &pkg.config, &classpath)?;
            let elapsed = start.elapsed();

            if !result.success {
                eprint!("{}", compiler::colorize_errors(&result.errors));
                if keep_going {
                    failed_modules.push(pkg_name.clone());
                    continue;
                }
                bail!("Compilation of '{}' failed", pkg_name);
            }

            let out_dir = config::output_classes_dir(&pkg.path);
            print_compile_result(pkg_name, &result, elapsed, &out_dir);
            workspace_classpath.push(out_dir);
        } else {
            let start = Instant::now();
            let cp_snapshot = workspace_classpath.clone();

            // Filter out modules with failed dependencies
            let compilable: Vec<&String> = if keep_going {
                level.iter().filter(|name| !has_failed_dependency(name, &failed_modules, &ws)).collect()
            } else {
                level.iter().collect()
            };

            let results: Vec<(String, Result<compiler::CompileResult>)> = compilable
                .par_iter()
                .map(|pkg_name| {
                    let pkg = ws.get_package(pkg_name.as_str()).unwrap();
                    let jars = per_module_jars.get(pkg_name.as_str()).cloned().unwrap_or_default();
                    let mut classpath = jars;
                    classpath.extend(cp_snapshot.clone());
                    let result = compile_project(&pkg.path, &pkg.config, &classpath);
                    (pkg_name.to_string(), result)
                })
                .collect();

            let elapsed = start.elapsed();

            for (pkg_name, result) in &results {
                match result {
                    Ok(r) if r.success => {
                        let pkg = ws.get_package(pkg_name.as_str()).unwrap();
                        print_compile_result(pkg_name, r, elapsed, &config::output_classes_dir(&pkg.path));
                    }
                    Ok(r) => {
                        eprint!("{}", compiler::colorize_errors(&r.errors));
                        if keep_going {
                            failed_modules.push(pkg_name.clone());
                        } else {
                            bail!("Compilation of '{}' failed", pkg_name);
                        }
                    }
                    Err(e) => {
                        if keep_going {
                            eprintln!("  {} Compilation of '{}' failed: {}", style("✗").red(), pkg_name, e);
                            failed_modules.push(pkg_name.clone());
                        } else {
                            bail!("Compilation of '{}' failed: {}", pkg_name, e);
                        }
                    }
                }
            }

            for pkg_name in level {
                if !failed_modules.contains(pkg_name) {
                    let pkg = ws.get_package(pkg_name).unwrap();
                    workspace_classpath.push(config::output_classes_dir(&pkg.path));
                }
            }
        }
    }

    if !failed_modules.is_empty() {
        println!();
        println!(
            "  {} {} module(s) failed: {}",
            style("✗").red().bold(),
            failed_modules.len(),
            failed_modules.join(", ")
        );
        bail!("Workspace build failed ({} module(s))", failed_modules.len());
    }

    if package {
        // Package fat JARs for modules with a `main` field
        // - If target specified: only that module
        // - If no target: all modules with a `main` field
        let jar_targets: Vec<&str> = if let Some(target) = target {
            vec![target]
        } else {
            packages.iter()
                .filter(|name| {
                    ws.get_package(name)
                        .map(|p| p.config.main.is_some())
                        .unwrap_or(false)
                })
                .map(|s| s.as_str())
                .collect()
        };

        for jar_target in &jar_targets {
            let pkg = ws.get_package(jar_target).unwrap();
            let closure = ws.transitive_closure(jar_target)?;
            let mut all_deps = Vec::new();
            for pkg_name in &closure {
                let p = ws.get_package(pkg_name).unwrap();
                if pkg_name != *jar_target {
                    all_deps.push(config::output_classes_dir(&p.path));
                }
            }
            let runtime_jars = resolve_deps_with_scopes(&pkg.path, &pkg.config, &["compile", "runtime"])?;
            all_deps.extend(runtime_jars);
            build_release_jar(&pkg.path, &pkg.config, &all_deps, Some(root))?;
        }

        if !jar_targets.is_empty() {
            println!(
                "{} {}",
                style(format!("{:>12}", "→")).dim(),
                style(root.join("out").join("release").display()).dim()
            );
        }
    }

    Ok(())
}

/// Check if a module depends (directly) on any failed module.
fn has_failed_dependency(pkg_name: &str, failed: &[String], ws: &WorkspaceGraph) -> bool {
    if let Some(pkg) = ws.get_package(pkg_name) {
        for dep in pkg.config.workspace_module_deps() {
            if failed.contains(&dep) {
                return true;
            }
        }
    }
    false
}

/// Group packages into levels where packages in the same level have no
/// mutual dependencies and can be compiled in parallel.
fn compute_parallel_levels(topo_sorted: &[String], ws: &WorkspaceGraph) -> Vec<Vec<String>> {
    let mut levels: Vec<Vec<String>> = Vec::new();
    let mut assigned: std::collections::HashMap<String, usize> = std::collections::HashMap::new();

    for pkg_name in topo_sorted {
        let pkg = ws.get_package(pkg_name).unwrap();
        let ws_deps = pkg.config.workspace_module_deps();

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

fn print_compile_result(name: &str, result: &compiler::CompileResult, elapsed: std::time::Duration, _output_dir: &Path) {
    if result.files_compiled == 0 {
        if result.errors.contains("restored from build cache") {
            println!(
                "{} {} (cached) {:>30}ms",
                style(format!("{:>12}", "Compiling")).green().bold(),
                name,
                elapsed.as_millis()
            );
        } else {
            println!(
                "{} {} (up to date)",
                style(format!("{:>12}", "Compiling")).green().bold(),
                name,
            );
        }
    } else {
        println!(
            "{} {} ({} files) {:>27}ms",
            style(format!("{:>12}", "Compiling")).green().bold(),
            name,
            result.files_compiled,
            elapsed.as_millis()
        );
    }
}

/// Build a fat/executable JAR containing all classes and dependencies.
pub(crate) fn build_release_jar(project: &Path, cfg: &YmConfig, jars: &[PathBuf], output_base: Option<&Path>) -> Result<()> {
    let out = config::output_classes_dir(project);
    let base = output_base.unwrap_or(project);
    let release_dir = base.join("out").join("release");
    std::fs::create_dir_all(&release_dir)?;

    let jar_name = format!("{}-{}.jar", cfg.name, cfg.version.as_deref().unwrap_or("0.0.0"));
    let jar_path = release_dir.join(&jar_name);

    let staging = project.join("out").join(".release-staging");
    if staging.exists() {
        std::fs::remove_dir_all(&staging)?;
    }
    std::fs::create_dir_all(&staging)?;

    copy_dir_recursive(&out, &staging)?;

    // Detect class name conflicts across dependency JARs
    detect_fat_jar_conflicts(jars);

    // Collect mergeable META-INF entries (services, spring.factories, spring/*.imports)
    let mut mergeable: std::collections::HashMap<String, Vec<String>> = std::collections::HashMap::new();

    for dep in jars {
        if !dep.exists() {
            continue;
        }
        if dep.is_dir() {
            copy_dir_recursive(dep, &staging)?;
        } else {
            // Pre-read mergeable entries before extraction (jar xf will overwrite)
            if let Ok(file) = std::fs::File::open(dep) {
                if let Ok(mut archive) = zip::ZipArchive::new(std::io::BufReader::new(file)) {
                    for i in 0..archive.len() {
                        if let Ok(mut entry) = archive.by_index(i) {
                            let name = entry.name().to_string();
                            let should_merge = !entry.is_dir() && (
                                name.starts_with("META-INF/services/") ||
                                name == "META-INF/spring.factories" ||
                                (name.starts_with("META-INF/spring/") && name.ends_with(".imports"))
                            );
                            if should_merge {
                                let mut content = String::new();
                                use std::io::Read;
                                let _ = entry.read_to_string(&mut content);
                                mergeable.entry(name).or_default().push(content);
                            }
                        }
                    }
                }
            }

            let _ = std::process::Command::new("jar")
                .arg("xf")
                .arg(dep)
                .current_dir(&staging)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
        }
    }

    // Merge collected META-INF files (combine entries from all JARs, deduplicate lines)
    for (meta_file, contents) in &mergeable {
        if contents.len() > 1 {
            let merged_path = staging.join(meta_file);
            if let Some(parent) = merged_path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let mut seen = std::collections::HashSet::new();
            let mut merged = String::new();
            for content in contents {
                for line in content.lines() {
                    let trimmed = line.trim();
                    if !trimmed.is_empty() && !trimmed.starts_with('#') && seen.insert(trimmed.to_string()) {
                        merged.push_str(line);
                        merged.push('\n');
                    }
                }
            }
            std::fs::write(&merged_path, &merged)?;
        }
    }

    let _ = std::fs::remove_file(staging.join("META-INF").join("MANIFEST.MF"));

    let manifest_dir = staging.join("META-INF");
    std::fs::create_dir_all(&manifest_dir)?;
    let mut manifest = String::from("Manifest-Version: 1.0\n");
    if let Some(ref main) = cfg.main {
        manifest.push_str(&format!("Main-Class: {}\n", main));
    }
    manifest.push_str(&format!("Implementation-Title: {}\n", cfg.name));
    if let Some(ref ver) = cfg.version {
        manifest.push_str(&format!("Implementation-Version: {}\n", ver));
    }
    manifest.push_str(&format!("Implementation-Vendor: {}\n", cfg.group_id));
    manifest.push_str(&format!("Built-By: ym {}\n", env!("CARGO_PKG_VERSION")));
    manifest.push('\n');
    std::fs::write(manifest_dir.join("MANIFEST.MF"), &manifest)?;

    let status = std::process::Command::new("jar")
        .arg("cfm")
        .arg(&jar_path)
        .arg(manifest_dir.join("MANIFEST.MF"))
        .arg("-C")
        .arg(&staging)
        .arg(".")
        .status()?;

    let _ = std::fs::remove_dir_all(&staging);

    if !status.success() {
        bail!("Failed to create release JAR");
    }

    println!(
        "{} {}",
        style(format!("{:>12}", "Packaging")).green().bold(),
        jar_name
    );

    Ok(())
}

/// Detect class name conflicts across dependency JARs for fat JAR packaging.
/// Prints warnings for duplicate .class entries found in different JARs.
fn detect_fat_jar_conflicts(jars: &[PathBuf]) {
    use std::collections::HashMap;

    // Map: class entry path -> list of JAR file names containing it
    let mut class_sources: HashMap<String, Vec<String>> = HashMap::new();

    for jar_path in jars {
        if !jar_path.exists() || jar_path.is_dir() {
            continue;
        }

        let jar_name = jar_path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| jar_path.display().to_string());

        let file = match std::fs::File::open(jar_path) {
            Ok(f) => f,
            Err(_) => continue,
        };

        let mut archive = match zip::ZipArchive::new(std::io::BufReader::new(file)) {
            Ok(a) => a,
            Err(_) => continue,
        };

        for i in 0..archive.len() {
            let entry = match archive.by_index(i) {
                Ok(e) => e,
                Err(_) => continue,
            };
            let name = entry.name().to_string();
            // Only check .class files, skip META-INF and module-info
            if name.ends_with(".class")
                && !name.starts_with("META-INF/")
                && name != "module-info.class"
            {
                class_sources
                    .entry(name)
                    .or_default()
                    .push(jar_name.clone());
            }
        }
    }

    // Collect conflicts (class found in 2+ JARs)
    let mut conflicts: Vec<(String, Vec<String>)> = class_sources
        .into_iter()
        .filter(|(_, sources)| sources.len() > 1)
        .collect();

    if conflicts.is_empty() {
        return;
    }

    conflicts.sort_by(|a, b| a.0.cmp(&b.0));

    let total = conflicts.len();
    println!(
        "  {} fat JAR: {} duplicate class(es) detected across dependency JARs",
        style("!").yellow(),
        total
    );

    // Show up to 10 conflicts
    let show = conflicts.iter().take(10);
    for (class_path, sources) in show {
        let class_name = class_path
            .strip_suffix(".class")
            .unwrap_or(class_path)
            .replace('/', ".");
        println!(
            "    {} → {}",
            class_name,
            sources.join(", ")
        );
    }
    if total > 10 {
        println!("    ... and {} more", total - 10);
    }
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

/// Resolve dependencies filtered by scope. Used for scope-specific classpath construction.
pub fn resolve_deps_with_scopes(project: &Path, cfg: &YmConfig, scopes: &[&str]) -> Result<Vec<PathBuf>> {
    use crate::workspace::resolver::RegistryEntry;
    let mut registries: Vec<RegistryEntry> = Vec::new();
    let mut resolutions = cfg.resolutions.as_ref().cloned().unwrap_or_default();

    // Resolve deps: if inside a workspace, resolve { workspace = true } from root
    let deps = if let Some(ws_root) = config::find_workspace_root(project) {
        if ws_root != project {
            let root_config_path = ws_root.join(config::CONFIG_FILE);
            if let Ok(root_cfg) = config::load_config(&root_config_path) {
                let errors = cfg.validate_workspace_deps(&root_cfg);
                if !errors.is_empty() {
                    anyhow::bail!("{}", errors.join("; "));
                }
                let mut d = cfg.maven_dependencies_for_scopes_with_root(scopes, &root_cfg);
                if let Some(ref root_resolutions) = root_cfg.resolutions {
                    for (k, v) in root_resolutions {
                        if d.contains_key(k.as_str()) {
                            d.insert(k.clone(), v.clone());
                        }
                        resolutions.insert(k.clone(), v.clone());
                    }
                }
                registries.extend(root_cfg.registry_entries());
                d
            } else {
                cfg.maven_dependencies_for_scopes(scopes)
            }
        } else {
            cfg.maven_dependencies_for_scopes(scopes)
        }
    } else {
        cfg.maven_dependencies_for_scopes(scopes)
    };

    let current_entries = cfg.registry_entries();
    for entry in current_entries {
        if !registries.iter().any(|e| e.url == entry.url) {
            registries.insert(0, entry);
        }
    }

    let cache = config::maven_cache_dir(project);

    if deps.is_empty() {
        // Even with no Maven deps, may have URL/Git/lib deps
        if scopes.contains(&"compile") {
            let mut jars = resolve_url_deps(cfg, &cache)?;
            jars.extend(resolve_git_deps(cfg, &cache)?);
            jars.extend(resolve_lib_dirs(project, cfg));
            return Ok(jars);
        }
        return Ok(vec![]);
    }

    let mut resolved = config::load_resolved_cache_checked(project, cfg)?;
    let mut exclusions = cfg.exclusions.as_ref().cloned().unwrap_or_default();
    exclusions.extend(cfg.per_dependency_exclusions());

    // Build dep_scopes: map each direct dep's GA to its declared scope
    let dep_scopes = build_dep_scope_map(cfg, scopes);

    let jars = crate::workspace::resolver::resolve_and_download_with_scopes(
        &deps, &cache, &mut resolved, &registries, &exclusions, &resolutions, &dep_scopes,
    )?;
    config::save_resolved_cache(project, &resolved)?;

    // Filter out JARs whose transitive scope doesn't match requested scopes
    let mut filtered = filter_jars_by_scope(&resolved, &jars, &cache, scopes);

    // Add URL/Git dependencies (they don't have Maven scope tracking,
    // so include them if compile scope is requested)
    if scopes.contains(&"compile") {
        filtered.extend(resolve_url_deps(cfg, &cache)?);
        filtered.extend(resolve_git_deps(cfg, &cache)?);
        filtered.extend(resolve_lib_dirs(project, cfg));
    }

    Ok(filtered)
}

/// Build a mapping of "groupId:artifactId" -> scope from the config's direct dependencies.
fn build_dep_scope_map(cfg: &YmConfig, _scopes: &[&str]) -> std::collections::HashMap<String, String> {
    use crate::config::schema::DependencyValue;
    let mut map = std::collections::HashMap::new();
    if let Some(ref deps) = cfg.dependencies {
        for (key, value) in deps {
            if !key.contains(':') {
                continue; // workspace module dep
            }
            let scope = match value {
                DependencyValue::Simple(_) => "compile".to_string(),
                DependencyValue::Detailed(spec) => spec.scope.clone().unwrap_or_else(|| "compile".to_string()),
            };
            map.insert(key.clone(), scope);
        }
    }
    map
}

/// Filter JAR list by checking each JAR's scope in the resolved cache.
/// Only include JARs whose scope is in the allowed scopes list.
fn filter_jars_by_scope(
    resolved: &crate::config::schema::ResolvedCache,
    jars: &[std::path::PathBuf],
    cache: &std::path::Path,
    scopes: &[&str],
) -> Vec<std::path::PathBuf> {
    jars.iter()
        .filter(|jar| {
            // Extract versioned_key from jar path: cache/group/artifact/version/artifact-version.jar
            if let Some(vk) = jar_path_to_versioned_key(jar, cache) {
                if let Some(entry) = resolved.dependencies.get(&vk) {
                    if let Some(ref scope) = entry.scope {
                        return scopes.contains(&scope.as_str());
                    }
                }
            }
            // If no scope info, include by default (backwards compat)
            true
        })
        .cloned()
        .collect()
}

/// Convert a JAR path back to "groupId:artifactId:version" key.
fn jar_path_to_versioned_key(jar: &std::path::Path, cache: &std::path::Path) -> Option<String> {
    let rel = jar.strip_prefix(cache).ok()?;
    let components: Vec<_> = rel.components().collect();
    // Expected: group / artifact / version / artifact-version.jar
    if components.len() >= 3 {
        let group = components[0].as_os_str().to_string_lossy();
        let artifact = components[1].as_os_str().to_string_lossy();
        let version = components[2].as_os_str().to_string_lossy();
        Some(format!("{}:{}:{}", group, artifact, version))
    } else {
        None
    }
}

pub fn resolve_deps(project: &Path, cfg: &YmConfig) -> Result<Vec<PathBuf>> {
    use crate::workspace::resolver::RegistryEntry;
    let mut registries: Vec<RegistryEntry> = Vec::new();
    let mut resolutions = cfg.resolutions.as_ref().cloned().unwrap_or_default();

    // Resolve deps: if inside a workspace, resolve { workspace = true } from root
    let deps = if let Some(ws_root) = config::find_workspace_root(project) {
        if ws_root != project {
            let root_config_path = ws_root.join(config::CONFIG_FILE);
            if let Ok(root_cfg) = config::load_config(&root_config_path) {
                let errors = cfg.validate_workspace_deps(&root_cfg);
                if !errors.is_empty() {
                    anyhow::bail!("{}", errors.join("; "));
                }
                let mut d = cfg.maven_dependencies_with_root(&root_cfg);
                if let Some(ref root_resolutions) = root_cfg.resolutions {
                    for (k, v) in root_resolutions {
                        if d.contains_key(k.as_str()) {
                            d.insert(k.clone(), v.clone());
                        }
                        resolutions.insert(k.clone(), v.clone());
                    }
                }
                registries.extend(root_cfg.registry_entries());
                d
            } else {
                cfg.maven_dependencies()
            }
        } else {
            cfg.maven_dependencies()
        }
    } else {
        cfg.maven_dependencies()
    };

    // Collect registries from current config
    let current_entries = cfg.registry_entries();
    for entry in current_entries {
        if !registries.iter().any(|e| e.url == entry.url) {
            registries.insert(0, entry);
        }
    }

    let cache = config::maven_cache_dir(project);

    if deps.is_empty() {
        // Even with no Maven deps, may have URL/Git/lib deps
        let mut jars = resolve_url_deps(cfg, &cache)?;
        jars.extend(resolve_git_deps(cfg, &cache)?);
        jars.extend(resolve_lib_dirs(project, cfg));
        return Ok(jars);
    }

    let mut resolved = config::load_resolved_cache_checked(project, cfg)?;
    let mut exclusions = cfg.exclusions.as_ref().cloned().unwrap_or_default();
    exclusions.extend(cfg.per_dependency_exclusions());

    // Build dep_scopes: map each direct dep's GA to its declared scope (all scopes)
    let dep_scopes = build_dep_scope_map(cfg, &["compile", "provided", "runtime", "test"]);
    let jars = crate::workspace::resolver::resolve_and_download_with_scopes(
        &deps, &cache, &mut resolved, &registries, &exclusions, &resolutions, &dep_scopes,
    )?;
    config::save_resolved_cache(project, &resolved)?;

    // Check for dependency version conflicts
    let conflicts = crate::workspace::resolver::check_conflicts(&resolved);
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
            "  {} Use [resolutions] in package.toml to pin a specific version",
            console::style("→").dim()
        );
    }

    // Resolve URL dependencies
    let url_jars = resolve_url_deps(cfg, &cache)?;
    let mut all_jars = jars;
    all_jars.extend(url_jars);

    // Resolve Git dependencies
    let git_jars = resolve_git_deps(cfg, &cache)?;
    all_jars.extend(git_jars);

    // Add local lib directories
    all_jars.extend(resolve_lib_dirs(project, cfg));

    Ok(all_jars)
}

/// Like resolve_deps but skip JAR downloads. Returns expected cache paths.
/// Used by `ym idea --json` so importing is never blocked by network I/O.
pub fn resolve_deps_no_download(project: &Path, cfg: &YmConfig) -> Result<Vec<PathBuf>> {
    use crate::workspace::resolver::RegistryEntry;
    let mut registries: Vec<RegistryEntry> = Vec::new();
    let mut resolutions = cfg.resolutions.as_ref().cloned().unwrap_or_default();

    let deps = if let Some(ws_root) = config::find_workspace_root(project) {
        if ws_root != project {
            let root_config_path = ws_root.join(config::CONFIG_FILE);
            if let Ok(root_cfg) = config::load_config(&root_config_path) {
                let errors = cfg.validate_workspace_deps(&root_cfg);
                if !errors.is_empty() {
                    anyhow::bail!("{}", errors.join("; "));
                }
                let mut d = cfg.maven_dependencies_with_root(&root_cfg);
                if let Some(ref root_resolutions) = root_cfg.resolutions {
                    for (k, v) in root_resolutions {
                        if d.contains_key(k.as_str()) {
                            d.insert(k.clone(), v.clone());
                        }
                        resolutions.insert(k.clone(), v.clone());
                    }
                }
                registries.extend(root_cfg.registry_entries());
                d
            } else {
                cfg.maven_dependencies()
            }
        } else {
            cfg.maven_dependencies()
        }
    } else {
        cfg.maven_dependencies()
    };

    let current_entries = cfg.registry_entries();
    for entry in current_entries {
        if !registries.iter().any(|e| e.url == entry.url) {
            registries.insert(0, entry);
        }
    }

    let cache = config::maven_cache_dir(project);

    if deps.is_empty() {
        let jars = resolve_lib_dirs(project, cfg);
        return Ok(jars);
    }

    let mut resolved = config::load_resolved_cache_checked(project, cfg)?;
    let mut exclusions = cfg.exclusions.as_ref().cloned().unwrap_or_default();
    exclusions.extend(cfg.per_dependency_exclusions());

    let dep_scopes = build_dep_scope_map(cfg, &["compile", "provided", "runtime", "test"]);
    let jars = crate::workspace::resolver::resolve_no_download(
        &deps, &cache, &mut resolved, &registries, &exclusions, &resolutions, &dep_scopes,
    )?;
    config::save_resolved_cache(project, &resolved)?;

    let mut all_jars = jars;
    all_jars.extend(resolve_lib_dirs(project, cfg));

    Ok(all_jars)
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

    let ap_jars = resolve_annotation_processors(project, cfg, classpath)?;

    let lint = cfg.compiler.as_ref().and_then(|c| c.lint.clone()).unwrap_or_default();
    let mut extra_args = cfg.compiler.as_ref().and_then(|c| c.args.clone()).unwrap_or_default();

    if is_strict() && !extra_args.iter().any(|a| a == "-Werror") {
        extra_args.push("-Werror".to_string());
    }

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

    let custom_res_ext = cfg.compiler.as_ref().and_then(|c| c.resource_extensions.as_ref());
    let res_exclude = cfg.compiler.as_ref().and_then(|c| c.resource_exclude.as_ref());
    resources::copy_resources_with_extensions(&src, &out, custom_res_ext.map(|v| v.as_slice()), res_exclude.map(|v| v.as_slice()))?;

    let resources_dir = project.join("src").join("main").join("resources");
    if resources_dir.exists() {
        resources::copy_resources_with_extensions(&resources_dir, &out, custom_res_ext.map(|v| v.as_slice()), res_exclude.map(|v| v.as_slice()))?;
    }

    let engine = compiler::CompilerEngine::from_config(
        cfg.compiler.as_ref().and_then(|c| c.engine.as_deref()),
    );

    incremental::incremental_compile(&compile_cfg, &cache, &engine)
}

fn resolve_annotation_processors(project: &Path, cfg: &YmConfig, classpath: &[PathBuf]) -> Result<Vec<PathBuf>> {
    if let Some(coords) = cfg.compiler.as_ref().and_then(|c| c.annotation_processors.as_ref()) {
        if !coords.is_empty() {
            let deps = cfg.maven_dependencies();
            let cache = config::maven_cache_dir(project);
            let mut jars = Vec::new();
            for coord in coords {
                if let Some(version) = deps.get(coord) {
                    let mc = crate::workspace::resolver::MavenCoord::parse(coord, version)?;
                    let jar = mc.jar_path(&cache);
                    if jar.exists() {
                        jars.push(jar);
                    }
                }
            }
            return Ok(jars);
        }
    }

    Ok(discover_annotation_processors(classpath))
}

fn discover_annotation_processors(classpath: &[PathBuf]) -> Vec<PathBuf> {
    classpath
        .iter()
        .filter(|jar| {
            jar.extension().and_then(|e| e.to_str()) == Some("jar")
                && jar.exists()
                && !is_framework_jar(jar)
                && has_annotation_processor(jar)
        })
        .cloned()
        .collect()
}

/// Check if a JAR is a known framework/library that should be excluded
/// from annotation processor auto-discovery (even if it has a Processor service file).
fn is_framework_jar(jar: &Path) -> bool {
    let stem = jar.file_stem().unwrap_or_default().to_string_lossy();
    let stem_lower = stem.to_lowercase();
    // Exclude JUnit, SLF4J, Log4j, Spring (non-processor), common test/logging frameworks
    const EXCLUDE_PREFIXES: &[&str] = &[
        "junit-", "junit5-", "org.junit", "hamcrest",
        "slf4j-", "log4j-", "logback-", "commons-logging",
        "mockito-", "assertj-", "byte-buddy",
    ];
    EXCLUDE_PREFIXES.iter().any(|prefix| stem_lower.starts_with(prefix))
}

pub fn has_annotation_processor(jar_path: &Path) -> bool {
    let file = match std::fs::File::open(jar_path) {
        Ok(f) => f,
        Err(_) => return false,
    };
    let mut archive = match zip::ZipArchive::new(file) {
        Ok(a) => a,
        Err(_) => return false,
    };
    archive
        .by_name("META-INF/services/javax.annotation.processing.Processor")
        .is_ok()
}

/// Ensure the JDK is available based on config.
pub fn ensure_jdk_for_config(cfg: &YmConfig) -> Result<()> {
    // Priority: jvm.version > target > .java-version file > default "21"
    let version = cfg
        .jvm
        .as_ref()
        .and_then(|j| j.version.clone())
        .or_else(|| cfg.target.clone())
        .or_else(|| read_java_version_file())
        .unwrap_or_else(|| "21".to_string());

    let vendor = cfg.jvm.as_ref().and_then(|j| j.vendor.as_deref());
    let auto_download = cfg
        .jvm
        .as_ref()
        .and_then(|j| j.auto_download)
        .unwrap_or(false);

    let java_home = jvm::ensure_jdk(&version, vendor, auto_download)?;

    if java_home != Path::new("system") && java_home.exists() {
        if !crate::is_json_quiet() {
            // Print JDK info: extract name from path (e.g. "jdk-21.0.2" from the directory name)
            let jdk_name = java_home.file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| format!("JDK {}", version));
            println!(
                "  {}  {}  {} ({})",
                style("➜").green(),
                style("JDK:").bold(),
                &jdk_name,
                style(java_home.display()).dim()
            );
        }
        unsafe {
            std::env::set_var("JAVA_HOME", &java_home);
            let bin_dir = java_home.join("bin");
            if let Ok(current_path) = std::env::var("PATH") {
                let sep = if cfg!(windows) { ";" } else { ":" };
                std::env::set_var(
                    "PATH",
                    format!("{}{}{}", bin_dir.display(), sep, current_path),
                );
            }
        }
    } else if !crate::is_json_quiet() {
        // System JDK — get path from which javac
        let javac_path = jvm::which_javac()
            .and_then(|p| p.parent().and_then(|b| b.parent()).map(|h| h.to_path_buf()));
        if let Some(home) = javac_path {
            let jdk_name = home.file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| "System JDK".to_string());
            println!(
                "  {}  {}  {} ({})",
                style("➜").green(),
                style("JDK:").bold(),
                &jdk_name,
                style(home.display()).dim()
            );
        }
    }

    Ok(())
}

/// Read `.java-version` file from project root (compatible with SDKMAN, jEnv).
fn read_java_version_file() -> Option<String> {
    let cwd = std::env::current_dir().ok()?;
    let file = cwd.join(".java-version");
    if file.exists() {
        let content = std::fs::read_to_string(&file).ok()?;
        let version = content.trim().to_string();
        if !version.is_empty() {
            return Some(version);
        }
    }
    None
}

/// Full compilation (ignoring incremental cache)
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

/// Download URL dependencies and return their JAR paths
fn resolve_url_deps(cfg: &YmConfig, cache: &Path) -> Result<Vec<PathBuf>> {
    let url_deps = cfg.url_dependencies();
    if url_deps.is_empty() {
        return Ok(vec![]);
    }

    let jar_dir = cache.join("url-deps");
    std::fs::create_dir_all(&jar_dir)?;

    let mut jars = Vec::new();
    for (_key, url, _scope) in &url_deps {
        let filename = url.rsplit('/').next().unwrap_or("unknown.jar");
        let jar_path = jar_dir.join(filename);

        if !jar_path.exists() {
            if !crate::is_json_quiet() {
                println!(
                    "  {} downloading {}...",
                    console::style("➜").green(),
                    filename
                );
            }
            let client = reqwest::blocking::Client::builder()
                .user_agent(format!("ym/{}", env!("CARGO_PKG_VERSION")))
                .build()?;
            let response = client.get(url).send()?;
            if !response.status().is_success() {
                eprintln!(
                    "  {} Failed to download {}: HTTP {}",
                    console::style("!").yellow(),
                    url,
                    response.status()
                );
                continue;
            }
            let bytes = response.bytes()?;
            std::fs::write(&jar_path, &bytes)?;
            if !crate::is_json_quiet() {
                println!(
                    "  {} downloaded {}",
                    console::style("✓").green(),
                    filename
                );
            }
        }

        jars.push(jar_path);
    }

    Ok(jars)
}

/// Clone Git dependencies and return their built JAR paths
fn resolve_git_deps(cfg: &YmConfig, cache: &Path) -> Result<Vec<PathBuf>> {
    let git_deps = cfg.git_dependencies();
    if git_deps.is_empty() {
        return Ok(vec![]);
    }

    let git_dir = cache.join("git-deps");
    std::fs::create_dir_all(&git_dir)?;

    let mut jars = Vec::new();
    for (name, git_url, git_ref, _scope) in &git_deps {
        let repo_dir = git_dir.join(name);

        // Clone or update
        if !repo_dir.exists() {
            if !crate::is_json_quiet() {
                println!(
                    "  {} cloning {}...",
                    console::style("➜").green(),
                    name
                );
            }
            let mut cmd = std::process::Command::new("git");
            cmd.arg("clone").arg("--depth").arg("1");
            if let Some(r) = git_ref {
                cmd.arg("--branch").arg(r);
            }
            cmd.arg(git_url).arg(&repo_dir);
            let status = cmd.status()?;
            if !status.success() {
                eprintln!(
                    "  {} Failed to clone {}",
                    console::style("!").yellow(),
                    git_url
                );
                continue;
            }
        }

        // Look for output JARs: check if it has package.toml (ym project)
        let pkg_toml = repo_dir.join("package.toml");
        if pkg_toml.exists() {
            // Build with ym
            if !crate::is_json_quiet() {
                println!(
                    "  {} building Git dependency {}...",
                    console::style("➜").green(),
                    name
                );
            }
            let status = std::process::Command::new("ymc")
                .arg("build")
                .current_dir(&repo_dir)
                .status();
            match status {
                Ok(s) if s.success() => {
                    // Collect output classes directory
                    let out = repo_dir.join("out").join("classes");
                    if out.exists() {
                        jars.push(out);
                    }
                }
                _ => {
                    eprintln!(
                        "  {} Failed to build Git dependency {}",
                        console::style("!").yellow(),
                        name
                    );
                }
            }
        } else {
            // Look for pre-built JARs in the repo
            for entry in std::fs::read_dir(&repo_dir)? {
                let entry = entry?;
                if entry.path().extension().is_some_and(|e| e == "jar") {
                    jars.push(entry.path());
                }
            }
        }
    }

    Ok(jars)
}

/// Scan `compiler.libs` directories for JAR files and return their paths.
fn resolve_lib_dirs(project: &Path, cfg: &YmConfig) -> Vec<PathBuf> {
    let lib_dirs = match cfg.compiler.as_ref().and_then(|c| c.libs.as_ref()) {
        Some(dirs) => dirs,
        None => return vec![],
    };

    let mut jars = Vec::new();
    for dir in lib_dirs {
        let abs_dir = if std::path::Path::new(dir).is_absolute() {
            PathBuf::from(dir)
        } else {
            project.join(dir)
        };
        if !abs_dir.exists() {
            continue;
        }
        if let Ok(entries) = std::fs::read_dir(&abs_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().is_some_and(|e| e == "jar") {
                    jars.push(path);
                }
            }
        }
    }
    jars
}
