pub mod commands;
pub mod compiler;
pub mod config;
pub mod hotreload;
pub mod jdk_manager;
pub mod jvm;
pub mod resources;
pub mod scripts;
pub mod watcher;
pub mod workspace;

use anyhow::Result;
use clap::{CommandFactory, Parser, Subcommand};
use clap_complete::{generate, Shell};

// ============================================================
//  JSON quiet mode — suppress human-readable stdout in --json
// ============================================================

use std::sync::atomic::AtomicBool;

/// When true, all human-readable progress/status output to stdout is suppressed.
/// Used by `--json` commands to ensure stdout contains only valid JSON.
pub static JSON_QUIET: AtomicBool = AtomicBool::new(false);

pub fn is_json_quiet() -> bool {
    JSON_QUIET.load(std::sync::atomic::Ordering::Relaxed)
}

// ============================================================
//  Color mode — shared by ym and ymc
// ============================================================

#[derive(Clone, Debug)]
enum ColorMode {
    Auto,
    Always,
    Never,
}

impl std::str::FromStr for ColorMode {
    type Err = String;
    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "auto" => Ok(ColorMode::Auto),
            "always" => Ok(ColorMode::Always),
            "never" => Ok(ColorMode::Never),
            _ => Err(format!("invalid color mode '{}' (expected auto, always, never)", s)),
        }
    }
}

fn apply_color_mode(mode: &ColorMode) {
    match mode {
        ColorMode::Auto => {} // console crate handles TTY + NO_COLOR automatically
        ColorMode::Always => console::set_colors_enabled(true),
        ColorMode::Never => console::set_colors_enabled(false),
    }
}

// ============================================================
//  YM — Package manager & script runner  (like Yarn)
// ============================================================

#[derive(Parser)]
#[command(name = "ym", about = format!("Yummy v{} - A modern Java package manager", env!("CARGO_PKG_VERSION")), version)]
struct YmCli {
    /// Color output: auto, always, never
    #[arg(long, global = true, default_value = "auto")]
    color: ColorMode,

    #[command(subcommand)]
    command: YmCommands,
}

