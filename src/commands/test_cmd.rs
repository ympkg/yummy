use anyhow::{bail, Result};
use console::style;
use std::process::Command;
use std::time::Duration;

use crate::compiler::javac;
use crate::config;
use crate::scripts;
use crate::watcher::FileWatcher;

pub fn execute(
    target: Option<String>,
    watch: bool,
    filter: Option<String>,
    verbose: bool,
    fail_fast: bool,
    timeout: Option<u64>,
    coverage: bool,
    list: bool,
) -> Result<()> {
    let (config_path, cfg) = config::load_or_find_config()?;
    let project = config::project_dir(&config_path);

    // Ensure JDK is available
    super::build::ensure_jdk_for_config(&cfg)?;

    // Workspace mode: test a specific module
    if cfg.workspaces.is_some() {
        let target = target.as_deref().unwrap_or_else(|| {
            eprintln!("  In workspace mode, specify a target: ym test <module>");
            std::process::exit(1);
        });
        if list {
            return list_test_classes_workspace(&project, target, filter.as_deref());
        }
        return test_workspace(&project, target, watch, filter, verbose, fail_fast);
    }

    // List test classes only
    if list {
        let test_dir = config::test_dir_for(&project, &cfg);
        return list_test_classes(&test_dir, filter.as_deref());
    }

    // Run pretest script
    scripts::run_script(&cfg.scripts, &cfg.env, "pretest", &project)?;

    run_tests(&project, &cfg, filter.as_deref(), verbose, fail_fast, timeout, coverage)?;

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

        println!();
        println!("  Watching for changes...");
        println!();

        loop {
            let changed = watcher.wait_for_changes(Duration::from_millis(100));
            if changed.is_empty() {
                continue;
            }

            for path in &changed {
                if let Some(name) = path.file_name() {
                    println!(
                        "  {} Changed: {}",
                        style("↻").blue(),
                        style(name.to_string_lossy()).yellow()
                    );
                }
            }

            if let Err(e) = run_tests(&project, &cfg, filter.as_deref(), verbose, fail_fast, timeout, coverage) {
                eprintln!("  {} {}", style("✗").red(), e);
            }
        }
    }

    // Run posttest script
    scripts::run_script(&cfg.scripts, &cfg.env, "posttest", &project)?;

    Ok(())
}

fn run_tests(
    project: &std::path::Path,
    cfg: &config::schema::YmConfig,
    filter: Option<&str>,
    verbose: bool,
    fail_fast: bool,
    timeout: Option<u64>,
    coverage: bool,
) -> Result<()> {
    // Resolve all deps including devDependencies
    let mut all_deps = cfg.dependencies.clone().unwrap_or_default();
    if let Some(dev_deps) = &cfg.dev_dependencies {
        all_deps.extend(dev_deps.clone());
    }

    let cache = config::maven_cache_dir(project);
    let lock_path = project.join(config::LOCK_FILE);
    let mut lock = config::load_lock(&lock_path)?;

    let jars = crate::workspace::resolver::resolve_and_download(&all_deps, &cache, &mut lock)?;
    config::save_lock(&lock_path, &lock)?;

    // Compile main + test sources
    let src_dir = config::source_dir_for(project, cfg);
    let test_dir = config::test_dir_for(project, cfg);
    let out_dir = config::output_classes_dir(project);

    let compile_cfg = crate::compiler::CompileConfig {
        source_dirs: vec![src_dir, test_dir.clone()],
        output_dir: out_dir.clone(),
        classpath: jars.clone(),
        java_version: cfg.target.clone(),
        encoding: cfg.compiler.as_ref().and_then(|c| c.encoding.clone()),
        annotation_processors: vec![],
        lint: vec![],
        extra_args: vec![],
    };

    let result = javac::compile(&compile_cfg)?;
    if !result.success {
        eprint!("{}", crate::compiler::colorize_errors(&result.errors));
        bail!("Test compilation failed");
    }

    // Find test classes
    let mut test_classes = find_test_classes(&test_dir)?;

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
        "  {} Running {} test class(es)...",
        style("→").blue(),
        test_classes.len()
    );

    let sep = if cfg!(windows) { ";" } else { ":" };
    let mut classpath = vec![out_dir.to_string_lossy().to_string()];
    classpath.extend(jars.iter().map(|p| p.to_string_lossy().to_string()));
    let cp = classpath.join(sep);

    // Try JUnit Platform Console standalone
    let junit_launcher = jars.iter().find(|p| {
        p.to_string_lossy()
            .contains("junit-platform-console-standalone")
    });

    // Set up JaCoCo coverage if requested
    let jacoco_agent = if coverage {
        find_jacoco_agent(&jars, project)
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

        if let Some(pattern) = filter {
            // Use include filter for class name
            cmd.arg("--include-classname").arg(format!(".*{}.*", pattern));
        }

        cmd.arg("--scan-class-path");

        let status = cmd.status()?;
        if !status.success() {
            bail!("Tests failed");
        }
    } else {
        // Fallback: run test classes directly
        let mut failures = 0;
        for class in &test_classes {
            println!("  Running {}...", style(class).cyan());
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
            println!(
                "  {} Use JaCoCo CLI or IDE to generate HTML report",
                style("→").dim()
            );
        }
    }

    Ok(())
}

