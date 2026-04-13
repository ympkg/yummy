use anyhow::{Result, bail};
use console::style;
use std::path::{Path, PathBuf};

use crate::config;

pub fn execute(yes: bool, pattern: Option<&str>) -> Result<()> {
    let project = std::env::current_dir()
        .ok()
        .and_then(|cwd| config::find_config(&cwd))
        .map(|cfg_path| config::project_dir(&cfg_path));

    let maven_cache = config::maven_cache_dir();
    let pom_cache = config::pom_cache_dir();

    match pattern {
        Some(pat) => clean_pattern(&maven_cache, &pom_cache, project.as_deref(), pat, yes),
        None => clean_all(&maven_cache, &pom_cache, project.as_deref(), yes),
    }
}

fn clean_all(
    maven_cache: &Path,
    pom_cache: &Path,
    project: Option<&Path>,
    yes: bool,
) -> Result<()> {
    let size = config::dir_size(maven_cache) + config::dir_size(pom_cache);
    // Filter upfront so "does it exist" is answered exactly once.
    let resolved = project
        .map(config::resolved_cache_path)
        .filter(|p| p.exists());

    if size == 0 && resolved.is_none() {
        println!("  {} No cache to clean", style("✓").green());
        return Ok(());
    }

    if !yes {
        let confirm = dialoguer::Confirm::new()
            .with_prompt(format!(
                "  Delete entire dependency cache ({})?",
                config::format_size(size)
            ))
            .default(false)
            .interact()?;
        if !confirm {
            println!("  {} Cancelled", style("!").yellow());
            return Ok(());
        }
    }

    if maven_cache.exists() {
        std::fs::remove_dir_all(maven_cache)?;
        println!("  {} Removed {}", style("✓").green(), maven_cache.display());
    }
    if pom_cache.exists() {
        std::fs::remove_dir_all(pom_cache)?;
        println!("  {} Removed {}", style("✓").green(), pom_cache.display());
    }
    if let Some(path) = resolved {
        std::fs::remove_file(&path)?;
        println!("  {} Removed {}", style("✓").green(), path.display());
    }

    println!(
        "  {} Cache clean complete ({} freed)",
        style("✓").green(),
        config::format_size(size)
    );
    Ok(())
}

#[derive(Debug)]
enum CachePattern {
    /// Match by artifactId only, across any groupId. The artifactId must
    /// equal the cache directory name exactly — no substring semantics.
    ArtifactAnyGroup(String),
    GroupWildcard(String),
    Ga { group: String, artifact: String },
    Gav { group: String, artifact: String, version: String },
}

impl CachePattern {
    fn parse(raw: &str) -> Result<Self> {
        let parts: Vec<&str> = raw.split(':').collect();
        match parts.as_slice() {
            [a] if !a.is_empty() => Ok(Self::ArtifactAnyGroup((*a).to_string())),
            [g, "*"] if !g.is_empty() => Ok(Self::GroupWildcard((*g).to_string())),
            [g, a] if !g.is_empty() && !a.is_empty() => Ok(Self::Ga {
                group: (*g).to_string(),
                artifact: (*a).to_string(),
            }),
            [g, a, v] if !g.is_empty() && !a.is_empty() && !v.is_empty() => Ok(Self::Gav {
                group: (*g).to_string(),
                artifact: (*a).to_string(),
                version: (*v).to_string(),
            }),
            _ => bail!(
                "Invalid pattern '{}'. Expected: <artifact>, <group>:*, <group>:<artifact>, or <group>:<artifact>:<version>",
                raw
            ),
        }
    }
}

