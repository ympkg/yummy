use anyhow::{bail, Result};
use console::style;
use std::process::Command;
use std::time::Duration;

use crate::config;
use crate::scripts;
use crate::watcher::FileWatcher;

#[derive(Clone, Copy)]
enum TestMode {
    Unit,
    Integration,
    All,
}

pub fn execute(
    target: Option<String>,
    watch: bool,
    filter: Option<String>,
    integration: bool,
    all: bool,
    tag: Option<String>,
    exclude_tag: Option<String>,
    verbose: bool,
    fail_fast: bool,
    timeout: Option<u64>,
    coverage: bool,
    list: bool,
    keep_going: bool,
    report: Option<String>,
    parallel: bool,
) -> Result<()> {
    let (config_path, cfg) = config::load_or_find_config()?;
    let project = config::project_dir(&config_path);

    super::idea::auto_sync_idea(&project, &cfg);

    // Ensure JDK is available
    super::build::ensure_jdk_for_config(&cfg)?;

    let test_mode = if all {
        TestMode::All
    } else if integration {
        TestMode::Integration
    } else {
        TestMode::Unit
    };

    // Workspace mode
    if cfg.workspaces.is_some() {
        if let Some(ref target) = target {
            if list {
                return list_test_classes_workspace(&project, target, filter.as_deref());
            }
            scripts::run_script(&cfg, "pretest", &project)?;
            let result = test_workspace(&project, target, watch, filter, verbose, fail_fast, &test_mode, parallel);
            scripts::run_script(&cfg, "posttest", &project)?;
            return result;
        }
        // No target: test all modules
        scripts::run_script(&cfg, "pretest", &project)?;
        let result = test_all_workspace_modules(&project, &cfg, filter, verbose, fail_fast, keep_going, &test_mode, parallel);
        scripts::run_script(&cfg, "posttest", &project)?;
        return result;
    }

    // List test classes only
    if list {
        let test_dir = config::test_dir_for(&project, &cfg);
        return list_test_classes(&test_dir, filter.as_deref());
    }

    // Run pretest script
    scripts::run_script(&cfg, "pretest", &project)?;

    run_tests(&project, &cfg, filter.as_deref(), verbose, fail_fast, timeout, coverage,
              &test_mode, tag.as_deref(), exclude_tag.as_deref(), report.as_deref(), parallel)?;

    if watch {
        let src_dir = config::source_dir(&project);
        let test_dir = config::test_dir(&project);

        let mut watch_dirs = vec![];
        if src_dir.exists() {
            watch_dirs.push(src_dir);
        }
        if test_dir.exists() {
            watch_dirs.push(test_dir);
        }

        if watch_dirs.is_empty() {
            println!("  {} No source directories to watch", style("!").yellow());
            return Ok(());
        }

        let extensions = vec![".java".to_string()];
        let watcher = FileWatcher::new(&watch_dirs, extensions)?;

        // Track failed tests for 'f' key
        let mut failed_tests: Vec<String> = Vec::new();
        let mut current_filter = filter.clone();

        // Spawn a thread to read keyboard input
        let (key_tx, key_rx) = std::sync::mpsc::channel::<char>();
        let is_tty = console::Term::stdout().is_term();
        if is_tty {
            std::thread::spawn(move || {
                let term = console::Term::stdout();
                loop {
                    if let Ok(ch) = term.read_char() {
                        if key_tx.send(ch).is_err() {
                            break;
                        }
                    }
                }
            });
        }

        print_watch_prompt();

        loop {
            // Check for keyboard input (non-blocking)
            if let Ok(ch) = key_rx.try_recv() {
                match ch {
                    'q' | 'Q' => break,
                    'a' | 'A' => {
                        println!("  {} running all tests...", style("➜").green());
                        current_filter = None;
                        match run_tests(&project, &cfg, None, verbose, fail_fast, timeout, coverage, &test_mode, tag.as_deref(), exclude_tag.as_deref(), None, parallel) {
                            Ok(()) => { failed_tests.clear(); }
                            Err(e) => {
                                collect_failed_from_error(&e, &mut failed_tests);
                                eprintln!("  {} {}", style("✗").red(), e);
                            }
                        }
                        print_watch_prompt();
                    }
                    'f' | 'F' => {
                        if failed_tests.is_empty() {
                            println!("  {} No failed tests to re-run", style("!").yellow());
                        } else {
                            println!("  {} re-running {} failed test(s)...", style("➜").green(), failed_tests.len());
                            // Run with each failed test as filter
                            for class in &failed_tests {
                                let _ = run_tests(&project, &cfg, Some(class), verbose, fail_fast, timeout, coverage, &test_mode, tag.as_deref(), exclude_tag.as_deref(), None, parallel);
                            }
                        }
                        print_watch_prompt();
                    }
                    'p' | 'P' => {
                        if is_tty {
                            let input: String = dialoguer::Input::new()
                                .with_prompt("  Filter pattern")
                                .allow_empty(true)
                                .interact_text()
                                .unwrap_or_default();
                            current_filter = if input.is_empty() { None } else { Some(input) };
                            println!("  {} running with filter: {}", style("➜").green(),
                                current_filter.as_deref().unwrap_or("(none)"));
                            match run_tests(&project, &cfg, current_filter.as_deref(), verbose, fail_fast, timeout, coverage, &test_mode, tag.as_deref(), exclude_tag.as_deref(), None, parallel) {
                                Ok(()) => {}
                                Err(e) => {
                                    collect_failed_from_error(&e, &mut failed_tests);
                                    eprintln!("  {} {}", style("✗").red(), e);
                                }
                            }
                        }
                        print_watch_prompt();
                    }
                    _ => {}
                }
                continue;
            }

            // Check for file changes
            let changed = watcher.wait_for_changes(Duration::from_millis(100));
            if changed.is_empty() {
                continue;
            }

            for path in &changed {
                if let Some(name) = path.file_name() {
                    println!(
                        "  {} Changed: {}",
                        style("➜").green(),
                        style(name.to_string_lossy()).yellow()
                    );
                }
            }

            match run_tests(&project, &cfg, current_filter.as_deref(), verbose, fail_fast, timeout, coverage, &test_mode, tag.as_deref(), exclude_tag.as_deref(), None, parallel) {
                Ok(()) => { failed_tests.clear(); }
                Err(e) => {
                    collect_failed_from_error(&e, &mut failed_tests);
                    eprintln!("  {} {}", style("✗").red(), e);
                }
            }
            print_watch_prompt();
        }
    }

    // Run posttest script
    scripts::run_script(&cfg, "posttest", &project)?;

    Ok(())
}

