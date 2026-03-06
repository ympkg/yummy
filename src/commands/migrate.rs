use anyhow::{bail, Result};
use console::style;
use std::collections::BTreeMap;
use std::path::Path;

use crate::config;
use crate::config::schema::YmConfig;

/// Standalone `ym migrate` command
pub fn execute() -> Result<()> {
    let cwd = std::env::current_dir()?;
    let config_path = cwd.join(config::CONFIG_FILE);

    if config_path.exists() {
        bail!("package.json already exists. Remove it first to re-migrate.");
    }

    let pom = cwd.join("pom.xml");
    let gradle = cwd.join("build.gradle");
    let gradle_kts = cwd.join("build.gradle.kts");
    let settings_gradle = cwd.join("settings.gradle");
    let settings_gradle_kts = cwd.join("settings.gradle.kts");

    // Check for multi-module Gradle project (settings.gradle with include statements)
    if settings_gradle.exists() || settings_gradle_kts.exists() {
        let settings_path = if settings_gradle_kts.exists() {
            &settings_gradle_kts
        } else {
            &settings_gradle
        };
        let modules = parse_settings_gradle(settings_path)?;
        if !modules.is_empty() {
            println!(
                "  {} Migrating multi-module Gradle project ({} modules)...",
                style("→").blue(),
                modules.len()
            );
            return migrate_gradle_multimodule(&cwd, settings_path, &modules);
        }
    }

    // Check for multi-module Maven project
    if pom.exists() {
        let content = std::fs::read_to_string(&pom)?;
        let doc = roxmltree::Document::parse(&content)?;
        let modules = find_modules(&doc.root_element());
        if !modules.is_empty() {
            println!(
                "  {} Migrating multi-module Maven project ({} modules)...",
                style("→").blue(),
                modules.len()
            );
            return migrate_maven_multimodule(&cwd, &pom, &modules);
        }
    }

    // Single-project migration
    let cfg = if pom.exists() {
        println!("  {} Migrating from pom.xml...", style("→").blue());
        migrate_from_pom(&pom)?
    } else if gradle.exists() {
        println!("  {} Migrating from build.gradle...", style("→").blue());
        migrate_from_gradle(&gradle)?
    } else if gradle_kts.exists() {
        println!(
            "  {} Migrating from build.gradle.kts...",
            style("→").blue()
        );
        migrate_from_gradle(&gradle_kts)?
    } else {
        bail!("No pom.xml or build.gradle found in current directory");
    };

    config::save_config(&config_path, &cfg)?;
    print_migration_summary(&cfg);

    Ok(())
}

fn print_migration_summary(cfg: &YmConfig) {
    println!("  {} Created package.json", style("✓").green());

    let dep_count = cfg.dependencies.as_ref().map(|d| d.len()).unwrap_or(0);
    let dev_count = cfg.dev_dependencies.as_ref().map(|d| d.len()).unwrap_or(0);

    if dep_count > 0 || dev_count > 0 {
        println!(
            "  {} Migrated {} dependencies, {} devDependencies",
            style("✓").green(),
            dep_count,
            dev_count
        );
    }

    if let Some(ref java) = cfg.target {
        println!(
            "  {} Java version: {}",
            style("✓").green(),
            style(java).cyan()
        );
    }

    println!();
    println!("  Run {} to start developing", style("ym dev").cyan());
}

// --- Gradle multi-module migration ---

/// Parse settings.gradle(.kts) to extract included module paths.
fn parse_settings_gradle(path: &Path) -> Result<Vec<String>> {
    let content = std::fs::read_to_string(path)?;
    let mut modules = Vec::new();

    for line in content.lines() {
        let line = line.trim();
        // Match: include 'module-a', ':module-b', ':parent:child'
        // Match: include("module-a", ":module-b")
        if !line.starts_with("include") {
            continue;
        }

        // Extract all quoted strings from the line
        let mut i = 0;
        let chars: Vec<char> = line.chars().collect();
        while i < chars.len() {
            if chars[i] == '\'' || chars[i] == '"' {
                let quote = chars[i];
                i += 1;
                let start = i;
                while i < chars.len() && chars[i] != quote {
                    i += 1;
                }
                if i < chars.len() {
                    let module = &line[start..i];
                    // Strip leading colon and convert : to /
                    let module_path = module.trim_start_matches(':').replace(':', "/");
                    if !module_path.is_empty() {
                        modules.push(module_path);
                    }
                }
            }
            i += 1;
        }
    }

    Ok(modules)
}

