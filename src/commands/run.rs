use anyhow::{bail, Result};
use console::style;
use std::path::PathBuf;
use std::process::Command;

use crate::compiler::javac;
use crate::config;
use crate::workspace::graph::WorkspaceGraph;

pub fn execute(target: Option<String>, class: Option<String>, debug: bool, debug_port: Option<u16>, suspend: bool, program_args: Vec<String>) -> Result<()> {
    let (config_path, cfg) = config::load_or_find_config()?;
    let project = config::project_dir(&config_path);

    if cfg.workspaces.is_some() {
        let target = target.as_deref().unwrap_or_else(|| {
            eprintln!("  In workspace mode, specify a target: ym run <module>");
            std::process::exit(1);
        });
        return run_workspace(&project, target, class.as_deref(), debug, debug_port, suspend, &program_args);
    }

    // Build first
    let jars = super::build::resolve_deps(&project, &cfg)?;
    let result = super::build::compile_project(&project, &cfg, &jars)?;

    if !result.success {
        eprint!("{}", crate::compiler::colorize_errors(&result.errors));
        bail!("Compilation failed");
    }

    // Find main class: --class flag takes priority
    let main_class = if let Some(cls) = class {
        cls
    } else {
        resolve_main_class(&cfg, &project, target.as_deref())?
    };

    // Build classpath
    let out_dir = config::output_classes_dir(&project);
    let mut classpath = vec![out_dir];
    classpath.extend(jars);

    let mut jvm_args = cfg.jvm_args.clone().unwrap_or_default();

    // Add JDWP debug agent if --debug
    if debug || suspend {
        let port = debug_port.unwrap_or(5005);
        let suspend_flag = if suspend { "y" } else { "n" };
        jvm_args.push(format!(
            "-agentlib:jdwp=transport=dt_socket,server=y,suspend={},address=*:{}",
            suspend_flag, port
        ));
        if suspend {
            println!(
                "  {} Debug mode: waiting for debugger on port {}...",
                style("!").yellow(),
                style(port).bold()
            );
        } else {
            println!(
                "  {} Debug mode: listening on port {}",
                style("!").yellow(),
                style(port).bold()
            );
        }
    }

    run_with_classpath(
        &main_class,
        &classpath,
        &jvm_args,
        &program_args,
    )
}

fn run_workspace(root: &std::path::Path, target: &str, class: Option<&str>, debug: bool, debug_port: Option<u16>, suspend: bool, program_args: &[String]) -> Result<()> {
    // Build first
    super::build::execute(Some(target.to_string()), false)?;

    let ws = WorkspaceGraph::build(root)?;
    let packages = ws.transitive_closure(target)?;

    // Build classpath from all packages in the closure
    let mut classpath: Vec<PathBuf> = Vec::new();
    for pkg_name in &packages {
        let pkg = ws.get_package(pkg_name).unwrap();
        classpath.push(config::output_classes_dir(&pkg.path));
        let jars = super::build::resolve_deps(&pkg.path, &pkg.config)?;
        classpath.extend(jars);
    }

    let target_pkg = ws.get_package(target).unwrap();
    let main_class = if let Some(cls) = class {
        cls.to_string()
    } else {
        resolve_main_class(&target_pkg.config, &target_pkg.path, None)?
    };
    let mut jvm_args = target_pkg.config.jvm_args.clone().unwrap_or_default();

    if debug || suspend {
        let port = debug_port.unwrap_or(5005);
        let suspend_flag = if suspend { "y" } else { "n" };
        jvm_args.push(format!(
            "-agentlib:jdwp=transport=dt_socket,server=y,suspend={},address=*:{}",
            suspend_flag, port
        ));
        if suspend {
            println!(
                "  {} Debug mode: waiting for debugger on port {}...",
                style("!").yellow(),
                style(port).bold()
            );
        } else {
            println!(
                "  {} Debug mode: listening on port {}",
                style("!").yellow(),
                style(port).bold()
            );
        }
    }

    run_with_classpath(&main_class, &classpath, &jvm_args, program_args)
}

fn run_with_classpath(
    main_class: &str,
    classpath: &[PathBuf],
    jvm_args: &[String],
    program_args: &[String],
) -> Result<()> {
    let sep = if cfg!(windows) { ";" } else { ":" };
    let cp = classpath
        .iter()
        .map(|p| p.to_string_lossy().to_string())
        .collect::<Vec<_>>()
        .join(sep);

    println!(
        "  {} Running {}",
        style("✓").green(),
        style(main_class).bold()
    );
    println!();

    let mut cmd = Command::new("java");
    cmd.args(jvm_args)
        .arg("-cp")
        .arg(&cp)
        .arg(main_class)
        .args(program_args);

    let status = cmd.status()?;

    if !status.success() {
        std::process::exit(status.code().unwrap_or(1));
    }

    Ok(())
}

pub fn resolve_main_class(
    cfg: &config::schema::YmConfig,
    project: &std::path::Path,
    _target: Option<&str>,
) -> Result<String> {
    // 1. Check ym.json "main" field
    if let Some(ref main) = cfg.main {
        return Ok(main.clone());
    }

    // 2. Scan source files for main methods
    let src = config::source_dir(project);
    let main_classes = javac::scan_main_methods(&[src])?;

    match main_classes.len() {
        0 => bail!("No main class found. Set \"main\" in ym.json or add a public static void main method."),
        1 => Ok(main_classes.into_iter().next().unwrap()),
        _ => {
            println!("  Multiple main classes found:");
            let selection = dialoguer::Select::new()
                .with_prompt("Select main class")
                .items(&main_classes)
                .default(0)
                .interact()?;
            Ok(main_classes[selection].clone())
        }
    }
}

/// Start a Java process and return the Child handle
pub fn start_java_process(
    main_class: &str,
    classpath: &[PathBuf],
    jvm_args: &[String],
) -> Result<std::process::Child> {
    start_java_process_with_args(main_class, classpath, jvm_args, &[])
}

/// Start a Java process with program arguments and return the Child handle
pub fn start_java_process_with_args(
    main_class: &str,
    classpath: &[PathBuf],
    jvm_args: &[String],
    program_args: &[String],
) -> Result<std::process::Child> {
    let sep = if cfg!(windows) { ";" } else { ":" };
    let cp = classpath
        .iter()
        .map(|p| p.to_string_lossy().to_string())
        .collect::<Vec<_>>()
        .join(sep);

    let child = Command::new("java")
        .args(jvm_args)
        .arg("-cp")
        .arg(&cp)
        .arg(main_class)
        .args(program_args)
        .spawn()?;

    Ok(child)
}
