use anyhow::{bail, Result};
use console::style;
use dialoguer::Select;

use crate::config;
use crate::workspace::resolver;

pub fn execute(dep: &str, dev: bool, workspace: bool) -> Result<()> {
    let (config_path, mut cfg) = config::load_or_find_config()?;

    // Workspace dependency: add module name to workspaceDependencies
    if workspace {
        let ws_deps = cfg.workspace_dependencies.get_or_insert_with(Vec::new);
        if ws_deps.contains(&dep.to_string()) {
            println!(
                "  {} {} is already in workspaceDependencies",
                style("!").yellow(),
                dep
            );
            return Ok(());
        }
        ws_deps.push(dep.to_string());
        config::save_config(&config_path, &cfg)?;
        println!(
            "  {} Added workspace dependency {}",
            style("✓").green(),
            style(dep).cyan()
        );
        return Ok(());
    }

    // Parse the dependency specification
    // Formats:
    //   com.google.guava:guava@33.0.0-jre  (full with version)
    //   com.google.guava:guava             (full, fetch latest)
    //   guava                              (fuzzy search)
    let (group_id, artifact_id, version) = parse_dep_spec(dep)?;

    let coord = format!("{}:{}", group_id, artifact_id);

    let deps = if dev {
        cfg.dev_dependencies.get_or_insert_with(Default::default)
    } else {
        cfg.dependencies.get_or_insert_with(Default::default)
    };

    if deps.contains_key(&coord) {
        let label = if dev { "devDependencies" } else { "dependencies" };
        println!(
            "  {} {} is already in {} (version {})",
            style("!").yellow(),
            coord,
            label,
            deps[&coord]
        );
        return Ok(());
    }

    deps.insert(coord.clone(), version.clone());
    config::save_config(&config_path, &cfg)?;

    println!(
        "  {} Added {} {}",
        style("✓").green(),
        style(&coord).cyan(),
        style(&version).dim()
    );

    // Try to download immediately
    let project = config::project_dir(&config_path);
    let cache = config::maven_cache_dir(&project);
    let lock_path = project.join(config::LOCK_FILE);
    let mut lock = config::load_lock(&lock_path)?;

    let mut single_dep = std::collections::BTreeMap::new();
    single_dep.insert(coord.clone(), version);

    match resolver::resolve_and_download(&single_dep, &cache, &mut lock) {
        Ok(jars) => {
            config::save_lock(&lock_path, &lock)?;
            println!(
                "  {} Downloaded {} artifact(s)",
                style("✓").green(),
                jars.len()
            );
        }
        Err(e) => {
            println!(
                "  {} Failed to download: {}",
                style("!").yellow(),
                e
            );
            println!("    Dependencies will be resolved on next build");
        }
    }

    Ok(())
}

fn parse_dep_spec(dep: &str) -> Result<(String, String, String)> {
    // Format: com.google.guava:guava@33.0.0-jre
    if dep.contains(':') {
        let (coord, version) = if dep.contains('@') {
            let parts: Vec<&str> = dep.splitn(2, '@').collect();
            (parts[0], Some(parts[1].to_string()))
        } else {
            (dep, None)
        };

        let parts: Vec<&str> = coord.split(':').collect();
        if parts.len() != 2 {
            bail!("Invalid coordinate: '{}'. Expected groupId:artifactId", coord);
        }

        let group_id = parts[0].to_string();
        let artifact_id = parts[1].to_string();

        let version = match version {
            Some(v) => v,
            None => {
                println!("  Fetching latest version for {}:{}...", group_id, artifact_id);
                let v = resolver::fetch_latest_version(&group_id, &artifact_id)?;
                format!("^{}", v)
            }
        };

        Ok((group_id, artifact_id, version))
    } else {
        // Fuzzy search
        println!("  Searching Maven Central for '{}'...", dep);
        let results = resolver::search_maven(dep)?;

        if results.is_empty() {
            bail!("No results found for '{}' on Maven Central", dep);
        }

        let items: Vec<String> = results
            .iter()
            .map(|(g, a, v)| format!("{}:{} ({})", g, a, v))
            .collect();

        let selection = Select::new()
            .with_prompt("Select dependency")
            .items(&items)
            .default(0)
            .interact()?;

        let (g, a, v) = &results[selection];
        Ok((g.clone(), a.clone(), format!("^{}", v)))
    }
}