/// Migrate a Gradle multi-module project.
fn migrate_gradle_multimodule(root: &Path, _settings_path: &Path, modules: &[String]) -> Result<()> {
    let root_gradle = root.join("build.gradle.kts");
    let root_gradle_alt = root.join("build.gradle");
    let root_gradle_path = if root_gradle.exists() {
        Some(root_gradle.as_path())
    } else if root_gradle_alt.exists() {
        Some(root_gradle_alt.as_path())
    } else {
        None
    };

    // Parse root build.gradle for shared settings
    let mut root_cfg = if let Some(path) = root_gradle_path {
        migrate_from_gradle(path)?
    } else {
        YmConfig::default()
    };

    if root_cfg.name.is_empty() {
        root_cfg.name = root
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("my-project")
            .to_string();
    }

    // Determine workspace patterns from module paths
    let mut workspace_dirs: Vec<String> = Vec::new();
    for module in modules {
        if let Some(parent) = Path::new(module).parent() {
            let parent_str = if parent.as_os_str().is_empty() {
                ".".to_string()
            } else {
                parent.to_string_lossy().to_string()
            };
            let pattern = format!("{}/*", parent_str);
            if !workspace_dirs.contains(&pattern) {
                workspace_dirs.push(pattern);
            }
        }
    }
    if workspace_dirs.is_empty() {
        workspace_dirs.push("./*".to_string());
    }

    root_cfg.private = Some(true);
    root_cfg.workspaces = Some(workspace_dirs);
    // Don't include deps in root if they're per-module
    if modules.len() > 1 {
        // Keep root deps as shared deps
    }

    let root_config_path = root.join(config::CONFIG_FILE);
    config::save_config(&root_config_path, &root_cfg)?;
    println!("  {} Created root package.json", style("✓").green());

    // All module names for inter-module dependency detection
    let all_module_names: Vec<String> = modules
        .iter()
        .map(|m| {
            Path::new(m)
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string()
        })
        .collect();

    // Migrate each submodule
    let mut migrated = 0;
    for module_path in modules {
        let module_dir = root.join(module_path);
        if !module_dir.exists() {
            continue;
        }

        let module_config_path = module_dir.join(config::CONFIG_FILE);
        if module_config_path.exists() {
            continue; // Already migrated
        }

        let gradle_file = module_dir.join("build.gradle.kts");
        let gradle_file_alt = module_dir.join("build.gradle");
        let gradle_path = if gradle_file.exists() {
            gradle_file
        } else if gradle_file_alt.exists() {
            gradle_file_alt
        } else {
            continue;
        };

        let mut module_cfg = migrate_from_gradle_with_projects(&gradle_path, &all_module_names)?;

        // Use directory name as module name if not set
        if module_cfg.name.is_empty() {
            module_cfg.name = module_dir
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("module")
                .to_string();
        }

        config::save_config(&module_config_path, &module_cfg)?;
        migrated += 1;
    }

    println!(
        "  {} Migrated {} submodules",
        style("✓").green(),
        migrated
    );
    println!();
    println!(
        "  Run {} to resolve dependencies",
        style("ym install").cyan()
    );

    Ok(())
}

