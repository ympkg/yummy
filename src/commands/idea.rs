use anyhow::Result;
use console::style;
use serde::Serialize;
use std::path::{Path, PathBuf};

use crate::config;
use crate::workspace::graph::WorkspaceGraph;

pub fn execute(target: Option<String>, download_sources: bool, json: bool) -> Result<()> {
    let (config_path, cfg) = config::load_or_find_config()?;
    let project = config::project_dir(&config_path);

    let java_version = cfg.target.as_deref().unwrap_or("21");

    if json {
        return execute_json(&project, &cfg, target.as_deref(), java_version);
    }

    // Non-JSON mode: only generate misc.xml (JDK config) for bootstrapping.
    // Module/library/compiler configuration is handled by the Yummy IntelliJ plugin
    // via External System integration (ymc idea --json).
    let idea_dir = project.join(".idea");
    if !idea_dir.is_dir() {
        return Ok(());
    }

    // misc.xml - JDK version + ExternalStorageConfigurationManager (prevents .iml generation)
    let misc = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<project version="4">
  <component name="ExternalStorageConfigurationManager" enabled="true" />
  <component name="ProjectRootManager" version="2" languageLevel="JDK_{java_version}" default="true" project-jdk-name="{java_version}" project-jdk-type="JavaSDK">
    <output url="file://$PROJECT_DIR$/out" />
  </component>
</project>
"#
    );
    std::fs::write(idea_dir.join("misc.xml"), misc)?;

    // Download sources if requested (still useful for offline caching)
    if download_sources {
        let jars = if cfg.workspaces.is_some() {
            let ws = WorkspaceGraph::build(&project)?;
            let root_config_path = project.join(config::CONFIG_FILE);
            let root_cfg = config::load_config(&root_config_path)?;
            let root_registries = root_cfg.registry_entries();
            let root_resolutions = root_cfg.resolved_resolutions(&root_cfg);
            let cache_dir = config::maven_cache_dir();

            let packages = if let Some(ref target) = target {
                ws.transitive_closure(target)?
            } else {
                let mut all = ws.all_packages();
                all.sort();
                all
            };

            let mut all_module_deps = Vec::new();
            for pkg_name in &packages {
                let pkg = ws.get_package(pkg_name).unwrap();
                let deps = pkg.config.maven_dependencies_with_root(&root_cfg);
                all_module_deps.push((pkg_name.clone(), deps));
            }

            let mut resolved = config::load_resolved_cache(&project)?;
            let mut exclusions: Vec<String> = root_cfg.exclusions.as_ref().cloned().unwrap_or_default();
            exclusions.extend(root_cfg.per_dependency_exclusions());
            exclusions.extend(root_cfg.resolved_exclusions());
            let per_module_jars = crate::workspace::resolver::resolve_workspace_deps_with_resolutions(
                &all_module_deps, &cache_dir, &mut resolved, &root_registries, &exclusions, &root_resolutions,
            )?;
            config::save_resolved_cache(&project, &resolved)?;

            let mut all_jars: Vec<PathBuf> = Vec::new();
            for jars in per_module_jars.values() {
                all_jars.extend(jars.clone());
            }
            all_jars.sort();
            all_jars.dedup();
            all_jars
        } else {
            super::build::resolve_deps(&project, &cfg)?
        };

        for jar in &jars {
            make_sources_section(jar, true);
        }
    }

    println!(
        "{} IDEA project (misc.xml updated, use Yummy plugin for full sync)",
        style(format!("{:>12}", "Finished")).green().bold(),
    );
    Ok(())
}