fn print_watch_prompt() {
    println!();
    println!(
        "  Press: {} run all  {} run failed  {} filter  {} quit",
        style("a").cyan().bold(),
        style("f").cyan().bold(),
        style("p").cyan().bold(),
        style("q").cyan().bold(),
    );
    println!();
}

fn collect_failed_from_error(e: &anyhow::Error, failed: &mut Vec<String>) {
    // Parse error message for failed test class names
    // Format: "Test failed: com.example.FooTest. Stopping (--fail-fast)"
    // or: "N test class(es) failed"
    let msg = e.to_string();
    if let Some(rest) = msg.strip_prefix("Test failed: ") {
        if let Some(dot_pos) = rest.find(". ") {
            let name = &rest[..dot_pos];
            if !failed.contains(&name.to_string()) {
                failed.push(name.to_string());
            }
        }
    }
}

fn run_tests(
    project: &std::path::Path,
    cfg: &config::schema::YmConfig,
    filter: Option<&str>,
    verbose: bool,
    fail_fast: bool,
    timeout: Option<u64>,
    coverage: bool,
    test_mode: &TestMode,
    tag: Option<&str>,
    exclude_tag: Option<&str>,
    report: Option<&str>,
    parallel: bool,
) -> Result<()> {
    // Resolve all deps first to populate cache, then use scope-filtered subsets
    let _all_jars = super::build::resolve_deps(project, cfg)?;

    // Main compilation classpath: compile + provided
    let compile_jars = super::build::resolve_deps_with_scopes(project, cfg, &["compile", "provided"])?;
    // Test compilation classpath: compile + provided + test
    let test_compile_jars = super::build::resolve_deps_with_scopes(project, cfg, &["compile", "provided", "test"])?;
    // Test runtime classpath: compile + runtime + provided + test
    let test_run_jars = super::build::resolve_deps_with_scopes(project, cfg, &["compile", "runtime", "provided", "test"])?;

    // Compile main sources → out/classes
    let src_dir = config::source_dir_for(project, cfg);
    let test_dir = config::test_dir_for(project, cfg);
    let out_dir = config::output_classes_dir(project);
    let test_out_dir = config::output_test_classes_dir(project);

    // Step 1: compile main source (compile + provided scope)
    let main_compile_cfg = crate::compiler::CompileConfig {
        source_dirs: vec![src_dir],
        output_dir: out_dir.clone(),
        classpath: compile_jars,
        java_version: cfg.target.clone(),
        encoding: cfg.compiler.as_ref().and_then(|c| c.encoding.clone()),
        annotation_processors: vec![],
        lint: vec![],
        extra_args: vec![],
    };

    let cache = config::cache_dir(project);
    let engine = crate::compiler::CompilerEngine::from_config(
        cfg.compiler.as_ref().and_then(|c| c.engine.as_deref()),
    );
    let result = crate::compiler::incremental::incremental_compile(&main_compile_cfg, &cache, &engine)?;
    if !result.success {
        eprint!("{}", crate::compiler::colorize_errors(&result.errors));
        bail!("Main compilation failed");
    }

    // Step 2: compile test source → out/test-classes (compile + provided + test scope)
    if test_dir.exists() {
        let mut test_classpath = vec![out_dir.clone()];
        test_classpath.extend(test_compile_jars);

        let test_compile_cfg = crate::compiler::CompileConfig {
            source_dirs: vec![test_dir.clone()],
            output_dir: test_out_dir.clone(),
            classpath: test_classpath,
            java_version: cfg.target.clone(),
            encoding: cfg.compiler.as_ref().and_then(|c| c.encoding.clone()),
            annotation_processors: vec![],
            lint: vec![],
            extra_args: vec![],
        };

        let result = crate::compiler::incremental::incremental_compile(&test_compile_cfg, &cache, &engine)?;
        if !result.success {
            eprint!("{}", crate::compiler::colorize_errors(&result.errors));
            bail!("Test compilation failed");
        }
    }

    // Copy main resources (src/main/resources → out/classes)
    let custom_res_ext = cfg.compiler.as_ref().and_then(|c| c.resource_extensions.as_ref());
    let res_exclude = cfg.compiler.as_ref().and_then(|c| c.resource_exclude.as_ref());
    let main_resources = project.join("src").join("main").join("resources");
    if main_resources.exists() {
        crate::resources::copy_resources_with_extensions(&main_resources, &out_dir, custom_res_ext.map(|v| v.as_slice()), res_exclude.map(|v| v.as_slice()))?;
    }

    // Copy test resources (src/test/resources → out/test-classes)
    let test_resources = project.join("src").join("test").join("resources");
    if test_resources.exists() {
        crate::resources::copy_resources_with_extensions(&test_resources, &test_out_dir, custom_res_ext.map(|v| v.as_slice()), res_exclude.map(|v| v.as_slice()))?;
    }

    // Find test classes based on mode
    let mut test_classes = find_test_classes_filtered(&test_dir, test_mode)?;

    if test_classes.is_empty() {
        println!("  {} No test classes found", style("!").yellow());
        return Ok(());
    }

    // Apply filter
    if let Some(pattern) = filter {
        test_classes.retain(|c| c.contains(pattern));
        if test_classes.is_empty() {
            println!(
                "  {} No test classes match filter '{}'",
                style("!").yellow(),
                pattern
            );
            return Ok(());
        }
    }

    println!(
        "  {} running {} test class(es)...",
        style("➜").green(),
        test_classes.len()
    );

    let sep = if cfg!(windows) { ";" } else { ":" };
    let mut classpath = vec![
        out_dir.to_string_lossy().to_string(),
        test_out_dir.to_string_lossy().to_string(),
    ];
    classpath.extend(test_run_jars.iter().map(|p| p.to_string_lossy().to_string()));
    let cp = classpath.join(sep);

    // Ensure JUnit Platform Console standalone launcher is available
    let junit_launcher = ensure_junit_launcher(&test_run_jars, project);

    // Set up JaCoCo coverage if requested
    let jacoco_version = cfg.compiler.as_ref()
        .and_then(|c| c.jacoco_version.as_deref())
        .unwrap_or("0.8.12");
    let jacoco_agent = if coverage {
        find_jacoco_agent(&test_run_jars, project, jacoco_version)
    } else {
        None
    };

    if let Some(launcher) = junit_launcher {
        let mut cmd = Command::new("java");

        if let Some(ref agent_jar) = jacoco_agent {
            let report_dir = project.join("out").join("coverage");
            std::fs::create_dir_all(&report_dir).ok();
            let exec_file = report_dir.join("jacoco.exec");
            cmd.arg(format!(
                "-javaagent:{}=destfile={}",
                agent_jar.display(),
                exec_file.display()
            ));
        }

        cmd.arg("-jar")
            .arg(launcher)
            .arg("--class-path")
            .arg(&cp);

        if verbose {
            cmd.arg("--details").arg("verbose");
        }

        if fail_fast {
            cmd.arg("--fail-if-no-tests")
                .arg("-c")
                .arg("junit.jupiter.execution.order.random.seed=0");
        }

        if let Some(secs) = timeout {
            cmd.arg("-c")
                .arg(format!("junit.jupiter.execution.timeout.default={}s", secs));
        }

        if parallel {
            cmd.arg("-c")
                .arg("junit.jupiter.execution.parallel.enabled=true")
                .arg("-c")
                .arg("junit.jupiter.execution.parallel.mode.default=concurrent");
            // Integration tests run sequentially by default
            match test_mode {
                TestMode::Integration => {}
                _ => {
                    cmd.arg("-c")
                        .arg("junit.jupiter.execution.parallel.mode.classes.default=concurrent");
                }
            }
        }

        if let Some(pattern) = filter {
            if pattern.contains('#') {
                // Method-level filter: TestClass#method → --select-method
                cmd.arg("--select-method").arg(pattern);
            } else {
                cmd.arg("--include-classname").arg(format!(".*{}.*", pattern));
            }
        }

        // Test mode filtering
        match test_mode {
            TestMode::Unit => {
                // Exclude integration tests
                cmd.arg("--exclude-classname").arg(".*IT$");
                cmd.arg("--exclude-classname").arg(".*IntegrationTest$");
            }
            TestMode::Integration => {
                // Only integration tests
                cmd.arg("--include-classname").arg(".*IT$|.*IntegrationTest$");
            }
            TestMode::All => {
                // No filtering — run everything
            }
        }

        // JUnit @Tag filtering
        if let Some(t) = tag {
            cmd.arg("--include-tag").arg(t);
        }
        if let Some(t) = exclude_tag {
            cmd.arg("--exclude-tag").arg(t);
        }

        // Generate test reports if requested
        if let Some(report_type) = report {
            let reports_dir = project.join("out").join("test-reports");
            std::fs::create_dir_all(&reports_dir).ok();
            match report_type {
                "junit-xml" | "xml" => {
                    cmd.arg("--reports-dir").arg(&reports_dir);
                }
                "html" => {
                    // Generate XML first, then convert to HTML after tests run
                    cmd.arg("--reports-dir").arg(&reports_dir);
                }
                _ => {
                    eprintln!(
                        "  {} Unknown report type '{}', supported: junit-xml, html",
                        console::style("!").yellow(),
                        report_type
                    );
                }
            }
        }

        // Only scan test-classes dir (not dependency JARs on the classpath)
        cmd.arg("--scan-class-path").arg(&test_out_dir);

        let status = cmd.status()?;

        // Print report location if generated
        if let Some(report_type) = report {
            let reports_dir = project.join("out").join("test-reports");
            if reports_dir.exists() {
                match report_type {
                    "html" => {
                        let html_file = reports_dir.join("index.html");
                        generate_test_html_report(&reports_dir, &html_file);
                        if html_file.exists() {
                            println!(
                                "  {} Test report: {}",
                                console::style("✓").green(),
                                html_file.display()
                            );
                        }
                    }
                    "junit-xml" | "xml" => {
                        println!(
                            "  {} Test reports: {}",
                            console::style("✓").green(),
                            reports_dir.display()
                        );
                    }
                    _ => {}
                }
            }
        }

        if !status.success() {
            bail!("Tests failed");
        }
    } else {
        // Fallback: run test classes directly
        let mut failures = 0;
        for class in &test_classes {
            println!("  running {}...", style(class).cyan());
            let status = Command::new("java")
                .arg("-cp")
                .arg(&cp)
                .arg("org.junit.platform.console.ConsoleLauncher")
                .arg("--select-class")
                .arg(class)
                .status();

            match status {
                Ok(s) if s.success() => {
                    println!("  {} {}", style("✓").green(), class);
                }
                _ => {
                    failures += 1;
                    println!(
                        "  {} {} (JUnit Platform not on classpath?)",
                        style("✗").red(),
                        class
                    );
                    if fail_fast {
                        bail!("Test failed: {}. Stopping (--fail-fast)", class);
                    }
                }
            }
        }
        if failures > 0 {
            bail!("{} test class(es) failed", failures);
        }
    }

    if jacoco_agent.is_some() {
        let exec_file = project.join("out").join("coverage").join("jacoco.exec");
        if exec_file.exists() {
            let size = std::fs::metadata(&exec_file).map(|m| m.len()).unwrap_or(0);
            println!(
                "  {} Coverage data: {} ({:.1} KB)",
                style("✓").green(),
                exec_file.display(),
                size as f64 / 1024.0
            );

            // Generate HTML report via JaCoCo CLI
            generate_jacoco_html_report(project, cfg, &exec_file, &out_dir);
        }
    }

    Ok(())
}

