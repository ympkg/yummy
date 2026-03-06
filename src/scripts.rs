use anyhow::Result;
use console::style;
use std::collections::BTreeMap;
use std::path::Path;

/// Execute a lifecycle script if defined in ym.json scripts.
///
/// Supported hooks:
///   preinit, postinit
///   prebuild, postbuild
///   predev, postdev
///   pretest, posttest
///   prepublish, postpublish
pub fn run_script(
    scripts: &Option<BTreeMap<String, String>>,
    env: &Option<BTreeMap<String, String>>,
    name: &str,
    project_dir: &Path,
) -> Result<()> {
    let scripts = match scripts {
        Some(s) => s,
        None => return Ok(()),
    };

    let cmd = match scripts.get(name) {
        Some(c) => c,
        None => return Ok(()),
    };

    println!(
        "  {} Running script: {}",
        style("→").blue(),
        style(name).dim()
    );

    let shell = if cfg!(windows) { "cmd" } else { "sh" };
    let flag = if cfg!(windows) { "/C" } else { "-c" };

    let mut command = std::process::Command::new(shell);
    command.arg(flag).arg(cmd).current_dir(project_dir);

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

    let status = command.status()?;

    if !status.success() {
        anyhow::bail!("Script '{}' failed with exit code {:?}", name, status.code());
    }

    Ok(())
}