/// Parse a Gradle build file, detecting inter-module (project) dependencies.
fn migrate_from_gradle_with_projects(
    gradle_path: &Path,
    all_module_names: &[String],
) -> Result<YmConfig> {
    let content = std::fs::read_to_string(gradle_path)?;

    let mut config = YmConfig::default();
    let mut dependencies = BTreeMap::new();
    let mut dev_dependencies = BTreeMap::new();
    let mut workspace_deps = Vec::new();

    for line in content.lines() {
        let line = line.trim();

        // Extract group/version
        if line.starts_with("group") && line.contains('=') {
            if let Some(val) = extract_string_value(line) {
                config.name = val;
            }
        }
        if line.starts_with("version") && line.contains('=') {
            if let Some(val) = extract_string_value(line) {
                config.version = Some(val);
            }
        }

        // sourceCompatibility
        if line.starts_with("sourceCompatibility") {
            if let Some(val) = extract_string_value(line) {
                config.target = Some(val.trim_start_matches("JavaVersion.VERSION_").to_string());
            }
        }

        // Detect inter-module project dependencies
        // implementation project(':module-name')
        // implementation(project(":module-name"))
        if let Some(proj_dep) = parse_gradle_project_dependency(line) {
            let dep_name = proj_dep.trim_start_matches(':').replace(':', "-");
            // Check this is actually a known module
            if all_module_names.iter().any(|n| *n == dep_name) {
                if !workspace_deps.contains(&dep_name) {
                    workspace_deps.push(dep_name);
                }
                continue;
            }
        }

        // External dependencies
        if let Some(dep) = parse_gradle_dependency(line, "implementation")
            .or_else(|| parse_gradle_dependency(line, "api"))
            .or_else(|| parse_gradle_dependency(line, "compile"))
        {
            dependencies.insert(dep.0, dep.1);
        }
        if let Some(dep) = parse_gradle_dependency(line, "testImplementation")
            .or_else(|| parse_gradle_dependency(line, "testCompile"))
        {
            dev_dependencies.insert(dep.0, dep.1);
        }
    }

    if config.name.is_empty() {
        config.name = gradle_path
            .parent()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            .unwrap_or("my-app")
            .to_string();
    }

    if !dependencies.is_empty() {
        config.dependencies = Some(dependencies);
    }
    if !dev_dependencies.is_empty() {
        config.dev_dependencies = Some(dev_dependencies);
    }
    if !workspace_deps.is_empty() {
        config.workspace_dependencies = Some(workspace_deps);
    }

    Ok(config)
}

/// Parse project(':module-name') from a Gradle dependency line.
fn parse_gradle_project_dependency(line: &str) -> Option<String> {
    // Match patterns like:
    //   implementation project(':module-name')
    //   implementation(project(":module-name"))
    //   api project(':module-name')
    let prefixes = [
        "implementation", "api", "compile",
        "testImplementation", "testCompile",
    ];
    let is_dep_line = prefixes.iter().any(|p| line.starts_with(p));
    if !is_dep_line {
        return None;
    }

    // Look for project( or project (
    let proj_idx = line.find("project(")?;
    let after_proj = &line[proj_idx + 8..]; // skip "project("
    // Find the quoted module name
    let quote_start = after_proj.find('\'').or_else(|| after_proj.find('"'))?;
    let quote_char = after_proj.chars().nth(quote_start)?;
    let rest = &after_proj[quote_start + 1..];
    let quote_end = rest.find(quote_char)?;
    Some(rest[..quote_end].to_string())
}

// --- Maven multi-module migration ---