/// Ensure junit-platform-console-standalone is available.
/// Detects the platform version from existing JARs and auto-downloads if needed.
fn ensure_junit_launcher(
    jars: &[std::path::PathBuf],
    project: &std::path::Path,
) -> Option<std::path::PathBuf> {
    // Already in classpath?
    for jar in jars {
        let name = jar.to_string_lossy();
        if name.contains("junit-platform-console-standalone") {
            return Some(jar.clone());
        }
    }

    // Detect JUnit Platform version from existing deps (e.g. junit-platform-engine-1.13.0-M3.jar)
    let platform_version = jars.iter().find_map(|jar| {
        let stem = jar.file_stem()?.to_string_lossy();
        stem.strip_prefix("junit-platform-engine-")
            .or_else(|| stem.strip_prefix("junit-platform-commons-"))
            .map(|v| v.to_string())
    });

    let version = match platform_version {
        Some(v) => v,
        None => return None, // No JUnit Platform on classpath at all
    };

    // Check cache
    let cache = config::cache_dir(project);
    let tools_dir = cache.join("tools");
    let launcher_jar = tools_dir.join(format!(
        "junit-platform-console-standalone-{}.jar",
        version
    ));
    if launcher_jar.exists() {
        return Some(launcher_jar);
    }

    // Download
    println!(
        "  {} downloading junit-platform-console-standalone {}...",
        style("➜").green(),
        version
    );

    std::fs::create_dir_all(&tools_dir).ok()?;
    let url = format!(
        "https://repo1.maven.org/maven2/org/junit/platform/junit-platform-console-standalone/{}/junit-platform-console-standalone-{}.jar",
        version, version
    );

    let client = reqwest::blocking::Client::builder()
        .user_agent(concat!("ym/", env!("CARGO_PKG_VERSION")))
        .timeout(std::time::Duration::from_secs(60))
        .build()
        .ok()?;

    let response = client.get(&url).send().ok()?;
    if !response.status().is_success() {
        println!(
            "  {} Failed to download JUnit launcher (HTTP {})",
            style("!").yellow(),
            response.status()
        );
        return None;
    }

    let bytes = response.bytes().ok()?;
    std::fs::write(&launcher_jar, &bytes).ok()?;

    println!(
        "  {} Downloaded junit-platform-console-standalone {}",
        style("✓").green(),
        version
    );
    Some(launcher_jar)
}