#[derive(Subcommand)]
enum YmCommands {
    /// Initialize a new project
    Init {
        /// Project name (creates new directory if provided)
        name: Option<String>,
        /// Interactive mode: select package, JDK, template
        #[arg(long, short = 'i')]
        interactive: bool,
        /// Project template: app (default), lib, spring-boot
        #[arg(long, short = 't')]
        template: Option<String>,
        /// Skip all prompts and use defaults
        #[arg(long, short = 'y')]
        yes: bool,
    },
    /// Add a dependency to package.toml
    Add {
        /// Dependency (e.g., guava, com.google.guava:guava@33.0, or Gradle scope)
        dep: Option<String>,
        /// Coordinate (when first arg is a Gradle scope like implementation)
        coord: Option<String>,
        /// Dependency scope: compile (default), runtime, provided, test
        #[arg(long)]
        scope: Option<String>,
        /// Classifier (e.g., natives-linux)
        #[arg(long)]
        classifier: Option<String>,
    },
    /// Remove a dependency from package.toml
    Remove {
        /// Dependency coordinate (groupId:artifactId or artifactId)
        dep: String,
    },
    /// Upgrade dependencies to latest versions
    Upgrade {
        /// Interactively select which dependencies to upgrade
        #[arg(long, short = 'i')]
        interactive: bool,
        /// Skip confirmation, upgrade all (for CI)
        #[arg(long, short = 'y')]
        yes: bool,
        /// Output upgradable deps as JSON (no modification)
        #[arg(long)]
        json: bool,
    },
    /// Convert from Maven/Gradle to package.toml
    Convert {
        /// Verify migration by running build after conversion
        #[arg(long)]
        verify: bool,
    },
    /// Publish to a Maven repository
    Publish {
        /// Target module name (workspace mode)
        target: Option<String>,
        /// Target registry name (from [registries])
        #[arg(long)]
        registry: Option<String>,
        /// Simulate publish without uploading
        #[arg(long)]
        dry_run: bool,
    },
    /// Login to a Maven repository
    Login {
        /// List saved credentials
        #[arg(long)]
        list: bool,
        /// Remove credentials for a registry URL
        #[arg(long)]
        remove: Option<String>,
    },
    /// Show project and environment info
    Info {
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
    /// Show dependency tree
    Tree {
        /// Max depth to display (0 = unlimited)
        #[arg(long, default_value = "0")]
        depth: usize,
        /// Output as JSON
        #[arg(long)]
        json: bool,
        /// Show flat list instead of tree
        #[arg(long)]
        flat: bool,
        /// Output as Graphviz DOT
        #[arg(long)]
        dot: bool,
        /// Show reverse dependencies for a specific dep
        #[arg(long)]
        reverse: Option<String>,
    },
    /// Diagnose environment issues
    Doctor {
        /// Attempt to auto-fix common issues
        #[arg(long)]
        fix: bool,
    },
    /// Run one or more scripts
    Run {
        /// Script names to run
        scripts: Vec<String>,
        /// Run scripts in parallel
        #[arg(long, short = 'p')]
        parallel: bool,
    },
    /// Workspace commands
    Workspace {
        #[command(subcommand)]
        action: WorkspaceAction,
    },
    /// Generate shell completions
    Completions {
        /// Shell type (bash, zsh, fish, powershell)
        shell: Shell,
    },
    /// Catch-all: run as script from [scripts]
    #[command(external_subcommand)]
    External(Vec<String>),
}

#[derive(Subcommand)]
enum WorkspaceAction {
    /// List all packages in the workspace
    List,
    /// Run a command in each package
    Foreach {
        /// Command and arguments (after --)
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
        /// Run in parallel across packages
        #[arg(long)]
        parallel: bool,
        /// Number of parallel jobs
        #[arg(long, short = 'j')]
        jobs: Option<usize>,
        /// Continue running even if a package fails
        #[arg(long)]
        keep_going: bool,
    },
}

// ============================================================
//  YMC — Java compiler & runtime engine  (like Vite)
// ============================================================

#[derive(Parser)]
#[command(name = "ymc", about = format!("Yummy v{} - Java compiler and runtime engine", env!("CARGO_PKG_VERSION")), version)]
struct YmcCli {
    /// Color output: auto, always, never
    #[arg(long, global = true, default_value = "auto")]
    color: ColorMode,

