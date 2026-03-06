use anyhow::{bail, Result};
use console::style;
use std::time::Instant;

use crate::compiler;
use crate::config;
use crate::workspace::graph::WorkspaceGraph;

pub fn execute(target: Option<String>, strict: bool) -> Result<()> {
    let start = Instant::now();

    let (config_path, mut cfg) = config::load_or_find_config()?;
    let project = config::project_dir(&config_path);

    super::build::ensure_jdk_for_config(&cfg)?;

    // In strict mode, add -Werror to compiler args
    if strict {
        let compiler = cfg.compiler.get_or_insert_with(Default::default);
        let args = compiler.args.get_or_insert_with(Vec::new);
        if !args.iter().any(|a| a == "-Werror") {
            args.push("-Werror".to_string());
        }
        println!(
            "  {} Strict mode: warnings treated as errors",
            style("!").yellow()
        );
    }

    if cfg.workspaces.is_some() {
        return check_workspace(&project, target.as_deref(), start, strict);
    }

    let jars = super::build::resolve_deps(&project, &cfg)?;
    let result = super::build::compile_project(&project, &cfg, &jars)?;

    print_check_result(&cfg.name, &result)?;

    let elapsed = start.elapsed();
    println!(
        "\n  {} Checked in {}ms",
        style("⚡").cyan(),
        elapsed.as_millis()
    );

    Ok(())
}

fn check_workspace(root: &std::path::Path, target: Option<&str>, start: Instant, strict: bool) -> Result<()> {
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

    let mut workspace_classpath = Vec::new();
    let mut total_errors = 0;
    let mut total_warnings = 0;

    for pkg_name in &packages {
        let pkg = ws.get_package(pkg_name).unwrap();
        let mut pkg_config = pkg.config.clone();
        if strict {
            let compiler = pkg_config.compiler.get_or_insert_with(Default::default);
            let args = compiler.args.get_or_insert_with(Vec::new);
            if !args.iter().any(|a| a == "-Werror") {
                args.push("-Werror".to_string());
            }
        }
        let jars = super::build::resolve_deps(&pkg.path, &pkg_config)?;
        let mut classpath = jars;
        classpath.extend(workspace_classpath.clone());

        let result = super::build::compile_project(&pkg.path, &pkg_config, &classpath)?;

        if !result.success {
            total_errors += count_occurrences(&result.errors, ": error:");
            total_warnings += count_occurrences(&result.errors, ": warning:");
            eprint!("{}", compiler::colorize_errors(&result.errors));
        } else {
            // Count warnings even on success
            total_warnings += count_occurrences(&result.errors, ": warning:");
            if !result.errors.is_empty() {
                eprint!("{}", compiler::colorize_errors(&result.errors));
            }
            println!(
                "  {} {}",
                style("✓").green(),
                style(pkg_name).bold()
            );
        }

        workspace_classpath.push(config::output_classes_dir(&pkg.path));
    }

    let elapsed = start.elapsed();

    if total_errors > 0 {
        println!(
            "\n  {} {} errors, {} warnings in {} packages ({}ms)",
            style("✗").red(),
            total_errors,
            total_warnings,
            packages.len(),
            elapsed.as_millis()
        );
        bail!("Check failed with {} errors", total_errors);
    } else if total_warnings > 0 {
        println!(
            "\n  {} {} warnings in {} packages ({}ms)",
            style("!").yellow(),
            total_warnings,
            packages.len(),
            elapsed.as_millis()
        );
    } else {
        println!(
            "\n  {} All {} packages OK ({}ms)",
            style("✓").green(),
            packages.len(),
            elapsed.as_millis()
        );
    }

    Ok(())
}

fn print_check_result(name: &str, result: &compiler::CompileResult) -> Result<()> {
    if !result.success {
        eprint!("{}", compiler::colorize_errors(&result.errors));
        bail!("Check failed for {}", name);
    }

    let warnings = count_occurrences(&result.errors, ": warning:");
    if !result.errors.is_empty() {
        eprint!("{}", compiler::colorize_errors(&result.errors));
    }

    if warnings > 0 {
        println!(
            "  {} {} — {} warnings",
            style("!").yellow(),
            style(name).bold(),
            warnings
        );
    } else {
        println!(
            "  {} {} — no issues found",
            style("✓").green(),
            style(name).bold()
        );
    }

    Ok(())
}

fn count_occurrences(text: &str, pattern: &str) -> usize {
    text.matches(pattern).count()
}