/// Find JaCoCo agent JAR from deps or download it.
fn find_jacoco_agent(
    jars: &[std::path::PathBuf],
    project: &std::path::Path,
    version: &str,
) -> Option<std::path::PathBuf> {
    // Check if jacocoagent is already in deps
    for jar in jars {
        let name = jar.to_string_lossy();
        if name.contains("jacoco") && name.contains("agent") {
            return Some(jar.clone());
        }
    }

    // Check cache (versioned filename to handle version changes)
    let cache = config::cache_dir(project);
    let tools_dir = cache.join("tools");
    let agent_jar = tools_dir.join(format!("jacocoagent-{}.jar", version));
    if agent_jar.exists() {
        return Some(agent_jar);
    }

    // Download JaCoCo
    println!(
        "  {} downloading JaCoCo agent {}...",
        style("➜").green(),
        version
    );

    std::fs::create_dir_all(&tools_dir).ok()?;
    let url = format!(
        "https://repo1.maven.org/maven2/org/jacoco/org.jacoco.agent/{}/org.jacoco.agent-{}-runtime.jar",
        version, version
    );

    let client = reqwest::blocking::Client::builder()
        .user_agent(concat!("ym/", env!("CARGO_PKG_VERSION")))
        .timeout(std::time::Duration::from_secs(60))
        .build()
        .ok()?;

    let response = client.get(&url).send().ok()?;
    if !response.status().is_success() {
        println!(
            "  {} Failed to download JaCoCo (HTTP {})",
            style("!").yellow(),
            response.status()
        );
        return None;
    }

    let bytes = response.bytes().ok()?;
    std::fs::write(&agent_jar, &bytes).ok()?;

    println!(
        "  {} Downloaded JaCoCo {}",
        style("✓").green(),
        version
    );
    Some(agent_jar)
}