/// Collect paths matching `pat` within a cache rooted at `cache`.
/// `gav_leaf` maps the `{group}/{artifact}/{version}` triple to the actual
/// filesystem entry that represents the GAV — a directory for the Maven
/// cache (`~/.ym/maven/...`) but a `{version}.json` file for the POM cache
/// (`~/.ym/pom-cache/...`). Non-existent candidates are filtered out.
fn match_cache<F>(cache: &Path, pat: &CachePattern, gav_leaf: F) -> Vec<PathBuf>
where
    F: Fn(&Path, &str, &str, &str) -> PathBuf,
{
    let mut matches = Vec::new();

    match pat {
        CachePattern::ArtifactAnyGroup(artifact) => {
            // `read_dir` on a missing cache path returns Err, handled here;
            // no separate `cache.exists()` pre-check needed.
            let Ok(groups) = std::fs::read_dir(cache) else { return matches };
            for entry in groups.flatten() {
                if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                    continue;
                }
                let artifact_dir = entry.path().join(artifact);
                if artifact_dir.is_dir() {
                    matches.push(artifact_dir);
                }
            }
        }
        CachePattern::GroupWildcard(group) => {
            let p = cache.join(group);
            if p.is_dir() {
                matches.push(p);
            }
        }
        CachePattern::Ga { group, artifact } => {
            let p = cache.join(group).join(artifact);
            if p.is_dir() {
                matches.push(p);
            }
        }
        CachePattern::Gav { group, artifact, version } => {
            let p = gav_leaf(cache, group, artifact, version);
            if p.exists() {
                matches.push(p);
            }
        }
    }

    matches
}

fn match_maven_cache(cache: &Path, pat: &CachePattern) -> Vec<PathBuf> {
    // Maven cache GAV leaf: `{group}/{artifact}/{version}/` (directory)
    match_cache(cache, pat, |root, g, a, v| root.join(g).join(a).join(v))
}

fn match_pom_cache(cache: &Path, pat: &CachePattern) -> Vec<PathBuf> {
    // POM cache GAV leaf: `{group}/{artifact}/{version}.json` (file)
    match_cache(cache, pat, |root, g, a, v| {
        root.join(g).join(a).join(format!("{}.json", v))
    })
}

fn clean_pattern(
    maven_cache: &Path,
    pom_cache: &Path,
    project: Option<&Path>,
    pattern: &str,
    yes: bool,
) -> Result<()> {
    let pat = CachePattern::parse(pattern)?;
    let mut matches = match_maven_cache(maven_cache, &pat);
    matches.extend(match_pom_cache(pom_cache, &pat));

    if matches.is_empty() {
        println!(
            "  {} No cache entries matched '{}'",
            style("!").yellow(),
            pattern
        );
        return Ok(());
    }

    let total_size: u64 = matches.iter().map(|p| path_size(p)).sum();

    if !yes {
        println!(
            "  {} Matched {} entr{} for '{}':",
            style("→").cyan(),
            matches.len(),
            if matches.len() == 1 { "y" } else { "ies" },
            pattern
        );
        for p in &matches {
            println!("    {}", p.display());
        }
        let confirm = dialoguer::Confirm::new()
            .with_prompt(format!(
                "  Delete matched entries ({})?",
                config::format_size(total_size)
            ))
            .default(false)
            .interact()?;
        if !confirm {
            println!("  {} Cancelled", style("!").yellow());
            return Ok(());
        }
    }

    for p in &matches {
        remove_path(p)?;
        println!("  {} Removed {}", style("✓").green(), p.display());
    }

    // Invalidate project-local resolved.json so the next build re-resolves
    // the cleared deps. Safe to remove even if the pattern matched only
    // entries outside the current project — resolved.json will rebuild.
    if let Some(p) = project {
        let path = config::resolved_cache_path(p);
        if path.exists() {
            std::fs::remove_file(&path)?;
            println!("  {} Invalidated {}", style("✓").green(), path.display());
        }
    }

    println!(
        "  {} Cache clean complete ({} freed)",
        style("✓").green(),
        config::format_size(total_size)
    );
    Ok(())
}