/// Find JaCoCo agent JAR from deps or download it.
fn find_jacoco_agent(
    jars: &[std::path::PathBuf],
    project: &std::path::Path,
) -> Option<std::path::PathBuf> {
    // Check if jacocoagent is already in deps
    for jar in jars {
        let name = jar.to_string_lossy();
        if name.contains("jacoco") && name.contains("agent") {
            return Some(jar.clone());
        }
    }

    // Check cache
    let cache = config::cache_dir(project);
    let tools_dir = cache.join("tools");
    let agent_jar = tools_dir.join("jacocoagent.jar");
    if agent_jar.exists() {
        return Some(agent_jar);
    }

    // Download JaCoCo
    println!(
        "  {} Downloading JaCoCo agent...",
        style("↓").blue()
    );

    std::fs::create_dir_all(&tools_dir).ok()?;
    let version = "0.8.12";
    let url = format!(
        "https://repo1.maven.org/maven2/org/jacoco/org.jacoco.agent/{}/org.jacoco.agent-{}-runtime.jar",
        version, version
    );

    let client = reqwest::blocking::Client::builder()
        .user_agent("ym/0.1.0")
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

fn test_workspace(
    root: &std::path::Path,
    target: &str,
    watch: bool,
    filter: Option<String>,
    verbose: bool,
    fail_fast: bool,
) -> Result<()> {
    use crate::workspace::graph::WorkspaceGraph;

    // Build the target and its dependencies first
    super::build::execute(Some(target.to_string()), false)?;

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

    // Also include devDependencies of the target
    if let Some(dev_deps) = &target_pkg.config.dev_dependencies {
        let cache = config::maven_cache_dir(&target_pkg.path);
        let lock_path = target_pkg.path.join(config::LOCK_FILE);
        let mut lock = config::load_lock(&lock_path)?;
        let dev_jars = crate::workspace::resolver::resolve_and_download(dev_deps, &cache, &mut lock)?;
        config::save_lock(&lock_path, &lock)?;
        classpath_jars.extend(dev_jars);
    }

    // Compile test sources for the target module
    let test_dir = config::test_dir(&target_pkg.path);
    let out_dir = config::output_classes_dir(&target_pkg.path);

    if test_dir.exists() {
        let compile_cfg = crate::compiler::CompileConfig {
            source_dirs: vec![test_dir.clone()],
            output_dir: out_dir.clone(),
            classpath: classpath_jars.clone(),
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

        let result = crate::compiler::javac::compile(&compile_cfg)?;
        if !result.success {
            eprint!("{}", crate::compiler::colorize_errors(&result.errors));
            bail!("Test compilation failed");
        }
    }

    // Find and run test classes
    let mut test_classes = find_test_classes(&test_dir)?;
    if test_classes.is_empty() {
        println!("  {} No test classes found in {}", style("!").yellow(), target);
        return Ok(());
    }

    if let Some(ref pattern) = filter {
        test_classes.retain(|c| c.contains(pattern));
    }

    println!(
        "  {} Running {} test class(es) in {}...",
        style("→").blue(),
        test_classes.len(),
        style(target).bold()
    );

    let sep = if cfg!(windows) { ";" } else { ":" };
    let cp = classpath_jars
        .iter()
        .map(|p| p.to_string_lossy().to_string())
        .collect::<Vec<_>>()
        .join(sep);

    // Try JUnit Platform Console standalone
    let junit_launcher = classpath_jars.iter().find(|p| {
        p.to_string_lossy()
            .contains("junit-platform-console-standalone")
    });

    if let Some(launcher) = junit_launcher {
        let mut cmd = std::process::Command::new("java");
        cmd.arg("-jar").arg(launcher).arg("--class-path").arg(&cp);
        if verbose {
            cmd.arg("--details").arg("verbose");
        }
        if fail_fast {
            cmd.arg("--fail-if-no-tests");
        }
        if let Some(ref pattern) = filter {
            cmd.arg("--include-classname")
                .arg(format!(".*{}.*", pattern));
        }
        cmd.arg("--scan-class-path");
        let status = cmd.status()?;
        if !status.success() {
            bail!("Tests failed");
        }
    } else {
        for class in &test_classes {
            println!("  Running {}...", style(class).cyan());
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
                    println!("  {} Changed: {}", style("↻").blue(), style(name.to_string_lossy()).yellow());
                }
            }
            if let Err(e) = test_workspace(root, target, false, filter.clone(), verbose, fail_fast) {
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

/// List test classes in a workspace module
fn list_test_classes_workspace(root: &std::path::Path, target: &str, filter: Option<&str>) -> Result<()> {
    let ws = crate::workspace::graph::WorkspaceGraph::build(root)?;
    let pkg = ws.get_package(target)
        .ok_or_else(|| anyhow::anyhow!("Package '{}' not found", target))?;
    let test_dir = config::test_dir(&pkg.path);
    list_test_classes(&test_dir, filter)
}

fn find_test_classes(test_dir: &std::path::Path) -> Result<Vec<String>> {
    let mut classes = Vec::new();

    if !test_dir.exists() {
        return Ok(classes);
    }

    for entry in walkdir::WalkDir::new(test_dir) {
        let entry = entry?;
        if entry.path().extension().and_then(|e| e.to_str()) != Some("java") {
            continue;
        }
        let content = std::fs::read_to_string(entry.path())?;
        if content.contains("@Test") || content.contains("@org.junit") {
            let rel = entry.path().strip_prefix(test_dir)?;
            let class = rel
                .to_string_lossy()
                .replace('/', ".")
                .replace('\\', ".")
                .trim_end_matches(".java")
                .to_string();
            classes.push(class);
        }
    }

    Ok(classes)
}