/// Generate HTML coverage report via JaCoCo CLI.
fn generate_jacoco_html_report(
    project: &std::path::Path,
    cfg: &config::schema::YmConfig,
    exec_file: &std::path::Path,
    classes_dir: &std::path::Path,
) {
    let version = cfg.compiler.as_ref()
        .and_then(|c| c.jacoco_version.as_deref())
        .unwrap_or("0.8.12");

    let cache = config::cache_dir(project);
    let tools_dir = cache.join("tools");
    let cli_jar = tools_dir.join(format!("jacococli-{}.jar", version));

    // Download JaCoCo CLI if not cached
    if !cli_jar.exists() {
        let url = format!(
            "https://repo1.maven.org/maven2/org/jacoco/org.jacoco.cli/{}/org.jacoco.cli-{}-nodeps.jar",
            version, version
        );
        let _ = std::fs::create_dir_all(&tools_dir);
        if let Ok(client) = reqwest::blocking::Client::builder()
            .user_agent(concat!("ym/", env!("CARGO_PKG_VERSION")))
            .timeout(std::time::Duration::from_secs(60))
            .build()
        {
            if let Ok(resp) = client.get(&url).send() {
                if resp.status().is_success() {
                    if let Ok(bytes) = resp.bytes() {
                        let _ = std::fs::write(&cli_jar, &bytes);
                    }
                }
            }
        }
    }

    if !cli_jar.exists() {
        println!(
            "  {} Use JaCoCo CLI or IDE to generate HTML report",
            style("→").dim()
        );
        return;
    }

    let html_dir = project.join("out").join("coverage").join("html");
    let _ = std::fs::create_dir_all(&html_dir);

    let src_dir = config::source_dir_for(project, cfg);
    let status = Command::new("java")
        .arg("-jar").arg(&cli_jar)
        .arg("report").arg(exec_file)
        .arg("--classfiles").arg(classes_dir)
        .arg("--sourcefiles").arg(&src_dir)
        .arg("--html").arg(&html_dir)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();

    match status {
        Ok(s) if s.success() => {
            println!(
                "  {} Coverage report: {}",
                style("✓").green(),
                html_dir.join("index.html").display()
            );
        }
        _ => {
            println!(
                "  {} Failed to generate HTML report, use JaCoCo CLI manually",
                style("!").yellow()
            );
        }
    }
}