/// Migrate a Maven multi-module project.
fn migrate_maven_multimodule(root: &Path, root_pom: &Path, modules: &[String]) -> Result<()> {
    // Parse root POM for shared settings
    let mut root_cfg = migrate_from_pom(root_pom)?;

    // workspaces is already set by migrate_from_pom via find_modules
    // Make sure it uses direct module names
    root_cfg.private = Some(true);
    root_cfg.workspaces = Some(modules.iter().map(|m| format!("{}/*", m)).collect());

    // Deduplicate workspace patterns
    let mut seen = std::collections::HashSet::new();
    if let Some(ref mut ws) = root_cfg.workspaces {
        ws.retain(|p| seen.insert(p.clone()));
    }

    let root_config_path = root.join(config::CONFIG_FILE);
    config::save_config(&root_config_path, &root_cfg)?;
    println!("  {} Created root package.json", style("✓").green());

    // Collect all module artifact IDs for inter-module dep detection
    let mut module_artifacts: BTreeMap<String, String> = BTreeMap::new(); // artifactId -> module_path
    for module_path in modules {
        let module_pom = root.join(module_path).join("pom.xml");
        if module_pom.exists() {
            if let Ok(content) = std::fs::read_to_string(&module_pom) {
                if let Ok(doc) = roxmltree::Document::parse(&content) {
                    if let Some(aid) = find_child_text(&doc.root_element(), "artifactId") {
                        module_artifacts.insert(aid, module_path.clone());
                    }
                }
            }
        }
    }

    // Migrate each submodule
    let mut migrated = 0;
    for module_path in modules {
        let module_dir = root.join(module_path);
        let module_pom = module_dir.join("pom.xml");
        if !module_pom.exists() {
            continue;
        }

        let module_config_path = module_dir.join(config::CONFIG_FILE);
        if module_config_path.exists() {
            continue;
        }

        let mut module_cfg = migrate_from_pom(&module_pom)?;

        // Detect inter-module dependencies: if a dependency's artifactId matches
        // a sibling module, convert it to a workspaceDependency
        let mut workspace_deps = Vec::new();
        if let Some(ref mut deps) = module_cfg.dependencies {
            let keys_to_remove: Vec<String> = deps
                .keys()
                .filter(|coord| {
                    let parts: Vec<&str> = coord.split(':').collect();
                    parts.len() == 2 && module_artifacts.contains_key(parts[1])
                })
                .cloned()
                .collect();
            for key in keys_to_remove {
                deps.remove(&key);
                let parts: Vec<&str> = key.split(':').collect();
                if parts.len() == 2 {
                    workspace_deps.push(parts[1].to_string());
                }
            }
            if deps.is_empty() {
                module_cfg.dependencies = None;
            }
        }
        if !workspace_deps.is_empty() {
            module_cfg.workspace_dependencies = Some(workspace_deps);
        }

        // Clear workspaces on submodules (only root has workspaces)
        module_cfg.workspaces = None;
        module_cfg.private = None;

        config::save_config(&module_config_path, &module_cfg)?;
        migrated += 1;
    }

    println!(
        "  {} Migrated {} submodules",
        style("✓").green(),
        migrated
    );
    println!();
    println!(
        "  Run {} to resolve dependencies",
        style("ym install").cyan()
    );

    Ok(())
}

/// Parse a Maven pom.xml and convert to YmConfig
pub fn migrate_from_pom(pom_path: &Path) -> Result<YmConfig> {
    let content = std::fs::read_to_string(pom_path)?;
    let doc = roxmltree::Document::parse(&content)?;

    let root = doc.root_element();

    let name = find_child_text(&root, "artifactId").unwrap_or_else(|| "my-app".to_string());
    let version = find_child_text(&root, "version");
    let _group_id = find_child_text(&root, "groupId");

    // Detect Java version from maven-compiler-plugin or properties
    let java_version = detect_java_version(&root);

    // Extract dependencies
    let mut dependencies = BTreeMap::new();
    let mut dev_dependencies = BTreeMap::new();

    for node in root.descendants() {
        if node.tag_name().name() != "dependencies" {
            continue;
        }
        // Skip dependencyManagement
        if let Some(parent) = node.parent() {
            if parent.tag_name().name() == "dependencyManagement" {
                continue;
            }
        }

        for dep in node.children() {
            if dep.tag_name().name() != "dependency" {
                continue;
            }

            let dep_group = find_child_text(&dep, "groupId");
            let dep_artifact = find_child_text(&dep, "artifactId");
            let dep_version = find_child_text(&dep, "version");
            let dep_scope = find_child_text(&dep, "scope");

            if let (Some(g), Some(a)) = (dep_group, dep_artifact) {
                let coord = format!("{}:{}", g, a);
                let ver = dep_version.unwrap_or_else(|| "LATEST".to_string());

                // Skip property references
                if ver.contains("${") {
                    continue;
                }

                match dep_scope.as_deref() {
                    Some("test") => {
                        dev_dependencies.insert(coord, ver);
                    }
                    Some("provided") | Some("system") => {
                        // Skip provided/system scope
                    }
                    _ => {
                        dependencies.insert(coord, ver);
                    }
                }
            }
        }
    }

    // Detect main class from exec-maven-plugin
    let main_class = detect_main_class(&root);

    // Build config
    let mut config = YmConfig {
        name,
        version,
        target: java_version,
        main: main_class,
        ..Default::default()
    };

    if !dependencies.is_empty() {
        config.dependencies = Some(dependencies);
    }
    if !dev_dependencies.is_empty() {
        config.dev_dependencies = Some(dev_dependencies);
    }

    // Detect if this is a multi-module project
    let modules = find_modules(&root);
    if !modules.is_empty() {
        config.private = Some(true);
        config.workspaces = Some(modules.iter().map(|m| format!("{}/*", m)).collect());
    }

    Ok(config)
}