    #[command(subcommand)]
    command: YmcCommands,
}

#[derive(Subcommand)]
enum YmcCommands {
    /// Compile the project and package release JAR
    Build {
        /// Target module name (workspace mode)
        target: Option<String>,
        /// Number of parallel compilation threads (default: CPU cores)
        #[arg(long, short = 'j')]
        parallel: Option<usize>,
        /// Show per-phase timing breakdown
        #[arg(long)]
        profile: bool,
        /// Show full compiler output
        #[arg(long, short = 'v')]
        verbose: bool,
        /// Clean build outputs before building
        #[arg(long)]
        clean: bool,
        /// Custom output directory (default: out/classes)
        #[arg(long, short = 'o')]
        output: Option<String>,
        /// Continue building other modules on failure
        #[arg(long)]
        keep_going: bool,
        /// Treat warnings as errors (-Werror)
        #[arg(long)]
        strict: bool,
    },
    /// Compile, run, and watch for changes (development mode)
    Dev {
        /// Target module name (workspace mode)
        target: Option<String>,
        /// Disable hot reload (always restart on change)
        #[arg(long)]
        no_reload: bool,
        /// Enable JDWP remote debugging (default port 5005)
        #[arg(long)]
        debug: bool,
        /// Custom debug port (default: 5005)
        #[arg(long)]
        debug_port: Option<u16>,
        /// Suspend JVM until debugger attaches
        #[arg(long)]
        suspend: bool,
        /// JVM arguments (after --)
        #[arg(last = true)]
        args: Vec<String>,
    },
    /// Run tests
    Test {
        /// Target module name (workspace mode)
        target: Option<String>,
        /// Watch mode: re-run affected tests on change
        #[arg(long)]
        watch: bool,
        /// Filter pattern for test classes/methods
        #[arg(long)]
        filter: Option<String>,
        /// Run integration tests only (*IT.java, *IntegrationTest.java)
        #[arg(long)]
        integration: bool,
        /// Run all tests (unit + integration)
        #[arg(long)]
        all: bool,
        /// Run only tests with this JUnit @Tag
        #[arg(long)]
        tag: Option<String>,
        /// Exclude tests with this JUnit @Tag
        #[arg(long)]
        exclude_tag: Option<String>,
        /// Show verbose test output
        #[arg(long, short = 'v')]
        verbose: bool,
        /// Stop on first test failure
        #[arg(long)]
        fail_fast: bool,
        /// Timeout per test class in seconds
        #[arg(long)]
        timeout: Option<u64>,
        /// Generate code coverage report (JaCoCo)
        #[arg(long)]
        coverage: bool,
        /// List test classes without running them
        #[arg(long)]
        list: bool,
        /// Continue running other tests on failure
        #[arg(long)]
        keep_going: bool,
        /// Generate test report: junit-xml or html
        #[arg(long)]
        report: Option<String>,
        /// Run tests in parallel (JUnit 5 parallel execution)
        #[arg(long)]
        parallel: bool,
    },
    /// Clean build outputs and caches
    Clean {
        /// Also remove Maven dependency cache
        #[arg(long)]
        all: bool,
        /// Skip confirmation
        #[arg(long, short = 'y')]
        yes: bool,
    },
    /// Generate IntelliJ IDEA project files
    Idea {
        /// Target module name
        target: Option<String>,
        /// Download source JARs for dependencies
        #[arg(long)]
        sources: bool,
        /// Output structured JSON project model (for IDE integration)
        #[arg(long)]
        json: bool,
    },
    /// Generate VSCode settings for Java development
    Vscode {
        /// Target module name (workspace mode)
        target: Option<String>,
    },
    /// Compile to native binary using GraalVM native-image
    Native {
        /// Use Docker container (no local GraalVM needed)
        #[arg(long)]
        docker: bool,
        /// Custom output binary name (default: project name)
        #[arg(long)]
        out: Option<String>,
        /// Target platform for Docker (e.g., linux/amd64, linux/arm64)
        #[arg(long)]
        platform: Option<String>,
        /// Install the native binary to ~/.ym/bin/ after building
        #[arg(long)]
        install: bool,
    },
}

// ============================================================
//  Public entry points
// ============================================================

pub fn run_ym() {
    run_result(ym_main());
}

pub fn run_ymc() {
    run_result(ymc_main());
}

fn run_result(result: Result<()>) {
    if let Err(e) = result {
        eprintln!("  {} {}", console::style("✗").red(), e);
        std::process::exit(1);
    }
}

// ============================================================
//  YM dispatch
// ============================================================

fn ym_main() -> Result<()> {
    let cli = YmCli::parse();
    apply_color_mode(&cli.color);
    match cli.command {
        YmCommands::Init { name, interactive, template, yes } => {
            commands::init::execute(name, interactive, template, yes)
        }
        YmCommands::Add { dep, coord, scope, classifier } => {
            let dep = match dep {
                Some(d) => d,
                None => return commands::add::execute_interactive(),
            };
            // Support: ym add implementation org.projectlombok:lombok:1.18.42
            let (dep, scope) = if let Some(coord) = coord {
                let ym_scope = match dep.as_str() {
                    "implementation" | "api" | "annotationProcessor" => scope,
                    "compileOnly" => scope.or(Some("provided".into())),
                    "runtimeOnly" => scope.or(Some("runtime".into())),
                    "testImplementation" | "testRuntimeOnly" | "testCompileOnly" => scope.or(Some("test".into())),
                    other => anyhow::bail!(
                        "Unknown Gradle scope '{}'. Use: ym add groupId:artifactId@version",
                        other
                    ),
                };
                (coord, ym_scope)
            } else {
                (dep, scope)
            };
            commands::add::execute(&dep, scope.as_deref(), classifier.as_deref())
        }
        YmCommands::Remove { dep } => commands::remove::execute(&dep),
        YmCommands::Upgrade { interactive, yes, json } => {
            commands::upgrade::execute(interactive, yes, json)
        }
        YmCommands::Convert { verify } => commands::migrate::execute(verify),
        YmCommands::Publish { target, registry, dry_run } => {
            commands::publish::execute(target, registry.as_deref(), dry_run)
        }
        YmCommands::Login { list, remove } => {
            commands::login::execute(list, remove.as_deref())
        }
        YmCommands::Info { json } => commands::info::execute(json),
        YmCommands::Tree { depth, json, flat, dot, reverse } => {
            commands::tree::execute(depth, json, flat, dot, reverse.as_deref())
        }
        YmCommands::Doctor { fix } => commands::doctor::execute(fix),
        YmCommands::Run { scripts: script_names, parallel } => {
            run_scripts(&script_names, parallel)
        }
        YmCommands::Workspace { action } => match action {
            WorkspaceAction::List => commands::workspace_cmd::list(),
            WorkspaceAction::Foreach { args, parallel, jobs, keep_going } => {
                commands::workspace_cmd::foreach(args, parallel, jobs, keep_going)
            }
        },
        YmCommands::Completions { shell } => {
            let mut cmd = YmCli::command();
            generate(shell, &mut cmd, "ym", &mut std::io::stdout());
            Ok(())
        }
        YmCommands::External(args) => run_script_external(&args),
    }
}

/// Simple Levenshtein distance for command suggestions.
fn strsim_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let (m, n) = (a.len(), b.len());
    let mut dp = vec![vec![0usize; n + 1]; m + 1];
    for i in 0..=m { dp[i][0] = i; }
    for j in 0..=n { dp[0][j] = j; }
    for i in 1..=m {
        for j in 1..=n {
            let cost = if a[i - 1] == b[j - 1] { 0 } else { 1 };
            dp[i][j] = (dp[i - 1][j] + 1)
                .min(dp[i][j - 1] + 1)
                .min(dp[i - 1][j - 1] + cost);
        }
    }
    dp[m][n]
}