fn test_workspace(
    root: &std::path::Path,
    target: &str,
    watch: bool,
    filter: Option<String>,
    verbose: bool,
    fail_fast: bool,
    test_mode: &TestMode,
    parallel: bool,
) -> Result<()> {
    use crate::workspace::graph::WorkspaceGraph;

    // Build the target and its dependencies first
    super::build::compile_only(Some(target.to_string()))?;

    let ws = WorkspaceGraph::build(root)?;
    let packages = ws.transitive_closure(target)?;
    let target_pkg = ws.get_package(target).unwrap();

    // Build classpath from all packages in the closure
    let mut classpath_jars: Vec<std::path::PathBuf> = Vec::new();
    for pkg_name in &packages {
        let pkg = ws.get_package(pkg_name).unwrap();
        classpath_jars.push(config::output_classes_dir(&pkg.path));
        let jars = super::build::resolve_deps(&pkg.path, &pkg.config)?;
        classpath_jars.extend(jars);
    }

    // Also resolve all Maven dependencies (including test-scoped) for the target
    {
        let all_deps = target_pkg.config.maven_dependencies();
        let cache = config::maven_cache_dir(&target_pkg.path);
        let mut resolved = config::load_resolved_cache(&target_pkg.path)?;
        let extra_jars = crate::workspace::resolver::resolve_and_download(&all_deps, &cache, &mut resolved)?;
        config::save_resolved_cache(&target_pkg.path, &resolved)?;
        classpath_jars.extend(extra_jars);
    }

    // Compile test sources for the target module → out/test-classes
    let test_dir = config::test_dir(&target_pkg.path);
    let out_dir = config::output_classes_dir(&target_pkg.path);
    let test_out_dir = config::output_test_classes_dir(&target_pkg.path);

    if test_dir.exists() {
        let mut test_cp = vec![out_dir.clone()];
        test_cp.extend(classpath_jars.clone());

        let compile_cfg = crate::compiler::CompileConfig {
            source_dirs: vec![test_dir.clone()],
            output_dir: test_out_dir.clone(),
            classpath: test_cp,
            java_version: target_pkg.config.target.clone(),
            encoding: target_pkg
                .config
                .compiler
                .as_ref()
                .and_then(|c| c.encoding.clone()),
            annotation_processors: vec![],
            lint: vec![],
            extra_args: vec![],
        };

        let ws_cache = config::cache_dir(&target_pkg.path);
        let ws_engine = crate::compiler::CompilerEngine::from_config(
            target_pkg.config.compiler.as_ref().and_then(|c| c.engine.as_deref()),
        );
        let result = crate::compiler::incremental::incremental_compile(&compile_cfg, &ws_cache, &ws_engine)?;
        if !result.success {
            eprint!("{}", crate::compiler::colorize_errors(&result.errors));
            bail!("Test compilation failed");
        }
    }

    // Find and run test classes
    let mut test_classes = find_test_classes_filtered(&test_dir, test_mode)?;
    if test_classes.is_empty() {
        println!("  {} No test classes found in {}", style("!").yellow(), target);
        return Ok(());
    }

    if let Some(ref pattern) = filter {
        test_classes.retain(|c| c.contains(pattern));
    }

    println!(
        "  {} running {} test class(es) in {}...",
        style("➜").green(),
        test_classes.len(),
        style(target).bold()
    );

    // Add test-classes to classpath
    classpath_jars.insert(0, test_out_dir);

    let sep = if cfg!(windows) { ";" } else { ":" };
    let cp = classpath_jars
        .iter()
        .map(|p| p.to_string_lossy().to_string())
        .collect::<Vec<_>>()
        .join(sep);

    // Ensure JUnit Platform Console standalone launcher is available
    let junit_launcher = ensure_junit_launcher(&classpath_jars, &target_pkg.path);

    if let Some(launcher) = junit_launcher {
        let mut cmd = std::process::Command::new("java");
        cmd.arg("-jar").arg(launcher).arg("--class-path").arg(&cp);
        if verbose {
            cmd.arg("--details").arg("verbose");
        }
        if fail_fast {
            cmd.arg("--fail-if-no-tests");
        }
        if parallel {
            cmd.arg("-c")
                .arg("junit.jupiter.execution.parallel.enabled=true")
                .arg("-c")
                .arg("junit.jupiter.execution.parallel.mode.default=concurrent")
                .arg("-c")
                .arg("junit.jupiter.execution.parallel.mode.classes.default=concurrent");
        }
        if let Some(ref pattern) = filter {
            cmd.arg("--include-classname")
                .arg(format!(".*{}.*", pattern));
        }
        // Only scan test-classes dir (not dependency JARs)
        let ws_test_out = config::output_test_classes_dir(&target_pkg.path);
        cmd.arg("--scan-class-path").arg(&ws_test_out);
        let status = cmd.status()?;
        if !status.success() {
            bail!("Tests failed");
        }
    } else {
        for class in &test_classes {
            println!("  running {}...", style(class).cyan());
            let status = std::process::Command::new("java")
                .arg("-cp")
                .arg(&cp)
                .arg(class)
                .status();
            match status {
                Ok(s) if s.success() => println!("  {} {}", style("✓").green(), class),
                _ => {
                    println!("  {} {}", style("✗").red(), class);
                    if fail_fast {
                        bail!("Test failed: {}. Stopping (--fail-fast)", class);
                    }
                }
            }
        }
    }

    if watch {
        let mut watch_dirs = vec![];
        let src_dir = config::source_dir(&target_pkg.path);
        if src_dir.exists() {
            watch_dirs.push(src_dir);
        }
        if test_dir.exists() {
            watch_dirs.push(test_dir);
        }
        let watcher = FileWatcher::new(&watch_dirs, vec![".java".to_string()])?;
        println!();
        println!("  Watching for changes...");
        loop {
            let changed = watcher.wait_for_changes(Duration::from_millis(100));
            if changed.is_empty() {
                continue;
            }
            for path in &changed {
                if let Some(name) = path.file_name() {
                    println!("  {} Changed: {}", style("➜").green(), style(name.to_string_lossy()).yellow());
                }
            }
            if let Err(e) = test_workspace(root, target, false, filter.clone(), verbose, fail_fast, &TestMode::Unit, parallel) {
                eprintln!("  {} {}", style("✗").red(), e);
            }
        }
    }

    Ok(())
}