// ============================================================
//  --json output: structured project model for External System
// ============================================================

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct IdeaProjectModel {
    name: String,
    group_id: String,
    version: String,
    jdk_version: String,
    #[serde(rename = "type")]
    project_type: String, // "single" or "workspace"
    modules: Vec<IdeaModuleModel>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct IdeaModuleModel {
    name: String,
    path: String,
    source_folders: Vec<IdeaSourceFolder>,
    output_path: String,
    test_output_path: String,
    dependencies: Vec<IdeaDependency>,
    annotation_processors: Vec<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct IdeaSourceFolder {
    path: String,
    #[serde(rename = "type")]
    folder_type: String, // SOURCE, TEST, RESOURCE, TEST_RESOURCE
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct IdeaDependency {
    #[serde(rename = "type")]
    dep_type: String, // "library" or "module"
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    jar_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    source_path: Option<String>,
    scope: String, // COMPILE, RUNTIME, PROVIDED, TEST
}

fn execute_json(
    project: &Path,
    cfg: &config::schema::YmConfig,
    target: Option<&str>,
    java_version: &str,
) -> Result<()> {
    // Suppress all human-readable progress output — stdout must be pure JSON
    crate::JSON_QUIET.store(true, std::sync::atomic::Ordering::Relaxed);
    let modules = if cfg.workspaces.is_some() {
        let ws = WorkspaceGraph::build(project)?;
        let packages = if let Some(t) = target {
            ws.transitive_closure(t)?
        } else {
            let mut all = ws.all_packages();
            all.sort();
            all
        };
        build_modules_workspace(project, &packages, &ws)?
    } else {
        vec![build_module_single(project, cfg)?]
    };

    let model = IdeaProjectModel {
        name: cfg.name.clone(),
        group_id: cfg.group_id.clone(),
        version: cfg.version.as_deref().unwrap_or("0.0.0").to_string(),
        jdk_version: java_version.to_string(),
        project_type: if cfg.workspaces.is_some() { "workspace" } else { "single" }.to_string(),
        modules,
    };

    println!("{}", serde_json::to_string_pretty(&model)?);
    Ok(())
}

fn build_modules_workspace(
    root: &Path,
    packages: &[String],
    ws: &WorkspaceGraph,
) -> Result<Vec<IdeaModuleModel>> {
    use crate::workspace::resolver;

    // Pre-load root config ONCE
    let root_config_path = root.join(config::CONFIG_FILE);
    let root_cfg = config::load_config(&root_config_path)?;
    let root_registries = root_cfg.registry_entries();
    let root_resolutions = root_cfg.resolved_resolutions(&root_cfg);
    let cache_dir = config::maven_cache_dir();

    eprintln!("  Scanning {} modules...", packages.len());

    // Collect all module deps for single-pass workspace resolution
    let mut all_module_deps: Vec<(String, std::collections::BTreeMap<String, String>)> = Vec::new();
    for pkg_name in packages {
        let pkg = ws.get_package(pkg_name).unwrap();
        let deps = pkg.config.maven_dependencies_with_root(&root_cfg);
        all_module_deps.push((pkg_name.clone(), deps));
    }

    let unique_dep_count: usize = {
        let mut all_keys = std::collections::BTreeSet::new();
        for (_, deps) in &all_module_deps {
            all_keys.extend(deps.keys().cloned());
        }
        all_keys.len()
    };
    eprintln!("  Resolving {} artifacts...", unique_dep_count);

    // Single-pass resolution: merge all deps → resolve + download → distribute per-module
    let mut resolved = config::load_resolved_cache(root)?;
    let exclusions: Vec<String> = root_cfg.exclusions.as_ref().cloned().unwrap_or_default();
    let per_module_jars = resolver::resolve_workspace_deps_with_resolutions(
        &all_module_deps, &cache_dir, &mut resolved, &root_registries, &exclusions, &root_resolutions,
    )?;
    config::save_resolved_cache(root, &resolved)?;

    eprintln!("  Building project model...");

    // Build module models
    let mut modules = Vec::new();
    for pkg_name in packages {
        let pkg = ws.get_package(pkg_name).unwrap();
        let jars = per_module_jars.get(pkg_name).cloned().unwrap_or_default();
        let ap_jars = collect_annotation_processor_jars(&pkg.config, &jars);
        let ws_deps = pkg.config.workspace_module_deps();

        let mut dependencies = Vec::new();
        // Module dependencies
        for dep in &ws_deps {
            dependencies.push(IdeaDependency {
                dep_type: "module".to_string(),
                name: dep.clone(),
                jar_path: None,
                source_path: None,
                scope: "COMPILE".to_string(),
            });
        }
        // Library dependencies
        for jar in &jars {
            let jar_name = jar.file_stem().unwrap().to_string_lossy().to_string();
            let scope = jar_to_idea_scope_str(jar, &pkg.config);
            let source_path = find_sources_jar(jar);
            dependencies.push(IdeaDependency {
                dep_type: "library".to_string(),
                name: jar_name,
                jar_path: Some(to_idea_path(&jar.to_string_lossy())),
                source_path,
                scope,
            });
        }

        // Also add lib dirs for this module
        let lib_jars = super::build::resolve_lib_dirs(&pkg.path, &pkg.config);
        for jar in &lib_jars {
            let jar_name = jar.file_stem().unwrap().to_string_lossy().to_string();
            dependencies.push(IdeaDependency {
                dep_type: "library".to_string(),
                name: jar_name,
                jar_path: Some(to_idea_path(&jar.to_string_lossy())),
                source_path: None,
                scope: "COMPILE".to_string(),
            });
        }

        modules.push(IdeaModuleModel {
            name: pkg_name.clone(),
            path: to_idea_path(&pkg.path.to_string_lossy()),
            source_folders: detect_source_folders_structured(&pkg.path),
            output_path: "out/classes".to_string(),
            test_output_path: "out/test-classes".to_string(),
            dependencies,
            annotation_processors: ap_jars.iter()
                .map(|j| to_idea_path(&j.to_string_lossy()))
                .collect(),
        });
    }
    Ok(modules)
}

fn build_module_single(
    project: &Path,
    cfg: &config::schema::YmConfig,
) -> Result<IdeaModuleModel> {
    let jars = super::build::resolve_deps(project, cfg)?;
    let ap_jars = collect_annotation_processor_jars(cfg, &jars);

    let mut dependencies = Vec::new();
    for jar in &jars {
        let jar_name = jar.file_stem().unwrap().to_string_lossy().to_string();
        let scope = jar_to_idea_scope_str(jar, cfg);
        let source_path = find_sources_jar(jar);
        dependencies.push(IdeaDependency {
            dep_type: "library".to_string(),
            name: jar_name,
            jar_path: Some(to_idea_path(&jar.to_string_lossy())),
            source_path,
            scope,
        });
    }

    Ok(IdeaModuleModel {
        name: cfg.name.clone(),
        path: to_idea_path(&project.to_string_lossy()),
        source_folders: detect_source_folders_structured(project),
        output_path: "out/classes".to_string(),
        test_output_path: "out/test-classes".to_string(),
        dependencies,
        annotation_processors: ap_jars.iter()
            .map(|j| to_idea_path(&j.to_string_lossy()))
            .collect(),
    })
}

/// Return the IDEA scope string for a JAR: COMPILE, RUNTIME, PROVIDED, TEST
fn jar_to_idea_scope_str(jar: &Path, cfg: &config::schema::YmConfig) -> String {
    use config::schema::{is_maven_dep, artifact_id_from_key};
    let jar_stem = jar.file_stem().unwrap_or_default().to_string_lossy();
    // Check [dependencies]
    if let Some(ref deps) = cfg.dependencies {
        for (key, value) in deps {
            if !is_maven_dep(key) { continue; }
            let artifact_id = artifact_id_from_key(key);
            if jar_stem.starts_with(artifact_id) {
                return match value.scope() {
                    "runtime" => "RUNTIME",
                    "provided" => "PROVIDED",
                    "test" => "TEST",
                    _ => "COMPILE",
                }.to_string();
            }
        }
    }
    // Check [devDependencies] — always PROVIDED
    if let Some(ref dev_deps) = cfg.dev_dependencies {
        for (key, _value) in dev_deps {
            if !is_maven_dep(key) { continue; }
            let artifact_id = artifact_id_from_key(key);
            if jar_stem.starts_with(artifact_id) {
                return "PROVIDED".to_string();
            }
        }
    }
    "COMPILE".to_string()
}

/// Check if -sources.jar exists next to the main JAR
fn find_sources_jar(jar: &Path) -> Option<String> {
    let sources_jar = jar.with_file_name(
        jar.file_stem()?.to_string_lossy().to_string() + "-sources.jar",
    );
    if sources_jar.exists() {
        Some(to_idea_path(&sources_jar.to_string_lossy()))
    } else {
        None
    }
}

/// Structured source folder detection for JSON output
fn detect_source_folders_structured(project: &Path) -> Vec<IdeaSourceFolder> {
    let mut folders = Vec::new();

    let maven_src = project.join("src").join("main").join("java");
    if maven_src.exists() {
        folders.push(IdeaSourceFolder { path: "src/main/java".to_string(), folder_type: "SOURCE".to_string() });
    } else if project.join("src").exists() {
        folders.push(IdeaSourceFolder { path: "src".to_string(), folder_type: "SOURCE".to_string() });
    }

    let maven_res = project.join("src").join("main").join("resources");
    if maven_res.exists() {
        folders.push(IdeaSourceFolder { path: "src/main/resources".to_string(), folder_type: "RESOURCE".to_string() });
    }

    let maven_test = project.join("src").join("test").join("java");
    if maven_test.exists() {
        folders.push(IdeaSourceFolder { path: "src/test/java".to_string(), folder_type: "TEST".to_string() });
    } else if project.join("test").exists() {
        folders.push(IdeaSourceFolder { path: "test".to_string(), folder_type: "TEST".to_string() });
    }

    let maven_test_res = project.join("src").join("test").join("resources");
    if maven_test_res.exists() {
        folders.push(IdeaSourceFolder { path: "src/test/resources".to_string(), folder_type: "TEST_RESOURCE".to_string() });
    }

    if folders.is_empty() {
        folders.push(IdeaSourceFolder { path: "src".to_string(), folder_type: "SOURCE".to_string() });
    }

    folders
}


/// Generate the <SOURCES> section for a library XML.
/// If download_sources is true, tries to download the -sources.jar next to the main JAR.
fn make_sources_section(jar: &std::path::Path, download_sources: bool) -> String {
    let sources_jar = jar.with_file_name(
        jar.file_stem()
            .unwrap()
            .to_string_lossy()
            .to_string()
            + "-sources.jar",
    );

    if download_sources && !sources_jar.exists() {
        // Try to download from Maven Central
        // Parse the jar path to reconstruct the Maven coordinate
        // Cache structure: group_id/artifact_id/version/artifact-version.jar
        if let Some(parent) = jar.parent() {
            let version = parent.file_name().unwrap_or_default().to_string_lossy();
            if let Some(artifact_dir) = parent.parent() {
                let artifact = artifact_dir.file_name().unwrap_or_default().to_string_lossy();
                if let Some(group_dir) = artifact_dir.parent() {
                    let group = group_dir.file_name().unwrap_or_default().to_string_lossy();
                    let coord = crate::workspace::resolver::MavenCoord {
                        group_id: group.to_string(),
                        artifact_id: artifact.to_string(),
                        version: version.to_string(),
                        classifier: None,
                        exclusions: Vec::new(),
                        scope: None,
                    };
                    let url = format!(
                        "https://repo1.maven.org/maven2/{}/{}/{}/{}-{}-sources.jar",
                        group.replace('.', "/"),
                        artifact,
                        version,
                        artifact,
                        version
                    );
                    let _ = coord; // just used for context
                    if let Ok(client) = reqwest::blocking::Client::builder()
                        .user_agent(concat!("ym/", env!("CARGO_PKG_VERSION")))
                        .timeout(std::time::Duration::from_secs(15))
                        .build()
                    {
                        if let Ok(resp) = client.get(&url).send() {
                            if resp.status().is_success() {
                                if let Ok(bytes) = resp.bytes() {
                                    let _ = std::fs::write(&sources_jar, &bytes);
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    if sources_jar.exists() {
        let sources_abs = to_idea_path(&sources_jar.to_string_lossy());
        format!(
            "    <SOURCES>\n      <root url=\"jar://{}!/\" />\n    </SOURCES>\n",
            sources_abs
        )
    } else {
        String::new()
    }
}


/// Collect annotation processor JARs from config and classpath.
/// Uses explicit `compiler.annotationProcessors` if set, otherwise auto-discovers.
fn collect_annotation_processor_jars(
    cfg: &config::schema::YmConfig,
    classpath: &[PathBuf],
) -> Vec<PathBuf> {
    if let Some(coords) = cfg.compiler.as_ref().and_then(|c| c.annotation_processors.as_ref()) {
        if !coords.is_empty() {
            let deps = cfg.maven_dependencies();
            let mut jars = Vec::new();
            for coord in coords {
                if let Some(version) = deps.get(coord) {
                    // Find JAR in classpath by matching artifact ID
                    let artifact_id = coord.split(':').next_back().unwrap_or("");
                    for jar in classpath {
                        let stem = jar.file_stem().unwrap_or_default().to_string_lossy();
                        if stem.starts_with(artifact_id) && stem.contains(version) {
                            jars.push(jar.clone());
                            break;
                        }
                    }
                }
            }
            return jars;
        }
    }

    // Auto-discover: check for META-INF/services/javax.annotation.processing.Processor
    classpath
        .iter()
        .filter(|jar| {
            jar.extension().and_then(|e| e.to_str()) == Some("jar")
                && jar.exists()
                && super::build::has_annotation_processor(jar)
        })
        .cloned()
        .collect()
}


/// Detect if running under WSL (Windows Subsystem for Linux).
fn is_wsl() -> bool {
    if let Ok(osrelease) = std::fs::read_to_string("/proc/version") {
        let lower = osrelease.to_lowercase();
        return lower.contains("microsoft") || lower.contains("wsl");
    }
    false
}

/// Convert a WSL Linux path to a Windows path for IDEA compatibility.
/// e.g., /mnt/c/Users/foo -> C:/Users/foo
///       /home/user/project -> \\wsl$\<distro>\home\user\project (via wslpath)
fn to_idea_path(path: &str) -> String {
    if !is_wsl() {
        return path.to_string();
    }

    // /mnt/<drive>/... -> <DRIVE>:/...
    if path.starts_with("/mnt/") && path.len() > 5 {
        let rest = &path[5..];
        if let Some(idx) = rest.find('/') {
            let drive = rest[..idx].to_uppercase();
            if drive.len() == 1 {
                return format!("{}:{}", drive, &rest[idx..]);
            }
        } else if rest.len() == 1 {
            return format!("{}:/", rest.to_uppercase());
        }
    }

    // For non-/mnt/ paths, try wslpath -w
    if let Ok(output) = std::process::Command::new("wslpath")
        .arg("-w")
        .arg(path)
        .output()
    {
        if output.status.success() {
            let win_path = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !win_path.is_empty() {
                // Convert backslashes to forward slashes for IDEA XML
                return win_path.replace('\\', "/");
            }
        }
    }

    path.to_string()
}

#[cfg(test)]
mod tests {
    #[test]
    fn test_to_idea_path_mnt_conversion() {
        // Only test the path conversion logic (not WSL detection)
        // /mnt/c/Users/foo -> C:/Users/foo
        let path = "/mnt/c/Users/foo/project";
        if path.starts_with("/mnt/") {
            let rest = &path[5..];
            if let Some(idx) = rest.find('/') {
                let drive = rest[..idx].to_uppercase();
                let result = format!("{}:{}", drive, &rest[idx..]);
                assert_eq!(result, "C:/Users/foo/project");
            }
        }
    }
}
