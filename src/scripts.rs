use anyhow::Result;
use console::style;
use std::collections::BTreeMap;
use std::path::Path;

use crate::config::schema::ScriptValue;

/// Execute a named script from [scripts] in package.toml.
///
/// For lifecycle hooks (pre*/post*), uses YM_LIFECYCLE=1 to prevent
/// recursive triggering. When a hook script calls ymc again, the
/// child process inherits YM_LIFECYCLE=1, so its hooks are skipped.
pub fn run_script(
    scripts: &Option<BTreeMap<String, ScriptValue>>,
    env: &Option<BTreeMap<String, String>>,
    name: &str,
    project_dir: &Path,
) -> Result<()> {
    run_script_with_args(scripts, env, name, project_dir, &[])
}

/// Execute a named script, appending extra_args (from `--` separator) to the command.
pub fn run_script_with_args(
    scripts: &Option<BTreeMap<String, ScriptValue>>,
    env: &Option<BTreeMap<String, String>>,
    name: &str,
    project_dir: &Path,
    extra_args: &[String],
) -> Result<()> {
    // Prevent recursive hook triggering: if we're already inside a
    // lifecycle chain (YM_LIFECYCLE=1), skip pre*/post* hooks
    let is_hook = name.starts_with("pre") || name.starts_with("post");
    if is_hook && std::env::var("YM_LIFECYCLE").is_ok() {
        return Ok(());
    }

    let scripts = match scripts {
        Some(s) => s,
        None => return Ok(()),
    };

    let script_value = match scripts.get(name) {
        Some(v) => v,
        None => return Ok(()),
    };

    let cmd = script_value.command();
    let timeout_secs = script_value.timeout_secs();

    println!(
        "  {} Running script: {}",
        style("→").blue(),
        style(name).dim()
    );

    let shell = if cfg!(windows) { "cmd" } else { "sh" };
    let flag = if cfg!(windows) { "/C" } else { "-c" };

    // Append extra_args (from `--` separator) to the script command
    let full_cmd = if extra_args.is_empty() {
        cmd.to_string()
    } else {
        let escaped: Vec<String> = extra_args.iter().map(|a| {
            if a.contains(' ') {
                format!("\"{}\"", a)
            } else {
                a.clone()
            }
        }).collect();
        format!("{} {}", cmd, escaped.join(" "))
    };

    let mut command = std::process::Command::new(shell);
    command.arg(flag).arg(&full_cmd).current_dir(project_dir);

    // For lifecycle hooks (pre*/post*), set YM_LIFECYCLE=1 to prevent
    // nested hooks from re-triggering if the script calls ymc again
    if is_hook {
        command.env("YM_LIFECYCLE", "1");
    }

    if let Some(env_map) = env {
        let home = std::env::var("HOME").unwrap_or_default();
        for (k, v) in env_map {
            let expanded = if v.starts_with("~/") {
                format!("{}{}", home, &v[1..])
            } else {
                v.clone()
            };
            command.env(k, expanded);
        }
    }

    let status = if let Some(secs) = timeout_secs {
        // Spawn and poll with timeout
        let mut child = command.spawn()?;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(secs);
        loop {
            match child.try_wait()? {
                Some(s) => break s,
                None => {
                    if std::time::Instant::now() >= deadline {
                        let _ = child.kill();
                        let _ = child.wait();
                        anyhow::bail!("Script '{}' timed out after {}s", name, secs);
                    }
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
            }
        }
    } else {
        command.status()?
    };

    if !status.success() {
        if name.starts_with("post") {
            // Post-hooks: warn and continue (don't block main command)
            eprintln!(
                "  {} Post-hook '{}' failed with exit code {:?}",
                style("!").yellow(),
                name,
                status.code()
            );
        } else {
            // Pre-hooks and user scripts: fail
            anyhow::bail!("Script '{}' failed with exit code {:?}", name, status.code());
        }
    }

    Ok(())
}
