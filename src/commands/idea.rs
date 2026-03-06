use anyhow::Result;
use console::style;
use std::path::{Path, PathBuf};

use crate::config;
use crate::workspace::graph::WorkspaceGraph;

pub fn execute(target: Option<String>, download_sources: bool) -> Result<()> {
    let (config_path, cfg) = config::load_or_find_config()?;
    let project = config::project_dir(&config_path);

    let java_version = cfg.target.as_deref().unwrap_or("21");

    if cfg.workspaces.is_some() {
        // Workspace mode: generate for target and its dependencies
        let target = target.as_deref().unwrap_or_else(|| {
            eprintln!("  In workspace mode, specify a target: ym idea <module>");
            std::process::exit(1);
        });

        let ws = WorkspaceGraph::build(&project)?;
        let packages = ws.transitive_closure(target)?;

        generate_idea_project(&project, &packages, &ws, java_version, download_sources)?;

        println!(
            "  {} Generated IDEA project for {} ({} modules)",
            style("✓").green(),
            style(target).bold(),
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

    let mut all_jars: Vec<PathBuf> = Vec::new();

    for pkg_name in packages {
        let pkg = ws.get_package(pkg_name).unwrap();
        let rel_path = pathdiff(root, &pkg.path);
        let iml_path = pkg.path.join(format!("{}.iml", pkg_name));

        // Resolve Maven deps for this package
        let jars = super::build::resolve_deps(&pkg.path, &pkg.config)?;
        all_jars.extend(jars.clone());

        // Build module dependencies
        let ws_deps = pkg.config.workspace_dependencies.as_ref().cloned().unwrap_or_default();

        let mut dep_entries = String::new();
        for dep in &ws_deps {
            dep_entries.push_str(&format!(
                "    <orderEntry type=\"module\" module-name=\"{}\" />\n",
                dep
            ));
        }

        // Add library dependencies
        for jar in &jars {
            let jar_name = jar.file_stem().unwrap().to_string_lossy().to_string();
            dep_entries.push_str(&format!(
                "    <orderEntry type=\"library\" name=\"{}\" level=\"project\" />\n",
                jar_name
            ));
        }

            let source_folders = detect_source_folders(&pkg.path);

        let iml = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<module type="JAVA_MODULE" version="4">
  <component name="NewModuleRootManager" inherit-compiler-output="false">
    <output url="file://$MODULE_DIR$/out/classes" />
    <output-test url="file://$MODULE_DIR$/out/test-classes" />
    <content url="file://$MODULE_DIR$">
{source_folders}      <excludeFolder url="file://$MODULE_DIR$/out" />
    </content>
    <orderEntry type="inheritedJdk" />
    <orderEntry type="sourceFolder" forTests="false" />
{dep_entries}  </component>
</module>
"#
        );
        std::fs::write(&iml_path, iml)?;

        module_refs.push(format!(
            "      <module fileurl=\"file://$PROJECT_DIR$/{rel}/{name}.iml\" filepath=\"$PROJECT_DIR$/{rel}/{name}.iml\" />",
            rel = rel_path,
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
    let jars = super::build::resolve_deps(project, cfg)?;
    let libraries_dir = idea_dir.join("libraries");
    std::fs::create_dir_all(&libraries_dir)?;

    let mut dep_entries = String::new();
    for jar in &jars {
        let jar_name = jar.file_stem().unwrap().to_string_lossy().to_string();
        let jar_abs = to_idea_path(&jar.to_string_lossy());
        dep_entries.push_str(&format!(
            "    <orderEntry type=\"library\" name=\"{}\" level=\"project\" />\n",
            jar_name
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

    let source_folders = detect_source_folders(project);

    let iml = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<module type="JAVA_MODULE" version="4">
  <component name="NewModuleRootManager" inherit-compiler-output="false">
    <output url="file://$MODULE_DIR$/out/classes" />
    <output-test url="file://$MODULE_DIR$/out/test-classes" />
    <content url="file://$MODULE_DIR$">
{source_folders}      <excludeFolder url="file://$MODULE_DIR$/out" />
    </content>
    <orderEntry type="inheritedJdk" />
    <orderEntry type="sourceFolder" forTests="false" />
{dep_entries}  </component>
</module>
"#
    );
    std::fs::write(
        project.join(format!("{}.iml", cfg.name)),
        iml,
    )?;

    let modules_xml = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<project version="4">
  <component name="ProjectModuleManager">
    <modules>
      <module fileurl="file://$PROJECT_DIR$/{name}.iml" filepath="$PROJECT_DIR$/{name}.iml" />
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
                        .user_agent("ym/0.1.0")
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

/// Detect source/test/resource folders for IDEA project generation.
/// Supports both Maven convention (src/main/java, src/test/java) and flat (src/, test/).
fn detect_source_folders(project: &Path) -> String {
    let mut folders = String::new();

    // Main sources
    let maven_src = project.join("src").join("main").join("java");
    if maven_src.exists() {
        folders.push_str("      <sourceFolder url=\"file://$MODULE_DIR$/src/main/java\" isTestSource=\"false\" />\n");
    } else if project.join("src").exists() {
        folders.push_str("      <sourceFolder url=\"file://$MODULE_DIR$/src\" isTestSource=\"false\" />\n");
    }

    // Main resources
    let maven_res = project.join("src").join("main").join("resources");
    if maven_res.exists() {
        folders.push_str("      <sourceFolder url=\"file://$MODULE_DIR$/src/main/resources\" type=\"java-resource\" />\n");
    }

    // Test sources
    let maven_test = project.join("src").join("test").join("java");
    if maven_test.exists() {
        folders.push_str("      <sourceFolder url=\"file://$MODULE_DIR$/src/test/java\" isTestSource=\"true\" />\n");
    } else if project.join("test").exists() {
        folders.push_str("      <sourceFolder url=\"file://$MODULE_DIR$/test\" isTestSource=\"true\" />\n");
    }

    // Test resources
    let maven_test_res = project.join("src").join("test").join("resources");
    if maven_test_res.exists() {
        folders.push_str("      <sourceFolder url=\"file://$MODULE_DIR$/src/test/resources\" type=\"java-test-resource\" />\n");
    }

    // Fallback if nothing detected
    if folders.is_empty() {
        folders.push_str("      <sourceFolder url=\"file://$MODULE_DIR$/src\" isTestSource=\"false\" />\n");
    }

    folders
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
        let folders = detect_source_folders(&tmpdir);
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

        let folders = detect_source_folders(&tmpdir);
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