/// List test classes without running them
fn list_test_classes(test_dir: &std::path::Path, filter: Option<&str>) -> Result<()> {
    let mut classes = find_test_classes(test_dir)?;

    if let Some(pattern) = filter {
        classes.retain(|c| c.contains(pattern));
    }

    println!();
    if classes.is_empty() {
        println!("  {} No test classes found", style("!").yellow());
    } else {
        println!("  {} test class(es):", classes.len());
        println!();
        for class in &classes {
            println!("  {} {}", style("·").dim(), style(class).cyan());
        }
    }
    println!();

    Ok(())
}

/// Test all modules in the workspace (no target specified).
fn test_all_workspace_modules(
    root: &std::path::Path,
    _cfg: &config::schema::YmConfig,
    filter: Option<String>,
    verbose: bool,
    fail_fast: bool,
    keep_going: bool,
    test_mode: &TestMode,
    parallel: bool,
) -> Result<()> {
    use crate::workspace::graph::WorkspaceGraph;

    let ws = WorkspaceGraph::build(root)?;
    let mut packages = ws.all_packages();
    packages.sort();

    // Build all modules first
    super::build::compile_only(None)?;

    let mut failures = Vec::new();

    for pkg_name in &packages {
        let pkg = ws.get_package(pkg_name).unwrap();
        let test_dir = config::test_dir(&pkg.path);
        if !test_dir.exists() {
            continue;
        }
        let classes = find_test_classes_filtered(&test_dir, test_mode)?;
        if classes.is_empty() {
            continue;
        }

        println!(
            "\n  {} Testing {}...",
            style("➜").green(),
            style(pkg_name).cyan()
        );

        match test_workspace(root, pkg_name, false, filter.clone(), verbose, fail_fast, test_mode, parallel) {
            Ok(()) => {
                println!(
                    "  {} {} tests passed",
                    style("✓").green(),
                    pkg_name
                );
            }
            Err(e) => {
                eprintln!(
                    "  {} {} tests failed: {}",
                    style("✗").red(),
                    pkg_name,
                    e
                );
                if !keep_going {
                    return Err(e);
                }
                failures.push(pkg_name.clone());
            }
        }
    }

    if !failures.is_empty() {
        bail!(
            "{} module(s) had test failures: {}",
            failures.len(),
            failures.join(", ")
        );
    }

    Ok(())
}

/// List test classes in a workspace module
fn list_test_classes_workspace(root: &std::path::Path, target: &str, filter: Option<&str>) -> Result<()> {
    let ws = crate::workspace::graph::WorkspaceGraph::build(root)?;
    let pkg = ws.get_package(target)
        .ok_or_else(|| anyhow::anyhow!("Package '{}' not found", target))?;
    let test_dir = config::test_dir(&pkg.path);
    list_test_classes(&test_dir, filter)
}

fn find_test_classes(test_dir: &std::path::Path) -> Result<Vec<String>> {
    find_test_classes_filtered(test_dir, &TestMode::Unit)
}

fn find_test_classes_filtered(test_dir: &std::path::Path, mode: &TestMode) -> Result<Vec<String>> {
    let mut classes = Vec::new();

    if !test_dir.exists() {
        return Ok(classes);
    }

    for entry in walkdir::WalkDir::new(test_dir) {
        let entry = entry?;
        if entry.path().extension().and_then(|e| e.to_str()) != Some("java") {
            continue;
        }

        let file_stem = entry
            .path()
            .file_stem()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();

        // Filename-based test discovery
        let is_unit = file_stem.ends_with("Test")
            || file_stem.starts_with("Test")
            || file_stem.ends_with("Tests");
        let is_integration =
            file_stem.ends_with("IT") || file_stem.ends_with("IntegrationTest");

        let matches_mode = match mode {
            TestMode::Unit => is_unit && !is_integration,
            TestMode::Integration => is_integration,
            TestMode::All => is_unit || is_integration,
        };

        if !matches_mode {
            continue;
        }

        // Verify file contains test annotations
        let content = std::fs::read_to_string(entry.path())?;
        if !content.contains("@Test") && !content.contains("@org.junit") {
            continue;
        }

        // Exclude abstract classes
        if content.contains("abstract class") {
            continue;
        }

        let rel = entry.path().strip_prefix(test_dir)?;
        let class = rel
            .to_string_lossy()
            .replace('/', ".")
            .replace('\\', ".")
            .trim_end_matches(".java")
            .to_string();
        classes.push(class);
    }

    Ok(classes)
}

