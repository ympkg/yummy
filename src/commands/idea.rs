use anyhow::Result;
use console::style;
use serde::Serialize;
use std::path::{Path, PathBuf};

use crate::config;
use crate::workspace::graph::WorkspaceGraph;

/// Auto-sync IDEA project files when `.idea/` exists but `.iml` is missing.
/// Called by build/dev/test so users get IDEA support without manual `ym idea`.
pub(crate) fn auto_sync_idea(project: &Path, cfg: &config::schema::YmConfig) {
    if crate::is_json_quiet() {
        return;
    }
    let idea_dir = project.join(".idea");
    if !idea_dir.is_dir() {
        return;
    }
    let modules_dir = idea_dir.join("modules");
    let iml_path = modules_dir.join(format!("{}.iml", cfg.name));
    if iml_path.exists() {
        return;
    }

    let java_version = cfg.target.as_deref().unwrap_or("21");
    let result = if cfg.workspaces.is_some() {
        WorkspaceGraph::build(project).and_then(|ws| {
            let mut all = ws.all_packages();
            all.sort();
            generate_idea_project(project, &all, &ws, java_version, false)
        })
    } else {
        generate_single_project_idea(project, cfg, java_version, false)
    };

    match result {
        Ok(()) => {
            eprintln!(
                "  {} Auto-synced IDEA project (detected .idea/ without .iml)",
                style("✓").green()
            );
        }
        Err(e) => {
            eprintln!(
                "  {} Failed to auto-sync IDEA project: {}",
                style("⚠").yellow(),
                e
            );
        }
    }
}

