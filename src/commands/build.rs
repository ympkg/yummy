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
        if crate::is_progress_quiet() {
            eprintln!("  {}", msg);
            return Self {
                running: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
                handle: None,
            };
        }
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

/// Frozen-lockfile mode: fail if ym-lock.json is missing or out of sync with ym.json
static FROZEN_LOCKFILE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

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

/// Enable frozen-lockfile mode (CI: fail if lock missing or out of sync, ADR-016).
pub fn set_frozen_lockfile(v: bool) {
    FROZEN_LOCKFILE.store(v, std::sync::atomic::Ordering::Relaxed);
}

/// Check if frozen-lockfile mode is enabled.
pub fn is_frozen_lockfile() -> bool {
    FROZEN_LOCKFILE.load(std::sync::atomic::Ordering::Relaxed)
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
    // execute_with_profile is the build main entry — self-heals ym-lock.json
    // for single-project mode. The follow-up scoped resolve is read-only.
    let _all_jars = resolve_and_persist_deps(&project, &cfg)?;
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
    // Resolve all deps to populate cache + self-heal ym-lock.json. Single-project
    // build is a declarative writer of the lockfile (ADR-020).
    let _all_jars = resolve_and_persist_deps(&project, &cfg)?;
    // Compilation classpath: compile + provided (exclude runtime and test).
    // Read-only — the lockfile was already saved above.
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

/// Deduplicate JAR paths by Maven groupId:artifactId.
/// If BOM constraints specify a version for a GA, use that version.
/// Otherwise keep the highest version.
/// Extracts groupId from the cache path structure: `~/.ym/maven/{groupId}/{artifactId}/{version}/`.
/// For JARs outside the cache (e.g. workspace thin JARs), uses filename as unique key (no dedup).
fn dedup_jars_by_artifact(jars: Vec<PathBuf>, bom_constraints: &std::collections::BTreeMap<String, String>) -> Vec<PathBuf> {
    let mut ga_map: std::collections::HashMap<String, (PathBuf, String)> = std::collections::HashMap::new();
    let mut order: Vec<String> = Vec::new();
    let mut non_cache: Vec<PathBuf> = Vec::new();

    for jar in jars {
        if jar.is_dir() {
            non_cache.push(jar);
            continue;
        }
        let filename = jar.file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        if !filename.ends_with(".jar") {
            non_cache.push(jar);
            continue;
        }

        // Try to extract groupId:artifactId from cache path:
        // .ym/maven/{groupId}/{artifactId}/{version}/{artifactId}-{version}.jar
        let path_str = jar.to_string_lossy();
        let ga_key = if let Some(maven_pos) = path_str.find("/maven/") {
            let after_caches = &path_str[maven_pos + 7..]; // skip "/maven/"
            let parts: Vec<&str> = after_caches.split('/').collect();
            if parts.len() >= 3 {
                // parts[0] = groupId, parts[1] = artifactId, parts[2] = version
                format!("{}:{}", parts[0], parts[1])
            } else {
                filename.clone()
            }
        } else {
            // Non-cache JAR (e.g. workspace thin JAR) — use filename, no dedup
            non_cache.push(jar);
            continue;
        };

        // Extract version from path: parent dir name is the version
        let version = jar.parent()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();

        // Check if BOM specifies a version for this GA
        let bom_version = bom_constraints.get(&ga_key);

        if let Some((_, existing_ver)) = ga_map.get(&ga_key) {
            let should_replace = if let Some(bv) = bom_version {
                // BOM wins: replace if current version matches BOM but existing doesn't
                version == *bv && existing_ver != bv
            } else {
                // No BOM: keep highest version
                let parse_ver = |s: &str| -> Vec<i64> {
                    s.split(|c: char| c == '.' || c == '-')
                        .map(|seg| seg.parse::<i64>().unwrap_or(0))
                        .collect()
                };
                let va = parse_ver(&version);
                let vb = parse_ver(existing_ver);
                let len = va.len().max(vb.len());
                let mut higher = false;
                for i in 0..len {
                    let a = va.get(i).copied().unwrap_or(0);
                    let b = vb.get(i).copied().unwrap_or(0);
                    if a > b { higher = true; break; }
                    if a < b { break; }
                }
                higher
            };
            if should_replace {
                ga_map.insert(ga_key, (jar, version));
            }
        } else {
            order.push(ga_key.clone());
            ga_map.insert(ga_key, (jar, version));
        }
    }

    let mut result = non_cache;
    for key in &order {
        if let Some((path, _)) = ga_map.get(key) {
            result.push(path.clone());
        }
    }
    result
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

    // Pre-warm compiler worker pool in background during dependency resolution.
    // Worker JVM startup (3-5s per worker) overlaps with dep resolution (8s+).
    let pool_size = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .min(packages.len());
    let pool_handle = if packages.len() > 1 {
        Some(std::thread::spawn(move || {
            compiler::worker::CompilerPool::new(pool_size).ok()
        }))
    } else {
        None
    };

    // Workspace-level dependency resolution
    let dep_start = Instant::now();

    // Collect each module's own Maven deps
    let own_module_deps: std::collections::HashMap<String, std::collections::BTreeMap<String, String>> = packages
        .iter()
        .map(|name| {
            let pkg = ws.get_package(name).unwrap();
            let mut deps = pkg.config.maven_dependencies_with_root(root_cfg);
            for (k, v) in root_cfg.resolved_resolutions(root_cfg) {
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

    let cache = config::maven_cache_dir();

    // Frozen-lockfile mode (ADR-016, CI use): fail fast if ym-lock.json is missing or
    // out of sync with ym.json. Replaces the legacy `find . -name resolved.json -delete`
    // CI hack with deterministic build behavior.
    if is_frozen_lockfile() {
        let raw_lock = config::load_lockfile(root)?;
        let current_hash = root_cfg.dependency_fingerprint();
        if raw_lock.config_hash.is_empty() {
            anyhow::bail!(
                "Lockfile not found at {}. \
                 --frozen-lockfile requires ym-lock.json to exist (commit it to git, see ADR-016).",
                config::lockfile_path(root).display()
            );
        }
        if raw_lock.config_hash != current_hash {
            let diff = crate::workspace::lockfile_diff::compute_diff(root_cfg, &raw_lock);
            anyhow::bail!("{}", crate::workspace::lockfile_diff::format_diff_error(&diff));
        }
    }

    let mut resolved = config::load_lockfile_checked(root, root_cfg)?;
    let registries = root_cfg.registry_entries();
    let mut exclusions = root_cfg.exclusions.as_ref().cloned().unwrap_or_default();
    exclusions.extend(root_cfg.per_dependency_exclusions());
    exclusions.extend(root_cfg.resolved_exclusions());

    let resolutions = root_cfg.resolved_resolutions(root_cfg);
    // Spinner stays alive during resolve — resolver updates spinner message with progress
    crate::SPINNER_ACTIVE.store(true, std::sync::atomic::Ordering::Relaxed);
    let per_module_jars = crate::workspace::resolver::resolve_workspace_deps_with_resolutions(
        &all_module_deps, &cache, &mut resolved, &registries, &exclusions, &resolutions,
    )?;
    crate::SPINNER_ACTIVE.store(false, std::sync::atomic::Ordering::Relaxed);
    spinner.finish_and_clear();
    config::save_lockfile(root, &resolved)?;
    let dep_time = dep_start.elapsed();
    let total_jars: usize = per_module_jars.values().next().map(|v| v.len()).unwrap_or(0);
    println!(
        "{} dependencies ({} jars) {:>25}ms",
        style(format!("{:>12}", "Resolving")).green().bold(),
        total_jars,
        dep_time.as_millis()
    );

    let cache_dir = config::cache_dir(root);
    // Pre-compute source + resource hashes for all modules (parallel, mtime fast path)
    let source_hashes_map: std::collections::HashMap<String, Vec<(String, String)>> = packages
        .par_iter()
        .filter_map(|name| {
            let pkg = ws.get_package(name)?;
            let src_dirs = vec![config::source_dir_for(&pkg.path, &pkg.config)];
            let output_dir = config::output_classes_dir(&pkg.path);
            let mut hashes = compiler::incremental::compute_source_content_hashes(
                &src_dirs, &cache_dir, &output_dir,
            ).unwrap_or_default();
            // Include resource files in cache key (resource changes must invalidate cache)
            let res_dir = pkg.path.join("src").join("main").join("resources");
            if res_dir.exists() {
                for entry in walkdir::WalkDir::new(&res_dir).into_iter().filter_map(|e| e.ok()) {
                    if entry.file_type().is_file() {
                        let rel = format!("res:{}", entry.path().strip_prefix(&res_dir)
                            .unwrap_or(entry.path()).to_string_lossy());
                        let hash = compiler::incremental::hash_file(entry.path())
                            .unwrap_or_default();
                        hashes.push((rel, hash));
                    }
                }
                hashes.sort_by(|a, b| a.0.cmp(&b.0));
            }
            Some((name.clone(), hashes))
        })
        .collect();

    let jar_sha256_index: std::collections::HashMap<String, String> = resolved.dependencies.iter()
        .filter_map(|(key, dep)| {
            let sha = dep.sha256.as_ref()?;
            let mc = crate::workspace::resolver::MavenCoord::from_versioned_key(key)?;
            let fname = mc.jar_path(std::path::Path::new(""))
                .file_name()?.to_string_lossy().to_string();
            Some((fname, sha.clone()))
        })
        .collect();

    let maven_sha256_map: std::collections::HashMap<String, Vec<(String, String)>> = packages
        .iter()
        .map(|name| {
            let jars = per_module_jars.get(name.as_str()).map(|v| v.as_slice()).unwrap_or(&[]);
            let mut sha256s: Vec<(String, String)> = jars.iter()
                .filter_map(|jar_path| {
                    let fname = jar_path.file_name()?.to_string_lossy().to_string();
                    let sha = jar_sha256_index.get(&fname)
                        .cloned()
                        .unwrap_or_else(|| compiler::incremental::hash_bytes(fname.as_bytes()));
                    Some((fname, sha))
                })
                .collect();
            sha256s.sort_by(|a, b| a.0.cmp(&b.0));
            (name.clone(), sha256s)
        })
        .collect();

    let mut abi_map: std::collections::HashMap<String, String> = std::collections::HashMap::new();

    {

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

    // Collect pre-warmed compiler worker pool (started during dependency resolution)
    let worker_pool = pool_handle
        .and_then(|h| h.join().ok())
        .flatten();

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

        // Snapshot abi_map for this wave (immutable during parallel iteration)
        let abi_snapshot = abi_map.clone();

        let results: Vec<_> = compilable
            .par_iter()
            .map(|pkg_name| {
                let pkg = ws.get_package(pkg_name.as_str()).unwrap();
                let out_dir = config::output_classes_dir(&pkg.path);

                // ── Build compile config (needed for both cache key and compilation) ──
                let jars = per_module_jars.get(pkg_name.as_str()).cloned().unwrap_or_default();
                let mut classpath = jars;
                classpath.extend(cp_snapshot.clone());
                let mut module_cfg = pkg.config.clone();
                if module_cfg.compiler.as_ref().and_then(|c| c.args.as_ref()).is_none() {
                    let root_args = root_cfg_snapshot.compiler.as_ref().and_then(|c| c.args.clone());
                    if let Some(args) = root_args {
                        let compiler = module_cfg.compiler.get_or_insert_with(Default::default);
                        compiler.args = Some(args);
                    }
                }
                if let Some(ref root_dev) = root_cfg_snapshot.dev_dependencies {
                    let module_dev = module_cfg.dev_dependencies.get_or_insert_with(Default::default);
                    for (k, v) in root_dev {
                        module_dev.entry(k.clone()).or_insert_with(|| v.clone());
                    }
                }

                let src_hashes = source_hashes_map.get(pkg_name.as_str())
                    .map(|v| v.as_slice()).unwrap_or(&[]);
                let mvn_sha256s = maven_sha256_map.get(pkg_name.as_str())
                    .map(|v| v.as_slice()).unwrap_or(&[]);

                // Collect workspace dependency ABI hashes from abi_snapshot
                let ws_deps = pkg.config.workspace_module_deps();
                let mut dep_abi_hashes: Vec<(String, String)> = ws_deps.iter()
                    .filter_map(|dep| {
                        abi_snapshot.get(dep.as_str())
                            .map(|abi| (dep.clone(), abi.clone()))
                    })
                    .collect();
                dep_abi_hashes.sort_by(|a, b| a.0.cmp(&b.0));

                // Build a CompileConfig for cache key computation (AP JARs covered by maven SHA-256s)
                let compile_cfg = compiler::CompileConfig {
                    source_dirs: Vec::new(),
                    // Resources are folded into the module cache key separately,
                    // via source_hashes_map (see the "res:" entries above).
                    resource_dirs: Vec::new(),
                    output_dir: std::path::PathBuf::new(),
                    classpath: Vec::new(),
                    java_version: module_cfg.target.clone(),
                    encoding: module_cfg.compiler.as_ref().and_then(|c| c.encoding.clone()),
                    annotation_processors: Vec::new(),
                    lint: module_cfg.compiler.as_ref().and_then(|c| c.lint.clone()).unwrap_or_default(),
                    extra_args: module_cfg.compiler.as_ref().and_then(|c| c.args.clone()).unwrap_or_default(),
                };

                let cache_key = compiler::incremental::compute_module_cache_key(
                    &compiler::incremental::ModuleCacheInput {
                        source_hashes: src_hashes,
                        dep_abi_hashes: &dep_abi_hashes,
                        maven_jar_sha256s: mvn_sha256s,
                        config: &compile_cfg,
                        ap_jar_sha256s: &[],
                    },
                );

                if let Ok(Some(abi_hash)) = compiler::incremental::try_restore_module_cache(
                    &cache_key, &out_dir,
                ) {
                    return (pkg_name.to_string(), Ok(compiler::CompileResult {
                        success: true,
                        outcome: compiler::CompileOutcome::Cached,
                        errors: String::new(),
                        module_abi_hash: Some(abi_hash.clone()),
                    }), std::time::Duration::ZERO, abi_hash);
                }

                let start = Instant::now();
                let result = compile_project_with_pool(&pkg.path, &module_cfg, &classpath, worker_pool.as_ref());
                let elapsed = start.elapsed();

                // Prefer ABI hash from compile result (avoids re-reading .class files)
                let abi_hash = result.as_ref().ok()
                    .and_then(|r| r.module_abi_hash.clone())
                    .unwrap_or_else(|| {
                        if result.as_ref().map(|r| r.success).unwrap_or(false) {
                            compiler::incremental::compute_module_abi_hash(&out_dir)
                                .unwrap_or_else(|_| compiler::incremental::hash_bytes(b"error"))
                        } else {
                            compiler::incremental::hash_bytes(b"error")
                        }
                    });

                // Save to content-addressed cache on successful compilation
                if result.as_ref().map(|r| r.success).unwrap_or(false) {
                    if let Err(e) = compiler::incremental::save_module_cache(
                        &cache_key, &out_dir, &abi_hash, pkg_name,
                    ) {
                        eprintln!("  Warning: failed to save build cache for '{}': {}", pkg_name, e);
                    }
                }

                (pkg_name.to_string(), result, elapsed, abi_hash)
            })
            .collect();

        for (pkg_name, result, elapsed, module_abi) in results {
            // Record ABI hash for downstream modules
            abi_map.insert(pkg_name.clone(), module_abi);

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

    print_workspace_summary(compiled_count, cached_count, up_to_date_count,
        0, skipped_count, total_start.elapsed());

    // Evict stale build cache entries (>30 days unused)
    compiler::incremental::evict_stale_build_cache();

    } // end of content-addressed cache block

    if package {
        // Package JARs for all modules:
        // - Modules with `main`: fat JAR (Spring Boot executable)
        // - Library modules (no `main`): thin JAR (own classes + resources only)
        let jar_targets: Vec<&str> = if !targets.is_empty() {
            targets.iter().map(|s| s.as_str()).collect()
        } else {
            packages.iter().map(|s| s.as_str()).collect()
        };

        // Library modules: thin JAR
        for pkg_name in &jar_targets {
            let pkg = ws.get_package(pkg_name).unwrap();
            if pkg.config.main.is_none() {
                let effective_version = pkg.config.version.as_deref()
                    .or(root_cfg.version.as_deref())
                    .unwrap_or("0.0.0");
                let jar_name = format!("{}-{}.jar", pkg.config.name, effective_version);
                let output_jar = pkg.path.join("out").join("release").join(&jar_name);
                let class_dir = config::output_classes_dir(&pkg.path);
                let resource_dir = pkg.path.join("src").join("main").join("resources");
                let fp = compute_packaging_fingerprint(&class_dir, &resource_dir, &[], &pkg.config)?;
                if !should_skip_packaging(&pkg.path, &fp, &output_jar) {
                    build_library_jar(&pkg.path, &pkg.config, root_cfg.version.as_deref())?;
                    save_packaging_fingerprint(&pkg.path, &fp)?;
                }
            }
        }

        // App modules: fat JAR (filter to only main modules)
        let jar_targets: Vec<&str> = jar_targets.into_iter()
            .filter(|name| ws.get_package(name).map(|p| p.config.main.is_some()).unwrap_or(false))
            .collect();

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

            // Collect BOM constraints for version selection during dedup
            let bom_constraints = collect_plugin_managed_versions(&pkg.path, &pkg.config)
                .unwrap_or_default();

            // Deduplicate: by path first, then by groupId:artifactId
            // Use BOM-managed version when available, otherwise keep highest version
            all_deps.sort();
            all_deps.dedup();
            all_deps = dedup_jars_by_artifact(all_deps, &bom_constraints);
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
                if pkg.config.main.is_none() {
                    // Library module: thin JAR (only own classes + resources)
                    build_library_jar(&pkg.path, &pkg.config, root_cfg.version.as_deref())?;
                } else if pkg.path.join("ym.config.java").exists() {
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

/// Build a thin library JAR containing only the module's own classes and resources.
fn build_library_jar(project: &Path, cfg: &YmConfig, root_version: Option<&str>) -> Result<()> {
    let classes_dir = config::output_classes_dir(project);
    let release_dir = project.join("out").join("release");
    std::fs::create_dir_all(&release_dir)?;

    let effective_version = cfg.version.as_deref()
        .or(root_version)
        .unwrap_or("0.0.0");
    let jar_name = format!("{}-{}.jar", cfg.name, effective_version);
    let jar_path = release_dir.join(&jar_name);

    let pack_start = Instant::now();

    // Build JAR from out/classes (already includes copied resources)
    let mut cmd = std::process::Command::new("jar");
    cmd.arg("cf").arg(&jar_path).arg("-C").arg(&classes_dir).arg(".");

    let status = cmd.status()?;
    if !status.success() {
        bail!("Failed to create library JAR for {}", cfg.name);
    }

    let jar_size = std::fs::metadata(&jar_path).map(|m| m.len()).unwrap_or(0);
    let size_str = if jar_size >= 1024 * 1024 {
        format!("{:.1} MB", jar_size as f64 / (1024.0 * 1024.0))
    } else if jar_size >= 1024 {
        format!("{:.1} KB", jar_size as f64 / 1024.0)
    } else {
        format!("{} B", jar_size)
    };
    println!(
        "{} {} ({}) {:>10}",
        style(format!("{:>12}", "Packaging")).green().bold(),
        jar_name,
        size_str,
        style(format!("{:.0}ms", pack_start.elapsed().as_millis())).dim()
    );
    println!(
        "{} {}",
        style(format!("{:>12}", "→")).dim(),
        style(release_dir.display()).dim()
    );

    Ok(())
}

/// Build a Spring Boot fat jar — entry point for ADR-009 packaging dispatch.
///
/// Application type detection (ADR-009 / KR17.1): if the fat jar closure (compile + runtime
/// scope, excluding provided + test) is a Spring Boot app, use Path A (nested layout);
/// otherwise fall back to Path B (uber jar with Shadow-style transformers).
///
/// Detection is equivalent to the spec's "contains org.springframework.boot:spring-boot core
/// artifact" check — SB apps transitively depend on spring-boot-autoconfigure, so we infer
/// the SB version from its jar name and locate/download the matching spring-boot-loader.
///
/// Path A (this function): spring-boot-loader nested layout
/// - loader classes at the root (org/springframework/boot/loader/**)
/// - app .class + resources under BOOT-INF/classes/
/// - dependency jars under BOOT-INF/lib/<artifactId>-<version>.jar (STORED, not unpacked)
/// - MANIFEST aligned with spring-boot-maven-plugin's default output
///
/// Path B (`build_release_jar_flat` fallback): used when spring-boot-loader cannot be located.
pub(crate) fn build_release_jar(project: &Path, cfg: &YmConfig, jars: &[PathBuf], output_base: Option<&Path>, root_version: Option<&str>) -> Result<()> {
    use std::io::{Read, Write};

    let out = config::output_classes_dir(project);
    let base = output_base.unwrap_or(project);
    let release_dir = base.join("out").join("release");
    std::fs::create_dir_all(&release_dir)?;

    let effective_version = cfg.version.as_deref()
        .or(root_version)
        .unwrap_or("0.0.0");
    let jar_name = format!("{}-{}.jar", cfg.name, effective_version);
    let jar_path = release_dir.join(&jar_name);

    let pack_start = Instant::now();

    // ── Step 1: Find spring-boot-loader JAR ──────────────────────────────
    let loader_jar: Option<PathBuf> = {
        // First, look in the jars list
        let from_jars = jars.iter().find(|j| {
            let name = j.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
            name.contains("spring-boot-loader") && !name.contains("spring-boot-loader-tools")
        });
        if let Some(found) = from_jars {
            Some(found.clone())
        } else {
            // Detect spring-boot version from spring-boot-autoconfigure JAR
            let version = jars.iter().find_map(|j| {
                let name = j.file_name()?.to_string_lossy().to_string();
                let prefix = "spring-boot-autoconfigure-";
                if name.starts_with(prefix) && name.ends_with(".jar") {
                    Some(name[prefix.len()..name.len() - 4].to_string())
                } else {
                    None
                }
            });
            if let Some(ver) = version {
                let cache_dir = dirs::home_dir()
                    .expect("Cannot determine home directory")
                    .join(crate::config::CACHE_DIR)
                    .join(crate::config::MAVEN_CACHE_DIR)
                    .join("org.springframework.boot")
                    .join("spring-boot-loader");
                // Try to find the loader JAR in the cache, or download it
                let candidate = cache_dir.join(&ver).join(format!("spring-boot-loader-{}.jar", ver));
                if candidate.exists() {
                    Some(candidate)
                } else {
                    // Download from Maven Central
                    let url = format!(
                        "https://repo1.maven.org/maven2/org/springframework/boot/spring-boot-loader/{}/spring-boot-loader-{}.jar",
                        ver, ver
                    );
                    let dest_dir = cache_dir.join(&ver);
                    let _ = std::fs::create_dir_all(&dest_dir);
                    let dest = dest_dir.join(format!("spring-boot-loader-{}.jar", ver));
                    println!(
                        "{} spring-boot-loader-{}.jar from Maven Central",
                        style(format!("{:>12}", "Downloading")).green().bold(),
                        ver
                    );
                    match reqwest::blocking::get(&url) {
                        Ok(resp) if resp.status().is_success() => {
                            match resp.bytes() {
                                Ok(bytes) => {
                                    let _ = std::fs::write(&dest, &bytes);
                                    if dest.exists() { Some(dest) } else { None }
                                }
                                Err(_) => None,
                            }
                        }
                        _ => None,
                    }
                }
            } else {
                None
            }
        }
    };

    // Detect Spring Boot version for MANIFEST.MF
    let spring_boot_version: String = jars.iter().find_map(|j| {
        let name = j.file_name()?.to_string_lossy().to_string();
        let prefix = "spring-boot-autoconfigure-";
        if name.starts_with(prefix) && name.ends_with(".jar") {
            Some(name[prefix.len()..name.len() - 4].to_string())
        } else {
            None
        }
    }).unwrap_or_else(|| "unknown".to_string());

    // If no loader JAR found, fall back to old flat JAR behavior with warning
    if loader_jar.is_none() {
        println!(
            "{} spring-boot-loader JAR not found, falling back to flat JAR packaging",
            style(format!("{:>12}", "warning")).yellow().bold(),
        );
        return build_release_jar_flat(project, cfg, jars, output_base, root_version);
    }
    let loader_jar = loader_jar.unwrap();

    // Determine loader JAR filename so we can exclude it from BOOT-INF/lib/
    let loader_jar_filename = loader_jar.file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();

    // ── Step 2: Create the output JAR directly (no staging directory) ────
    let jar_file = std::fs::File::create(&jar_path)?;
    let mut zip_writer = zip::ZipWriter::new(std::io::BufWriter::new(jar_file));
    let deflated_options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    let stored_options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Stored);

    let total_deps = jars.len();

    // ── Step 3: Write META-INF/MANIFEST.MF (must be first entry) ─────────
    eprint!(
        "\r{} {} [manifest] {:.1}s   ",
        style(format!("{:>12}", "Packaging")).green().bold(),
        jar_name,
        pack_start.elapsed().as_secs_f64()
    );

    let main_class = cfg.main.as_deref().unwrap_or("com.example.Application");
    // MANIFEST fields aligned with spring-boot-maven-plugin's default output (ADR-009).
    // Build-Jdk-Spec is taken from the app's target version rather than the host JDK.
    let target_jdk = cfg.target.as_deref().unwrap_or("25");
    let manifest = format!(
        "Manifest-Version: 1.0\n\
         Created-By: ym/{}\n\
         Build-Jdk-Spec: {}\n\
         Implementation-Title: {}\n\
         Implementation-Version: {}\n\
         Main-Class: org.springframework.boot.loader.launch.JarLauncher\n\
         Start-Class: {}\n\
         Spring-Boot-Version: {}\n\
         Spring-Boot-Classes: BOOT-INF/classes/\n\
         Spring-Boot-Lib: BOOT-INF/lib/\n\
         Spring-Boot-Classpath-Index: BOOT-INF/classpath.idx\n\
         Spring-Boot-Layers-Index: BOOT-INF/layers.idx\n\n",
        env!("CARGO_PKG_VERSION"), target_jdk, cfg.name, effective_version,
        main_class, spring_boot_version
    );

    zip_writer.add_directory("META-INF/", deflated_options)?;
    zip_writer.start_file("META-INF/MANIFEST.MF", deflated_options)?;
    zip_writer.write_all(manifest.as_bytes())?;

    // ── Step 4: Prepare META-INF/services/ directory entry ────────────────
    // The FileSystemProvider SPI file itself is extracted from the loader jar in Step 5
    // (tracks loader version, avoids hardcoding the provider class name).
    zip_writer.add_directory("META-INF/services/", deflated_options)?;

    // ── Step 5: Extract spring-boot-loader resources ──────────────────────
    // From the loader jar:
    // - org/springframework/boot/loader/** classes (recursive, all subpackages)
    // - META-INF/services/java.nio.file.spi.FileSystemProvider (required for nested:// URL scheme)
    // - META-INF/LICENSE.txt + NOTICE.txt (Apache 2.0 redistribution compliance)
    eprint!(
        "\r{} {} [loader classes] {:.1}s   ",
        style(format!("{:>12}", "Packaging")).green().bold(),
        jar_name,
        pack_start.elapsed().as_secs_f64()
    );
    {
        let loader_file = std::fs::File::open(&loader_jar)
            .with_context(|| format!("Failed to open spring-boot-loader JAR: {}", loader_jar.display()))?;
        let mut loader_archive = zip::ZipArchive::new(std::io::BufReader::new(loader_file))?;

        // Collect directory entries to add them first
        let mut loader_dirs: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        for i in 0..loader_archive.len() {
            let entry = match loader_archive.by_index(i) {
                Ok(e) => e,
                Err(_) => continue,
            };
            let name = entry.name().to_string();
            if name.starts_with("org/springframework/boot/loader/") {
                // Collect parent directories
                let mut parts: Vec<&str> = name.split('/').collect();
                if !entry.is_dir() {
                    parts.pop(); // remove filename
                }
                let mut dir = String::new();
                for part in parts {
                    if part.is_empty() { continue; }
                    dir.push_str(part);
                    dir.push('/');
                    loader_dirs.insert(dir.clone());
                }
            }
        }
        // Also add "org/" and "org/springframework/" etc.
        loader_dirs.insert("org/".to_string());
        loader_dirs.insert("org/springframework/".to_string());
        loader_dirs.insert("org/springframework/boot/".to_string());
        loader_dirs.insert("org/springframework/boot/loader/".to_string());

        for dir in &loader_dirs {
            let _ = zip_writer.add_directory(dir, deflated_options);
        }

        for i in 0..loader_archive.len() {
            let mut entry = match loader_archive.by_index(i) {
                Ok(e) => e,
                Err(_) => continue,
            };
            let name = entry.name().to_string();
            let should_extract = !entry.is_dir() && (
                name.starts_with("org/springframework/boot/loader/")
                || name == "META-INF/services/java.nio.file.spi.FileSystemProvider"
                || name == "META-INF/LICENSE.txt"
                || name == "META-INF/NOTICE.txt"
            );
            if !should_extract {
                continue;
            }
            zip_writer.start_file(&name, deflated_options)?;
            let mut buf = Vec::new();
            entry.read_to_end(&mut buf)?;
            zip_writer.write_all(&buf)?;
        }
    }

    // ── Step 6: Write BOOT-INF/classes/ (application classes + resources) ─
    eprint!(
        "\r{} {} [app classes] {:.1}s   ",
        style(format!("{:>12}", "Packaging")).green().bold(),
        jar_name,
        pack_start.elapsed().as_secs_f64()
    );

    zip_writer.add_directory("BOOT-INF/", deflated_options)?;
    zip_writer.add_directory("BOOT-INF/classes/", deflated_options)?;

    if out.exists() {
        for walk_entry in walkdir::WalkDir::new(&out).sort_by_file_name() {
            let walk_entry = walk_entry?;
            let path = walk_entry.path();
            let relative = path.strip_prefix(&out)?;
            let name = relative.to_string_lossy().replace('\\', "/");
            if name.is_empty() {
                continue;
            }

            let boot_name = format!("BOOT-INF/classes/{}", name);
            if walk_entry.file_type().is_dir() {
                let dir_name = if boot_name.ends_with('/') { boot_name } else { format!("{}/", boot_name) };
                zip_writer.add_directory(dir_name, deflated_options)?;
            } else {
                zip_writer.start_file(&boot_name, deflated_options)?;
                let mut f = std::fs::File::open(path)?;
                std::io::copy(&mut f, &mut zip_writer)?;
            }
        }
    }

    // ── Step 7: Write BOOT-INF/lib/ (dependency JARs as STORED entries) ──
    zip_writer.add_directory("BOOT-INF/lib/", stored_options)?;

    let mut classpath_entries: Vec<String> = Vec::new();
    let mut dep_jar_filenames: Vec<String> = Vec::new();
    // Defensive duplicate-entry guard for BOOT-INF/lib/ (ADR-009 / KR17.4).
    // dedup_jars_by_artifact already removes duplicates by GA; this catches edge cases.
    let mut lib_entries_seen: std::collections::HashMap<String, Vec<String>> = std::collections::HashMap::new();

    for (idx, dep) in jars.iter().enumerate() {
        if !dep.exists() || dep.is_dir() {
            continue;
        }

        let dep_filename = dep.file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| dep.display().to_string());

        // Skip the loader JAR itself — its classes are already at the root
        if dep_filename == loader_jar_filename {
            continue;
        }

        // Skip non-JAR files
        if !dep_filename.ends_with(".jar") {
            continue;
        }

        eprint!(
            "\r{} {} [{}/{}] {:.1}s   ",
            style(format!("{:>12}", "Packaging")).green().bold(),
            jar_name,
            idx + 1,
            total_deps,
            pack_start.elapsed().as_secs_f64()
        );

        let lib_entry_name = format!("BOOT-INF/lib/{}", dep_filename);

        if let Some(sources) = lib_entries_seen.get_mut(&lib_entry_name) {
            sources.push(dep.display().to_string());
            continue;
        }
        lib_entries_seen.insert(lib_entry_name.clone(), vec![dep.display().to_string()]);

        // Read the entire JAR file and write as STORED entry
        let mut jar_bytes = Vec::new();
        let mut f = std::fs::File::open(dep)?;
        f.read_to_end(&mut jar_bytes)?;

        zip_writer.start_file(&lib_entry_name, stored_options)?;
        zip_writer.write_all(&jar_bytes)?;

        classpath_entries.push(format!("- \"BOOT-INF/lib/{}\"", dep_filename));
        dep_jar_filenames.push(dep_filename);
    }

    // Clear progress line
    eprint!("\r{}\r", " ".repeat(80));

    // Fail with all source jars listed if any BOOT-INF/lib/ entry was duplicated.
    let dup_entries: Vec<(&String, &Vec<String>)> = lib_entries_seen.iter()
        .filter(|(_, sources)| sources.len() > 1)
        .collect();
    if !dup_entries.is_empty() {
        let mut msg = String::from("Duplicate entries in BOOT-INF/lib/:\n");
        for (name, sources) in &dup_entries {
            msg.push_str(&format!(
                "  Duplicate entry: {} (from: {})\n",
                name,
                sources.join(", ")
            ));
        }
        msg.push_str("\nThis indicates a bug in dependency deduplication. \
                      Please report at https://github.com/ympkg/yummy/issues");
        bail!(msg);
    }

    // ── Step 8: Write BOOT-INF/classpath.idx ─────────────────────────────
    eprint!(
        "\r{} {} [classpath.idx] {:.1}s   ",
        style(format!("{:>12}", "Packaging")).green().bold(),
        jar_name,
        pack_start.elapsed().as_secs_f64()
    );

    let classpath_idx = classpath_entries.join("\n") + "\n";
    zip_writer.start_file("BOOT-INF/classpath.idx", deflated_options)?;
    zip_writer.write_all(classpath_idx.as_bytes())?;

    // ── Step 9: Write BOOT-INF/layers.idx ────────────────────────────────
    let mut layers_idx = String::new();
    layers_idx.push_str("- \"dependencies\":\n");
    for dep_name in &dep_jar_filenames {
        layers_idx.push_str(&format!("  - \"BOOT-INF/lib/{}\"\n", dep_name));
    }
    layers_idx.push_str("- \"spring-boot-loader\":\n");
    layers_idx.push_str("  - \"org/\"\n");
    layers_idx.push_str("- \"snapshot-dependencies\":\n");
    layers_idx.push_str("- \"application\":\n");
    layers_idx.push_str("  - \"BOOT-INF/classes/\"\n");
    layers_idx.push_str("  - \"BOOT-INF/classpath.idx\"\n");
    layers_idx.push_str("  - \"BOOT-INF/layers.idx\"\n");
    layers_idx.push_str("  - \"META-INF/\"\n");

    zip_writer.start_file("BOOT-INF/layers.idx", deflated_options)?;
    zip_writer.write_all(layers_idx.as_bytes())?;

    // ── Step 10: Finalize ────────────────────────────────────────────────
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

    Ok(())
}

/// Fallback: build a flat/uber JAR (used when spring-boot-loader is not found).
fn build_release_jar_flat(project: &Path, cfg: &YmConfig, jars: &[PathBuf], output_base: Option<&Path>, root_version: Option<&str>) -> Result<()> {
    use std::io::Read;

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

    incremental::copy_dir_recursive(&out, &staging)?;

    // Path B execution order (ADR-009 / KR17.3):
    //   1. Collect app entries (via copy_dir_recursive above) + entries from each dependency
    //   2. Per-path rules: whitelist merge / blacklist discard / first-wins on conflict
    //   3. Duplicate guard (KR17.4): if anything still collides after step 2, fail
    //   4. Write everything to the zip in one pass
    let mut mergeable: std::collections::HashMap<String, Vec<String>> = std::collections::HashMap::new();
    // For non-mergeable conflicts: record the source jars that lost (first writer wins).
    let mut conflicts: std::collections::HashMap<String, Vec<String>> = std::collections::HashMap::new();

    let total_deps = jars.len();
    let pack_start = Instant::now();
    for (idx, dep) in jars.iter().enumerate() {
        if !dep.exists() {
            continue;
        }

        eprint!(
            "\r{} {} [{}/{}] {:.1}s   ",
            style(format!("{:>12}", "Packaging")).green().bold(),
            jar_name,
            idx + 1,
            total_deps,
            pack_start.elapsed().as_secs_f64()
        );

        let dep_filename = dep.file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| dep.display().to_string());

        if dep.is_dir() {
            incremental::copy_dir_recursive(dep, &staging)?;
        } else {
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

                // Blacklist (matches Shadow Plugin's default exclusions):
                // - dependency MANIFEST.MF (ym generates the final one)
                // - INDEX.LIST (jar index, invalid after flattening)
                // - signature blocks (.SF/.DSA/.RSA/.EC, original signature invalidated)
                // - traversal / unsafe paths
                if name == "META-INF/MANIFEST.MF"
                    || name == "META-INF/INDEX.LIST"
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

                // Whitelist merge (matches Shadow Plugin's recommended transformers):
                // - META-INF/services/* (Java SPI / ServiceLoader)
                // - META-INF/spring.factories (still used by Spring Framework)
                // - META-INF/spring/*.imports (SB 3.0+ AutoConfiguration / EnvironmentPostProcessor)
                let is_mergeable = !entry.is_dir() && (
                    name.starts_with("META-INF/services/") ||
                    name == "META-INF/spring.factories" ||
                    (name.starts_with("META-INF/spring/") && name.ends_with(".imports"))
                );
                if is_mergeable {
                    let mut content = String::new();
                    let _ = entry.read_to_string(&mut content);
                    mergeable.entry(name).or_default().push(content);
                    continue;
                }

                if entry.is_dir() {
                    let _ = std::fs::create_dir_all(staging.join(&name));
                } else {
                    let target = staging.join(&name);
                    if let Some(parent) = target.parent() {
                        let _ = std::fs::create_dir_all(parent);
                    }
                    if target.exists() {
                        // First writer wins; collect every losing source for the summary warning.
                        conflicts.entry(name).or_default().push(dep_filename.clone());
                    } else if let Ok(mut out_file) = std::fs::File::create(&target) {
                        let _ = std::io::copy(&mut entry, &mut out_file);
                    }
                }
            }
        }
    }
    eprint!("\r{}\r", " ".repeat(60));

    // Summary warning: list every losing source jar for the first few conflicts.
    if !conflicts.is_empty() {
        let total = conflicts.len();
        println!(
            "{} {} conflicting entries (kept first occurrence)",
            style(format!("{:>12}", "warning")).yellow().bold(),
            total
        );
        for (path, sources) in conflicts.iter().take(5) {
            println!(
                "             {}: ignored from {}",
                path,
                sources.join(", ")
            );
        }
        if total > 5 {
            println!("             ... and {} more", total - 5);
        }
    }

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

    let jar_file = std::fs::File::create(&jar_path)?;
    let mut zip_writer = zip::ZipWriter::new(std::io::BufWriter::new(jar_file));
    let zip_options = zip::write::SimpleFileOptions::default();

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

    let _ = std::fs::remove_dir_all(&staging);

    Ok(())
}


/// Run packaging via the plugin system.
/// Resolves plugins, downloads plugin JARs, starts a JVM to run ConfigRunner,
/// and lets the plugin decide how to package (Spring Boot JAR, fat JAR, etc.).
/// Ensure spring-boot-loader JAR is downloaded to Maven cache.
/// Called before plugin-based packaging so the plugin can find it.
fn ensure_spring_boot_loader(jars: &[PathBuf]) {
    // Detect spring-boot version from spring-boot-autoconfigure JAR
    let version = jars.iter().find_map(|j| {
        let name = j.file_name()?.to_string_lossy().to_string();
        let prefix = "spring-boot-autoconfigure-";
        if name.starts_with(prefix) && name.ends_with(".jar") {
            Some(name[prefix.len()..name.len() - 4].to_string())
        } else {
            None
        }
    });
    if let Some(ver) = version {
        let cache_dir = dirs::home_dir()
            .expect("Cannot determine home directory")
            .join(crate::config::CACHE_DIR)
            .join(crate::config::MAVEN_CACHE_DIR)
            .join("org.springframework.boot")
            .join("spring-boot-loader");
        let candidate = cache_dir.join(&ver).join(format!("spring-boot-loader-{}.jar", ver));
        if !candidate.exists() {
            let url = format!(
                "https://repo1.maven.org/maven2/org/springframework/boot/spring-boot-loader/{}/spring-boot-loader-{}.jar",
                ver, ver
            );
            let dest_dir = cache_dir.join(&ver);
            let _ = std::fs::create_dir_all(&dest_dir);
            println!(
                "{} spring-boot-loader-{}.jar from Maven Central",
                style(format!("{:>12}", "Downloading")).green().bold(),
                ver
            );
            if let Ok(resp) = reqwest::blocking::get(&url) {
                if resp.status().is_success() {
                    if let Ok(bytes) = resp.bytes() {
                        let _ = std::fs::write(&candidate, &bytes);
                    }
                }
            }
        }
    }
}

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

    // Collect the plugin JARs' classpath
    let plugin_cp = resolve_plugin_classpath(project, cfg)?;
    if plugin_cp.is_empty() {
        bail!("No plugin JARs found. Ensure plugins are installed.");
    }

    // runtime classpath string
    let runtime_cp: String = runtime_jars.iter()
        .filter(|j| j.exists())
        .map(|j| j.to_string_lossy().to_string())
        .collect::<Vec<_>>()
        .join(":");

    // java executable
    let java_home = jvm::ensure_jdk(cfg.target.as_deref().unwrap_or("25"), None, false)?;
    let java = if java_home.as_os_str() == "system" {
        PathBuf::from("java")
    } else {
        java_home.join("bin").join("java")
    };

    // Serialize ym.json to a temp file and pass it to ConfigRunner
    let config_json_path = project.join("out").join(".ym-config.json");
    if let Some(parent) = config_json_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let config_json = serde_json::to_string(cfg)?;
    std::fs::write(&config_json_path, &config_json)?;

    let pack_start = Instant::now();

    // If ym.config.java exists, copy it to a valid filename and compile it
    let ym_config_java = project.join("ym.config.java");
    let mut extra_cp = String::new();
    if ym_config_java.exists() {
        let config_out = project.join("out").join(".ym-config-classes");
        std::fs::create_dir_all(&config_out)?;

        // ym.config.java -> YmConfig.java (a valid Java filename)
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

    // Invoke ym.internal.ConfigRunner
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

    // Parse the Build Plan JSON (stdout)
    let build_plan = String::from_utf8_lossy(&status.stdout);
    eprintln!(
        "{} Build Plan received ({} bytes)",
        style(format!("{:>12}", "Plugins")).cyan().bold(),
        build_plan.len()
    );

    // TODO: parse the Build Plan JSON and execute the Task DAG.
    // For now: the plugin Task runs to completion inside the JVM directly.
    // Later: the ym core parses the DAG and schedules it.

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
    let source_dir = config::source_dir_for(project, cfg);
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

    write_classes_jar(&jar_path, &classes_dir, &source_dir, &cfg.name, version)?;
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
    let cache_dir = config::maven_cache_dir();
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

// Content-addressed cache replaces the old mtime+size workspace/module fingerprint system.
// Module-level cache keys are computed in the wave loop using:
//   compute_module_cache_key() + try_restore_module_cache() + save_module_cache()
// from compiler::incremental.

/// Resolve dependencies filtered by scope, used for scope-specific classpath
/// construction. **Read-only** — does NOT write `ym-lock.json`. There is no
/// `_with_scopes` persisting variant because every declarative caller
/// (install / add / build main entry) saves the full unscoped resolution; a
/// scope-filtered save would write a partial set and is never desired. See
/// ADR-020.
pub fn resolve_deps_with_scopes(project: &Path, cfg: &YmConfig, scopes: &[&str]) -> Result<Vec<PathBuf>> {
    use crate::workspace::resolver::RegistryEntry;
    let mut registries: Vec<RegistryEntry> = Vec::new();
    let mut resolutions = cfg.resolved_resolutions(cfg);

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
                for (k, v) in root_cfg.resolved_resolutions(&root_cfg) {
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

    let cache = config::maven_cache_dir();

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

    let mut resolved = config::load_lockfile_checked(project, cfg)?;
    let mut exclusions = cfg.exclusions.as_ref().cloned().unwrap_or_default();
    exclusions.extend(cfg.per_dependency_exclusions());
    exclusions.extend(cfg.resolved_exclusions());
    // Also inherit exclusions from workspace root
    if let Some(ws_root) = config::find_workspace_root(project) {
        if ws_root != project {
            if let Ok(root_cfg) = config::load_config(&ws_root.join(config::CONFIG_FILE)) {
                exclusions.extend(root_cfg.exclusions.as_ref().cloned().unwrap_or_default());
                exclusions.extend(root_cfg.per_dependency_exclusions());
                exclusions.extend(root_cfg.resolved_exclusions());
            }
        }
    }

    // Build dep_scopes: map each direct dep's GA to its declared scope
    let dep_scopes = build_dep_scope_map(cfg, scopes);

    let jars = crate::workspace::resolver::resolve_and_download_with_constraints(
        &deps, &cache, &mut resolved, &registries, &exclusions, &resolutions, &constraints, &dep_scopes,
    )?;

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
    resolved: &crate::config::schema::Lockfile,
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

/// Resolve all dependencies and return classpath JARs. **Read-only** — does
/// NOT write `ym-lock.json`. Callers that want to refresh the lockfile must
/// use [`resolve_and_persist_deps`] instead. See ADR-020 for the contract:
/// lockfile writes are reserved for declarative commands (`ym install` / `add`
/// / `remove` / `upgrade` / `ymc build` main entry), executive commands (dev /
/// test / native / packaging) only read.
pub fn resolve_deps(project: &Path, cfg: &YmConfig) -> Result<Vec<PathBuf>> {
    resolve_deps_inner(project, cfg, false)
}

/// Resolve all dependencies, **persist** the result to `ym-lock.json`, and
/// return classpath JARs. The persisting variant of [`resolve_deps`]. Only
/// safe to call from the workspace root path (or single-project root) — the
/// `is_workspace_child` guard in `save_lockfile` will silently no-op writes
/// from child paths, see ADR-020.
pub fn resolve_and_persist_deps(project: &Path, cfg: &YmConfig) -> Result<Vec<PathBuf>> {
    resolve_deps_inner(project, cfg, true)
}

fn resolve_deps_inner(project: &Path, cfg: &YmConfig, persist: bool) -> Result<Vec<PathBuf>> {
    use crate::workspace::resolver::RegistryEntry;
    let mut registries: Vec<RegistryEntry> = Vec::new();
    let mut resolutions = cfg.resolved_resolutions(cfg);

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
                for (k, v) in root_cfg.resolved_resolutions(&root_cfg) {
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

    let cache = config::maven_cache_dir();

    if deps.is_empty() {
        // Even with no Maven deps, may have URL/Git/lib deps
        let mut jars = resolve_url_deps(cfg, &cache)?;
        jars.extend(resolve_git_deps(cfg, &cache)?);
        jars.extend(resolve_lib_dirs(project, cfg));
        return Ok(jars);
    }

    let mut resolved = config::load_lockfile_checked(project, cfg)?;
    let mut exclusions = cfg.exclusions.as_ref().cloned().unwrap_or_default();
    exclusions.extend(cfg.per_dependency_exclusions());
    exclusions.extend(cfg.resolved_exclusions());
    // Also inherit exclusions from workspace root
    if let Some(ws_root) = config::find_workspace_root(project) {
        if ws_root != project {
            if let Ok(root_cfg) = config::load_config(&ws_root.join(config::CONFIG_FILE)) {
                exclusions.extend(root_cfg.exclusions.as_ref().cloned().unwrap_or_default());
                exclusions.extend(root_cfg.per_dependency_exclusions());
                exclusions.extend(root_cfg.resolved_exclusions());
            }
        }
    }

    // Build dep_scopes: map each direct dep's GA to its declared scope (all scopes)
    let dep_scopes = build_dep_scope_map(cfg, &["compile", "provided", "runtime", "test"]);
    let constraints = collect_plugin_managed_versions(project, cfg).unwrap_or_default();
    let jars = crate::workspace::resolver::resolve_and_download_with_constraints(
        &deps, &cache, &mut resolved, &registries, &exclusions, &resolutions, &constraints, &dep_scopes,
    )?;
    if persist {
        config::save_lockfile(project, &resolved)?;
    }

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

/// Workspace-optimized variant of [`resolve_deps_no_download`] that takes the
/// pre-loaded root config + shared cache + registry list, avoiding repeated
/// I/O across modules. **Read-only** — does NOT write `ym-lock.json`. See
/// ADR-020.
pub fn resolve_deps_no_download_with_root(
    project: &Path,
    cfg: &YmConfig,
    root_cfg: &YmConfig,
    shared_cache_dir: &Path,
    root_registries: &[crate::workspace::resolver::RegistryEntry],
    root_resolutions: &std::collections::BTreeMap<String, String>,
) -> Result<Vec<PathBuf>> {
    let mut registries: Vec<crate::workspace::resolver::RegistryEntry> = root_registries.to_vec();
    let mut resolutions = cfg.resolved_resolutions(cfg);
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

    let mut resolved = config::load_lockfile_checked(project, cfg)?;
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

/// Like [`resolve_deps`] but skips JAR downloads — returns expected cache
/// paths even when the JARs haven't been fetched. Used by `ymc idea --json`
/// so importing into IDEA is never blocked by network I/O. **Read-only** —
/// does NOT write `ym-lock.json`. See ADR-020.
pub fn resolve_deps_no_download(project: &Path, cfg: &YmConfig) -> Result<Vec<PathBuf>> {
    use crate::workspace::resolver::RegistryEntry;
    let mut registries: Vec<RegistryEntry> = Vec::new();
    let mut resolutions = cfg.resolved_resolutions(cfg);

    let deps = if let Some(ws_root) = config::find_workspace_root(project) {
        if ws_root != project {
            let root_config_path = ws_root.join(config::CONFIG_FILE);
            if let Ok(root_cfg) = config::load_config(&root_config_path) {
                let errors = cfg.validate_workspace_deps(&root_cfg);
                if !errors.is_empty() {
                    anyhow::bail!("{}", errors.join("; "));
                }
                let mut d = cfg.maven_dependencies_with_root(&root_cfg);
                for (k, v) in root_cfg.resolved_resolutions(&root_cfg) {
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

    let cache = config::maven_cache_dir();

    if deps.is_empty() {
        let jars = resolve_lib_dirs(project, cfg);
        return Ok(jars);
    }

    let mut resolved = config::load_lockfile_checked(project, cfg)?;
    let mut exclusions = cfg.exclusions.as_ref().cloned().unwrap_or_default();
    exclusions.extend(cfg.per_dependency_exclusions());
    exclusions.extend(cfg.resolved_exclusions());

    let dep_scopes = build_dep_scope_map(cfg, &["compile", "provided", "runtime", "test"]);
    let jars = crate::workspace::resolver::resolve_no_download(
        &deps, &cache, &mut resolved, &registries, &exclusions, &resolutions, &dep_scopes,
    )?;

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

    let resources_dir = project.join("src").join("main").join("resources");

    let compile_cfg = compiler::CompileConfig {
        source_dirs: vec![src.clone()],
        // Both dirs are copied into output_dir and packaged into the jar:
        // non-.java files under src/main/java, and everything under
        // src/main/resources. Folding them into the build cache key makes a
        // resource-only change invalidate the cache (see compute_build_cache_key).
        resource_dirs: vec![src.clone(), resources_dir.clone()],
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
    // Sync (copy + prune orphans) resources into output_dir so a resource since
    // deleted/renamed in src cannot linger and be packaged into the jar.
    let fp_dir = incremental::fingerprint_dir_for(&cache, &out);
    resources::sync_resources(
        &compile_cfg.resource_dirs,
        &out,
        &fp_dir,
        custom_res_ext.map(|v| v.as_slice()),
        res_exclude.map(|v| v.as_slice()),
    )?;

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

    let resources_dir = project.join("src").join("main").join("resources");

    let compile_cfg = compiler::CompileConfig {
        source_dirs: vec![src.clone()],
        // See compile_project: resources packaged into the jar must invalidate
        // the build cache key when their contents change.
        resource_dirs: vec![src.clone(), resources_dir.clone()],
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
    // Sync (copy + prune orphans) resources into output_dir so a resource since
    // deleted/renamed in src cannot linger and be packaged into the jar.
    let fp_dir = incremental::fingerprint_dir_for(&cache, &out);
    resources::sync_resources(
        &compile_cfg.resource_dirs,
        &out,
        &fp_dir,
        custom_res_ext.map(|v| v.as_slice()),
        res_exclude.map(|v| v.as_slice()),
    )?;

    incremental::incremental_compile(&compile_cfg, &cache, pool)
}

fn resolve_annotation_processors(project: &Path, cfg: &YmConfig, classpath: &[PathBuf]) -> Result<Vec<PathBuf>> {
    if let Some(coords) = cfg.compiler.as_ref().and_then(|c| c.annotation_processors.as_ref()) {
        if !coords.is_empty() {
            let deps = cfg.maven_dependencies();
            let cache = config::maven_cache_dir();
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
    let mut dev_artifact_ids = collect_dev_dependency_artifact_ids(cfg);
    // Inherit workspace root devDependencies if module has none
    if dev_artifact_ids.is_empty() {
        if let Some(ws_root) = config::find_workspace_root(project) {
            if let Ok(root_cfg) = config::load_config(&ws_root.join(config::CONFIG_FILE)) {
                dev_artifact_ids = collect_dev_dependency_artifact_ids(&root_cfg);
            }
        }
    }
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
        // compile_project_full calls javac directly — no build cache involved.
        resource_dirs: vec![],
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

/// Write a standard JAR from a classes directory, including META-INF/MANIFEST.MF
/// and proper directory entries (required for Spring Boot nested JAR scanning).
///
/// `source_dir` is used to differentiate two failure modes (see ADR-010):
/// - has .java sources but 0 valid .class output → bail (javac failed silently or wrong config)
/// - no .java sources at all (placeholder module) → warn + skip (commit 267ec7b semantics)
///
/// Any 0-byte .class file aborts unconditionally — never a legitimate state.
pub fn write_classes_jar(
    jar_path: &Path,
    classes_dir: &Path,
    source_dir: &Path,
    name: &str,
    version: &str,
) -> anyhow::Result<()> {
    let jar_file = std::fs::File::create(jar_path)?;
    let mut zip = zip::ZipWriter::new(std::io::BufWriter::new(jar_file));
    let options = zip::write::SimpleFileOptions::default();

    // META-INF/MANIFEST.MF
    zip.add_directory("META-INF/", options)?;
    zip.start_file("META-INF/MANIFEST.MF", options)?;
    std::io::Write::write_all(&mut zip, format!(
        "Manifest-Version: 1.0\nImplementation-Title: {}\nImplementation-Version: {}\nBuilt-By: ym\n",
        name, version
    ).as_bytes())?;

    // Classes + directory entries
    let mut class_count = 0u32;
    if classes_dir.exists() {
        // Fix for jarvis-commerce Duplicate filename bug:
        // META-INF/ was added above; seed added_dirs so walkdir doesn't add it again.
        // Also skip the user's META-INF/MANIFEST.MF so it doesn't overwrite the one above.
        let mut added_dirs = std::collections::HashSet::new();
        added_dirs.insert("META-INF/".to_string());

        for entry in walkdir::WalkDir::new(classes_dir) {
            let entry = entry?;
            let path = entry.path();
            let rel = path.strip_prefix(classes_dir)?;
            let entry_name = rel.to_string_lossy().replace('\\', "/");
            if entry_name.is_empty() { continue; }

            if entry.file_type().is_dir() {
                let dir_name = if entry_name.ends_with('/') { entry_name } else { format!("{}/", entry_name) };
                if added_dirs.insert(dir_name.clone()) {
                    zip.add_directory(&dir_name, options)?;
                }
            } else {
                if entry_name == "META-INF/MANIFEST.MF" {
                    continue;
                }
                if entry_name.ends_with(".class") {
                    // ADR-010 Defense ①: refuse 0-byte class — never a legitimate state.
                    let len = entry.metadata()?.len();
                    if len == 0 {
                        anyhow::bail!(
                            "Refusing to package 0-byte class file: {}\n\
                             This usually indicates an interrupted javac run, OOM, or filesystem error.\n\
                             To recover, remove stale state and rebuild:\n\
                             \n\
                                 rm -rf {}/out ~/.ym/build-cache\n\
                                 rm -rf {}/.ym/fingerprints\n\
                                 ymc build {}",
                            path.display(),
                            classes_dir.parent().and_then(|p| p.parent()).map(|p| p.display().to_string()).unwrap_or_else(|| "<module>".to_string()),
                            classes_dir.ancestors().nth(3).map(|p| p.display().to_string()).unwrap_or_else(|| "<workspace-root>".to_string()),
                            name
                        );
                    }
                    class_count += 1;
                }
                zip.start_file(&entry_name, options)?;
                let mut f = std::fs::File::open(path)?;
                std::io::copy(&mut f, &mut zip)?;
            }
        }
    }

    if class_count == 0 {
        // ADR-010 Defense ②: differentiate "has sources but 0 classes" from "no sources at all".
        // The former is a real bug (javac failed silently or misconfig); the latter is the
        // legitimate placeholder-module case that commit 267ec7b downgraded to warn.
        if has_java_sources(source_dir) {
            anyhow::bail!(
                "JAR for '{}' contains no .class files but source dir '{}' has .java files.\n\
                 This usually indicates a silent javac failure or misconfigured sourceDir/target.\n\
                 To recover, remove stale state and rebuild:\n\
                 \n\
                     rm -rf {}/out ~/.ym/build-cache\n\
                     ymc build {}\n\
                 \n\
                 If the source files are intentionally all-commented placeholder code, add at least\n\
                 one class declaration (e.g. `public class Foo {{}}`) so javac produces a .class.",
                name,
                source_dir.display(),
                source_dir.parent().map(|p| p.parent()).flatten().map(|p| p.parent()).flatten().map(|p| p.display().to_string()).unwrap_or_else(|| "<module>".to_string()),
                name
            );
        }
        eprintln!(
            "  {} JAR for '{}' contains no .class files — source dir '{}' has no .java files (placeholder module). Skipping.",
            console::style("Warning:").yellow().bold(), name, source_dir.display()
        );
    }

    zip.finish()?;
    Ok(())
}

/// Walk `src_dir` and return true if any `.java` file exists.
/// Used by `write_classes_jar` to differentiate placeholder modules from compile failures.
fn has_java_sources(src_dir: &Path) -> bool {
    if !src_dir.exists() { return false; }
    walkdir::WalkDir::new(src_dir)
        .into_iter()
        .flatten()
        .any(|e| e.path().extension().and_then(|s| s.to_str()) == Some("java"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    /// Regression for the jarvis-commerce Duplicate filename bug (Phase 10 / KR17.4).
    /// When classes_dir contains arbitrary META-INF/ resources, write_classes_jar must
    /// not raise "invalid Zip archive: Duplicate filename" — the root cause was
    /// META-INF/ being added once by add_directory and again by walkdir.
    #[test]
    fn test_write_classes_jar_with_meta_inf_resources() {
        let tmp = tempfile::tempdir().unwrap();
        let classes_dir = tmp.path().join("classes");
        let meta_inf = classes_dir.join("META-INF");
        let spring_dir = meta_inf.join("spring");
        std::fs::create_dir_all(&spring_dir).unwrap();

        // Same shape of META-INF/ resources that triggered the jarvis-commerce bug.
        std::fs::write(meta_inf.join("spring.factories"), "key=value\n").unwrap();
        std::fs::write(meta_inf.join("anything.txt"), "trigger\n").unwrap();
        std::fs::write(spring_dir.join("X.imports"), "com.example.A\n").unwrap();

        let class_dir = classes_dir.join("com").join("example");
        std::fs::create_dir_all(&class_dir).unwrap();
        std::fs::write(class_dir.join("Foo.class"), b"\xCA\xFE\xBA\xBE").unwrap();

        let src_dir = tmp.path().join("src");
        std::fs::create_dir_all(&src_dir).unwrap();
        let jar_path = tmp.path().join("test.jar");
        write_classes_jar(&jar_path, &classes_dir, &src_dir, "test", "0.1.0")
            .expect("write_classes_jar must not fail with Duplicate filename");

        let file = std::fs::File::open(&jar_path).unwrap();
        let mut archive = zip::ZipArchive::new(file).unwrap();
        let names: Vec<String> = (0..archive.len())
            .map(|i| archive.by_index(i).unwrap().name().to_string())
            .collect();

        assert!(names.contains(&"META-INF/MANIFEST.MF".to_string()), "MANIFEST.MF missing");
        assert!(names.contains(&"META-INF/spring.factories".to_string()), "spring.factories missing");
        assert!(names.contains(&"META-INF/anything.txt".to_string()), "anything.txt (jarvis-commerce trigger) missing");
        assert!(names.contains(&"META-INF/spring/X.imports".to_string()), "spring/*.imports missing");
        assert!(names.contains(&"com/example/Foo.class".to_string()), "Foo.class missing");

        // Core regression assertion: META-INF/ must appear exactly once.
        let meta_inf_dir_count = names.iter().filter(|n| *n == "META-INF/").count();
        assert_eq!(meta_inf_dir_count, 1, "META-INF/ should appear exactly once (no Duplicate)");
    }

    /// A user-supplied META-INF/MANIFEST.MF in classes_dir must not overwrite the one
    /// generated by ym; walkdir must skip it.
    #[test]
    fn test_write_classes_jar_skips_user_manifest() {
        let tmp = tempfile::tempdir().unwrap();
        let classes_dir = tmp.path().join("classes");
        let meta_inf = classes_dir.join("META-INF");
        std::fs::create_dir_all(&meta_inf).unwrap();

        let user_manifest = "Manifest-Version: 1.0\nUser-Custom: should-not-leak\n";
        std::fs::write(meta_inf.join("MANIFEST.MF"), user_manifest).unwrap();
        std::fs::write(classes_dir.join("Foo.class"), b"\xCA\xFE\xBA\xBE").unwrap();

        let src_dir = tmp.path().join("src");
        std::fs::create_dir_all(&src_dir).unwrap();
        let jar_path = tmp.path().join("test.jar");
        write_classes_jar(&jar_path, &classes_dir, &src_dir, "mylib", "0.1.0").unwrap();

        let file = std::fs::File::open(&jar_path).unwrap();
        let mut archive = zip::ZipArchive::new(file).unwrap();
        let mut manifest_file = archive.by_name("META-INF/MANIFEST.MF").unwrap();
        let mut manifest_content = String::new();
        manifest_file.read_to_string(&mut manifest_content).unwrap();

        assert!(manifest_content.contains("Implementation-Title: mylib"),
            "manifest should be ym-generated, got:\n{}", manifest_content);
        assert!(!manifest_content.contains("User-Custom: should-not-leak"),
            "user-provided MANIFEST.MF should not leak into the final jar");
    }

    /// Edge case: an empty classes_dir + empty source_dir (placeholder module per ADR-010 ②)
    /// must not panic — should still produce a jar containing at least META-INF/ + MANIFEST.MF
    /// and just print a warning.
    #[test]
    fn test_write_classes_jar_empty_classes_dir_no_sources() {
        let tmp = tempfile::tempdir().unwrap();
        let classes_dir = tmp.path().join("classes");
        std::fs::create_dir_all(&classes_dir).unwrap();
        let src_dir = tmp.path().join("src");
        std::fs::create_dir_all(&src_dir).unwrap();

        let jar_path = tmp.path().join("test.jar");
        write_classes_jar(&jar_path, &classes_dir, &src_dir, "empty", "0.1.0").unwrap();

        assert!(jar_path.exists(), "jar should be created for placeholder module");
        let file = std::fs::File::open(&jar_path).unwrap();
        let archive = zip::ZipArchive::new(file).unwrap();
        assert!(archive.len() >= 2, "jar should at least have META-INF/ + MANIFEST.MF");
    }

    /// ADR-010 Defense ①: a 0-byte .class file must abort packaging.
    /// This is the core regression — interrupted javac / OOM / WSL2 fs glitch may leave
    /// a 0-byte .class behind; previously class_count counted it as 1 and let it slip through.
    #[test]
    fn test_write_classes_jar_rejects_zero_byte_class() {
        let tmp = tempfile::tempdir().unwrap();
        let classes_dir = tmp.path().join("classes");
        let pkg_dir = classes_dir.join("com").join("example");
        std::fs::create_dir_all(&pkg_dir).unwrap();
        // Simulate an interrupted javac: 0-byte .class file.
        std::fs::File::create(pkg_dir.join("Broken.class")).unwrap();

        let src_dir = tmp.path().join("src");
        std::fs::create_dir_all(&src_dir).unwrap();
        std::fs::write(src_dir.join("Broken.java"), "class Broken {}").unwrap();

        let jar_path = tmp.path().join("test.jar");
        let err = write_classes_jar(&jar_path, &classes_dir, &src_dir, "broken", "0.1.0")
            .expect_err("must reject 0-byte class file");
        let msg = err.to_string();
        assert!(msg.contains("0-byte"), "error must mention 0-byte, got: {}", msg);
        assert!(msg.contains("Broken.class"), "error must name the offending file, got: {}", msg);
    }

    /// ADR-010 Defense ②: classes dir empty BUT source dir has .java files → bail.
    /// This is the "silent javac failure" path — previously commit 267ec7b downgraded
    /// to warn and let an empty jar publish. Now we differentiate by source presence.
    #[test]
    fn test_write_classes_jar_bails_when_sources_exist_but_no_classes() {
        let tmp = tempfile::tempdir().unwrap();
        let classes_dir = tmp.path().join("classes");
        std::fs::create_dir_all(&classes_dir).unwrap();
        // Source dir has a .java file but classes dir is empty — would have been a silent warn.
        let src_dir = tmp.path().join("src");
        std::fs::create_dir_all(&src_dir).unwrap();
        std::fs::write(src_dir.join("Real.java"), "class Real {}").unwrap();

        let jar_path = tmp.path().join("test.jar");
        let err = write_classes_jar(&jar_path, &classes_dir, &src_dir, "real", "0.1.0")
            .expect_err("must bail when sources exist but 0 classes produced");
        let msg = err.to_string();
        assert!(msg.contains("no .class"), "error must explain 0-class, got: {}", msg);
        assert!(msg.contains(".java files"), "error must mention .java sources, got: {}", msg);
    }

    // ───────────────────────── Path A: Spring Boot nested layout (ADR-009 / KR17.1-17.4) ─────────────────────────
    //
    // build_release_jar packs directly to a zip; there is no extractable helper, so these are
    // integration tests that drive the real function and read the produced jar back via
    // zip::ZipArchive — they hit the packaging path itself, not a helper. Fixtures put a fake
    // spring-boot-loader jar in the `jars` list so the from_jars branch hits and NO network /
    // home-dir download is triggered.

    /// 4-byte Java class magic (0xCAFEBABE). Typed &[u8] so make_jar entry slices stay homogeneous.
    const CLASS_MAGIC: &[u8] = &[0xCA, 0xFE, 0xBA, 0xBE];

    /// Build a minimal jar from (entry-name, content) pairs. Fakes loader / autoconfigure / dep jars.
    fn make_jar(path: &Path, entries: &[(&str, &[u8])]) {
        let f = std::fs::File::create(path).unwrap();
        let mut zip = zip::ZipWriter::new(std::io::BufWriter::new(f));
        let opts = zip::write::SimpleFileOptions::default();
        for (name, content) in entries {
            zip.start_file(*name, opts).unwrap();
            std::io::Write::write_all(&mut zip, content).unwrap();
        }
        zip.finish().unwrap();
    }

    /// Fake spring-boot-loader jar: launcher class + a DEEP subpackage class (net/protocol/nested)
    /// + the FileSystemProvider SPI + LICENSE/NOTICE — the four things ADR-009 says must be embedded.
    fn make_loader_jar(path: &Path) {
        make_jar(path, &[
            ("org/springframework/boot/loader/launch/JarLauncher.class", CLASS_MAGIC),
            ("org/springframework/boot/loader/net/protocol/nested/NestedUrlConnection.class", CLASS_MAGIC),
            ("META-INF/services/java.nio.file.spi.FileSystemProvider", "org.springframework.boot.loader.nio.file.NestedFileSystemProvider\n".as_bytes()),
            ("META-INF/LICENSE.txt", "Apache License 2.0\n".as_bytes()),
            ("META-INF/NOTICE.txt", "Spring Boot\n".as_bytes()),
        ]);
    }

    /// Lay down out/classes/ with one app class + one resource (mimics post-compile state).
    fn setup_app_project(proj: &Path) {
        let pkg = config::output_classes_dir(proj).join("com").join("example");
        std::fs::create_dir_all(&pkg).unwrap();
        std::fs::write(pkg.join("App.class"), CLASS_MAGIC).unwrap();
        std::fs::write(config::output_classes_dir(proj).join("application.properties"), "server.port=8080\n").unwrap();
    }

    fn sb_app_config() -> YmConfig {
        let mut cfg = YmConfig::default();
        cfg.name = "myapp".to_string();
        cfg.version = Some("1.0.0".to_string());
        cfg.main = Some("com.example.App".to_string());
        cfg.target = Some("21".to_string());
        cfg
    }

    fn jar_entry_names(jar_path: &Path) -> Vec<String> {
        let file = std::fs::File::open(jar_path).unwrap();
        let mut archive = zip::ZipArchive::new(file).unwrap();
        (0..archive.len()).map(|i| archive.by_index(i).unwrap().name().to_string()).collect()
    }

    fn read_jar_entry(jar_path: &Path, entry: &str) -> String {
        let file = std::fs::File::open(jar_path).unwrap();
        let mut archive = zip::ZipArchive::new(file).unwrap();
        let mut s = String::new();
        archive.by_name(entry).unwrap().read_to_string(&mut s).unwrap();
        s
    }

    /// KR17.2 — full nested layout: loader classes + SPI + LICENSE/NOTICE at root,
    /// app classes/resources under BOOT-INF/classes/, deps under BOOT-INF/lib/, MANIFEST aligned.
    #[test]
    fn test_path_a_produces_spring_boot_nested_layout() {
        let tmp = tempfile::tempdir().unwrap();
        let proj = tmp.path();
        setup_app_project(proj);
        let loader = proj.join("spring-boot-loader-3.5.0.jar");
        make_loader_jar(&loader);
        let autoconf = proj.join("spring-boot-autoconfigure-3.5.0.jar");
        make_jar(&autoconf, &[("org/springframework/boot/autoconfigure/Marker.class", CLASS_MAGIC)]);
        let guava = proj.join("guava-33.0.0.jar");
        make_jar(&guava, &[("com/google/common/Foo.class", CLASS_MAGIC)]);

        build_release_jar(proj, &sb_app_config(), &[loader, autoconf, guava], None, None)
            .expect("Path A packaging must succeed");

        let jar_path = proj.join("out").join("release").join("myapp-1.0.0.jar");
        assert!(jar_path.exists(), "release jar must be produced");
        let names = jar_entry_names(&jar_path);

        assert!(names.iter().any(|n| n == "org/springframework/boot/loader/launch/JarLauncher.class"),
            "loader JarLauncher must be embedded at root");
        assert!(names.iter().any(|n| n == "META-INF/services/java.nio.file.spi.FileSystemProvider"),
            "FileSystemProvider SPI (nested:// scheme) must be present");
        assert!(names.iter().any(|n| n == "META-INF/LICENSE.txt"), "Apache LICENSE.txt must be kept");
        assert!(names.iter().any(|n| n == "META-INF/NOTICE.txt"), "Apache NOTICE.txt must be kept");
        assert!(names.iter().any(|n| n == "BOOT-INF/classes/com/example/App.class"),
            "app class must live under BOOT-INF/classes/");
        assert!(names.iter().any(|n| n == "BOOT-INF/classes/application.properties"),
            "app resource must live under BOOT-INF/classes/");
        assert!(names.iter().any(|n| n == "BOOT-INF/lib/guava-33.0.0.jar"),
            "dependency must be nested under BOOT-INF/lib/");

        let manifest = read_jar_entry(&jar_path, "META-INF/MANIFEST.MF");
        assert!(manifest.contains("Main-Class: org.springframework.boot.loader.launch.JarLauncher"),
            "Main-Class must be JarLauncher, got:\n{}", manifest);
        assert!(manifest.contains("Start-Class: com.example.App"),
            "Start-Class must be the user main, got:\n{}", manifest);
        assert!(manifest.contains("Spring-Boot-Version: 3.5.0"),
            "Spring-Boot-Version inferred from autoconfigure jar, got:\n{}", manifest);
        assert!(manifest.contains("Spring-Boot-Classes: BOOT-INF/classes/"));
        assert!(manifest.contains("Spring-Boot-Lib: BOOT-INF/lib/"));
        assert!(manifest.contains("Spring-Boot-Classpath-Index: BOOT-INF/classpath.idx"));
    }

    /// KR17.2 — BOOT-INF/lib/*.jar MUST be STORED (uncompressed). DEFLATED makes
    /// spring-boot-loader throw "Unable to open nested entry" at runtime.
    #[test]
    fn test_path_a_boot_inf_lib_entries_are_stored() {
        let tmp = tempfile::tempdir().unwrap();
        let proj = tmp.path();
        setup_app_project(proj);
        let loader = proj.join("spring-boot-loader-3.5.0.jar");
        make_loader_jar(&loader);
        let guava = proj.join("guava-33.0.0.jar");
        make_jar(&guava, &[("com/google/common/Foo.class", CLASS_MAGIC)]);

        build_release_jar(proj, &sb_app_config(), &[loader, guava], None, None).unwrap();

        let jar_path = proj.join("out").join("release").join("myapp-1.0.0.jar");
        let file = std::fs::File::open(&jar_path).unwrap();
        let mut archive = zip::ZipArchive::new(file).unwrap();
        let lib = archive.by_name("BOOT-INF/lib/guava-33.0.0.jar").unwrap();
        assert_eq!(lib.compression(), zip::CompressionMethod::Stored,
            "BOOT-INF/lib/*.jar must be STORED, was {:?}", lib.compression());
    }

    /// KR17.2 — classpath.idx strict format: each line `- "BOOT-INF/lib/<jar>"`.
    #[test]
    fn test_path_a_classpath_idx_strict_format() {
        let tmp = tempfile::tempdir().unwrap();
        let proj = tmp.path();
        setup_app_project(proj);
        let loader = proj.join("spring-boot-loader-3.5.0.jar");
        make_loader_jar(&loader);
        let guava = proj.join("guava-33.0.0.jar");
        make_jar(&guava, &[("com/google/common/Foo.class", CLASS_MAGIC)]);

        build_release_jar(proj, &sb_app_config(), &[loader, guava], None, None).unwrap();

        let jar_path = proj.join("out").join("release").join("myapp-1.0.0.jar");
        let idx = read_jar_entry(&jar_path, "BOOT-INF/classpath.idx");
        assert!(idx.contains("- \"BOOT-INF/lib/guava-33.0.0.jar\""),
            "classpath.idx must use strict `- \"BOOT-INF/lib/<jar>\"` format, got:\n{}", idx);
    }

    /// KR17.2 — the loader jar itself must NOT be nested under BOOT-INF/lib/; its classes
    /// are extracted to the root instead.
    #[test]
    fn test_path_a_excludes_loader_jar_from_boot_inf_lib() {
        let tmp = tempfile::tempdir().unwrap();
        let proj = tmp.path();
        setup_app_project(proj);
        let loader = proj.join("spring-boot-loader-3.5.0.jar");
        make_loader_jar(&loader);
        let guava = proj.join("guava-33.0.0.jar");
        make_jar(&guava, &[("com/google/common/Foo.class", CLASS_MAGIC)]);

        build_release_jar(proj, &sb_app_config(), &[loader, guava], None, None).unwrap();

        let names = jar_entry_names(&proj.join("out").join("release").join("myapp-1.0.0.jar"));
        assert!(!names.iter().any(|n| n == "BOOT-INF/lib/spring-boot-loader-3.5.0.jar"),
            "loader jar must NOT be nested in BOOT-INF/lib/");
        assert!(names.iter().any(|n| n == "org/springframework/boot/loader/launch/JarLauncher.class"),
            "loader classes must be at root instead");
    }

    /// KR17.2 — loader classes copied recursively incl. deep subpackages
    /// (net/protocol/nested/...). Missing any subdir → ClassNotFoundException at runtime.
    #[test]
    fn test_path_a_embeds_loader_deep_subpackages() {
        let tmp = tempfile::tempdir().unwrap();
        let proj = tmp.path();
        setup_app_project(proj);
        let loader = proj.join("spring-boot-loader-3.5.0.jar");
        make_loader_jar(&loader);

        build_release_jar(proj, &sb_app_config(), &[loader], None, None).unwrap();

        let names = jar_entry_names(&proj.join("out").join("release").join("myapp-1.0.0.jar"));
        assert!(names.iter().any(|n| n == "org/springframework/boot/loader/net/protocol/nested/NestedUrlConnection.class"),
            "deep loader subpackage class must be embedded recursively, got: {:?}", names);
    }

    /// KR17.4 — two deps resolving to the same BOOT-INF/lib/ filename must bail loudly,
    /// listing the offending entry, instead of letting the zip writer throw a path-less error.
    #[test]
    fn test_path_a_duplicate_boot_inf_lib_entry_bails() {
        let tmp = tempfile::tempdir().unwrap();
        let proj = tmp.path();
        setup_app_project(proj);
        let loader = proj.join("spring-boot-loader-3.5.0.jar");
        make_loader_jar(&loader);
        let a = proj.join("a");
        let b = proj.join("b");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        let dup_a = a.join("guava-33.0.0.jar");
        let dup_b = b.join("guava-33.0.0.jar");
        make_jar(&dup_a, &[("com/google/common/A.class", CLASS_MAGIC)]);
        make_jar(&dup_b, &[("com/google/common/B.class", CLASS_MAGIC)]);

        let err = build_release_jar(proj, &sb_app_config(), &[loader, dup_a, dup_b], None, None)
            .expect_err("duplicate BOOT-INF/lib/ filename must bail");
        let msg = err.to_string();
        assert!(msg.contains("Duplicate entry") || msg.contains("Duplicate entries"),
            "error must mention Duplicate, got: {}", msg);
        assert!(msg.contains("guava-33.0.0.jar"), "error must name the duplicated entry, got: {}", msg);
    }

    /// KR17.1 — dispatch + fallback.
    /// DEVIATION NOTE (spec-code): ADR-009 KR17.1 specifies dispatch by "fat-jar closure contains
    /// org.springframework.boot:spring-boot core artifact". The implementation instead dispatches by
    /// "can a spring-boot-loader jar be located (in jars, cache, or downloadable via autoconfigure
    /// version)". With neither loader nor autoconfigure present it falls back to the flat/uber layout
    /// (Path B). This pins the CURRENT behavior; if KR17.1 dispatch is later aligned to the spec,
    /// update this test.
    #[test]
    fn test_path_a_falls_back_to_flat_when_no_loader() {
        let tmp = tempfile::tempdir().unwrap();
        let proj = tmp.path();
        setup_app_project(proj);
        let plain = proj.join("guava-33.0.0.jar");
        make_jar(&plain, &[("com/google/common/Foo.class", CLASS_MAGIC)]);

        build_release_jar(proj, &sb_app_config(), &[plain], None, None).unwrap();

        let names = jar_entry_names(&proj.join("out").join("release").join("myapp-1.0.0.jar"));
        assert!(names.iter().any(|n| n == "com/example/App.class"),
            "flat layout puts app classes at the root, got: {:?}", names);
        assert!(!names.iter().any(|n| n.starts_with("BOOT-INF/")),
            "fallback must NOT produce BOOT-INF/ layout, got: {:?}", names);
    }

    /// DEVIATION NOTE (spec-code): ADR-009 says "layers.idx not generated yet (P2)", but the implementation
    /// DOES generate BOOT-INF/layers.idx and writes Spring-Boot-Layers-Index into the MANIFEST.
    /// Pinning current behavior — layers.idx is harmless and matches spring-boot-maven-plugin's
    /// default; spec ADR-009 should be updated to match (or the impl reverted).
    #[test]
    fn test_path_a_generates_layers_idx_currently() {
        let tmp = tempfile::tempdir().unwrap();
        let proj = tmp.path();
        setup_app_project(proj);
        let loader = proj.join("spring-boot-loader-3.5.0.jar");
        make_loader_jar(&loader);

        build_release_jar(proj, &sb_app_config(), &[loader], None, None).unwrap();

        let jar_path = proj.join("out").join("release").join("myapp-1.0.0.jar");
        let names = jar_entry_names(&jar_path);
        assert!(names.iter().any(|n| n == "BOOT-INF/layers.idx"),
            "impl currently generates layers.idx (spec ADR-009 says deferred)");
        let manifest = read_jar_entry(&jar_path, "META-INF/MANIFEST.MF");
        assert!(manifest.contains("Spring-Boot-Layers-Index: BOOT-INF/layers.idx"),
            "impl writes Spring-Boot-Layers-Index (spec's MANIFEST field list omits it)");
    }

    // ───────────────────────── Path B: flat/uber layout (ADR-009 / KR17.3) ─────────────────────────
    //
    // build_release_jar_flat is the non-Spring-Boot path (and the no-loader fallback). It merges
    // dependency jars flat into an uber jar via a staging dir, applying Shadow-style transformers.
    // Driven directly (module-private, reachable via `use super::*`).

    /// KR17.3 — whitelist merge: META-INF/services/*, spring.factories, spring/*.imports from
    /// multiple jars are concatenated (line-deduped), not dropped by first-wins.
    #[test]
    fn test_path_b_merges_services_and_spring_files() {
        let tmp = tempfile::tempdir().unwrap();
        let proj = tmp.path();
        setup_app_project(proj);
        let dep1 = proj.join("dep1.jar");
        make_jar(&dep1, &[
            ("META-INF/services/com.example.Service", "com.example.Impl1\n".as_bytes()),
            ("META-INF/spring.factories", "feature.x=enabled\n".as_bytes()),
            ("META-INF/spring/org.example.AutoConfiguration.imports", "com.example.AutoA\n".as_bytes()),
        ]);
        let dep2 = proj.join("dep2.jar");
        make_jar(&dep2, &[
            ("META-INF/services/com.example.Service", "com.example.Impl2\n".as_bytes()),
            ("META-INF/spring.factories", "feature.y=enabled\n".as_bytes()),
            ("META-INF/spring/org.example.AutoConfiguration.imports", "com.example.AutoB\n".as_bytes()),
        ]);

        build_release_jar_flat(proj, &sb_app_config(), &[dep1, dep2], None, None).unwrap();

        let jar_path = proj.join("out").join("release").join("myapp-1.0.0.jar");
        let services = read_jar_entry(&jar_path, "META-INF/services/com.example.Service");
        assert!(services.contains("com.example.Impl1") && services.contains("com.example.Impl2"),
            "services file must merge both impls, got:\n{}", services);
        let factories = read_jar_entry(&jar_path, "META-INF/spring.factories");
        assert!(factories.contains("feature.x=enabled") && factories.contains("feature.y=enabled"),
            "spring.factories must merge both, got:\n{}", factories);
        let imports = read_jar_entry(&jar_path, "META-INF/spring/org.example.AutoConfiguration.imports");
        assert!(imports.contains("com.example.AutoA") && imports.contains("com.example.AutoB"),
            "*.imports must merge both, got:\n{}", imports);
    }

    /// KR17.3 — blacklist discard: dependency MANIFEST.MF, INDEX.LIST and signature blocks
    /// (*.SF/*.DSA/*.RSA) are dropped; normal classes are kept.
    #[test]
    fn test_path_b_discards_manifest_index_and_signatures() {
        let tmp = tempfile::tempdir().unwrap();
        let proj = tmp.path();
        setup_app_project(proj);
        let dep = proj.join("signed.jar");
        make_jar(&dep, &[
            ("META-INF/MANIFEST.MF", "Manifest-Version: 1.0\nFrom-Dep: leak\n".as_bytes()),
            ("META-INF/INDEX.LIST", "JarIndex-Version: 1.0\n".as_bytes()),
            ("META-INF/SIG.SF", "Signature-Version: 1.0\n".as_bytes()),
            ("META-INF/SIG.RSA", "rsa-sig\n".as_bytes()),
            ("META-INF/SIG.DSA", "dsa-sig\n".as_bytes()),
            ("com/example/Real.class", CLASS_MAGIC),
        ]);

        build_release_jar_flat(proj, &sb_app_config(), &[dep], None, None).unwrap();

        let jar_path = proj.join("out").join("release").join("myapp-1.0.0.jar");
        let names = jar_entry_names(&jar_path);
        assert!(!names.iter().any(|n| n == "META-INF/INDEX.LIST"), "INDEX.LIST must be discarded");
        assert!(!names.iter().any(|n| n == "META-INF/SIG.SF"), "*.SF signature must be discarded");
        assert!(!names.iter().any(|n| n == "META-INF/SIG.RSA"), "*.RSA signature must be discarded");
        assert!(!names.iter().any(|n| n == "META-INF/SIG.DSA"), "*.DSA signature must be discarded");
        assert!(names.iter().any(|n| n == "com/example/Real.class"), "normal class must be kept");
        let manifest = read_jar_entry(&jar_path, "META-INF/MANIFEST.MF");
        assert!(!manifest.contains("From-Dep: leak"), "dependency MANIFEST.MF must not leak, got:\n{}", manifest);
    }

    /// KR17.3 — non-mergeable same-path entry: first writer wins. App classes are staged first,
    /// then deps in order; here two deps carry the same class path and dep1 must win.
    #[test]
    fn test_path_b_first_writer_wins_on_conflict() {
        let tmp = tempfile::tempdir().unwrap();
        let proj = tmp.path();
        setup_app_project(proj);
        let dep1 = proj.join("dep1.jar");
        make_jar(&dep1, &[("com/example/Dup.class", "FROM_DEP1".as_bytes())]);
        let dep2 = proj.join("dep2.jar");
        make_jar(&dep2, &[("com/example/Dup.class", "FROM_DEP2".as_bytes())]);

        build_release_jar_flat(proj, &sb_app_config(), &[dep1, dep2], None, None).unwrap();

        let jar_path = proj.join("out").join("release").join("myapp-1.0.0.jar");
        let dup = read_jar_entry(&jar_path, "com/example/Dup.class");
        assert_eq!(dup, "FROM_DEP1", "first writer (dep1) must win the conflict");
    }

    /// KR17.3 — final MANIFEST is ym-generated (Main-Class + Implementation-*) and the layout
    /// is flat (no BOOT-INF/, deps + app classes flattened to root).
    #[test]
    fn test_path_b_generates_flat_uber_manifest() {
        let tmp = tempfile::tempdir().unwrap();
        let proj = tmp.path();
        setup_app_project(proj);
        let dep = proj.join("dep.jar");
        make_jar(&dep, &[("com/example/Lib.class", CLASS_MAGIC)]);

        build_release_jar_flat(proj, &sb_app_config(), &[dep], None, None).unwrap();

        let jar_path = proj.join("out").join("release").join("myapp-1.0.0.jar");
        let manifest = read_jar_entry(&jar_path, "META-INF/MANIFEST.MF");
        assert!(manifest.contains("Main-Class: com.example.App"), "must set user Main-Class, got:\n{}", manifest);
        assert!(manifest.contains("Implementation-Title: myapp"), "must carry Implementation-Title, got:\n{}", manifest);
        assert!(manifest.contains("Implementation-Version: 1.0.0"), "must carry Implementation-Version, got:\n{}", manifest);

        let names = jar_entry_names(&jar_path);
        assert!(!names.iter().any(|n| n.starts_with("BOOT-INF/")), "Path B must be flat (no BOOT-INF/)");
        assert!(names.iter().any(|n| n == "com/example/Lib.class"), "dep class must be flattened to root");
        assert!(names.iter().any(|n| n == "com/example/App.class"), "app class must be flattened to root");
    }

    // ─────────────────────────────────────────────────────────────────────────
    // ADR-020 resolve_deps path matrix: runtime proof that the persist column has
    // exactly one ✓ — the "negative tests".
    //
    // 03-package-management.md ("resolve_deps path matrix test requirements" #1)
    // mandates: the 4 read-only functions (resolve_deps / resolve_deps_with_scopes
    // / resolve_deps_no_download / resolve_deps_no_download_with_root) must leave
    // ym-lock.json byte- and mtime-identical; only resolve_and_persist_deps writes.
    // Testing "writes correctly" is not enough — we must prove "does NOT write when
    // it shouldn't".
    //
    // Isolation: HomeGuard points $HOME at a tempdir so the maven/pom cache lands in
    // an isolated dir; a mockito scoped registry (matches_scope → repos_for_group_id
    // returns it exclusively, never appending real Maven Central) fully mocks the POM
    // + jar — zero real network. The sentinel lockfile's config_hash is deliberately
    // different from the cfg fingerprint so a real resolve always yields different
    // bytes; that's what lets "bytes unchanged" distinguish "read-only didn't write"
    // from "wrote, but content happened to match". Trap: with empty deps,
    // resolve_deps_inner returns early without touching the lockfile, so the persist
    // difference is unobservable — hence a non-empty dep to drive a full resolve.
    // ─────────────────────────────────────────────────────────────────────────

    /// Minimal POM with no transitive deps — BFS terminates after the single artifact.
    fn adr020_pom() -> String {
        r#"<?xml version="1.0"?>
<project>
  <modelVersion>4.0.0</modelVersion>
  <groupId>com.ymtestadr020</groupId>
  <artifactId>foo</artifactId>
  <version>1.0</version>
</project>"#
            .to_string()
    }

    /// A distinct-content lockfile written before each resolve call. Its config_hash
    /// never matches the cfg fingerprint, so a real resolve always produces different
    /// bytes — making "bytes unchanged" a meaningful negative (rules out "wrote, but
    /// content happened to match").
    fn write_sentinel_lock(lock_path: &Path) -> Vec<u8> {
        let mut lock = crate::config::schema::Lockfile::default();
        lock.lockfile_version = 1;
        lock.version_winner_strategy = "latest-wins".to_string();
        lock.config_hash = "SENTINEL_NEVER_OVERWRITTEN".to_string();
        lock.ymc_version = "0.0.0-sentinel".to_string();
        lock.generated_at = "2000-01-01T00:00:00Z".to_string();
        let bytes = (serde_json::to_string_pretty(&lock).unwrap() + "\n").into_bytes();
        std::fs::write(lock_path, &bytes).unwrap();
        bytes
    }

    /// Live fixture for an ADR-020 resolve test: `$HOME` redirected to an isolated
    /// tempdir, a mockito registry serving the foo POM + jar, a single-project
    /// ym.json depending on `com.ymtestadr020:foo`, and a sentinel lockfile on disk.
    /// Every guard is kept alive in the struct for the test's duration; field order
    /// is the teardown order (restore HOME + unlock → stop server → delete tempdirs).
    struct Adr020Fixture {
        _home_guard: crate::test_support::HomeGuard,
        _server: mockito::ServerGuard,
        _pom: mockito::Mock,
        _jar: mockito::Mock,
        _home: tempfile::TempDir,
        _project: tempfile::TempDir,
        project_dir: PathBuf,
        cfg: YmConfig,
        lock_path: PathBuf,
        sentinel: Vec<u8>,
    }

    fn adr020_fixture() -> Adr020Fixture {
        let home = tempfile::tempdir().unwrap();
        let home_guard = crate::test_support::HomeGuard::redirect(home.path());
        // Fast-fail: prove the $HOME redirect actually reaches the cache dir BEFORE any
        // network/download side effect can pollute the real ~/.ym.
        assert!(
            config::maven_cache_dir().starts_with(home.path()),
            "HOME redirect must steer maven_cache_dir into the tempdir (got {})",
            config::maven_cache_dir().display(),
        );

        let mut server = mockito::Server::new();
        let pom = server
            .mock("GET", "/com/ymtestadr020/foo/1.0/foo-1.0.pom")
            .with_status(200)
            .with_body(adr020_pom())
            .create();
        let jar = server
            .mock("GET", "/com/ymtestadr020/foo/1.0/foo-1.0.jar")
            .with_status(200)
            .with_body(b"PK\x03\x04 fake jar bytes".to_vec())
            .create();

        let project = tempfile::tempdir().unwrap();
        let project_dir = project.path().to_path_buf();
        let ym_json = format!(
            r#"{{"name":"adr020","groupId":"com.example","dependencies":{{"com.ymtestadr020:foo":"1.0"}},"registries":{{"mock":{{"url":"{}","scope":"com.ymtestadr020.*"}}}}}}"#,
            server.url(),
        );
        std::fs::write(project_dir.join(config::CONFIG_FILE), ym_json).unwrap();
        let cfg = config::load_config(&project_dir.join(config::CONFIG_FILE)).unwrap();

        let lock_path = config::lockfile_path(&project_dir);
        let sentinel = write_sentinel_lock(&lock_path);

        Adr020Fixture {
            _home_guard: home_guard,
            _server: server,
            _pom: pom,
            _jar: jar,
            _home: home,
            _project: project,
            project_dir,
            cfg,
            lock_path,
            sentinel,
        }
    }

    /// Assert the on-disk lockfile is byte-for-byte the sentinel (the read-only
    /// function did not persist) and that its mtime did not change either.
    fn assert_lock_untouched(fx: &Adr020Fixture, mtime_before: std::time::SystemTime) {
        let after = std::fs::read(&fx.lock_path).unwrap();
        assert_eq!(after, fx.sentinel, "read-only resolve must not rewrite ym-lock.json");
        let mtime_after = std::fs::metadata(&fx.lock_path).unwrap().modified().unwrap();
        assert_eq!(mtime_before, mtime_after, "read-only resolve must not touch ym-lock.json mtime");
    }

    #[test]
    fn resolve_deps_is_readonly_does_not_persist_lockfile() {
        let fx = adr020_fixture();
        let mtime = std::fs::metadata(&fx.lock_path).unwrap().modified().unwrap();
        resolve_deps(&fx.project_dir, &fx.cfg)
            .expect("resolve_deps must succeed against mock registry");
        assert_lock_untouched(&fx, mtime);
    }

    #[test]
    fn resolve_deps_with_scopes_is_readonly_does_not_persist_lockfile() {
        let fx = adr020_fixture();
        let mtime = std::fs::metadata(&fx.lock_path).unwrap().modified().unwrap();
        resolve_deps_with_scopes(&fx.project_dir, &fx.cfg, &["compile"])
            .expect("resolve_deps_with_scopes must succeed against mock registry");
        assert_lock_untouched(&fx, mtime);
    }

    #[test]
    fn resolve_deps_no_download_is_readonly_does_not_persist_lockfile() {
        let fx = adr020_fixture();
        let mtime = std::fs::metadata(&fx.lock_path).unwrap().modified().unwrap();
        resolve_deps_no_download(&fx.project_dir, &fx.cfg)
            .expect("resolve_deps_no_download must succeed against mock registry");
        assert_lock_untouched(&fx, mtime);
    }

    #[test]
    fn resolve_deps_no_download_with_root_is_readonly_does_not_persist_lockfile() {
        let fx = adr020_fixture();
        let mtime = std::fs::metadata(&fx.lock_path).unwrap().modified().unwrap();
        let registries = fx.cfg.registry_entries();
        let resolutions = std::collections::BTreeMap::new();
        let cache = config::maven_cache_dir();
        // A single project acts as its own workspace root.
        resolve_deps_no_download_with_root(
            &fx.project_dir, &fx.cfg, &fx.cfg, &cache, &registries, &resolutions,
        )
        .expect("resolve_deps_no_download_with_root must succeed against mock registry");
        assert_lock_untouched(&fx, mtime);
    }

    /// Positive contrast: the one ✓ in the persist column. resolve_and_persist_deps
    /// MUST write the resolved closure through — otherwise the negative tests above
    /// could pass merely because resolution silently no-ops.
    #[test]
    fn resolve_and_persist_deps_writes_lockfile() {
        let fx = adr020_fixture();
        resolve_and_persist_deps(&fx.project_dir, &fx.cfg)
            .expect("resolve_and_persist_deps must succeed against mock registry");
        let after = std::fs::read(&fx.lock_path).unwrap();
        assert_ne!(after, fx.sentinel, "resolve_and_persist_deps must overwrite the sentinel lockfile");
        let written = String::from_utf8(after).unwrap();
        assert!(
            written.contains("com.ymtestadr020:foo:1.0"),
            "persisted lockfile must contain the resolved dependency, got:\n{}",
            written,
        );
    }
}
