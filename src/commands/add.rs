use anyhow::{bail, Result};
use console::style;
use dialoguer::Select;

use crate::config;
use crate::config::schema::{DependencySpec, DependencyValue};
use crate::workspace::resolver;

pub fn execute(dep: &str, scope: Option<&str>, classifier: Option<&str>) -> Result<()> {
    let (config_path, mut cfg) = config::load_or_find_config()?;

    // Handle URL dependencies
    if dep.starts_with("https://") || dep.starts_with("http://") {
        return add_url_dependency(dep, scope, &config_path, &mut cfg);
    }

    // Handle Git dependencies
    if dep.starts_with("git+") {
        return add_git_dependency(dep, scope, &config_path, &mut cfg);
    }

    // Parse the dependency specification
    let (group_id, artifact_id, version) = parse_dep_spec(dep)?;
    let coord = if let Some(cls) = classifier {
        format!("{}:{}:{}", group_id, artifact_id, cls)
    } else {
        format!("{}:{}", group_id, artifact_id)
    };

    let deps = cfg.dependencies.get_or_insert_with(Default::default);

    // Need detailed format if scope or classifier specified
    let needs_detailed = scope.is_some_and(|s| s != "compile") || classifier.is_some();

    // If dependency already exists, update version (and scope/classifier if specified)
    if deps.contains_key(&coord) {
        let existing = deps.get_mut(&coord).unwrap();
        match existing {
            DependencyValue::Simple(v) => {
                if needs_detailed {
                    *existing = DependencyValue::Detailed(DependencySpec {
                        version: Some(version.clone()),
                        scope: scope.map(|s| s.to_string()),
                        classifier: classifier.map(|c| c.to_string()),
                        ..Default::default()
                    });
                } else {
                    *v = version.clone();
                }
            }
            DependencyValue::Detailed(spec) => {
                spec.version = Some(version.clone());
                if let Some(s) = scope {
                    spec.scope = Some(s.to_string());
                }
                if let Some(c) = classifier {
                    spec.classifier = Some(c.to_string());
                }
            }
        }
        config::save_config(&config_path, &cfg)?;
        println!(
            "  {} Updated {} {}",
            style("✓").green(),
            style(&coord).cyan(),
            style(&version).dim()
        );
        return Ok(());
    }

    // Build the dependency value
    let value = if needs_detailed {
        DependencyValue::Detailed(DependencySpec {
            version: Some(version.clone()),
            scope: scope.map(|s| s.to_string()),
            classifier: classifier.map(|c| c.to_string()),
            ..Default::default()
        })
    } else {
        // No scope/classifier → simple format (compile)
        DependencyValue::Simple(version.clone())
    };

    deps.insert(coord.clone(), value);
    config::save_config(&config_path, &cfg)?;

    println!(
        "  {} Added {}@{}",
        style("✓").green(),
        style(&coord).cyan(),
        style(&version).dim()
    );

    // Try to download immediately
    let project = config::project_dir(&config_path);
    let cache = config::maven_cache_dir(&project);
    let mut resolved = config::load_resolved_cache(&project)?;

    let mut single_dep = std::collections::BTreeMap::new();
    single_dep.insert(coord.clone(), version);

    match resolver::resolve_and_download(&single_dep, &cache, &mut resolved) {
        Ok(jars) => {
            config::save_resolved_cache(&project, &resolved)?;
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
                resolver::fetch_latest_version(&group_id, &artifact_id)?
            }
        };

        Ok((group_id, artifact_id, version))
    } else {
        // Fuzzy search by artifactId
        println!("  Searching Maven Central for '{}'...", dep);
        let results = resolver::search_maven(dep)?;

        if results.is_empty() {
            bail!("No results found for '{}' on Maven Central", dep);
        }

        // Non-interactive mode: require exact match
        if !atty_is_interactive() && results.len() > 1 {
            bail!(
                "Multiple matches for '{}'. Use full groupId:artifactId format.\n  Candidates: {}",
                dep,
                results.iter().map(|(g, a, _)| format!("{}:{}", g, a)).collect::<Vec<_>>().join(", ")
            );
        }

        let items: Vec<String> = results
            .iter()
            .map(|(g, a, v)| format!("{}:{} ({})", g, a, v))
            .collect();

        let selection = Select::new()
            .with_prompt("Select package")
            .items(&items)
            .default(0)
            .interact()?;

        let (g, a, v) = &results[selection];
        Ok((g.clone(), a.clone(), v.clone()))
    }
}