pub fn execute(target: Option<String>, download_sources: bool, json: bool) -> Result<()> {
    let (config_path, cfg) = config::load_or_find_config()?;
    let project = config::project_dir(&config_path);

    let java_version = cfg.target.as_deref().unwrap_or("21");

    if json {
        return execute_json(&project, &cfg, target.as_deref(), java_version);
    }

    if cfg.workspaces.is_some() {
        let ws = WorkspaceGraph::build(&project)?;

        let packages = if let Some(ref target) = target {
            // Generate for target and its dependencies
            ws.transitive_closure(target)?
        } else {
            // No target: generate for ALL modules
            let mut all = ws.all_packages();
            all.sort();
            all
        };

        generate_idea_project(&project, &packages, &ws, java_version, download_sources)?;

        println!(
            "  {} Generated IDEA project ({} modules)",
            style("✓").green(),
            packages.len()
        );
    } else {
        // Single project mode
        generate_single_project_idea(&project, &cfg, java_version, download_sources)?;

        println!(
            "  {} Generated IDEA project for {}",
            style("✓").green(),
            style(&cfg.name).bold()
        );
    }

    println!("  Open this directory in IntelliJ IDEA to get started.");
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
    _root: &Path,
    packages: &[String],
    ws: &WorkspaceGraph,
) -> Result<Vec<IdeaModuleModel>> {
    let mut modules = Vec::new();
    for pkg_name in packages {
        let pkg = ws.get_package(pkg_name).unwrap();
        let jars = super::build::resolve_deps_no_download(&pkg.path, &pkg.config)?;
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
    let jars = super::build::resolve_deps_no_download(project, cfg)?;
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
    let jar_stem = jar.file_stem().unwrap_or_default().to_string_lossy();
    if let Some(ref deps) = cfg.dependencies {
        for (key, value) in deps {
            if !key.contains(':') { continue; }
            let artifact_id = key.split(':').last().unwrap_or("");
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

fn generate_idea_project(
    root: &Path,
    packages: &[String],
    ws: &WorkspaceGraph,
    java_version: &str,
    download_sources: bool,
) -> Result<()> {
    let idea_dir = root.join(".idea");
    std::fs::create_dir_all(&idea_dir)?;

    // misc.xml - JDK version
    let misc = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<project version="4">
  <component name="ProjectRootManager" version="2" languageLevel="JDK_{java_version}" default="true" project-jdk-name="{java_version}" project-jdk-type="JavaSDK">
    <output url="file://$PROJECT_DIR$/out" />
  </component>
</project>
"#
    );
    std::fs::write(idea_dir.join("misc.xml"), misc)?;

    // Generate .iml for each module
    let mut module_refs = Vec::new();
    let libraries_dir = idea_dir.join("libraries");
    std::fs::create_dir_all(&libraries_dir)?;
    let modules_dir = idea_dir.join("modules");
    std::fs::create_dir_all(&modules_dir)?;

    let mut all_jars: Vec<PathBuf> = Vec::new();

    for pkg_name in packages {
        let pkg = ws.get_package(pkg_name).unwrap();
        let rel_path = pathdiff(root, &pkg.path);
        let iml_path = modules_dir.join(format!("{}.iml", pkg_name));

        // Resolve Maven deps for this package
        let jars = super::build::resolve_deps_no_download(&pkg.path, &pkg.config)?;
        all_jars.extend(jars.clone());

        // Build module dependencies
        let ws_deps = pkg.config.workspace_module_deps();

        let mut dep_entries = String::new();
        for dep in &ws_deps {
            dep_entries.push_str(&format!(
                "    <orderEntry type=\"module\" module-name=\"{}\" />\n",
                dep
            ));
        }

        // Add library dependencies with scope mapping
        for jar in &jars {
            let jar_name = jar.file_stem().unwrap().to_string_lossy().to_string();
            let scope_attr = jar_to_idea_scope(jar, &pkg.config);
            dep_entries.push_str(&format!(
                "    <orderEntry type=\"library\" name=\"{}\" level=\"project\"{} />\n",
                jar_name, scope_attr
            ));
        }

        let base_url = format!("$PROJECT_DIR$/{}", rel_path);
            let source_folders = detect_source_folders(&pkg.path, &base_url);

        let iml = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<module type="JAVA_MODULE" version="4">
  <component name="NewModuleRootManager" inherit-compiler-output="false">
    <output url="file://{base_url}/out/classes" />
    <output-test url="file://{base_url}/out/test-classes" />
    <content url="file://{base_url}">
{source_folders}      <excludeFolder url="file://{base_url}/out" />
    </content>
    <orderEntry type="inheritedJdk" />
    <orderEntry type="sourceFolder" forTests="false" />
{dep_entries}  </component>
</module>
"#
        );
        std::fs::write(&iml_path, iml)?;

        module_refs.push(format!(
            "      <module fileurl=\"file://$PROJECT_DIR$/.idea/modules/{name}.iml\" filepath=\"$PROJECT_DIR$/.idea/modules/{name}.iml\" />",
            name = pkg_name
        ));
    }

    // Generate library XMLs for Maven deps
    all_jars.sort();
    all_jars.dedup();
    for jar in &all_jars {
        let jar_name = jar.file_stem().unwrap().to_string_lossy().to_string();
        let jar_abs = to_idea_path(&jar.to_string_lossy());
        let sources_section = make_sources_section(jar, download_sources);
        let lib_xml = format!(
            r#"<component name="libraryTable">
  <library name="{jar_name}">
    <CLASSES>
      <root url="jar://{jar_abs}!/" />
    </CLASSES>
{sources_section}  </library>
</component>
"#
        );
        std::fs::write(
            libraries_dir.join(format!("{}.xml", jar_name)),
            lib_xml,
        )?;
    }

    // compiler.xml — annotation processor config
    generate_compiler_xml(&idea_dir, packages, ws)?;

    // modules.xml
    let modules_xml = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<project version="4">
  <component name="ProjectModuleManager">
    <modules>
{}
    </modules>
  </component>
</project>
"#,
        module_refs.join("\n")
    );
    std::fs::write(idea_dir.join("modules.xml"), modules_xml)?;

    Ok(())
}

fn generate_single_project_idea(
    project: &Path,
    cfg: &config::schema::YmConfig,
    java_version: &str,
    download_sources: bool,
) -> Result<()> {
    let idea_dir = project.join(".idea");
    std::fs::create_dir_all(&idea_dir)?;

    let misc = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<project version="4">
  <component name="ProjectRootManager" version="2" languageLevel="JDK_{java_version}" default="true" project-jdk-name="{java_version}" project-jdk-type="JavaSDK">
    <output url="file://$PROJECT_DIR$/out" />
  </component>
</project>
"#
    );
    std::fs::write(idea_dir.join("misc.xml"), misc)?;

    // Resolve deps for library references
    let jars = super::build::resolve_deps_no_download(project, cfg)?;
    let libraries_dir = idea_dir.join("libraries");
    std::fs::create_dir_all(&libraries_dir)?;

    let mut dep_entries = String::new();
    for jar in &jars {
        let jar_name = jar.file_stem().unwrap().to_string_lossy().to_string();
        let jar_abs = to_idea_path(&jar.to_string_lossy());
        let scope_attr = jar_to_idea_scope(jar, cfg);
        dep_entries.push_str(&format!(
            "    <orderEntry type=\"library\" name=\"{}\" level=\"project\"{} />\n",
            jar_name, scope_attr
        ));

        let sources_section = make_sources_section(jar, download_sources);
        let lib_xml = format!(
            r#"<component name="libraryTable">
  <library name="{jar_name}">
    <CLASSES>
      <root url="jar://{jar_abs}!/" />
    </CLASSES>
{sources_section}  </library>
</component>
"#
        );
        std::fs::write(
            libraries_dir.join(format!("{}.xml", jar_name)),
            lib_xml,
        )?;
    }

    let base_url = "$PROJECT_DIR$";
    let source_folders = detect_source_folders(project, base_url);

    let iml = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<module type="JAVA_MODULE" version="4">
  <component name="NewModuleRootManager" inherit-compiler-output="false">
    <output url="file://{base_url}/out/classes" />
    <output-test url="file://{base_url}/out/test-classes" />
    <content url="file://{base_url}">
{source_folders}      <excludeFolder url="file://{base_url}/out" />
    </content>
    <orderEntry type="inheritedJdk" />
    <orderEntry type="sourceFolder" forTests="false" />
{dep_entries}  </component>
</module>
"#
    );
    let modules_dir = idea_dir.join("modules");
    std::fs::create_dir_all(&modules_dir)?;
    std::fs::write(
        modules_dir.join(format!("{}.iml", cfg.name)),
        iml,
    )?;

    // compiler.xml — annotation processor config
    generate_compiler_xml_single(&idea_dir, project, cfg)?;

    let modules_xml = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<project version="4">
  <component name="ProjectModuleManager">
    <modules>
      <module fileurl="file://$PROJECT_DIR$/.idea/modules/{name}.iml" filepath="$PROJECT_DIR$/.idea/modules/{name}.iml" />
    </modules>
  </component>