/// `ym <unknown>` — try running as a script
fn run_script_external(args: &[String]) -> Result<()> {
    let name = args
        .first()
        .cloned()
        .unwrap_or_default();

    if name.is_empty() {
        anyhow::bail!("No script name provided");
    }

    let (config_path, cfg) = config::load_or_find_config()?;
    let project = config::project_dir(&config_path);

    let script_map = cfg.scripts.as_ref();
    match script_map.and_then(|s| s.get(&name)) {
        Some(_) => {
            let extra_args: Vec<String> = args.iter().skip(1).cloned().collect();
            scripts::run_script_with_args(&cfg, &name, &project, &extra_args)
        }
        None => {
            // Fallback: delegate known ymc commands so they work without scripts
            let ymc_commands = ["build", "dev", "test", "idea", "clean", "vscode", "native"];
            if ymc_commands.contains(&name.as_str()) {
                let mut ymc_args = vec![name.clone()];
                ymc_args.extend(args.iter().skip(1).cloned());
                return dispatch_ymc_args(&ymc_args);
            }

            let mut msg = format!(
                "Unknown command '{}'. Not a built-in command or script.",
                name
            );
            // Suggest similar script names
            if let Some(scripts) = script_map {
                let similar: Vec<&String> = scripts.keys()
                    .filter(|k| {
                        k.contains(&name) || name.contains(k.as_str())
                            || strsim_distance(k, &name) <= 2
                    })
                    .collect();
                if !similar.is_empty() {
                    msg.push_str("\n\nDid you mean?");
                    for s in similar {
                        msg.push_str(&format!("\n  ym {}", s));
                    }
                }
            }
            msg.push_str("\n\nRun 'ym --help' for available commands.");
            anyhow::bail!("{}", msg);
        }
    }
}