fn remove_path(p: &Path) -> Result<()> {
    match std::fs::metadata(p) {
        Ok(m) if m.is_dir() => std::fs::remove_dir_all(p)?,
        Ok(_) => std::fs::remove_file(p)?,
        // Already gone between match and remove — idempotent, fine.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    Ok(())
}

fn path_size(p: &Path) -> u64 {
    // Single stat for both file-or-dir dispatch + size readout.
    match std::fs::metadata(p) {
        Ok(m) if m.is_file() => m.len(),
        Ok(_) => config::dir_size(p),
        Err(_) => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_artifact_any_group() {
        match CachePattern::parse("theme-service").unwrap() {
            CachePattern::ArtifactAnyGroup(a) => assert_eq!(a, "theme-service"),
            other => panic!("expected ArtifactAnyGroup, got {:?}", other),
        }
    }

    #[test]
    fn parse_group_wildcard() {
        match CachePattern::parse("com.summer.jarvis:*").unwrap() {
            CachePattern::GroupWildcard(g) => assert_eq!(g, "com.summer.jarvis"),
            other => panic!("expected GroupWildcard, got {:?}", other),
        }
    }

    #[test]
    fn parse_ga_exact() {
        match CachePattern::parse("com.summer.jarvis:theme-service").unwrap() {
            CachePattern::Ga { group, artifact } => {
                assert_eq!(group, "com.summer.jarvis");
                assert_eq!(artifact, "theme-service");
            }
            other => panic!("expected Ga, got {:?}", other),
        }
    }

    #[test]
    fn parse_gav_exact() {
        match CachePattern::parse("com.summer.jarvis:theme-service:4.0.12").unwrap() {
            CachePattern::Gav { group, artifact, version } => {
                assert_eq!(group, "com.summer.jarvis");
                assert_eq!(artifact, "theme-service");
                assert_eq!(version, "4.0.12");
            }
            other => panic!("expected Gav, got {:?}", other),
        }
    }

    #[test]
    fn parse_invalid_empty() {
        assert!(CachePattern::parse("").is_err());
        assert!(CachePattern::parse(":").is_err());
        assert!(CachePattern::parse("a:").is_err());
        assert!(CachePattern::parse(":b").is_err());
    }

    #[test]
    fn parse_invalid_too_many_parts() {
        assert!(CachePattern::parse("a:b:c:d").is_err());
    }

    #[test]
    fn match_maven_artifact_fuzzy() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = tmp.path();
        std::fs::create_dir_all(cache.join("com.example").join("foo").join("1.0")).unwrap();
        std::fs::create_dir_all(cache.join("org.other").join("foo").join("2.0")).unwrap();
        std::fs::create_dir_all(cache.join("com.example").join("bar").join("1.0")).unwrap();

        let matches = match_maven_cache(cache, &CachePattern::ArtifactAnyGroup("foo".to_string()));
        assert_eq!(matches.len(), 2);
    }

    #[test]
    fn match_maven_gav_exact() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = tmp.path();
        std::fs::create_dir_all(cache.join("com.example").join("foo").join("1.0")).unwrap();
        std::fs::create_dir_all(cache.join("com.example").join("foo").join("2.0")).unwrap();

        let matches = match_maven_cache(
            cache,
            &CachePattern::Gav {
                group: "com.example".to_string(),
                artifact: "foo".to_string(),
                version: "1.0".to_string(),
            },
        );
        assert_eq!(matches.len(), 1);
        assert!(matches[0].ends_with("1.0"));
    }

    #[test]
    fn match_pom_gav_is_file() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = tmp.path();
        let group_dir = cache.join("com.example").join("foo");
        std::fs::create_dir_all(&group_dir).unwrap();
        std::fs::write(group_dir.join("1.0.json"), "[]").unwrap();

        let matches = match_pom_cache(
            cache,
            &CachePattern::Gav {
                group: "com.example".to_string(),
                artifact: "foo".to_string(),
                version: "1.0".to_string(),
            },
        );
        assert_eq!(matches.len(), 1);
        assert!(matches[0].is_file());
    }
}