</project>
"#,
        name = cfg.name
    );
    std::fs::write(idea_dir.join("modules.xml"), modules_xml)?;

    Ok(())
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

/// Generate `.idea/compiler.xml` with annotation processor config for workspace mode.
fn generate_compiler_xml(
    idea_dir: &Path,
    packages: &[String],
    ws: &WorkspaceGraph,
) -> Result<()> {
    let mut ap_entries = Vec::new();

    for pkg_name in packages {
        let pkg = ws.get_package(pkg_name).unwrap();
        let jars = super::build::resolve_deps_no_download(&pkg.path, &pkg.config)?;
        let ap_jars = collect_annotation_processor_jars(&pkg.config, &jars);
        if !ap_jars.is_empty() {
            ap_entries.push((pkg_name.clone(), ap_jars));
        }
    }

    if ap_entries.is_empty() {
        return Ok(());
    }

    let mut profiles = String::new();
    for (module_name, jars) in &ap_entries {
        let mut processor_path = String::new();
        for jar in jars {
            let jar_abs = to_idea_path(&jar.to_string_lossy());
            processor_path.push_str(&format!(
                "        <entry name=\"{}\" />\n",
                jar_abs
            ));
        }
        profiles.push_str(&format!(
            r#"    <profile name="{module_name}" enabled="true">
      <processorPath useClasspath="false">
{processor_path}      </processorPath>
      <module name="{module_name}" />
    </profile>
"#
        ));
    }

    let compiler_xml = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<project version="4">
  <component name="CompilerConfiguration">
    <annotationProcessing>
{profiles}    </annotationProcessing>
  </component>
</project>
"#
    );
    std::fs::write(idea_dir.join("compiler.xml"), compiler_xml)?;
    Ok(())
}

/// Generate `.idea/compiler.xml` with annotation processor config for single project mode.
fn generate_compiler_xml_single(
    idea_dir: &Path,
    project: &Path,
    cfg: &config::schema::YmConfig,
) -> Result<()> {
    let jars = super::build::resolve_deps_no_download(project, cfg)?;
    let ap_jars = collect_annotation_processor_jars(cfg, &jars);

    if ap_jars.is_empty() {
        return Ok(());
    }

    let mut processor_path = String::new();
    for jar in &ap_jars {
        let jar_abs = to_idea_path(&jar.to_string_lossy());
        processor_path.push_str(&format!(
            "        <entry name=\"{}\" />\n",
            jar_abs
        ));
    }

    let compiler_xml = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<project version="4">
  <component name="CompilerConfiguration">
    <annotationProcessing>
    <profile name="Default" enabled="true">
      <processorPath useClasspath="false">
{processor_path}      </processorPath>
    </profile>
    </annotationProcessing>
  </component>
</project>
"#
    );
    std::fs::write(idea_dir.join("compiler.xml"), compiler_xml)?;
    Ok(())
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
                    let artifact_id = coord.split(':').last().unwrap_or("");
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

