use anyhow::{bail, Context, Result};
use console::style;
use rayon::prelude::*;
use std::path::{Path, PathBuf};
use std::time::Instant;

use crate::compiler;
use crate::compiler::incremental;
use crate::compiler::javac;
use crate::config;
use crate::config::schema::{YmConfig, artifact_id_from_key};
use crate::jvm;
use crate::resources;
use crate::scripts;
use crate::workspace::graph::WorkspaceGraph;

/// Simple spinner that uses raw eprint! — avoids indicatif's ANSI escape issues on WSL 1.
/// Reads message from global SPINNER_MSG so resolver can update progress in-place.
struct SimpleSpinner {
    running: std::sync::Arc<std::sync::atomic::AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl SimpleSpinner {
    fn new(msg: &str) -> Self {
        crate::set_spinner_msg(msg);
        let running = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        let r = running.clone();
        let handle = std::thread::spawn(move || {
            let chars = ['⠖', '⠲', '⠴', '⠦'];
            let mut i = 0;
            while r.load(std::sync::atomic::Ordering::Relaxed) {
                let c = chars[i % chars.len()];
                let msg = crate::SPINNER_MSG.lock().map(|m| m.clone()).unwrap_or_default();
                // \r + trailing spaces ensures previous longer messages are fully overwritten
                eprint!("\r  {} {}  \x1b[K", c, msg);
                i += 1;
                std::thread::sleep(std::time::Duration::from_millis(80));
            }
        });
        Self { running, handle: Some(handle) }
    }

    fn set_message(&self, msg: impl Into<String>) {
        crate::set_spinner_msg(msg);
    }

    fn finish_and_clear(mut self) {
        self.running.store(false, std::sync::atomic::Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            h.join().ok();
        }
        eprint!("\r{}\r", " ".repeat(80));
    }
}

impl Drop for SimpleSpinner {
    fn drop(&mut self) {
        self.running.store(false, std::sync::atomic::Ordering::Relaxed);
    }
}

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
pub fn execute_with_profile(_targets: Vec<String>) -> Result<()> {
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
        result.outcome.files_compiled(),
        compile_time.as_millis()
    );