/// `ym run script1 script2 [--parallel]` — run one or more scripts
fn run_scripts(script_names: &[String], parallel: bool) -> Result<()> {
    if script_names.is_empty() {
        anyhow::bail!("No script names provided. Usage: ym run <script1> [script2 ...] [--parallel]");
    }

    let (config_path, cfg) = config::load_or_find_config()?;
    let project = config::project_dir(&config_path);

    // Validate all script names exist
    let script_map = cfg.scripts.as_ref();
    for name in script_names {
        if script_map.and_then(|s| s.get(name)).is_none() {
            anyhow::bail!("Script '{}' not found in [scripts]", name);
        }
    }

    if parallel && script_names.len() > 1 {
        use console::style;
        println!(
            "  {} running {} scripts in parallel...",
            style("➜").green(),
            script_names.len()
        );

        let handles: Vec<_> = script_names
            .iter()
            .map(|name| {
                let name = name.clone();
                let cfg = cfg.clone();
                let project = project.clone();
                std::thread::spawn(move || {
                    scripts::run_script_with_args(&cfg, &name, &project, &[])
                })
            })
            .collect();

        let mut errors = Vec::new();
        for (i, handle) in handles.into_iter().enumerate() {
            match handle.join() {
                Ok(Ok(())) => {}
                Ok(Err(e)) => errors.push(format!("{}: {}", script_names[i], e)),
                Err(_) => errors.push(format!("{}: thread panicked", script_names[i])),
            }
        }

        if !errors.is_empty() {
            anyhow::bail!("Scripts failed:\n  {}", errors.join("\n  "));
        }
    } else {
        // Sequential execution
        for name in script_names {
            scripts::run_script_with_args(&cfg, name, &project, &[])?;
        }
    }

    Ok(())
}

// ============================================================
//  YMC dispatch
// ============================================================

/// Dispatch ymc sub-commands from string args (used by ym fallback).
fn dispatch_ymc_args(args: &[String]) -> Result<()> {
    let mut full_args = vec!["ymc".to_string()];
    full_args.extend(args.iter().cloned());
    let cli = YmcCli::try_parse_from(&full_args)
        .map_err(|e| anyhow::anyhow!("{}", e))?;
    dispatch_ymc(cli)
}

fn print_version_banner(context: &str) {
    let version = env!("CARGO_PKG_VERSION");
    println!(
        "\n  {}  {}\n",
        console::style(format!("ym v{}", version)).green().bold(),
        console::style(context).green(),
    );
}

fn dispatch_ymc(cli: YmcCli) -> Result<()> {
    apply_color_mode(&cli.color);

    // Print version banner (skip for JSON output modes)
    let is_json = matches!(&cli.command, YmcCommands::Idea { json: true, .. });
    if !is_json && !is_json_quiet() {
        let context = match &cli.command {
            YmcCommands::Build { .. } => "building...",
            YmcCommands::Dev { .. } => "dev server starting...",
            YmcCommands::Test { .. } => "running tests...",
            YmcCommands::Clean { .. } => "cleaning...",
            YmcCommands::Idea { .. } => "generating IDEA project...",
            YmcCommands::Vscode { .. } => "generating VSCode settings...",
            YmcCommands::Native { .. } => "compiling native binary...",
        };
        print_version_banner(context);
    }

    match cli.command {
        YmcCommands::Build { target, parallel, profile, verbose, clean, output, keep_going, strict } => {
            if let Some(n) = parallel {
                commands::build::set_parallelism(n);
            }
            if clean {
                commands::clean::execute(false, true)?;
            }
            if let Some(ref out) = output {
                commands::build::set_output_dir(out);
            }
            if verbose {
                commands::build::set_verbose(true);
            }
            if strict {
                commands::build::set_strict(true);
            }
            if profile {
                commands::build::execute_with_profile(target)
            } else if keep_going {
                commands::build::execute_keep_going(target)
            } else {
                commands::build::execute(target)
            }
        }
        YmcCommands::Dev { target, no_reload, debug, debug_port, suspend, args } => {
            commands::dev::execute(target, no_reload, debug, debug_port, suspend, args)
        }
        YmcCommands::Test {
            target, watch, filter, integration, all, tag, exclude_tag,
            verbose, fail_fast, timeout, coverage, list, keep_going, report, parallel,
        } => {
            commands::test_cmd::execute(
                target, watch, filter, integration, all, tag, exclude_tag,
                verbose, fail_fast, timeout, coverage, list, keep_going, report, parallel,
            )
        }
        YmcCommands::Clean { all, yes } => commands::clean::execute(all, yes),
        YmcCommands::Idea { target, sources, json } => commands::idea::execute(target, sources, json),
        YmcCommands::Vscode { target } => commands::vscode::execute(target),
        YmcCommands::Native { docker, out, platform, install } => commands::native_cmd::execute(docker, out, platform, install),
    }
}

fn ymc_main() -> Result<()> {
    let cli = YmcCli::parse();
    dispatch_ymc(cli)
}