/// Detect source/test/resource folders for IDEA project generation.
/// Supports both Maven convention (src/main/java, src/test/java) and flat (src/, test/).
/// `base_url` is the IDEA URL prefix for the module root (e.g. "$PROJECT_DIR$" or "$PROJECT_DIR$/submodule").
fn detect_source_folders(project: &Path, base_url: &str) -> String {
    let mut folders = String::new();

    // Main sources
    let maven_src = project.join("src").join("main").join("java");
    if maven_src.exists() {
        folders.push_str(&format!("      <sourceFolder url=\"file://{}/src/main/java\" isTestSource=\"false\" />\n", base_url));
    } else if project.join("src").exists() {
        folders.push_str(&format!("      <sourceFolder url=\"file://{}/src\" isTestSource=\"false\" />\n", base_url));
    }

    // Main resources
    let maven_res = project.join("src").join("main").join("resources");
    if maven_res.exists() {
        folders.push_str(&format!("      <sourceFolder url=\"file://{}/src/main/resources\" type=\"java-resource\" />\n", base_url));
    }

    // Test sources
    let maven_test = project.join("src").join("test").join("java");
    if maven_test.exists() {
        folders.push_str(&format!("      <sourceFolder url=\"file://{}/src/test/java\" isTestSource=\"true\" />\n", base_url));
    } else if project.join("test").exists() {
        folders.push_str(&format!("      <sourceFolder url=\"file://{}/test\" isTestSource=\"true\" />\n", base_url));
    }

    // Test resources
    let maven_test_res = project.join("src").join("test").join("resources");
    if maven_test_res.exists() {
        folders.push_str(&format!("      <sourceFolder url=\"file://{}/src/test/resources\" type=\"java-test-resource\" />\n", base_url));
    }

    // Fallback if nothing detected
    if folders.is_empty() {
        folders.push_str(&format!("      <sourceFolder url=\"file://{}/src\" isTestSource=\"false\" />\n", base_url));
    }

    folders
}

/// Map a JAR file to its IDEA scope attribute based on the dependency's scope in config.
/// Returns empty string for COMPILE (default), or ` scope="RUNTIME"` etc.
fn jar_to_idea_scope(jar: &Path, cfg: &config::schema::YmConfig) -> String {
    // Extract artifactId from JAR filename to match against dependencies
    let jar_stem = jar.file_stem().unwrap_or_default().to_string_lossy();

    if let Some(ref deps) = cfg.dependencies {
        for (key, value) in deps {
            if !key.contains(':') {
                continue;
            }
            let artifact_id = key.split(':').last().unwrap_or("");
            // Check if JAR filename starts with the artifactId
            if jar_stem.starts_with(artifact_id) {
                let scope = value.scope();
                return match scope {
                    "runtime" => " scope=\"RUNTIME\"".to_string(),
                    "provided" => " scope=\"PROVIDED\"".to_string(),
                    "test" => " scope=\"TEST\"".to_string(),
                    _ => String::new(), // compile = default, no attribute needed
                };
            }
        }
    }
    String::new() // transitive deps default to COMPILE
}

fn pathdiff(base: &Path, target: &Path) -> String {
    target
        .strip_prefix(base)
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|_| target.to_string_lossy().to_string())
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
    use super::*;

    #[test]
    fn test_detect_source_folders_fallback() {
        let tmpdir = std::env::temp_dir().join("ym-idea-src-test");
        let _ = std::fs::remove_dir_all(&tmpdir);
        std::fs::create_dir_all(&tmpdir).unwrap();

        // No src dir at all -> fallback
        let folders = detect_source_folders(&tmpdir, "$MODULE_DIR$");
        assert!(folders.contains("$MODULE_DIR$/src"));

        let _ = std::fs::remove_dir_all(&tmpdir);
    }

    #[test]
    fn test_detect_source_folders_maven_convention() {
        let tmpdir = std::env::temp_dir().join("ym-idea-maven-test");
        let _ = std::fs::remove_dir_all(&tmpdir);
        std::fs::create_dir_all(tmpdir.join("src/main/java")).unwrap();
        std::fs::create_dir_all(tmpdir.join("src/test/java")).unwrap();
        std::fs::create_dir_all(tmpdir.join("src/main/resources")).unwrap();

        let folders = detect_source_folders(&tmpdir, "$MODULE_DIR$");
        assert!(folders.contains("src/main/java"));
        assert!(folders.contains("src/test/java"));
        assert!(folders.contains("src/main/resources"));

        let _ = std::fs::remove_dir_all(&tmpdir);
    }

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

    #[test]
    fn test_pathdiff_relative() {
        let base = Path::new("/home/user/project");
        let target = Path::new("/home/user/project/modules/core");
        assert_eq!(pathdiff(base, target), "modules/core");
    }

    #[test]
    fn test_pathdiff_absolute_fallback() {
        let base = Path::new("/home/user/project");
        let target = Path::new("/other/path");
        assert_eq!(pathdiff(base, target), "/other/path");
    }
}