/// Parse a build.gradle and extract basic info
pub fn migrate_from_gradle(gradle_path: &Path) -> Result<YmConfig> {
    migrate_from_gradle_with_projects(gradle_path, &[])
}

fn find_child_text(node: &roxmltree::Node, name: &str) -> Option<String> {
    node.children()
        .find(|n| n.tag_name().name() == name)
        .and_then(|n| n.text())
        .map(|s| s.trim().to_string())
}

fn detect_java_version(root: &roxmltree::Node) -> Option<String> {
    // Check <properties><maven.compiler.source>
    for node in root.descendants() {
        if node.tag_name().name() == "properties" {
            for prop in node.children() {
                let name = prop.tag_name().name();
                if name == "maven.compiler.source"
                    || name == "maven.compiler.target"
                    || name == "maven.compiler.release"
                    || name == "java.version"
                {
                    if let Some(text) = prop.text() {
                        let v = text.trim().to_string();
                        if !v.contains("${") {
                            return Some(v);
                        }
                    }
                }
            }
        }
    }
    None
}

fn detect_main_class(root: &roxmltree::Node) -> Option<String> {
    for node in root.descendants() {
        if node.tag_name().name() == "mainClass" {
            if let Some(text) = node.text() {
                let v = text.trim().to_string();
                if !v.contains("${") {
                    return Some(v);
                }
            }
        }
    }
    None
}

fn find_modules(root: &roxmltree::Node) -> Vec<String> {
    let mut modules = Vec::new();
    for node in root.descendants() {
        if node.tag_name().name() == "modules" {
            for child in node.children() {
                if child.tag_name().name() == "module" {
                    if let Some(text) = child.text() {
                        modules.push(text.trim().to_string());
                    }
                }
            }
        }
    }
    modules
}

fn extract_string_value(line: &str) -> Option<String> {
    // Handle: key = 'value' or key = "value" or key = value
    let parts: Vec<&str> = line.splitn(2, '=').collect();
    if parts.len() != 2 {
        return None;
    }
    let val = parts[1].trim().trim_matches('\'').trim_matches('"').trim();
    if val.is_empty() {
        None
    } else {
        Some(val.to_string())
    }
}

