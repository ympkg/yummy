use anyhow::Result;
use std::process::Command;

use crate::config;

/// Run an arbitrary command with the project's classpath set.
/// Example: `ym exec java -cp {classpath} com.example.Tool`
/// Or:      `ym exec -- javap -c MyClass`
pub fn execute(args: Vec<String>) -> Result<()> {
    let (config_path, cfg) = config::load_or_find_config()?;
    let project = config::project_dir(&config_path);

    // Build classpath
    let jars = super::build::resolve_deps(&project, &cfg)?;
    let out_dir = config::output_classes_dir(&project);
    let mut classpath = vec![out_dir];
    classpath.extend(jars);

    let sep = if cfg!(windows) { ";" } else { ":" };
    let cp = classpath
        .iter()
        .map(|p| p.to_string_lossy().to_string())
        .collect::<Vec<_>>()
        .join(sep);

    if args.is_empty() {
        // No command given — just print the classpath
        println!("{}", cp);
        return Ok(());
    }

    let cmd_name = &args[0];
    let cmd_args: Vec<String> = args[1..]
        .iter()
        .map(|a| a.replace("{classpath}", &cp))
        .collect();

    // Set CLASSPATH env var
    let status = Command::new(cmd_name)
        .args(&cmd_args)
        .env("CLASSPATH", &cp)
        .status()?;

    if !status.success() {
        std::process::exit(status.code().unwrap_or(1));
    }

    Ok(())
}
