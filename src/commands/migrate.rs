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
        bail!("ym.json already exists. Remove it first to re-migrate.");
    }

    let pom = cwd.join("pom.xml");
    let gradle = cwd.join("build.gradle");
    let gradle_kts = cwd.join("build.gradle.kts");

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

    println!("  {} Created ym.json", style("✓").green());

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
    let content = std::fs::read_to_string(gradle_path)?;

    let mut config = YmConfig::default();
    let mut dependencies = BTreeMap::new();
    let mut dev_dependencies = BTreeMap::new();

    for line in content.lines() {
        let line = line.trim();

        // Extract group/version
        if line.starts_with("group") && line.contains('=') {
            // group = 'com.example'
            if let Some(val) = extract_string_value(line) {
                config.name = val;
            }
        }
        if line.starts_with("version") && line.contains('=') {
            if let Some(val) = extract_string_value(line) {
                config.version = Some(val);
            }
        }

        // Extract sourceCompatibility
        if line.starts_with("sourceCompatibility") {
            if let Some(val) = extract_string_value(line) {
                config.target = Some(val.trim_start_matches("JavaVersion.VERSION_").to_string());
            }
        }

        // Extract dependencies
        // implementation 'com.google.guava:guava:33.0.0-jre'
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

    Ok(config)
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