fn parse_gradle_dependency(line: &str, prefix: &str) -> Option<(String, String)> {
    if !line.starts_with(prefix) {
        return None;
    }
    // Skip project() dependencies
    if line.contains("project(") {
        return None;
    }
    // implementation 'group:artifact:version'
    // implementation "group:artifact:version"
    let start = line.find('\'').or_else(|| line.find('"'))?;
    let end = line.rfind('\'').or_else(|| line.rfind('"'))?;
    if start >= end {
        return None;
    }
    let dep = &line[start + 1..end];
    let parts: Vec<&str> = dep.split(':').collect();
    if parts.len() >= 3 {
        let coord = format!("{}:{}", parts[0], parts[1]);
        let version = parts[2].to_string();
        Some((coord, version))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_settings_gradle_basic() {
        let tmpdir = std::env::temp_dir().join("ym-settings-test");
        let _ = std::fs::remove_dir_all(&tmpdir);
        std::fs::create_dir_all(&tmpdir).unwrap();

        std::fs::write(
            tmpdir.join("settings.gradle"),
            "include ':module-a', ':module-b'\ninclude ':parent:child'\n",
        )
        .unwrap();

        let modules = parse_settings_gradle(&tmpdir.join("settings.gradle")).unwrap();
        assert!(modules.contains(&"module-a".to_string()));
        assert!(modules.contains(&"module-b".to_string()));
        assert!(modules.contains(&"parent/child".to_string()));

        let _ = std::fs::remove_dir_all(&tmpdir);
    }

    #[test]
    fn test_parse_settings_gradle_kts() {
        let tmpdir = std::env::temp_dir().join("ym-settings-kts-test");
        let _ = std::fs::remove_dir_all(&tmpdir);
        std::fs::create_dir_all(&tmpdir).unwrap();

        std::fs::write(
            tmpdir.join("settings.gradle.kts"),
            r#"include(":core", ":web", ":api")"#,
        )
        .unwrap();

        let modules = parse_settings_gradle(&tmpdir.join("settings.gradle.kts")).unwrap();
        assert_eq!(modules.len(), 3);
        assert!(modules.contains(&"core".to_string()));
        assert!(modules.contains(&"web".to_string()));
        assert!(modules.contains(&"api".to_string()));

        let _ = std::fs::remove_dir_all(&tmpdir);
    }

    #[test]
    fn test_parse_gradle_project_dependency() {
        assert_eq!(
            parse_gradle_project_dependency("implementation project(':module-a')"),
            Some(":module-a".to_string())
        );
        assert_eq!(
            parse_gradle_project_dependency(r#"implementation(project(":core"))"#),
            Some(":core".to_string())
        );
        assert_eq!(
            parse_gradle_project_dependency("implementation 'com.example:lib:1.0'"),
            None
        );
    }

    #[test]
    fn test_parse_gradle_dependency_skips_project() {
        assert!(parse_gradle_dependency("implementation project(':core')", "implementation").is_none());
    }

    #[test]
    fn test_migrate_from_pom_basic() {
        let tmpdir = std::env::temp_dir().join("ym-pom-migrate-test");
        let _ = std::fs::remove_dir_all(&tmpdir);
        std::fs::create_dir_all(&tmpdir).unwrap();

        let pom = r#"<?xml version="1.0"?>
<project>
    <groupId>com.example</groupId>
    <artifactId>my-app</artifactId>
    <version>1.0.0</version>
    <properties>
        <maven.compiler.source>17</maven.compiler.source>
    </properties>
    <dependencies>
        <dependency>
            <groupId>com.google.guava</groupId>
            <artifactId>guava</artifactId>
            <version>33.0.0-jre</version>
        </dependency>
        <dependency>
            <groupId>junit</groupId>
            <artifactId>junit</artifactId>
            <version>4.13</version>
            <scope>test</scope>
        </dependency>
    </dependencies>
</project>"#;
        let pom_path = tmpdir.join("pom.xml");
        std::fs::write(&pom_path, pom).unwrap();

        let cfg = migrate_from_pom(&pom_path).unwrap();
        assert_eq!(cfg.name, "my-app");
        assert_eq!(cfg.version.as_deref(), Some("1.0.0"));
        assert_eq!(cfg.target.as_deref(), Some("17"));
        let deps = cfg.dependencies.unwrap();
        assert!(deps.contains_key("com.google.guava:guava"));
        let dev_deps = cfg.dev_dependencies.unwrap();
        assert!(dev_deps.contains_key("junit:junit"));

        let _ = std::fs::remove_dir_all(&tmpdir);
    }

    #[test]
    fn test_migrate_from_gradle_basic() {
        let tmpdir = std::env::temp_dir().join("ym-gradle-migrate-test");
        let _ = std::fs::remove_dir_all(&tmpdir);
        std::fs::create_dir_all(&tmpdir).unwrap();

        let gradle = r#"
group = 'com.example'
version = '2.0.0'
sourceCompatibility = '21'

dependencies {
    implementation 'org.springframework:spring-core:5.3.0'
    testImplementation 'junit:junit:4.13'
}
"#;
        let gradle_path = tmpdir.join("build.gradle");
        std::fs::write(&gradle_path, gradle).unwrap();

        let cfg = migrate_from_gradle(&gradle_path).unwrap();
        assert_eq!(cfg.name, "com.example");
        assert_eq!(cfg.version.as_deref(), Some("2.0.0"));
        assert_eq!(cfg.target.as_deref(), Some("21"));
        let deps = cfg.dependencies.unwrap();
        assert!(deps.contains_key("org.springframework:spring-core"));

        let _ = std::fs::remove_dir_all(&tmpdir);
    }

    #[test]
    fn test_find_modules_maven() {
        let pom = r#"<?xml version="1.0"?>
<project>
    <modules>
        <module>core</module>
        <module>web</module>
        <module>api</module>
    </modules>
</project>"#;
        let doc = roxmltree::Document::parse(pom).unwrap();
        let modules = find_modules(&doc.root_element());
        assert_eq!(modules, vec!["core", "web", "api"]);
    }
}
