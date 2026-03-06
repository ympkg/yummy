use anyhow::Result;
use clap::{CommandFactory, Parser, Subcommand};
use clap_complete::{generate, Shell};

mod commands;
mod compiler;
mod config;
mod hotreload;
mod jdk_manager;
mod jvm;
mod resources;
mod scripts;
mod watcher;
mod workspace;

// ============================================================
//  YM — Package manager & script runner  (like yarn)
// ============================================================

#[derive(Parser)]
#[command(name = "ym", about = "Yummy - A modern Java package manager", version)]
struct YmCli {
    #[command(subcommand)]
    command: YmCommands,
}

#[derive(Subcommand)]
enum YmCommands {
    /// Initialize a new project
    Init {
        /// Project name (creates new directory if provided)
        name: Option<String>,
        /// Project template: app (default), lib, spring, cli
        #[arg(long, short = 't')]
        template: Option<String>,
        /// Skip all prompts and use defaults
        #[arg(long, short = 'y')]
        yes: bool,
    },
    /// Install all dependencies from ym.json
    Install,
    /// Add a dependency
    Add {
        /// Dependency (e.g., guava or com.google.guava:guava@33.0)
        dep: String,
        /// Add as dev dependency
        #[arg(long, short = 'D')]
        dev: bool,
        /// Add as workspace dependency (by module name)
        #[arg(long, short = 'W')]
        workspace: bool,
    },
    /// Remove a dependency
    Remove {
        /// Dependency coordinate (groupId:artifactId)
        dep: String,
        /// Remove from devDependencies
        #[arg(long, short = 'D')]
        dev: bool,
    },
    /// Convert from Maven/Gradle to ym.json (auto-detects pom.xml or build.gradle)
    Convert,
    /// Migrate from Maven/Gradle to ym.json (alias for convert)
    #[command(hide = true)]
    Migrate,
    /// Publish to a Maven repository
    Publish {
        /// Simulate publish without uploading
        #[arg(long)]
        dry_run: bool,
    },
    /// Login to a Maven repository
    Login,
    /// Link a local package for cross-project development
    Link {
        /// Package name to link (omit to register current package)
        target: Option<String>,
        /// List all linked packages
        #[arg(long)]
        list: bool,
        /// Unlink a previously linked package
        #[arg(long)]
        unlink: bool,
    },
    /// Workspace commands
    Workspace {
        #[command(subcommand)]
        action: WorkspaceAction,
    },
    /// Regenerate lock file from scratch
    Lock {
        /// Check if lock file is up-to-date (exit 1 if not, for CI)
        #[arg(long)]
        check: bool,
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
    },
    /// Check for outdated dependencies
    Outdated {
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
    /// Upgrade dependencies to latest versions
    Upgrade {
        /// Interactively select which dependencies to upgrade
        #[arg(long, short = 'i')]
        interactive: bool,
    },
    /// Show project info
    Info {
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
    /// Check dependencies for known vulnerabilities
    Audit {
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
    /// Verify cached dependency integrity against lock file
    Verify,
    /// Explain why a dependency is included
    Why {
        /// Dependency name or partial match
        dep: String,
    },
    /// Search Maven Central for packages
    Search {
        /// Search query
        query: String,
        /// Max results to show
        #[arg(long, default_value = "10")]
        limit: usize,
    },
    /// List all resolved dependencies (flat)
    Deps {
        /// Output as JSON
        #[arg(long)]
        json: bool,
        /// Show only outdated dependencies
        #[arg(long)]
        outdated: bool,
    },
    /// View or set ym.json configuration
    Config {
        /// Config key (e.g., "java", "compiler.engine")
        key: Option<String>,
        /// Value to set (omit to read)
        value: Option<String>,
    },
    /// Download source JARs for all dependencies
    Sources,
    /// Validate ym.json configuration
    Validate,
    /// Pin a dependency version (prevent ym upgrade from changing it)
    Pin {
        /// Dependency coordinate (groupId:artifactId)
        dep: String,
        /// Unpin instead
        #[arg(long)]
        unpin: bool,
    },
    /// Deduplicate dependency versions in lock file
    Dedupe {
        /// Show what would change without modifying
        #[arg(long)]
        dry_run: bool,
    },
    /// Manage the dependency cache
    Cache {
        #[command(subcommand)]
        action: CacheAction,
    },
    /// Show environment and tool info
    Env,
    /// Diagnose environment issues
    Doctor {
        /// Attempt to auto-fix common issues
        #[arg(long)]
        fix: bool,
    },
    /// Check dependency licenses
    License {
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
    /// Generate shell completions
    Completions {
        /// Shell type (bash, zsh, fish, powershell)
        shell: Shell,
    },
    /// Run a named script from ym.json
    Run {
        /// Script name
        name: String,
        /// Arguments to pass to the script (after --)
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Catch-all: run as script
    #[command(external_subcommand)]
    External(Vec<String>),
}

#[derive(Subcommand)]
enum WorkspaceAction {
    /// List all packages
    List,
    /// Show dependency graph
    Graph,
    /// List changed packages since last commit
    Changed,
    /// Run a command in each package
    Foreach {
        /// Command and arguments
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
        /// Run in parallel across packages
        #[arg(long)]
        parallel: bool,
    },
    /// Show which packages are affected by changes to a module
    Impact {
        /// Module name to analyze
        target: String,
    },
    /// Build workspace packages
    Build {
        /// Target module name (omit for all)
        target: Option<String>,
        /// Build release jar
        #[arg(long)]
        release: bool,
    },
    /// Run a workspace module
    Run {
        /// Target module name
        target: String,
        /// Specify the main class to run
        #[arg(long)]
        class: Option<String>,
        /// Arguments to pass to the Java program (after --)
        #[arg(last = true)]
        args: Vec<String>,
    },
    /// Test a workspace module
    Test {
        /// Target module name
        target: String,
        /// Filter pattern
        #[arg(long)]
        filter: Option<String>,
        /// Show verbose test output
        #[arg(long, short = 'v')]
        verbose: bool,
    },
    /// Clean all workspace module outputs
    Clean,
    /// Show workspace summary info
    Info,
    /// Show full dependency details for a module
    Focus {
        /// Module name
        target: String,
    },
}

#[derive(Subcommand)]
enum CacheAction {
    /// Show cache size and contents
    List,
    /// Remove cached artifacts
    Clean {
        /// Only remove Maven artifacts (keep fingerprints)
        #[arg(long)]
        maven_only: bool,
    },
}

// ============================================================
//  YMC — Java compiler & runtime  (like vite)
// ============================================================

#[derive(Parser)]
#[command(name = "ymc", about = "Yummy - Java compiler and runtime", version)]
struct YmcCli {
    #[command(subcommand)]
    command: YmcCommands,
}

#[derive(Subcommand)]
enum YmcCommands {
    /// Compile the project
    Build {
        /// Target module name
        target: Option<String>,
        /// Build release jar
        #[arg(long)]
        release: bool,
        /// Watch for changes and rebuild automatically
        #[arg(long)]
        watch: bool,
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
    },
    /// Compile, run, and watch for changes
    Dev {
        /// Target module name
        target: Option<String>,
        /// Disable hot reload (always restart on change)
        #[arg(long)]
        no_reload: bool,
        /// Arguments to pass to the Java program (after --)
        #[arg(last = true)]
        args: Vec<String>,
    },
    /// Compile and run (no watch)
    Run {
        /// Target module name
        target: Option<String>,
        /// Specify the main class to run
        #[arg(long)]
        class: Option<String>,
        /// Enable remote debugging (JDWP on port 5005)
        #[arg(long)]
        debug: bool,
        /// Custom debug port (default: 5005)
        #[arg(long)]
        debug_port: Option<u16>,
        /// Suspend JVM until debugger attaches (use with --debug)
        #[arg(long)]
        suspend: bool,
        /// Arguments to pass to the Java program (after --)
        #[arg(last = true)]
        args: Vec<String>,
    },
    /// Run tests
    Test {
        /// Target module name
        target: Option<String>,
        /// Watch mode
        #[arg(long)]
        watch: bool,
        /// Filter pattern
        #[arg(long)]
        filter: Option<String>,
        /// Show verbose test output
        #[arg(long, short = 'v')]
        verbose: bool,
        /// Stop on first test failure
        #[arg(long)]
        fail_fast: bool,
        /// Timeout per test class in seconds (default: no timeout)
        #[arg(long)]
        timeout: Option<u64>,
        /// Generate code coverage report (JaCoCo)
        #[arg(long)]
        coverage: bool,
        /// List test classes without running them
        #[arg(long)]
        list: bool,
    },
    /// Check compilation without running
    Check {
        /// Target module name
        target: Option<String>,
        /// Treat warnings as errors (-Werror)
        #[arg(long)]
        strict: bool,
    },
    /// Clean build outputs and caches
    Clean {
        /// Also remove Maven dependency cache
        #[arg(long)]
        all: bool,
    },
    /// Format Java source files
    Fmt {
        /// Check formatting without modifying files
        #[arg(long)]
        check: bool,
        /// Show diff of what would change
        #[arg(long)]
        diff: bool,
    },
    /// Generate Javadoc documentation
    Doc {
        /// Target module name
        target: Option<String>,
        /// Open docs in browser after generation
        #[arg(long)]
        open: bool,
    },
    /// Run JMH benchmarks
    Bench {
        /// Target module name
        target: Option<String>,
        /// Filter pattern for benchmark methods
        #[arg(long)]
        filter: Option<String>,
    },
    /// Create a JAR file (without dependencies)
    Jar {
        /// Target module name
        target: Option<String>,
    },
    /// Run a command with the project classpath
    Exec {
        /// Command and arguments (use {classpath} to insert classpath)
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Print the project classpath
    Classpath {
        /// Target module name
        target: Option<String>,
    },
    /// Compute project content hash (for CI cache keys)
    Hash {
        /// Target module name
        target: Option<String>,
    },
    /// Show changed files since last build
    Diff {
        /// Target module name
        target: Option<String>,
    },
    /// Analyze project sizes (sources, classes, deps)
    Size {
        /// Target module name
        target: Option<String>,
    },
    /// Show dependency graph (text or DOT format)
    Graph {
        /// Target module name
        target: Option<String>,
        /// Output as Graphviz DOT
        #[arg(long)]
        dot: bool,
        /// Show reverse dependencies (who depends on what)
        #[arg(long)]
        reverse: bool,
        /// Max transitive dependency depth (0 = unlimited)
        #[arg(long, default_value = "0")]
        depth: usize,
    },
    /// Watch files and run a command on change
    Watch {
        /// Command to run when files change
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
        /// File extensions to watch (comma-separated, default: .java)
        #[arg(long, default_value = ".java")]
        ext: String,
    },
    /// Clean and rebuild from scratch
    Rebuild {
        /// Target module name
        target: Option<String>,
        /// Build release jar
        #[arg(long)]
        release: bool,
    },
    /// Generate IntelliJ IDEA project files
    Idea {
        /// Target module name
        target: Option<String>,
        /// Download source JARs for dependencies
        #[arg(long)]
        sources: bool,
    },
    /// Create a new module in a workspace
    Create {
        /// Module name
        name: String,
        /// Template: app (default) or lib
        #[arg(long, short = 't')]
        template: Option<String>,
        /// Include template-specific starter dependencies
        #[arg(long)]
        deps: bool,
    },
}

// ============================================================
//  Entry point — dispatch by binary name
// ============================================================

fn main() -> Result<()> {
    let exe_name = std::env::args()
        .next()
        .and_then(|arg| {
            std::path::Path::new(&arg)
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
        })
        .unwrap_or_else(|| "ym".to_string());

    if exe_name == "ymc" {
        ymc_main()
    } else {
        ym_main()
    }
}

// ============================================================
//  YM dispatch
// ============================================================

fn ym_main() -> Result<()> {
    let cli = YmCli::parse();
    match cli.command {
        YmCommands::Init { name, template, yes } => commands::init::execute_with_options(name, template, yes),
        YmCommands::Install => commands::install::execute(),
        YmCommands::Add { dep, dev, workspace } => commands::add::execute(&dep, dev, workspace),
        YmCommands::Remove { dep, dev } => commands::remove::execute(&dep, dev),
        YmCommands::Convert => commands::migrate::execute(),
        YmCommands::Migrate => commands::migrate::execute(),
        YmCommands::Publish { dry_run } => commands::publish::execute(dry_run),
        YmCommands::Login => commands::login::execute(),
        YmCommands::Link { target, list, unlink } => commands::link::execute(target, list, unlink),
        YmCommands::Workspace { action } => match action {
            WorkspaceAction::List => commands::workspace_cmd::list(),
            WorkspaceAction::Graph => commands::workspace_cmd::graph(),
            WorkspaceAction::Changed => commands::workspace_cmd::changed(),
            WorkspaceAction::Foreach { args, parallel } => commands::workspace_cmd::foreach(args, parallel),
            WorkspaceAction::Impact { target } => commands::workspace_cmd::impact(&target),
            WorkspaceAction::Build { target, release } => commands::build::execute(target, release),
            WorkspaceAction::Run { target, class, args } => {
                commands::run::execute(Some(target), class, false, None, false, args)
            }
            WorkspaceAction::Test { target, filter, verbose } => {
                commands::test_cmd::execute(Some(target), false, filter, verbose, false, None, false, false)
            }
            WorkspaceAction::Clean => commands::workspace_cmd::clean_all(),
            WorkspaceAction::Info => commands::workspace_cmd::info(),
            WorkspaceAction::Focus { target } => commands::workspace_cmd::focus(&target),
        },
        YmCommands::Lock { check } => commands::lock::execute(check),
        YmCommands::Tree { depth, json, flat } => commands::tree::execute(depth, json, flat),
        YmCommands::Outdated { json } => commands::outdated::execute(json),
        YmCommands::Upgrade { interactive } => commands::upgrade::execute(interactive),
        YmCommands::Info { json } => commands::info::execute(json),
        YmCommands::Audit { json } => commands::audit::execute(json),
        YmCommands::Verify => commands::verify::execute(),
        YmCommands::Why { dep } => commands::why::execute(&dep),
        YmCommands::Search { query, limit } => commands::search::execute(&query, limit),
        YmCommands::Deps { json, outdated } => commands::deps::execute(json, outdated),
        YmCommands::Config { key, value } => commands::config_cmd::execute(key, value),
        YmCommands::Sources => commands::sources::execute(),
        YmCommands::Validate => commands::validate::execute(),
        YmCommands::Pin { dep, unpin } => commands::pin::execute(&dep, unpin),
        YmCommands::Dedupe { dry_run } => commands::dedupe::execute(dry_run),
        YmCommands::Cache { action } => match action {
            CacheAction::List => commands::cache::list(),
            CacheAction::Clean { maven_only } => commands::cache::clean(maven_only),
        },
        YmCommands::Env => commands::env::execute(),
        YmCommands::Doctor { fix } => commands::doctor::execute(fix),
        YmCommands::License { json } => commands::license::execute(json),
        YmCommands::Completions { shell } => {
            let mut cmd = YmCli::command();
            generate(shell, &mut cmd, "ym", &mut std::io::stdout());
            Ok(())
        }
        YmCommands::Run { name, .. } => commands::script::execute(Some(name)),
        YmCommands::External(args) => run_script_external(&args),
    }
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
        Some(_) => scripts::run_script(&cfg.scripts, &cfg.env, &name, &project),
        None => anyhow::bail!(
            "Unknown command '{}'. Not a built-in command or script.\nRun 'ym --help' for available commands.",
            name
        ),
    }
}

// ============================================================
//  YMC dispatch
// ============================================================

fn ymc_main() -> Result<()> {
    let cli = YmcCli::parse();
    match cli.command {
        YmcCommands::Build { target, release, watch, parallel, profile, verbose, clean, output } => {
            if let Some(n) = parallel {
                commands::build::set_parallelism(n);
            }
            if clean {
                commands::clean::execute(false)?;
            }
            if let Some(ref out) = output {
                commands::build::set_output_dir(out);
            }
            if verbose {
                commands::build::set_verbose(true);
            }
            if profile {
                commands::build::execute_with_profile(target.clone(), release)?;
            } else {
                commands::build::execute(target.clone(), release)?;
            }
            if watch {
                commands::build::watch_loop(target, release)?;
            }
            Ok(())
        }
        YmcCommands::Dev { target, no_reload, args } => commands::dev::execute_with_options(target, no_reload, args),
        YmcCommands::Run { target, class, debug, debug_port, suspend, args } => {
            commands::run::execute(target, class, debug, debug_port, suspend, args)
        }
        YmcCommands::Test { target, watch, filter, verbose, fail_fast, timeout, coverage, list } => {
            commands::test_cmd::execute(target, watch, filter, verbose, fail_fast, timeout, coverage, list)
        }
        YmcCommands::Check { target, strict } => commands::check::execute(target, strict),
        YmcCommands::Clean { all } => commands::clean::execute(all),
        YmcCommands::Fmt { check, diff } => commands::fmt::execute(check, diff),
        YmcCommands::Doc { target, open } => commands::doc::execute(target, open),
        YmcCommands::Bench { target, filter } => commands::bench::execute(target, filter),
        YmcCommands::Jar { target } => commands::jar::execute(target),
        YmcCommands::Exec { args } => commands::exec::execute(args),
        YmcCommands::Classpath { target } => commands::classpath::execute(target),
        YmcCommands::Hash { target } => commands::hash::execute(target),
        YmcCommands::Diff { target } => commands::diff::execute(target),
        YmcCommands::Size { target } => commands::size::execute(target),
        YmcCommands::Graph { target, dot, reverse, depth } => commands::graph::execute(target, dot, reverse, depth),
        YmcCommands::Watch { args, ext } => commands::watch::execute(args, ext),
        YmcCommands::Rebuild { target, release } => commands::rebuild::execute(target, release),
        YmcCommands::Idea { target, sources } => commands::idea::execute(target, sources),
        YmcCommands::Create { name, template, deps } => commands::create::execute(name, template, deps),
    }
}