    if cfg.main.is_some() {
        let jar_start = Instant::now();
        let runtime_jars = resolve_deps_with_scopes(&project, &cfg, &["compile", "runtime"])?;
        build_release_jar(&project, &cfg, &runtime_jars, None, None)?;
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
    let targets = target.into_iter().collect();
    build_impl(targets, false, false)
}

pub fn execute(targets: Vec<String>, jar: bool) -> Result<()> {
    build_impl(targets, jar, false)
}

pub fn execute_keep_going(targets: Vec<String>, jar: bool) -> Result<()> {
    build_impl(targets, jar, true)
}

fn build_impl(targets: Vec<String>, package: bool, keep_going: bool) -> Result<()> {
    let total_start = Instant::now();

    let (config_path, cfg) = config::load_or_find_config()?;
    let project = config::project_dir(&config_path);

    // Run prebuild script
    scripts::run_script(&cfg, "prebuild", &project)?;

    // Ensure JDK is available
    ensure_jdk_for_config(&cfg)?;

    if cfg.workspaces.is_some() {
        let result = build_workspace(&project, &cfg, &targets, package, keep_going, total_start);
        scripts::run_script(&cfg, "postbuild", &project)?;
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
        "{} dependencies {:>40}ms",
        style(format!("{:>12}", "Resolving")).green().bold(),
        resolve_time.as_millis()
    );

    print_compile_result(&cfg.name, &result, compile_time);

    if package && cfg.main.is_some() {
        // Fat JAR: compile + runtime (exclude provided and test)
        let runtime_jars = resolve_deps_with_scopes(&project, &cfg, &["compile", "runtime"])?;
        let class_dir = config::output_classes_dir(&project);
        let resource_dir = project.join("src").join("main").join("resources");
        let fp = compute_packaging_fingerprint(&class_dir, &resource_dir, &runtime_jars, &cfg)?;
        let jar_name = format!("{}-{}.jar", cfg.name, cfg.version.as_deref().unwrap_or("0.0.0"));
        let output_jar = project.join("out").join("release").join(&jar_name);

        if should_skip_packaging(&project, &fp, &output_jar) {
            let jar_size = std::fs::metadata(&output_jar).map(|m| m.len()).unwrap_or(0);
            let size_str = if jar_size >= 1024 * 1024 {
                format!("{:.1} MB", jar_size as f64 / (1024.0 * 1024.0))
            } else {
                format!("{:.0} KB", jar_size as f64 / 1024.0)
            };
            println!(
                "{} {} ({}) (up to date)",
                style(format!("{:>12}", "Packaging")).green().bold(),
                jar_name,
                size_str,
            );
        } else {
            if project.join("ym.config.java").exists() {
                build_with_plugins(&project, &cfg, &runtime_jars, None)?;
            } else {
                build_release_jar(&project, &cfg, &runtime_jars, None, None)?;
            }
            save_packaging_fingerprint(&project, &fp)?;
        }
    }

    scripts::run_script(&cfg, "postbuild", &project)?;

    print_total_time(total_start);
    Ok(())
}

fn print_total_time(start: Instant) {
    let elapsed = start.elapsed();
    let time = if elapsed.as_millis() > 1000 {
        format!("{:.2}s", elapsed.as_secs_f64())
    } else {
        format!("{}ms", elapsed.as_millis())
    };
    println!(
        "{} build in {}",
        style(format!("{:>12}", "Finished")).green().bold(),
        time
    );
}

fn print_workspace_summary(
    compiled: usize, cached: usize, up_to_date: usize,
    failed: usize, skipped: usize, elapsed: std::time::Duration,
) {
    let mut parts = Vec::new();
    if compiled > 0 { parts.push(format!("{} compiled", compiled)); }
    if cached > 0 { parts.push(format!("{} cached", cached)); }
    if failed > 0 { parts.push(format!("{} failed", failed)); }
    if skipped > 0 { parts.push(format!("{} skipped", skipped)); }
    if up_to_date > 0 { parts.push(format!("{} up to date", up_to_date)); }

    let icon = if failed > 0 {
        style(format!("{:>12}", "Compiling")).red().bold()
    } else {
        style(format!("{:>12}", "Compiling")).green().bold()
    };
    let time = if elapsed.as_millis() > 1000 {
        format!("{:.2}s", elapsed.as_secs_f64())
    } else {
        format!("{}ms", elapsed.as_millis())
    };
    println!("{} {} in {}", icon, parts.join(", "), time);
}

fn build_workspace(root: &Path, root_cfg: &YmConfig, targets: &[String], package: bool, keep_going: bool, total_start: Instant) -> Result<()> {
    let spinner = SimpleSpinner::new("Scanning workspace...");

    let ws = WorkspaceGraph::build(root)?;

    let packages = if !targets.is_empty() {
        let mut all = Vec::new();
        for t in targets {
            let closure = ws.transitive_closure(t)?;
            for pkg in closure {
                if !all.contains(&pkg) {
                    all.push(pkg);
                }
            }
        }
        all
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

    spinner.set_message(format!("Scanning workspace ({} modules)...", packages.len()));

    // Validate workspace dependency declarations
    for name in &packages {
        let pkg = ws.get_package(name).unwrap();
        let errors = pkg.config.validate_workspace_deps(root_cfg);
        if !errors.is_empty() {
            for e in &errors {
                eprintln!(
                    "{} {}: {}",
                    console::style(format!("{:>12}", "error")).red().bold(),
                    name, e
                );
            }
            anyhow::bail!("Invalid workspace dependency declarations in '{}'", name);
        }
    }

    // Workspace-level dependency resolution
    let dep_start = Instant::now();

    // Collect each module's own Maven deps
    let own_module_deps: std::collections::HashMap<String, std::collections::BTreeMap<String, String>> = packages
        .iter()
        .map(|name| {
            let pkg = ws.get_package(name).unwrap();
            let mut deps = pkg.config.maven_dependencies_with_root(root_cfg);
            for (k, v) in root_cfg.resolved_resolutions() {
                if deps.contains_key(&k) {
                    deps.insert(k, v);
                }
            }
            (name.clone(), deps)
        })
        .collect();

    // Pre-compute transitive closures for all packages (avoids O(N³) repeated BFS)
    let closure_cache: std::collections::HashMap<String, Vec<String>> = packages
        .iter()
        .map(|name| (name.clone(), ws.transitive_closure(name).unwrap_or_default()))
        .collect();

    // Propagate Maven deps from workspace module dependencies (transitive)
    let all_module_deps: Vec<(String, std::collections::BTreeMap<String, String>)> = packages
        .iter()
        .map(|name| {
            let mut deps = own_module_deps.get(name).cloned().unwrap_or_default();
            // Walk workspace dep graph to include transitive Maven deps
            if let Some(closure) = closure_cache.get(name) {
                for ws_dep in closure {
                    if ws_dep != name {
                        if let Some(ws_dep_deps) = own_module_deps.get(ws_dep) {
                            for (k, v) in ws_dep_deps {
                                deps.entry(k.clone()).or_insert(v.clone());
                            }
                        }
                    }
                }
            }
            (name.clone(), deps)
        })
        .collect();

    let total_deps: usize = all_module_deps.iter().map(|(_, deps)| deps.len()).sum();
    spinner.set_message(format!("Resolving dependencies ({} modules, {} artifacts)...", packages.len(), total_deps));

    let cache = config::maven_cache_dir(root);
    let mut resolved = config::load_resolved_cache_checked(root, root_cfg)?;
    let registries = root_cfg.registry_entries();
    let mut exclusions = root_cfg.exclusions.as_ref().cloned().unwrap_or_default();
    exclusions.extend(root_cfg.resolved_exclusions());

    let resolutions = root_cfg.resolved_resolutions();
    // Spinner stays alive during resolve — resolver updates spinner message with progress
    crate::SPINNER_ACTIVE.store(true, std::sync::atomic::Ordering::Relaxed);
    let per_module_jars = crate::workspace::resolver::resolve_workspace_deps_with_resolutions(
        &all_module_deps, &cache, &mut resolved, &registries, &exclusions, &resolutions,
    )?;
    crate::SPINNER_ACTIVE.store(false, std::sync::atomic::Ordering::Relaxed);
    spinner.finish_and_clear();
    config::save_resolved_cache(root, &resolved)?;
    let dep_time = dep_start.elapsed();
    let total_jars: usize = per_module_jars.values().next().map(|v| v.len()).unwrap_or(0);
    println!(
        "{} dependencies ({} jars) {:>25}ms",
        style(format!("{:>12}", "Resolving")).green().bold(),
        total_jars,
        dep_time.as_millis()
    );

    // Workspace-level build fingerprint: skip entire compilation if nothing changed
    let (ws_build_fp, current_module_fps) = compute_workspace_build_fingerprint(root, targets, &packages, &ws)?;

    if should_skip_workspace_build(root, targets, &ws_build_fp, &packages, &ws) {
        print_workspace_summary(0, 0, packages.len(), 0, 0, total_start.elapsed());
    } else {

    // Load stored per-module fingerprints for shortcut comparison
    let stored_module_fps = load_module_fingerprints(root, targets);

    // Wave scheduling: in-degree based parallel compilation
    let mut in_degree: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    let mut dependents: std::collections::HashMap<String, Vec<String>> = std::collections::HashMap::new();

    for pkg_name in &packages {
        let pkg = ws.get_package(pkg_name).unwrap();
        let ws_deps = pkg.config.workspace_module_deps();
        let relevant_dep_count = ws_deps.iter()
            .filter(|d| packages.contains(d))
            .count();
        in_degree.insert(pkg_name.clone(), relevant_dep_count);
        for dep in ws_deps.iter().filter(|d| packages.contains(d)) {
            dependents.entry(dep.clone()).or_default().push(pkg_name.clone());
        }
    }

    // Create compiler worker pool (warm JVM instances for parallel compilation)
    let pool_size = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .min(packages.len())
        .min(8);
    let worker_pool = if packages.len() > 1 {
        match compiler::worker::CompilerPool::new(pool_size) {
            Ok(p) => Some(p),
            Err(_) => None, // Fall back to direct javac silently
        }
    } else {
        None
    };

    let mut workspace_classpath: Vec<PathBuf> = Vec::new();
    let mut failed_modules: Vec<String> = Vec::new();

    // Workspace output stats
    let total_modules = packages.len();
    let mut compiled_count: usize = 0;
    let mut cached_count: usize = 0;
    let mut up_to_date_count: usize = 0;
    let mut skipped_count: usize = 0;
    let mut processed: usize = 0;
    let verbose = is_verbose();
    let is_tty = console::Term::stderr().is_term();

    // Print initial progress header for non-verbose mode
    if !verbose && total_modules > 1 && is_tty {
        eprint!("   Compiling [{}/{}]", processed, total_modules);
    }

    loop {
        let wave: Vec<String> = in_degree.iter()
            .filter(|(_, deg)| **deg == 0)
            .map(|(name, _)| name.clone())
            .collect();

        if wave.is_empty() { break; }

        for name in &wave {
            in_degree.remove(name);
        }

        // In keep_going mode, skip modules whose dependencies have failed
        let (compilable, skipped): (Vec<&String>, Vec<&String>) = if keep_going {
            let mut comp = Vec::new();
            let mut skip = Vec::new();
            for name in &wave {
                if has_failed_dependency(name, &failed_modules, &ws) {
                    skip.push(name);
                } else {
                    comp.push(name);
                }
            }
            (comp, skip)
        } else {
            (wave.iter().collect(), Vec::new())
        };

        for name in &skipped {
            failed_modules.push((*name).clone());
            skipped_count += 1;
            processed += 1;
            if verbose {
                println!(
                    "{} {} (depends on failed module)",
                    style(format!("{:>12}", "Skipping")).yellow().bold(),
                    name
                );
            } else if is_tty {
                eprint!("\r   Compiling [{}/{}]   ", processed, total_modules);
            }
            if let Some(deps) = dependents.get(*name) {
                for dep in deps {
                    if let Some(deg) = in_degree.get_mut(dep) {
                        *deg -= 1;
                    }
                }
            }
        }

        if compilable.is_empty() { continue; }

        let cp_snapshot = workspace_classpath.clone();
        let root_cfg_snapshot = root_cfg.clone();

        let results: Vec<_> = compilable
            .par_iter()
            .map(|pkg_name| {
                // Per-module fingerprint shortcut: skip compile_project() if unchanged
                if let (Some(current), Some(stored)) = (
                    current_module_fps.get(pkg_name.as_str()),
                    stored_module_fps.get(pkg_name.as_str()),
                ) {
                    if current == stored {
                        return (pkg_name.to_string(), Ok(compiler::CompileResult {
                            success: true,
                            outcome: compiler::CompileOutcome::UpToDate,
                            errors: String::new(),
                        }), std::time::Duration::ZERO);
                    }
                }
                let pkg = ws.get_package(pkg_name.as_str()).unwrap();
                let start = Instant::now();
                let jars = per_module_jars.get(pkg_name.as_str()).cloned().unwrap_or_default();
                let mut classpath = jars;
                classpath.extend(cp_snapshot.clone());
                // Inherit compiler args from workspace root if module doesn't specify them
                let mut module_cfg = pkg.config.clone();
                if module_cfg.compiler.as_ref().and_then(|c| c.args.as_ref()).is_none() {
                    let root_args = root_cfg_snapshot.compiler.as_ref().and_then(|c| c.args.clone());
                    if let Some(args) = root_args {
                        let compiler = module_cfg.compiler.get_or_insert_with(Default::default);
                        compiler.args = Some(args);
                    }
                }
                let result = compile_project_with_pool(&pkg.path, &module_cfg, &classpath, worker_pool.as_ref());
                (pkg_name.to_string(), result, start.elapsed())
            })
            .collect();

        for (pkg_name, result, elapsed) in results {
            let success = match &result {
                Ok(r) if r.success => {
                    processed += 1;
                    match r.outcome {
                        compiler::CompileOutcome::UpToDate => {
                            up_to_date_count += 1;
                            if verbose {
                                print_compile_result(&pkg_name, r, elapsed);
                            } else if is_tty {
                                eprint!("\r   Compiling [{}/{}]   ", processed, total_modules);
                            }
                        }
                        compiler::CompileOutcome::Cached => {
                            cached_count += 1;
                            if is_tty { eprint!("\r{}\r", " ".repeat(40)); }
                            print_compile_result(&pkg_name, r, elapsed);
                        }
                        compiler::CompileOutcome::Compiled(_) => {
                            compiled_count += 1;
                            if is_tty { eprint!("\r{}\r", " ".repeat(40)); }
                            print_compile_result(&pkg_name, r, elapsed);
                        }
                    }
                    if verbose && !r.errors.is_empty() {
                        eprint!("{}", compiler::colorize_errors(&r.errors));
                    }
                    true
                }
                Ok(r) => {
                    processed += 1;
                    if is_tty { eprint!("\r{}\r", " ".repeat(40)); }
                    eprint!("{}", compiler::colorize_errors(&r.errors));
                    if keep_going {
                        failed_modules.push(pkg_name.clone());
                        false
                    } else {
                        bail!("Compilation of '{}' failed", pkg_name);
                    }
                }
                Err(e) => {
                    processed += 1;
                    if is_tty { eprint!("\r{}\r", " ".repeat(40)); }
                    if keep_going {
                        eprintln!("{} compilation of '{}' failed: {}", style(format!("{:>12}", "error")).red().bold(), pkg_name, e);
                        failed_modules.push(pkg_name.clone());
                        false
                    } else {
                        bail!("Compilation of '{}' failed: {}", pkg_name, e);
                    }
                }
            };

            if success {
                let pkg = ws.get_package(&pkg_name).unwrap();
                workspace_classpath.push(config::output_classes_dir(&pkg.path));
            }

            if let Some(deps) = dependents.get(&pkg_name) {
                for dep in deps {
                    if let Some(deg) = in_degree.get_mut(dep) {
                        *deg -= 1;
                    }
                }
            }
        }
    }

    // Clear progress line
    if is_tty { eprint!("\r{}\r", " ".repeat(40)); }

    if !failed_modules.is_empty() {
        print_workspace_summary(compiled_count, cached_count, up_to_date_count,
            failed_modules.len(), skipped_count, total_start.elapsed());
        println!(
            "{} failed: {}",
            style(format!("{:>12}", "error")).red().bold(),
            failed_modules.join(", ")
        );
        bail!("Workspace build failed ({} module(s))", failed_modules.len());
    }

    // Save workspace build fingerprint after successful compilation
    save_workspace_build_fingerprint(root, targets, &ws_build_fp)?;
    save_module_fingerprints(root, targets, &current_module_fps)?;

    print_workspace_summary(compiled_count, cached_count, up_to_date_count,
        0, skipped_count, total_start.elapsed());

    } // end of else block (workspace fingerprint skip)

    if package {
        // Package fat JARs for modules with a `main` field
        // - If target specified: only that module
        // - If no target: all modules with a `main` field
        let jar_targets: Vec<&str> = if !targets.is_empty() {
            targets.iter().map(|s| s.as_str()).collect()
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
            let ws_module_names: std::collections::HashSet<String> = closure.iter()
                .map(|n| n.to_string())
                .collect();

            // Package workspace modules as thin JARs (like Gradle's jar task).
            // This avoids class/resource duplication between BOOT-INF/classes/ and BOOT-INF/lib/.
            let effective_version = pkg.config.version.as_deref()
                .or(root_cfg.version.as_deref())
                .unwrap_or("0.0.0");
            for pkg_name in &closure {
                let p = ws.get_package(pkg_name).unwrap();
                if pkg_name != *jar_target {
                    let thin_jar = package_thin_jar(&p.path, &p.config, effective_version)?;
                    all_deps.push(thin_jar);
                }
            }

            // Resolve all workspace modules' external Maven deps
            crate::RESOLVER_QUIET.store(true, std::sync::atomic::Ordering::Relaxed);
            for pkg_name in &closure {
                let p = ws.get_package(pkg_name).unwrap();
                let module_jars = resolve_deps_with_scopes(&p.path, &p.config, &["compile", "runtime"])?;
                all_deps.extend(module_jars);
            }
            // Also resolve the target app module's own Maven deps
            let runtime_jars = resolve_deps_with_scopes(&pkg.path, &pkg.config, &["compile", "runtime"])?;
            crate::RESOLVER_QUIET.store(false, std::sync::atomic::Ordering::Relaxed);
            all_deps.extend(runtime_jars);

            // Deduplicate and remove published JARs that duplicate workspace modules
            all_deps.sort();
            all_deps.dedup();
            all_deps.retain(|path| {
                if path.is_dir() { return true; }
                let file_name = path.file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default();
                for module_name in &ws_module_names {
                    if file_name.starts_with(&format!("{}-", module_name))
                        && file_name.ends_with(".jar")
                        && !file_name.contains(".thin.") // keep our thin JARs
                    {
                        return false;
                    }
                }
                true
            });

            let class_dir = config::output_classes_dir(&pkg.path);
            let resource_dir = pkg.path.join("src").join("main").join("resources");
            let fp = compute_packaging_fingerprint(&class_dir, &resource_dir, &all_deps, &pkg.config)?;
            let effective_version = pkg.config.version.as_deref()
                .or(root_cfg.version.as_deref())
                .unwrap_or("0.0.0");
            let jar_name = format!("{}-{}.jar", pkg.config.name, effective_version);
            let output_jar = pkg.path.join("out").join("release").join(&jar_name);

            if should_skip_packaging(&pkg.path, &fp, &output_jar) {
                let jar_size = std::fs::metadata(&output_jar).map(|m| m.len()).unwrap_or(0);
                let size_str = if jar_size >= 1024 * 1024 {
                    format!("{:.1} MB", jar_size as f64 / (1024.0 * 1024.0))
                } else {
                    format!("{:.0} KB", jar_size as f64 / 1024.0)
                };
                println!(
                    "{} {} ({}) (up to date)",
                    style(format!("{:>12}", "Packaging")).green().bold(),
                    jar_name,
                    size_str,
                );
            } else {
                if pkg.path.join("ym.config.java").exists() {
                    build_with_plugins(&pkg.path, &pkg.config, &all_deps, root_cfg.version.as_deref())?;
                } else {
                    build_release_jar(&pkg.path, &pkg.config, &all_deps, None, root_cfg.version.as_deref())?;
                }
                save_packaging_fingerprint(&pkg.path, &fp)?;
            }
        }

        if !jar_targets.is_empty() {
            println!(
                "{} {}",
                style(format!("{:>12}", "→")).dim(),
                style(root.join("out").join("release").display()).dim()
            );
        }
    }

    let total_time = total_start.elapsed();
    if total_time.as_millis() > 1000 {
        println!(
            "{} build in {:.2}s",
            style(format!("{:>12}", "Finished")).green().bold(),
            total_time.as_secs_f64()
        );
    } else {
        println!(
            "{} build in {}ms",
            style(format!("{:>12}", "Finished")).green().bold(),
            total_time.as_millis()
        );
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


fn print_compile_result(name: &str, result: &compiler::CompileResult, elapsed: std::time::Duration) {
    match result.outcome {
        compiler::CompileOutcome::UpToDate => {
            println!(
                "{} {} (up to date)",
                style(format!("{:>12}", "Compiling")).green().bold(),
                name,
            );
        }
        compiler::CompileOutcome::Cached => {
            println!(
                "{} {} (cached) {:>30}ms",
                style(format!("{:>12}", "Compiling")).green().bold(),
                name,
                elapsed.as_millis()
            );
        }
        compiler::CompileOutcome::Compiled(n) => {
            println!(
                "{} {} ({} files) {:>27}ms",
                style(format!("{:>12}", "Compiling")).green().bold(),
                name,
                n,
                elapsed.as_millis()
            );
        }
    }
}

/// Build a fat/executable JAR containing all classes and dependencies.
pub(crate) fn build_release_jar(project: &Path, cfg: &YmConfig, jars: &[PathBuf], output_base: Option<&Path>, root_version: Option<&str>) -> Result<()> {
    let out = config::output_classes_dir(project);
    let base = output_base.unwrap_or(project);
    let release_dir = base.join("out").join("release");
    std::fs::create_dir_all(&release_dir)?;

    let effective_version = cfg.version.as_deref()
        .or(root_version)
        .unwrap_or("0.0.0");
    let jar_name = format!("{}-{}.jar", cfg.name, effective_version);
    let jar_path = release_dir.join(&jar_name);

    let staging = project.join("out").join(".release-staging");
    if staging.exists() {
        std::fs::remove_dir_all(&staging)?;
    }
    std::fs::create_dir_all(&staging)?;

    copy_dir_recursive(&out, &staging)?;

    // Single-pass: extract deps + collect mergeable META-INF + detect class conflicts
    let mut mergeable: std::collections::HashMap<String, Vec<String>> = std::collections::HashMap::new();
    let mut class_sources: std::collections::HashMap<String, Vec<String>> = std::collections::HashMap::new();

    let total_deps = jars.len();
    let pack_start = Instant::now();
    for (idx, dep) in jars.iter().enumerate() {
        if !dep.exists() {
            continue;
        }

        // Progress: show count and elapsed time during dependency extraction
        eprint!(
            "\r{} {} [{}/{}] {:.1}s   ",
            style(format!("{:>12}", "Packaging")).green().bold(),
            jar_name,
            idx + 1,
            total_deps,
            pack_start.elapsed().as_secs_f64()
        );

        if dep.is_dir() {
            copy_dir_recursive(dep, &staging)?;
        } else {
            let jar_file_name = dep.file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| dep.display().to_string());

            let file = match std::fs::File::open(dep) {
                Ok(f) => f,
                Err(_) => continue,
            };
            let mut archive = match zip::ZipArchive::new(std::io::BufReader::new(file)) {
                Ok(a) => a,
                Err(_) => continue,
            };

            for i in 0..archive.len() {
                let mut entry = match archive.by_index(i) {
                    Ok(e) => e,
                    Err(_) => continue,
                };
                let name = entry.name().to_string();

                // Skip META-INF/MANIFEST.MF (we write our own), signature files
                // (cause SecurityException in fat JARs), and unsafe paths
                if name == "META-INF/MANIFEST.MF"
                    || name.starts_with('/')
                    || name.contains("..")
                    || (name.starts_with("META-INF/") && (
                        name.ends_with(".SF")
                        || name.ends_with(".DSA")
                        || name.ends_with(".RSA")
                        || name.ends_with(".EC")
                    ))
                {
                    continue;
                }

                // Track class conflicts
                if name.ends_with(".class")
                    && !name.starts_with("META-INF/")
                    && name != "module-info.class"
                {
                    class_sources.entry(name.clone()).or_default().push(jar_file_name.clone());
                }

                // Collect mergeable META-INF entries (don't extract, will merge later)
                let is_mergeable = !entry.is_dir() && (
                    name.starts_with("META-INF/services/") ||
                    name == "META-INF/spring.factories" ||
                    (name.starts_with("META-INF/spring/") && name.ends_with(".imports"))
                );
                if is_mergeable {
                    use std::io::Read;
                    let mut content = String::new();
                    let _ = entry.read_to_string(&mut content);
                    mergeable.entry(name).or_default().push(content);
                    continue;
                }

                // Extract entry to staging
                if entry.is_dir() {
                    let _ = std::fs::create_dir_all(staging.join(&name));
                } else {
                    let target = staging.join(&name);
                    if let Some(parent) = target.parent() {
                        let _ = std::fs::create_dir_all(parent);
                    }
                    if let Ok(mut out_file) = std::fs::File::create(&target) {
                        let _ = std::io::copy(&mut entry, &mut out_file);
                    }
                }
            }
        }
    }
    // Clear progress line
    eprint!("\r{}\r", " ".repeat(60));

    // Report class conflicts across dependency JARs
    {
        let mut conflicts: Vec<(String, Vec<String>)> = class_sources
            .into_iter()
            .filter(|(_, sources)| sources.len() > 1)
            .collect();
        if !conflicts.is_empty() {
            conflicts.sort_by(|a, b| a.0.cmp(&b.0));
            let total = conflicts.len();
            println!(
                "{} fat JAR: {} duplicate class(es) detected across dependency JARs",
                style(format!("{:>12}", "warning")).yellow().bold(),
                total
            );
            for (class_path, sources) in conflicts.iter().take(10) {
                let class_name = class_path
                    .strip_suffix(".class")
                    .unwrap_or(class_path)
                    .replace('/', ".");
                println!("    {} → {}", class_name, sources.join(", "));
            }
            if total > 10 {
                println!("    ... and {} more", total - 10);
            }
        }
    }

    // Write merged META-INF files (combine entries from all JARs, deduplicate lines)
    for (meta_file, contents) in &mergeable {
        let merged_path = staging.join(meta_file);
        if let Some(parent) = merged_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        if contents.len() == 1 {
            std::fs::write(&merged_path, &contents[0])?;
        } else {
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

    // Create JAR using zip crate (no JVM subprocess needed)
    let jar_file = std::fs::File::create(&jar_path)?;
    let mut zip_writer = zip::ZipWriter::new(std::io::BufWriter::new(jar_file));
    let zip_options = zip::write::SimpleFileOptions::default();

    eprint!(
        "\r{} {} [creating jar] {:.1}s   ",
        style(format!("{:>12}", "Packaging")).green().bold(),
        jar_name,
        pack_start.elapsed().as_secs_f64()
    );

    // JAR spec: META-INF/MANIFEST.MF must be the first entry
    zip_writer.add_directory("META-INF/", zip_options)?;
    zip_writer.start_file("META-INF/MANIFEST.MF", zip_options)?;
    std::io::copy(
        &mut std::fs::File::open(manifest_dir.join("MANIFEST.MF"))?,
        &mut zip_writer,
    )?;

    for walk_entry in walkdir::WalkDir::new(&staging).sort_by_file_name() {
        let walk_entry = walk_entry?;
        let path = walk_entry.path();
        let relative = path.strip_prefix(&staging)?;
        let name = relative.to_string_lossy().replace('\\', "/");
        if name.is_empty() || name == "META-INF" || name == "META-INF/MANIFEST.MF" {
            continue;
        }

        if walk_entry.file_type().is_dir() {
            let dir_name = if name.ends_with('/') { name } else { format!("{}/", name) };
            zip_writer.add_directory(dir_name, zip_options)?;
        } else {
            zip_writer.start_file(&name, zip_options)?;
            let mut f = std::fs::File::open(path)?;
            std::io::copy(&mut f, &mut zip_writer)?;
        }
    }

    zip_writer.finish()?;

    let pack_elapsed = pack_start.elapsed();
    let jar_size = std::fs::metadata(&jar_path).map(|m| m.len()).unwrap_or(0);
    let size_str = if jar_size >= 1024 * 1024 {
        format!("{:.1} MB", jar_size as f64 / (1024.0 * 1024.0))
    } else {
        format!("{:.0} KB", jar_size as f64 / 1024.0)
    };
    eprint!("\r{}\r", " ".repeat(80));
    println!(
        "{} {} ({}) {:>22}ms",
        style(format!("{:>12}", "Packaging")).green().bold(),
        jar_name,
        size_str,
        pack_elapsed.as_millis()
    );

    // Clean up staging directory (can be slow for large projects with many files)
    eprint!(
        "\r{} cleaning up staging...   ",
        style(format!("{:>12}", "Packaging")).green().bold(),
    );
    let _ = std::fs::remove_dir_all(&staging);
    eprint!("\r{}\r", " ".repeat(60));

    Ok(())
}


/// 通过插件系统执行打包。
/// 解析 plugins，下载插件 JAR，启动 JVM 执行 ConfigRunner，
/// 由插件决定如何打包（Spring Boot JAR、fat JAR 等）。
pub fn build_with_plugins(
    project: &Path,
    cfg: &YmConfig,
    runtime_jars: &[PathBuf],
    root_version: Option<&str>,
) -> Result<()> {
    let out = config::output_classes_dir(project);
    let resources_dir = project.join("src").join("main").join("resources");
    let effective_version = cfg.version.as_deref()
        .or(root_version)
        .unwrap_or("0.0.0");
    let jar_name = format!("{}-{}.jar", cfg.name, effective_version);

    println!(
        "{} {} (plugins)",
        style(format!("{:>12}", "Packaging")).green().bold(),
        jar_name,
    );

    // 收集插件 JAR 的 classpath
    let plugin_cp = resolve_plugin_classpath(project, cfg)?;
    if plugin_cp.is_empty() {
        bail!("No plugin JARs found. Ensure plugins are installed.");
    }

    // runtime classpath 字符串
    let runtime_cp: String = runtime_jars.iter()
        .filter(|j| j.exists())
        .map(|j| j.to_string_lossy().to_string())
        .collect::<Vec<_>>()
        .join(":");

    // java 可执行文件
    let java_home = jvm::ensure_jdk(cfg.target.as_deref().unwrap_or("25"), None, false)?;
    let java = if java_home.as_os_str() == "system" {
        PathBuf::from("java")
    } else {
        java_home.join("bin").join("java")
    };

    // ym.json 序列化为临时文件，传给 ConfigRunner
    let config_json_path = project.join("out").join(".ym-config.json");
    if let Some(parent) = config_json_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let config_json = serde_json::to_string(cfg)?;
    std::fs::write(&config_json_path, &config_json)?;

    let pack_start = Instant::now();

    // 检查是否有 ym.config.java，复制为合法文件名后编译
    let ym_config_java = project.join("ym.config.java");
    let mut extra_cp = String::new();
    if ym_config_java.exists() {
        let config_out = project.join("out").join(".ym-config-classes");
        std::fs::create_dir_all(&config_out)?;

        // ym.config.java → YmConfig.java（合法 Java 文件名）
        let temp_source = config_out.join("YmConfig.java");
        std::fs::copy(&ym_config_java, &temp_source)?;

        let javac = if java_home.as_os_str() == "system" {
            PathBuf::from("javac")
        } else {
            java_home.join("bin").join("javac")
        };
        let javac_status = std::process::Command::new(&javac)
            .arg("--enable-preview")
            .arg("--source").arg("25")
            .arg("-cp").arg(&plugin_cp)
            .arg("-d").arg(&config_out)
            .arg(&temp_source)
            .status()?;
        if !javac_status.success() {
            bail!("Failed to compile ym.config.java");
        }
        extra_cp = format!(":{}", config_out.display());
    }

    // 调用 ym.internal.ConfigRunner
    let full_cp = format!("{}{}", plugin_cp, extra_cp);
    let status = std::process::Command::new(&java)
        .arg("--enable-preview")
        .arg("-cp").arg(&full_cp)
        .arg(format!("-Dym.project.dir={}", project.display()))
        .arg(format!("-Dym.config.json={}", config_json_path.display()))
        .arg(format!("-Dym.runtime.classpath={}", runtime_cp))
        .arg(format!("-Dym.project.name={}", cfg.name))
        .arg(format!("-Dym.project.version={}", effective_version))
        .arg("ym.internal.ConfigRunner")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit())
        .output()
        .with_context(|| "Failed to start JVM for plugin execution")?;

    if !status.status.success() {
        bail!("Plugin execution failed (exit code: {})", status.status.code().unwrap_or(-1));
    }

    // 解析 Build Plan JSON（stdout）
    let build_plan = String::from_utf8_lossy(&status.stdout);
    eprintln!(
        "{} Build Plan received ({} bytes)",
        style(format!("{:>12}", "Plugins")).cyan().bold(),
        build_plan.len()
    );

    // TODO: 解析 Build Plan JSON，执行 Task DAG
    // 当前阶段：直接交给插件 Task 在 JVM 中执行完成
    // 后续：ym 核心解析 DAG 并调度

    let pack_elapsed = pack_start.elapsed();
    let output_jar = project.join("out").join("release").join(&jar_name);
    if output_jar.exists() {
        let jar_size = std::fs::metadata(&output_jar).map(|m| m.len()).unwrap_or(0);
        let size_str = if jar_size >= 1024 * 1024 {
            format!("{:.1} MB", jar_size as f64 / (1024.0 * 1024.0))
        } else {
            format!("{:.0} KB", jar_size as f64 / 1024.0)
        };
        println!(
            "{} {} ({}) {:>22}ms",
            style(format!("{:>12}", "Packaged")).green().bold(),
            jar_name,
            size_str,
            pack_elapsed.as_millis()
        );
    }

    Ok(())
}

/// Resolve plugin classpath: extract plugin dependencies from devDependencies,
/// resolve their full transitive dependency tree via ym's Maven resolver,
/// and return the complete classpath string.
/// Package a workspace module's compiled classes + resources into a thin JAR.
/// Similar to Gradle's `jar` task. Output: <module>/out/release/<name>.thin.<version>.jar
fn package_thin_jar(project: &Path, cfg: &config::schema::YmConfig, version: &str) -> Result<PathBuf> {
    let classes_dir = config::output_classes_dir(project);
    let jar_name = format!("{}.thin.{}.jar", cfg.name, version);
    let release_dir = project.join("out").join("release");
    std::fs::create_dir_all(&release_dir)?;
    let jar_path = release_dir.join(&jar_name);

    // Skip if up-to-date (thin JAR newer than classes dir)
    if jar_path.exists() {
        let jar_mtime = std::fs::metadata(&jar_path).and_then(|m| m.modified()).ok();
        let classes_mtime = walkdir::WalkDir::new(&classes_dir)
            .into_iter()
            .flatten()
            .filter(|e| e.file_type().is_file())
            .filter_map(|e| e.metadata().ok()?.modified().ok())
            .max();
        if let (Some(jm), Some(cm)) = (jar_mtime, classes_mtime) {
            if jm >= cm {
                return Ok(jar_path);
            }
        }
    }

    let jar_file = std::fs::File::create(&jar_path)?;
    let mut zos = zip::ZipWriter::new(std::io::BufWriter::new(jar_file));
    let zip_options = zip::write::SimpleFileOptions::default();

    // Add classes
    if classes_dir.exists() {
        for entry in walkdir::WalkDir::new(&classes_dir) {
            let entry = entry?;
            let path = entry.path();
            let relative = path.strip_prefix(&classes_dir)?;
            let name = relative.to_string_lossy().replace('\\', "/");
            if name.is_empty() { continue; }

            if entry.file_type().is_dir() {
                let dir_name = if name.ends_with('/') { name } else { format!("{}/", name) };
                zos.add_directory(dir_name, zip_options)?;
            } else {
                zos.start_file(&name, zip_options)?;
                let mut f = std::fs::File::open(path)?;
                std::io::copy(&mut f, &mut zos)?;
            }
        }
    }

    zos.finish()?;
    Ok(jar_path)
}

fn resolve_plugin_classpath(project: &Path, cfg: &YmConfig) -> Result<String> {
    // Build a config that contains only plugin-related devDependencies
    let mut plugin_cfg = cfg.clone();
    let mut plugin_deps = std::collections::BTreeMap::new();

    if let Some(ref dev_deps) = cfg.dev_dependencies {
        for (key, value) in dev_deps {
            let artifact_id = artifact_id_from_key(key);
            if artifact_id.contains("yummy-plugin") {
                plugin_deps.insert(key.clone(), value.clone());
            }
        }
    }
    if let Some(ref deps) = cfg.dependencies {
        for (key, value) in deps {
            let artifact_id = artifact_id_from_key(key);
            if artifact_id.contains("yummy-plugin") {
                plugin_deps.insert(key.clone(), value.clone());
            }
        }
    }

    if plugin_deps.is_empty() {
        return Ok(String::new());
    }

    // Use ym's standard dependency resolver to get the full transitive classpath
    plugin_cfg.dependencies = Some(plugin_deps);
    plugin_cfg.dev_dependencies = None;
    let jars = resolve_deps_with_scopes(project, &plugin_cfg, &["compile", "runtime"])?;

    Ok(jars.iter()
        .filter(|j| j.exists())
        .map(|j| j.to_string_lossy().to_string())
        .collect::<Vec<_>>()
        .join(":"))
}

/// Scan all yummy-plugin JARs in Maven cache for `dependencyManagement` declarations.
/// Downloads BOM POMs and extracts managed versions.
/// Plugin version is used as BOM version (e.g., yummy-plugin-spring-boot:4.0.3 → spring-boot-dependencies:4.0.3).
pub fn collect_plugin_managed_versions(project: &Path, cfg: &YmConfig) -> Result<std::collections::BTreeMap<String, String>> {
    use std::sync::Mutex;
    static LOGGED_BOMS: std::sync::LazyLock<Mutex<std::collections::HashSet<String>>> =
        std::sync::LazyLock::new(|| Mutex::new(std::collections::HashSet::new()));

    let mut managed = std::collections::BTreeMap::new();
    let cache_dir = config::maven_cache_dir(project);
    let plugin_base = cache_dir.join("sh.yummy");

    if !plugin_base.exists() { return Ok(managed); }

    // Scan sh.yummy/<artifact>/<version>/<artifact>-<version>.jar
    for artifact_dir in std::fs::read_dir(&plugin_base)?.flatten() {
        let artifact_name = artifact_dir.file_name().to_string_lossy().to_string();
        if !artifact_name.contains("yummy-plugin") { continue; }

        for version_dir in std::fs::read_dir(artifact_dir.path())?.flatten() {
            let version = version_dir.file_name().to_string_lossy().to_string();
            let jar_path = version_dir.path().join(format!("{}-{}.jar", artifact_name, version));
            if !jar_path.exists() { continue; }

            // Read plugin metadata from JAR
            let file = match std::fs::File::open(&jar_path) {
                Ok(f) => f,
                Err(_) => continue,
            };
            let mut archive = match zip::ZipArchive::new(std::io::BufReader::new(file)) {
                Ok(a) => a,
                Err(_) => continue,
            };

            for i in 0..archive.len() {
                let mut entry = match archive.by_index(i) {
                    Ok(e) => e,
                    Err(_) => continue,
                };
                let name = entry.name().to_string();
                if !name.starts_with("META-INF/ym-plugins/") || !name.ends_with(".json") { continue; }

                let mut content = String::new();
                use std::io::Read;
                let _ = entry.read_to_string(&mut content);

                // Extract dependencyManagement field (format: "groupId:artifactId" or "groupId:artifactId:version")
                let dm_coord = extract_json_field(&content, "dependencyManagement");
                if let Some(bom_ga) = dm_coord {
                    let bom_parts: Vec<&str> = bom_ga.split(':').collect();
                    if bom_parts.len() < 2 { continue; }

                    // BOM version: from metadata GAV if present, otherwise use plugin version
                    let bom_version = if bom_parts.len() >= 3 {
                        bom_parts[2].to_string()
                    } else {
                        version.clone()
                    };

                    let bom_pom_path = cache_dir
                        .join(bom_parts[0])
                        .join(bom_parts[1])
                        .join(&bom_version)
                        .join(format!("{}-{}.pom", bom_parts[1], bom_version));

                    // Download BOM POM if not cached
                    if !bom_pom_path.exists() {
                        let url = format!(
                            "https://repo1.maven.org/maven2/{}/{}/{}/{}-{}.pom",
                            bom_parts[0].replace('.', "/"),
                            bom_parts[1], bom_version, bom_parts[1], bom_version
                        );
                        if let Some(parent) = bom_pom_path.parent() {
                            let _ = std::fs::create_dir_all(parent);
                        }
                        if let Ok(client) = client_for_bom() {
                            if let Ok(resp) = client.get(&url).send() {
                                if resp.status().is_success() {
                                    if let Ok(bytes) = resp.bytes() {
                                        let _ = std::fs::write(&bom_pom_path, &bytes);
                                    }
                                }
                            }
                        }
                    }

                    // Parse BOM POM and extract managed versions
                    if bom_pom_path.exists() {
                        if let Ok(pom_content) = std::fs::read_to_string(&bom_pom_path) {
                            if let Ok(doc) = roxmltree::Document::parse(&pom_content) {
                                let props = crate::workspace::resolver::collect_pom_properties(&doc);
                                if let Ok(client) = client_for_bom() {
                                    let bom_managed = crate::workspace::resolver::collect_managed_versions_with_bom(
                                        &doc, &props, &client, &cache_dir,
                                        &cfg.registry_entries(), 0,
                                    );
                                    for (k, v) in bom_managed {
                                        managed.entry(k).or_insert(v);
                                    }
                                    let bom_key = format!("{}:{}", bom_ga, managed.len());
                                    if LOGGED_BOMS.lock().unwrap().insert(bom_key) {
                                        eprintln!(
                                            "{} Applied {} ({} version constraints)",
                                            console::style(format!("{:>12}", "BOM")).cyan().bold(),
                                            bom_ga,
                                            managed.len()
                                        );
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    Ok(managed)
}

fn client_for_bom() -> Result<reqwest::blocking::Client> {
    Ok(reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()?)
}

fn extract_json_field(json: &str, field: &str) -> Option<String> {
    let pattern = format!("\"{}\"", field);
    let idx = json.find(&pattern)?;
    let rest = &json[idx + pattern.len()..];
    let colon = rest.find(':')?;
    let rest = &rest[colon + 1..];
    let quote_start = rest.find('"')?;
    let rest = &rest[quote_start + 1..];
    let quote_end = rest.find('"')?;
    Some(rest[..quote_end].to_string())
}

/// Compute a fingerprint for packaging inputs (class files, dependencies, resources, config).
/// If all inputs are unchanged and the output JAR exists, packaging can be skipped.
fn compute_packaging_fingerprint(
    class_dir: &Path,
    resource_dir: &Path,
    dep_jars: &[PathBuf],
    cfg: &YmConfig,
) -> Result<String> {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();

    // 1. Dependency JARs: path + size (sorted for determinism)
    let mut dep_entries: Vec<(String, u64)> = Vec::new();
    for jar in dep_jars {
        if let Ok(meta) = std::fs::metadata(jar) {
            dep_entries.push((jar.to_string_lossy().to_string(), meta.len()));
        }
    }
    dep_entries.sort_by(|a, b| a.0.cmp(&b.0));
    for (path, size) in &dep_entries {
        hasher.update(b"dep:");
        hasher.update(path.as_bytes());
        hasher.update(&size.to_le_bytes());
    }

    // 2. Class files: path + size + mtime (sorted)
    if class_dir.exists() {
        let mut class_entries: Vec<(String, u64, u64)> = Vec::new();
        for entry in walkdir::WalkDir::new(class_dir).sort_by_file_name() {
            let entry = entry?;
            if entry.file_type().is_file() {
                let rel = entry.path().strip_prefix(class_dir)
                    .unwrap_or(entry.path())
                    .to_string_lossy().to_string();
                let meta = entry.metadata()?;
                let mtime = meta.modified().ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                class_entries.push((rel, meta.len(), mtime));
            }
        }
        for (path, size, mtime) in &class_entries {
            hasher.update(b"cls:");
            hasher.update(path.as_bytes());
            hasher.update(&size.to_le_bytes());
            hasher.update(&mtime.to_le_bytes());
        }
    }

    // 3. Resource files: path + size + mtime (sorted)
    if resource_dir.exists() {
        let mut res_entries: Vec<(String, u64, u64)> = Vec::new();
        for entry in walkdir::WalkDir::new(resource_dir).sort_by_file_name() {
            let entry = entry?;
            if entry.file_type().is_file() {
                let rel = entry.path().strip_prefix(resource_dir)
                    .unwrap_or(entry.path())
                    .to_string_lossy().to_string();
                let meta = entry.metadata()?;
                let mtime = meta.modified().ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                res_entries.push((rel, meta.len(), mtime));
            }
        }
        for (path, size, mtime) in &res_entries {
            hasher.update(b"res:");
            hasher.update(path.as_bytes());
            hasher.update(&size.to_le_bytes());
            hasher.update(&mtime.to_le_bytes());
        }
    }

    // 4. Config: main_class + version
    if let Some(ref main) = cfg.main {
        hasher.update(b"main:");
        hasher.update(main.as_bytes());
    }
    if let Some(ref ver) = cfg.version {
        hasher.update(b"ver:");
        hasher.update(ver.as_bytes());
    }
    hasher.update(b"name:");
    hasher.update(cfg.name.as_bytes());
    hasher.update(b"group:");
    hasher.update(cfg.group_id.as_bytes());

    Ok(format!("{:x}", hasher.finalize()))
}

/// Check if packaging can be skipped by comparing fingerprint with stored value.
fn should_skip_packaging(project: &Path, fingerprint: &str, output_jar: &Path) -> bool {
    if !output_jar.exists() {
        return false;
    }
    let name = project.file_name().unwrap_or_default().to_string_lossy().to_string();
    let fps = load_packaging_fingerprints(project);
    fps.get(&name).map(|s| s.as_str()) == Some(fingerprint)
}

/// Save the packaging fingerprint after a successful build.
fn save_packaging_fingerprint(project: &Path, fingerprint: &str) -> Result<()> {
    let name = project.file_name().unwrap_or_default().to_string_lossy().to_string();
    let fp_path = packaging_fingerprints_path(project);
    if let Some(parent) = fp_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut fps = load_packaging_fingerprints(project);
    fps.insert(name, fingerprint.to_string());
    let json = serde_json::to_string_pretty(&fps)?;
    std::fs::write(&fp_path, json)?;
    Ok(())
}

fn packaging_fingerprints_path(project: &Path) -> PathBuf {
    let root = config::find_workspace_root(project).unwrap_or_else(|| project.to_path_buf());
    root.join(config::CACHE_DIR).join("packaging-fingerprints.json")
}

fn load_packaging_fingerprints(project: &Path) -> std::collections::HashMap<String, String> {
    let fp_path = packaging_fingerprints_path(project);
    std::fs::read_to_string(&fp_path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// Compute a workspace-level build fingerprint from source mtimes, config mtimes, and dep state.
fn compute_workspace_build_fingerprint(
    root: &Path,
    targets: &[String],
    packages: &[String],
    ws: &WorkspaceGraph,
) -> Result<(String, std::collections::HashMap<String, String>)> {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();

    // 1. Target names (different targets produce different fingerprints)
    hasher.update(b"target:");
    hasher.update(targets_cache_key(targets).as_bytes());

    // 2. Root package.toml mtime
    let root_config = root.join(config::CONFIG_FILE);
    if let Ok(meta) = std::fs::metadata(&root_config) {
        let mtime = meta.modified().ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_millis())
            .unwrap_or(0);
        hasher.update(b"root_cfg:");
        hasher.update(&mtime.to_le_bytes());
    }

    // 3. resolved.json content hash (dependency changes)
    let resolved_path = root.join(config::CACHE_DIR).join(config::RESOLVED_FILE);
    if let Ok(content) = std::fs::read(&resolved_path) {
        hasher.update(b"resolved:");
        hasher.update(&content);
    }

    // 4. Per-module: config mtime + source file mtimes + resource file mtimes
    // Use rayon for parallel scanning of module source directories
    let module_fingerprints: Vec<(String, String)> = packages
        .par_iter()
        .filter_map(|name| {
            let pkg = ws.get_package(name)?;
            let mut mod_hasher = Sha256::new();

            // Module config mtime
            let config_path = pkg.path.join(config::CONFIG_FILE);
            if let Ok(meta) = std::fs::metadata(&config_path) {
                let mtime = meta.modified().ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_millis())
                    .unwrap_or(0);
                mod_hasher.update(&mtime.to_le_bytes());
            }

            // Source files: collect (relative_path, mtime_millis, size) sorted by path
            let src_dir = config::source_dir_for(&pkg.path, &pkg.config);
            if src_dir.exists() {
                let mut entries: Vec<(String, u128, u64)> = Vec::new();
                for entry in walkdir::WalkDir::new(&src_dir).into_iter().filter_map(|e| e.ok()) {
                    if entry.file_type().is_file() {
                        let rel = entry.path().strip_prefix(&src_dir)
                            .unwrap_or(entry.path())
                            .to_string_lossy().to_string();
                        if let Ok(meta) = entry.metadata() {
                            let mtime = meta.modified().ok()
                                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                                .map(|d| d.as_millis())
                                .unwrap_or(0);
                            entries.push((rel, mtime, meta.len()));
                        }
                    }
                }
                entries.sort_by(|a, b| a.0.cmp(&b.0));
                for (path, mtime, size) in &entries {
                    mod_hasher.update(path.as_bytes());
                    mod_hasher.update(&mtime.to_le_bytes());
                    mod_hasher.update(&size.to_le_bytes());
                }
            }

            // Resource files
            let res_dir = pkg.path.join("src").join("main").join("resources");
            if res_dir.exists() {
                let mut entries: Vec<(String, u128, u64)> = Vec::new();
                for entry in walkdir::WalkDir::new(&res_dir).into_iter().filter_map(|e| e.ok()) {
                    if entry.file_type().is_file() {
                        let rel = entry.path().strip_prefix(&res_dir)
                            .unwrap_or(entry.path())
                            .to_string_lossy().to_string();
                        if let Ok(meta) = entry.metadata() {
                            let mtime = meta.modified().ok()
                                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                                .map(|d| d.as_millis())
                                .unwrap_or(0);
                            entries.push((rel, mtime, meta.len()));
                        }
                    }
                }
                entries.sort_by(|a, b| a.0.cmp(&b.0));
                for (path, mtime, size) in &entries {
                    mod_hasher.update(path.as_bytes());
                    mod_hasher.update(&mtime.to_le_bytes());
                    mod_hasher.update(&size.to_le_bytes());
                }
            }

            Some((name.clone(), format!("{:x}", mod_hasher.finalize())))
        })
        .collect();

    // Merge module fingerprints in sorted order for determinism
    let mut sorted_fps = module_fingerprints;
    sorted_fps.sort_by(|a, b| a.0.cmp(&b.0));
    for (name, fp) in &sorted_fps {
        hasher.update(b"mod:");
        hasher.update(name.as_bytes());
        hasher.update(fp.as_bytes());
    }

    let per_module: std::collections::HashMap<String, String> = sorted_fps.into_iter().collect();

    Ok((format!("{:x}", hasher.finalize()), per_module))
}

/// Get the fingerprint file path for a workspace build target.
/// Build a stable cache key from the targets list.
fn targets_cache_key(targets: &[String]) -> String {
    if targets.is_empty() {
        "__all__".to_string()
    } else {
        let mut sorted = targets.to_vec();
        sorted.sort();
        sorted.join(",")
    }
}

fn workspace_build_fp_path(root: &Path, targets: &[String]) -> PathBuf {
    let key = targets_cache_key(targets);
    let target_hash = incremental::hash_bytes(key.as_bytes());
    root.join(config::CACHE_DIR).join(format!("workspace-build-fingerprint-{}", &target_hash[..12]))
}

/// Check if the workspace build can be skipped.
/// Requires both fingerprint match AND output directories exist (guards against
/// stale fingerprints on self-hosted runners where .ym/ persists but out/ is cleaned).
fn should_skip_workspace_build(root: &Path, targets: &[String], fingerprint: &str, packages: &[String], ws: &crate::workspace::graph::WorkspaceGraph) -> bool {
    let fp_path = workspace_build_fp_path(root, targets);
    match std::fs::read_to_string(&fp_path) {
        Ok(stored) if stored.trim() == fingerprint => {
            // Verify at least one module's out/classes dir exists
            let any_output = packages.iter().any(|name| {
                ws.get_package(name).map_or(false, |pkg| {
                    pkg.path.join(config::OUTPUT_DIR).join(config::CLASSES_DIR).exists()
                })
            });
            if !any_output {
                eprintln!("  {} Stale build fingerprint (output dirs missing), rebuilding...",
                    console::style("!").yellow());
            }
            any_output
        }
        _ => false,
    }
}

/// Save the workspace build fingerprint after a successful build.
fn save_workspace_build_fingerprint(root: &Path, targets: &[String], fingerprint: &str) -> Result<()> {
    let fp_path = workspace_build_fp_path(root, targets);
    if let Some(parent) = fp_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&fp_path, fingerprint)?;
    Ok(())
}

/// Load per-module fingerprints from disk.
fn load_module_fingerprints(root: &Path, targets: &[String]) -> std::collections::HashMap<String, String> {
    let key = targets_cache_key(targets);
    let target_hash = incremental::hash_bytes(key.as_bytes());
    let path = root.join(config::CACHE_DIR)
        .join(format!("workspace-module-fps-{}", &target_hash[..12]));
    match std::fs::read_to_string(&path) {
        Ok(content) => serde_json::from_str(&content).unwrap_or_default(),
        Err(_) => std::collections::HashMap::new(),
    }
}

/// Save per-module fingerprints to disk after a successful build.
fn save_module_fingerprints(root: &Path, targets: &[String], fps: &std::collections::HashMap<String, String>) -> Result<()> {
    let key = targets_cache_key(targets);
    let target_hash = incremental::hash_bytes(key.as_bytes());
    let path = root.join(config::CACHE_DIR)
        .join(format!("workspace-module-fps-{}", &target_hash[..12]));
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, serde_json::to_string(fps)?)?;
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

/// Resolve dependencies filtered by scope. Used for scope-specific classpath construction.
pub fn resolve_deps_with_scopes(project: &Path, cfg: &YmConfig, scopes: &[&str]) -> Result<Vec<PathBuf>> {
    use crate::workspace::resolver::RegistryEntry;
    let mut registries: Vec<RegistryEntry> = Vec::new();
    let mut resolutions = cfg.resolved_resolutions();

    // Apply BOM managed versions from plugins as constraints ("at least this version").
    // Unlike resolutions (forced), constraints only upgrade versions, never downgrade.
    // Artifacts not in the dependency tree are ignored (constraints don't introduce new deps).
    let constraints = collect_plugin_managed_versions(project, cfg).unwrap_or_default();

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
                for (k, v) in root_cfg.resolved_resolutions() {
                    if d.contains_key(&k) {
                        d.insert(k.clone(), v.clone());
                    }
                    resolutions.insert(k, v);
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
    exclusions.extend(cfg.resolved_exclusions());
    // Also inherit exclusions from workspace root
    if let Some(ws_root) = config::find_workspace_root(project) {
        if ws_root != project {
            if let Ok(root_cfg) = config::load_config(&ws_root.join(config::CONFIG_FILE)) {
                exclusions.extend(root_cfg.resolved_exclusions());
            }
        }
    }

    // Build dep_scopes: map each direct dep's GA to its declared scope
    let dep_scopes = build_dep_scope_map(cfg, scopes);

    let jars = crate::workspace::resolver::resolve_and_download_with_constraints(
        &deps, &cache, &mut resolved, &registries, &exclusions, &resolutions, &constraints, &dep_scopes,
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
    use crate::config::schema::{DependencyValue, is_maven_dep};
    let mut map = std::collections::HashMap::new();
    // Process [dependencies]
    if let Some(ref deps) = cfg.dependencies {
        for (key, value) in deps {
            if !is_maven_dep(key) { continue; }
            let resolved = cfg.resolve_key(key);
            let scope = match value {
                DependencyValue::Simple(_) => "compile".to_string(),
                DependencyValue::Detailed(spec) => spec.scope.clone().unwrap_or_else(|| "compile".to_string()),
            };
            map.insert(resolved, scope);
        }
    }
    // Process [devDependencies] — effective scope "provided"
    if let Some(ref dev_deps) = cfg.dev_dependencies {
        for (key, _value) in dev_deps {
            if !is_maven_dep(key) { continue; }
            let resolved = cfg.resolve_key(key);
            map.insert(resolved, "provided".to_string());
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
    let mut resolutions = cfg.resolved_resolutions();

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
                for (k, v) in root_cfg.resolved_resolutions() {
                    if d.contains_key(&k) {
                        d.insert(k.clone(), v.clone());
                    }
                    resolutions.insert(k, v);
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
    exclusions.extend(cfg.resolved_exclusions());

    // Build dep_scopes: map each direct dep's GA to its declared scope (all scopes)
    let dep_scopes = build_dep_scope_map(cfg, &["compile", "provided", "runtime", "test"]);
    let constraints = collect_plugin_managed_versions(project, cfg).unwrap_or_default();
    let jars = crate::workspace::resolver::resolve_and_download_with_constraints(
        &deps, &cache, &mut resolved, &registries, &exclusions, &resolutions, &constraints, &dep_scopes,
    )?;
    config::save_resolved_cache(project, &resolved)?;

    // Check for dependency version conflicts
    let conflicts = crate::workspace::resolver::check_conflicts(&resolved);
    if !conflicts.is_empty() {
        for (ga, versions) in &conflicts {
            eprintln!(
                "{} version conflict: {} has versions: {}",
                console::style(format!("{:>12}", "warning")).yellow().bold(),
                console::style(ga).bold(),
                versions.join(", ")
            );
        }
        eprintln!(
            "             Use [resolutions] in package.toml to pin a specific version"
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
/// Batch resolve for workspace: pre-loaded root config avoids repeated I/O.
/// Skips save_resolved_cache (read-only, for idea --json).
pub fn resolve_deps_no_download_with_root(
    project: &Path,
    cfg: &YmConfig,
    root_cfg: &YmConfig,
    shared_cache_dir: &Path,
    root_registries: &[crate::workspace::resolver::RegistryEntry],
    root_resolutions: &std::collections::BTreeMap<String, String>,
) -> Result<Vec<PathBuf>> {
    let mut registries: Vec<crate::workspace::resolver::RegistryEntry> = root_registries.to_vec();
    let mut resolutions = cfg.resolved_resolutions();
    for (k, v) in root_resolutions {
        resolutions.insert(k.clone(), v.clone());
    }

    let deps = cfg.maven_dependencies_with_root(root_cfg);

    let current_entries = cfg.registry_entries();
    for entry in current_entries {
        if !registries.iter().any(|e| e.url == entry.url) {
            registries.insert(0, entry);
        }
    }

    if deps.is_empty() {
        return Ok(resolve_lib_dirs(project, cfg));
    }

    let mut resolved = config::load_resolved_cache_checked(project, cfg)?;
    let mut exclusions = cfg.exclusions.as_ref().cloned().unwrap_or_default();
    exclusions.extend(cfg.per_dependency_exclusions());
    exclusions.extend(cfg.resolved_exclusions());

    let dep_scopes = build_dep_scope_map(cfg, &["compile", "provided", "runtime", "test"]);
    let jars = crate::workspace::resolver::resolve_no_download(
        &deps, shared_cache_dir, &mut resolved, &registries, &exclusions, &resolutions, &dep_scopes,
    )?;

    let mut all_jars = jars;
    all_jars.extend(resolve_lib_dirs(project, cfg));
    Ok(all_jars)
}

pub fn resolve_deps_no_download(project: &Path, cfg: &YmConfig) -> Result<Vec<PathBuf>> {
    use crate::workspace::resolver::RegistryEntry;
    let mut registries: Vec<RegistryEntry> = Vec::new();
    let mut resolutions = cfg.resolved_resolutions();

    let deps = if let Some(ws_root) = config::find_workspace_root(project) {
        if ws_root != project {
            let root_config_path = ws_root.join(config::CONFIG_FILE);
            if let Ok(root_cfg) = config::load_config(&root_config_path) {
                let errors = cfg.validate_workspace_deps(&root_cfg);
                if !errors.is_empty() {
                    anyhow::bail!("{}", errors.join("; "));
                }
                let mut d = cfg.maven_dependencies_with_root(&root_cfg);
                for (k, v) in root_cfg.resolved_resolutions() {
                    if d.contains_key(&k) {
                        d.insert(k.clone(), v.clone());
                    }
                    resolutions.insert(k, v);
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
    exclusions.extend(cfg.resolved_exclusions());

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
    let mut extra_args = cfg.compiler.as_ref().and_then(|c| c.args.clone()).unwrap_or_else(|| {
        // Inherit compiler args from workspace root if not specified in module
        if let Some(ws_root) = config::find_workspace_root(project) {
            if let Ok(root_cfg) = config::load_config(&ws_root.join(config::CONFIG_FILE)) {
                return root_cfg.compiler.as_ref().and_then(|c| c.args.clone()).unwrap_or_default();
            }
        }
        Vec::new()
    });

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

    incremental::incremental_compile(&compile_cfg, &cache, None)
}

/// Compile a project using a compiler worker pool (for workspace builds).
pub fn compile_project_with_pool(
    project: &Path,
    cfg: &YmConfig,
    classpath: &[PathBuf],
    pool: Option<&compiler::worker::CompilerPool>,
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
    let mut extra_args = cfg.compiler.as_ref().and_then(|c| c.args.clone()).unwrap_or_else(|| {
        if let Some(ws_root) = config::find_workspace_root(project) {
            if let Ok(root_cfg) = config::load_config(&ws_root.join(config::CONFIG_FILE)) {
                return root_cfg.compiler.as_ref().and_then(|c| c.args.clone()).unwrap_or_default();
            }
        }
        Vec::new()
    });
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

    incremental::incremental_compile(&compile_cfg, &cache, pool)
}

fn resolve_annotation_processors(project: &Path, cfg: &YmConfig, classpath: &[PathBuf]) -> Result<Vec<PathBuf>> {
    if let Some(coords) = cfg.compiler.as_ref().and_then(|c| c.annotation_processors.as_ref()) {
        if !coords.is_empty() {
            let deps = cfg.maven_dependencies();
            let cache = config::maven_cache_dir(project);
            let mut jars = Vec::new();
            for coord in coords {
                // Resolve @scope/name to groupId:artifactId for lookup
                let resolved = cfg.resolve_key(coord);
                if let Some(version) = deps.get(&resolved) {
                    // Direct version available — resolve from cache
                    let mc = crate::workspace::resolver::MavenCoord::parse(&resolved, version)?;
                    let jar = mc.jar_path(&cache);
                    if jar.exists() {
                        jars.push(jar);
                    }
                } else {
                    // Workspace-inherited dep: version not in local maven_dependencies().
                    // Fall back to searching the already-resolved classpath by artifactId.
                    let artifact_id = artifact_id_from_key(coord);
                    if let Some(jar) = classpath.iter().find(|p| {
                        p.file_name()
                            .and_then(|f| f.to_str())
                            .map(|f| f.starts_with(artifact_id))
                            .unwrap_or(false)
                    }) {
                        jars.push(jar.clone());
                    }
                }
            }
            return Ok(jars);
        }
    }

    // Auto-discover: only look in devDependencies jars (like Gradle's annotationProcessor config).
    // This prevents compile-scope jars (e.g. auto-service via selenium) from being accidentally
    // loaded as annotation processors when their own dependencies aren't on the processor path.
    let dev_artifact_ids = collect_dev_dependency_artifact_ids(cfg);
    if dev_artifact_ids.is_empty() {
        return Ok(vec![]);
    }
    Ok(discover_annotation_processors_from_dev_deps(classpath, &dev_artifact_ids))
}

/// Collect artifact IDs from devDependencies for annotation processor filtering.
fn collect_dev_dependency_artifact_ids(cfg: &YmConfig) -> Vec<String> {
    let mut ids = Vec::new();
    if let Some(ref dev_deps) = cfg.dev_dependencies {
        for key in dev_deps.keys() {
            ids.push(artifact_id_from_key(key).to_string());
        }
    }
    ids
}

/// Discover annotation processors only from jars that match devDependencies artifact IDs.
fn discover_annotation_processors_from_dev_deps(classpath: &[PathBuf], dev_artifact_ids: &[String]) -> Vec<PathBuf> {
    classpath
        .iter()
        .filter(|jar| {
            let fname = jar.file_name().and_then(|f| f.to_str()).unwrap_or("");
            jar.extension().and_then(|e| e.to_str()) == Some("jar")
                && jar.exists()
                && dev_artifact_ids.iter().any(|id| fname.starts_with(id.as_str()))
                && has_annotation_processor(jar)
        })
        .cloned()
        .collect()
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
        .or_else(read_java_version_file)
        .unwrap_or_else(|| "21".to_string());

    let vendor = cfg.jvm.as_ref().and_then(|j| j.vendor.as_deref());
    let auto_download = cfg
        .jvm
        .as_ref()
        .and_then(|j| j.auto_download)
        .unwrap_or(false);

    let java_home = jvm::ensure_jdk(&version, vendor, auto_download)?;

    if java_home != Path::new("system") && java_home.exists() {
        if !crate::is_json_quiet() && is_verbose() {
            let jdk_name = java_home.file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| format!("JDK {}", version));
            println!(
                "{} jdk {} ({})",
                style(format!("{:>12}", "Using")).green().bold(),
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
    } else if !crate::is_json_quiet() && is_verbose() {
        let javac_path = jvm::which_javac()
            .and_then(|p| p.parent().and_then(|b| b.parent()).map(|h| h.to_path_buf()));
        if let Some(home) = javac_path {
            let jdk_name = home.file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| "System JDK".to_string());
            println!(
                "{} jdk {} ({})",
                style(format!("{:>12}", "Using")).green().bold(),
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
                    "{} {}...",
                    console::style(format!("{:>12}", "Downloading")).green().bold(),
                    filename
                );
            }
            let client = reqwest::blocking::Client::builder()
                .user_agent(format!("ym/{}", env!("CARGO_PKG_VERSION")))
                .build()?;
            let response = client.get(url).send()?;
            if !response.status().is_success() {
                eprintln!(
                    "{} failed to download {}: HTTP {}",
                    console::style(format!("{:>12}", "warning")).yellow().bold(),
                    url,
                    response.status()
                );
                continue;
            }
            let bytes = response.bytes()?;
            std::fs::write(&jar_path, &bytes)?;
            if !crate::is_json_quiet() {
                println!(
                    "{} {}",
                    console::style(format!("{:>12}", "Downloaded")).green().bold(),
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
                    "{} {}...",
                    console::style(format!("{:>12}", "Cloning")).green().bold(),
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
                    "{} failed to clone {}",
                    console::style(format!("{:>12}", "warning")).yellow().bold(),
                    git_url
                );
                continue;
            }
        }

        // Look for output JARs: check if it has package.toml (ym project)
        let pkg_toml = repo_dir.join(config::CONFIG_FILE);
        if pkg_toml.exists() {
            // Build with ym
            if !crate::is_json_quiet() {
                println!(
                    "{} Git dependency {}...",
                    console::style(format!("{:>12}", "Building")).green().bold(),
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
                        "{} failed to build Git dependency {}",
                        console::style(format!("{:>12}", "warning")).yellow().bold(),
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
pub fn resolve_lib_dirs(project: &Path, cfg: &YmConfig) -> Vec<PathBuf> {
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