/// Generate an HTML test report from JUnit XML files in the reports directory.
fn generate_test_html_report(xml_dir: &std::path::Path, html_file: &std::path::Path) {
    use std::fs;

    let mut suites: Vec<(String, usize, usize, usize, f64)> = Vec::new(); // name, tests, failures, errors, time
    let mut total_tests = 0usize;
    let mut total_failures = 0usize;
    let mut total_errors = 0usize;
    let mut total_time = 0.0f64;

    // Parse all TEST-*.xml files
    let entries = match fs::read_dir(xml_dir) {
        Ok(e) => e,
        Err(_) => return,
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("xml") {
            continue;
        }
        let content = match fs::read_to_string(&path) {
            Ok(c) => c,
            Err(_) => continue,
        };

        // Simple XML parsing for testsuite attributes
        if let Some(ts_start) = content.find("<testsuite") {
            let ts_end = content[ts_start..].find('>').unwrap_or(0) + ts_start;
            let tag = &content[ts_start..=ts_end];

            let name = extract_attr(tag, "name").unwrap_or_else(|| "unknown".to_string());
            let tests: usize = extract_attr(tag, "tests").and_then(|v| v.parse().ok()).unwrap_or(0);
            let failures: usize = extract_attr(tag, "failures").and_then(|v| v.parse().ok()).unwrap_or(0);
            let errors: usize = extract_attr(tag, "errors").and_then(|v| v.parse().ok()).unwrap_or(0);
            let time: f64 = extract_attr(tag, "time").and_then(|v| v.parse().ok()).unwrap_or(0.0);

            total_tests += tests;
            total_failures += failures;
            total_errors += errors;
            total_time += time;
            suites.push((name, tests, failures, errors, time));
        }
    }

    suites.sort_by(|a, b| a.0.cmp(&b.0));

    let passed = total_tests.saturating_sub(total_failures + total_errors);
    let status_color = if total_failures + total_errors > 0 { "#dc3545" } else { "#28a745" };

    let mut rows = String::new();
    for (name, tests, failures, errors, time) in &suites {
        let suite_passed = tests.saturating_sub(failures + errors);
        let row_class = if *failures + *errors > 0 { " class=\"failed\"" } else { "" };
        rows.push_str(&format!(
            "    <tr{}><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{:.3}s</td></tr>\n",
            row_class, name, tests, suite_passed, failures, errors, time
        ));
    }

    let html = format!(
        r#"<!DOCTYPE html>
<html>
<head>
<meta charset="utf-8">
<title>Test Report</title>
<style>
  body {{ font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', sans-serif; margin: 2em; background: #f8f9fa; }}
  h1 {{ color: #333; }}
  .summary {{ display: flex; gap: 1.5em; margin: 1em 0; }}
  .stat {{ padding: 1em 1.5em; border-radius: 8px; background: white; box-shadow: 0 1px 3px rgba(0,0,0,0.1); }}
  .stat .value {{ font-size: 2em; font-weight: bold; }}
  .stat .label {{ color: #666; font-size: 0.9em; }}
  table {{ border-collapse: collapse; width: 100%; background: white; border-radius: 8px; overflow: hidden; box-shadow: 0 1px 3px rgba(0,0,0,0.1); }}
  th {{ background: #343a40; color: white; text-align: left; padding: 0.75em 1em; }}
  td {{ padding: 0.6em 1em; border-bottom: 1px solid #eee; }}
  tr:hover {{ background: #f1f3f5; }}
  tr.failed td {{ background: #fff5f5; }}
  .status {{ font-size: 1.2em; font-weight: bold; color: {status_color}; }}
</style>
</head>
<body>
<h1>Test Report</h1>
<p class="status">{} passed, {} failed, {} errors — {:.3}s</p>
<div class="summary">
  <div class="stat"><div class="value">{}</div><div class="label">Total</div></div>
  <div class="stat"><div class="value" style="color:#28a745">{}</div><div class="label">Passed</div></div>
  <div class="stat"><div class="value" style="color:#dc3545">{}</div><div class="label">Failed</div></div>
  <div class="stat"><div class="value" style="color:#fd7e14">{}</div><div class="label">Errors</div></div>
</div>
<table>
  <thead><tr><th>Test Suite</th><th>Tests</th><th>Passed</th><th>Failures</th><th>Errors</th><th>Time</th></tr></thead>
  <tbody>
{}  </tbody>
</table>
<p style="color:#999;margin-top:2em;font-size:0.85em">Generated by ym test</p>
</body>
</html>
"#,
        passed, total_failures, total_errors, total_time,
        total_tests, passed, total_failures, total_errors,
        rows
    );

    let _ = fs::write(html_file, html);
}

fn extract_attr(tag: &str, name: &str) -> Option<String> {
    let pattern = format!("{}=\"", name);
    let start = tag.find(&pattern)? + pattern.len();
    let end = tag[start..].find('"')? + start;
    Some(tag[start..end].to_string())
}