fn atty_is_interactive() -> bool {
    use std::io::IsTerminal;
    std::io::stdin().is_terminal()
}

/// Add a URL dependency (direct JAR URL)
fn add_url_dependency(
    url: &str,
    scope: Option<&str>,
    config_path: &std::path::Path,
    cfg: &mut config::schema::YmConfig,
) -> Result<()> {
    // Derive a key name from the URL filename
    let filename = url.rsplit('/').next().unwrap_or("unknown");
    let key = filename
        .strip_suffix(".jar")
        .unwrap_or(filename)
        .to_string();

    let deps = cfg.dependencies.get_or_insert_with(Default::default);

    let value = DependencyValue::Detailed(DependencySpec {
        url: Some(url.to_string()),
        scope: scope.map(|s| s.to_string()),
        ..Default::default()
    });

    deps.insert(key.clone(), value);
    config::save_config(config_path, cfg)?;

    println!(
        "  {} Added URL dependency {} → {}",
        style("✓").green(),
        style(&key).cyan(),
        style(url).dim()
    );

    // Try to download immediately
    let project = config::project_dir(config_path);
    let cache = config::maven_cache_dir(&project);
    let jar_dir = cache.join("url-deps");
    std::fs::create_dir_all(&jar_dir)?;
    let jar_path = jar_dir.join(filename);

    if !jar_path.exists() {
        println!("  Downloading {}...", filename);
        match download_url_jar(url, &jar_path) {
            Ok(()) => {
                println!(
                    "  {} Downloaded {}",
                    style("✓").green(),
                    jar_path.display()
                );
            }
            Err(e) => {
                println!(
                    "  {} Failed to download: {}",
                    style("!").yellow(),
                    e
                );
                println!("    JAR will be downloaded on next build");
            }
        }
    }

    Ok(())
}

/// Add a Git dependency
fn add_git_dependency(
    dep: &str,
    scope: Option<&str>,
    config_path: &std::path::Path,
    cfg: &mut config::schema::YmConfig,
) -> Result<()> {
    // Parse git+https://github.com/user/repo.git[@ref]
    let url_part = dep.strip_prefix("git+").unwrap_or(dep);
    let (git_url, git_ref) = if url_part.contains('@') {
        let parts: Vec<&str> = url_part.rsplitn(2, '@').collect();
        (parts[1].to_string(), Some(parts[0].to_string()))
    } else {
        (url_part.to_string(), None)
    };

    // Derive name from repo URL
    let repo_name = git_url
        .rsplit('/')
        .next()
        .unwrap_or("unknown")
        .strip_suffix(".git")
        .unwrap_or("unknown")
        .to_string();

    let deps = cfg.dependencies.get_or_insert_with(Default::default);

    let value = DependencyValue::Detailed(DependencySpec {
        git: Some(git_url.clone()),
        git_ref: git_ref.clone(),
        scope: scope.map(|s| s.to_string()),
        ..Default::default()
    });

    deps.insert(repo_name.clone(), value);
    config::save_config(config_path, cfg)?;

    println!(
        "  {} Added Git dependency {} → {}{}",
        style("✓").green(),
        style(&repo_name).cyan(),
        style(&git_url).dim(),
        git_ref.as_ref().map(|r| format!("@{}", r)).unwrap_or_default()
    );
    println!("    Git dependencies are cloned and built on next build");

    Ok(())
}

fn download_url_jar(url: &str, dest: &std::path::Path) -> Result<()> {
    let client = reqwest::blocking::Client::builder()
        .user_agent(format!("ym/{}", env!("CARGO_PKG_VERSION")))
        .build()?;
    let response = client.get(url).send()?;
    if !response.status().is_success() {
        bail!("HTTP {}: {}", response.status(), url);
    }
    let bytes = response.bytes()?;
    std::fs::write(dest, &bytes)?;
    Ok(())
}
