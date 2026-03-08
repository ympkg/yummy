use anyhow::{bail, Result};
use console::style;
use std::collections::BTreeMap;
use std::path::Path;

use crate::config;
use crate::config::schema::{DependencySpec, DependencyValue, YmConfig};

/// Standalone `ym migrate` command
pub fn execute(verify: bool) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let config_path = cwd.join(config::CONFIG_FILE);

    if config_path.exists() {
        bail!("package.toml already exists. Remove it first to re-migrate.");
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
            migrate_gradle_multimodule(&cwd, settings_path, &modules)?;
            if verify {
                run_post_migration_verify()?;
            }
            return Ok(());
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
            migrate_maven_multimodule(&cwd, &pom, &modules)?;
            if verify {
                run_post_migration_verify()?;
            }
            return Ok(());
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

    if verify {
        run_post_migration_verify()?;
    }

    Ok(())
}

/// Run post-migration verification: resolve deps + build
fn run_post_migration_verify() -> Result<()> {
    println!();
    println!(
        "  {} Verifying migration...",
        style("→").blue()
    );

    match super::build::execute(None, false) {
        Ok(()) => {
            println!(
                "  {} Migration verified — build succeeded",
                style("✓").green()
            );
            Ok(())
        }
        Err(e) => {
            eprintln!(
                "  {} Migration verification failed: {}",
                style("✗").red(),
                e
            );
            eprintln!(
                "  {} package.toml was generated but may need manual adjustments",
                style("!").yellow()
            );
            Ok(()) // Don't fail the convert command itself
        }
    }
}

fn print_migration_summary(cfg: &YmConfig) {
    println!("  {} Created package.toml", style("✓").green());

    let dep_count = cfg.dependencies.as_ref().map(|d| d.len()).unwrap_or(0);

    if dep_count > 0 {
        println!(
            "  {} Migrated {} dependencies",
            style("✓").green(),
            dep_count
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
    println!("  {} Created root package.toml", style("✓").green());

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
/// Parse Gradle Version Catalog (gradle/libs.versions.toml).
/// Returns a map of alias → (groupId:artifactId, version).
fn parse_version_catalog(project_dir: &Path) -> BTreeMap<String, (String, String)> {
    let catalog_path = project_dir.join("gradle").join("libs.versions.toml");
    let mut result = BTreeMap::new();
    let content = match std::fs::read_to_string(&catalog_path) {
        Ok(c) => c,
        Err(_) => return result,
    };
    let doc: toml::Value = match content.parse() {
        Ok(v) => v,
        Err(_) => return result,
    };

    // Collect [versions]
    let mut versions: BTreeMap<String, String> = BTreeMap::new();
    if let Some(vers) = doc.get("versions").and_then(|v| v.as_table()) {
        for (k, v) in vers {
            if let Some(s) = v.as_str() {
                versions.insert(k.clone(), s.to_string());
            }
        }
    }

    // Collect [libraries]
    if let Some(libs) = doc.get("libraries").and_then(|v| v.as_table()) {
        for (alias, val) in libs {
            let (module, version) = match val {
                toml::Value::String(s) => {
                    // "group:artifact:version" shorthand
                    let parts: Vec<&str> = s.splitn(3, ':').collect();
                    if parts.len() == 3 {
                        (format!("{}:{}", parts[0], parts[1]), parts[2].to_string())
                    } else {
                        continue;
                    }
                }
                toml::Value::Table(t) => {
                    let module = t.get("module").and_then(|m| m.as_str()).unwrap_or("");
                    if module.is_empty() {
                        // Try group + name format
                        let g = t.get("group").and_then(|v| v.as_str()).unwrap_or("");
                        let n = t.get("name").and_then(|v| v.as_str()).unwrap_or("");
                        if g.is_empty() || n.is_empty() { continue; }
                        let module = format!("{}:{}", g, n);
                        let ver = resolve_catalog_version(t, &versions);
                        if ver.is_empty() { continue; }
                        (module, ver)
                    } else {
                        let ver = resolve_catalog_version(t, &versions);
                        if ver.is_empty() { continue; }
                        (module.to_string(), ver)
                    }
                }
                _ => continue,
            };
            // Normalize alias: spring-boot-starter-web → spring.boot.starter.web (for Gradle accessor matching)
            result.insert(alias.clone(), (module, version));
        }
    }

    result
}

/// Resolve version from a catalog library entry.
fn resolve_catalog_version(table: &toml::value::Table, versions: &BTreeMap<String, String>) -> String {
    // Direct version string
    if let Some(v) = table.get("version") {
        if let Some(s) = v.as_str() {
            return s.to_string();
        }
        // version.ref = "key"
        if let Some(t) = v.as_table() {
            if let Some(ref_key) = t.get("ref").and_then(|r| r.as_str()) {
                return versions.get(ref_key).cloned().unwrap_or_default();
            }
        }
    }
    // version.ref at top level (TOML flattened: "version.ref" = "key")
    if let Some(vr) = table.get("version.ref").and_then(|v| v.as_str()) {
        return versions.get(vr).cloned().unwrap_or_default();
    }
    String::new()
}

fn migrate_from_gradle_with_projects(
    gradle_path: &Path,
    all_module_names: &[String],
) -> Result<YmConfig> {
    let content = std::fs::read_to_string(gradle_path)?;

    // Load Version Catalog if available
    let project_dir = gradle_path.parent().unwrap_or(Path::new("."));
    let catalog = parse_version_catalog(project_dir);

    let mut config = YmConfig::default();
    let mut dependencies: BTreeMap<String, DependencyValue> = BTreeMap::new();

    // Pre-scan for Spring Boot plugin version (used for BOM-managed dependencies)
    let mut spring_boot_version: Option<String> = None;
    for line in content.lines() {
        let line = line.trim();
        // id 'org.springframework.boot' version '3.4.0'
        // id("org.springframework.boot") version "3.4.0"
        if line.contains("org.springframework.boot") && line.contains("version") {
            if let Some(start) = line.rfind('\'').or_else(|| line.rfind('"')) {
                let before = &line[..start];
                if let Some(vstart) = before.rfind('\'').or_else(|| before.rfind('"')) {
                    let ver = &line[vstart + 1..start];
                    if !ver.is_empty() && ver.chars().next().map_or(false, |c| c.is_ascii_digit()) {
                        spring_boot_version = Some(ver.to_string());
                    }
                }
            }
        }
    }

    // Pre-scan settings.gradle for rootProject.name
    let settings_path = project_dir.join("settings.gradle");
    let settings_kts_path = project_dir.join("settings.gradle.kts");
    let settings = if settings_kts_path.exists() {
        std::fs::read_to_string(&settings_kts_path).ok()
    } else if settings_path.exists() {
        std::fs::read_to_string(&settings_path).ok()
    } else {
        None
    };
    if let Some(ref settings_content) = settings {
        for line in settings_content.lines() {
            let line = line.trim();
            if line.starts_with("rootProject.name") {
                if let Some(val) = extract_string_value(line) {
                    config.name = val;
                }
            }
        }
    }

    for line in content.lines() {
        let line = line.trim();

        // Extract group/version
        if line.starts_with("group") && !line.starts_with("groupId") && line.contains('=') {
            if let Some(val) = extract_string_value(line) {
                config.group_id = val.clone();
                // Also set package if not already set
                if config.package.is_none() {
                    config.package = Some(val);
                }
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

        // java toolchain: languageVersion = JavaLanguageVersion.of(21)
        if line.contains("languageVersion") && line.contains("JavaLanguageVersion.of") {
            if let Some(start) = line.find("JavaLanguageVersion.of(") {
                let rest = &line[start + "JavaLanguageVersion.of(".len()..];
                if let Some(end) = rest.find(')') {
                    let ver = rest[..end].trim();
                    if config.target.is_none() {
                        config.target = Some(ver.to_string());
                    }
                }
            }
        }

        // Detect inter-module project dependencies
        if let Some(proj_dep) = parse_gradle_project_dependency(line) {
            let dep_name = proj_dep.trim_start_matches(':').replace(':', "-");
            if all_module_names.iter().any(|n| *n == dep_name) {
                dependencies.entry(dep_name).or_insert(DependencyValue::Detailed(DependencySpec {
                    workspace: Some(true),
                    ..Default::default()
                }));
                continue;
            }
        }

        // External dependencies
        if let Some(dep) = parse_gradle_dependency(line, "implementation")
            .or_else(|| parse_gradle_dependency(line, "api"))
            .or_else(|| parse_gradle_dependency(line, "compile"))
        {
            let version = resolve_bom_version(&dep.0, &dep.1, &spring_boot_version);
            dependencies.insert(dep.0, DependencyValue::Simple(version));
        }
        if let Some(dep) = parse_gradle_dependency(line, "testImplementation")
            .or_else(|| parse_gradle_dependency(line, "testCompile"))
        {
            let version = resolve_bom_version(&dep.0, &dep.1, &spring_boot_version);
            dependencies.insert(dep.0, DependencyValue::Detailed(DependencySpec {
                version: Some(version),
                scope: Some("test".to_string()),
                ..Default::default()
            }));
        }
        if let Some(dep) = parse_gradle_dependency(line, "compileOnly") {
            let version = resolve_bom_version(&dep.0, &dep.1, &spring_boot_version);
            dependencies.insert(dep.0, DependencyValue::Detailed(DependencySpec {
                version: Some(version),
                scope: Some("provided".to_string()),
                ..Default::default()
            }));
        }
        if let Some(dep) = parse_gradle_dependency(line, "runtimeOnly") {
            let version = resolve_bom_version(&dep.0, &dep.1, &spring_boot_version);
            dependencies.insert(dep.0, DependencyValue::Detailed(DependencySpec {
                version: Some(version),
                scope: Some("runtime".to_string()),
                ..Default::default()
            }));
        }

        // Version Catalog references: implementation(libs.spring.boot.starter.web)
        if !catalog.is_empty() {
            if let Some((scope, alias)) = parse_catalog_reference(line) {
                // Normalize: libs.spring.boot.starter.web → spring-boot-starter-web
                let normalized = alias.replace('.', "-");
                if let Some((module, version)) = catalog.get(&normalized) {
                    let dep_val = match scope {
                        "compile" => DependencyValue::Simple(version.clone()),
                        _ => DependencyValue::Detailed(DependencySpec {
                            version: Some(version.clone()),
                            scope: Some(scope.to_string()),
                            ..Default::default()
                        }),
                    };
                    dependencies.insert(module.clone(), dep_val);
                }
            }
        }
    }

    // Detect common Gradle plugins and map to ym config
    detect_gradle_plugins(&content, &mut config, &dependencies);

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

    Ok(config)
}

/// Detect common Gradle plugins and map to ym compiler/config settings.
fn detect_gradle_plugins(
    content: &str,
    config: &mut YmConfig,
    deps: &BTreeMap<String, DependencyValue>,
) {
    let mut hints: Vec<String> = Vec::new();

    // Spring Boot plugin
    if content.contains("org.springframework.boot") || content.contains("spring-boot") {
        // Detect main class from SpringBootApplication
        if config.main.is_none() {
            if let Some(pkg) = config.package.as_ref() {
                config.main = Some(format!("{}.Application", pkg));
            }
        }
        hints.push("Spring Boot plugin detected".to_string());
    }

    // Annotation processing (Lombok, MapStruct, etc.)
    let has_lombok = deps.keys().any(|k| k.contains("lombok"));
    let has_mapstruct = deps.keys().any(|k| k.contains("mapstruct"));
    if has_lombok || has_mapstruct {
        let mut ap = Vec::new();
        if has_lombok {
            ap.push("org.projectlombok:lombok".to_string());
        }
        if has_mapstruct {
            ap.push("org.mapstruct:mapstruct-processor".to_string());
        }
        let compiler = config.compiler.get_or_insert_with(Default::default);
        compiler.annotation_processors = Some(ap.clone());
        hints.push(format!("Annotation processors: {}", ap.join(", ")));
    }

    // Java compilation target from plugins { java { targetCompatibility } }
    for line in content.lines() {
        let line = line.trim();
        if line.starts_with("targetCompatibility") {
            if let Some(val) = extract_string_value(line) {
                let ver = val.trim_start_matches("JavaVersion.VERSION_").to_string();
                if config.target.is_none() {
                    config.target = Some(ver);
                }
            }
        }
    }

    // Print detected plugin hints
    for hint in &hints {
        println!("  {} {}", style("→").blue(), style(hint).dim());
    }
}

/// Parse Version Catalog reference from a Gradle dependency line.
/// Returns (scope, alias) where alias is the dotted accessor after "libs."
/// Examples:
///   implementation(libs.spring.boot.starter.web) → ("compile", "spring.boot.starter.web")
///   testImplementation libs.junit.jupiter → ("test", "junit.jupiter")
fn parse_catalog_reference(line: &str) -> Option<(&str, String)> {
    let line = line.trim();
    let scope_prefixes: &[(&str, &str)] = &[
        ("implementation", "compile"),
        ("api", "compile"),
        ("compile", "compile"),
        ("testImplementation", "test"),
        ("testCompile", "test"),
        ("compileOnly", "provided"),
        ("runtimeOnly", "runtime"),
    ];
    for &(prefix, scope) in scope_prefixes {
        if !line.starts_with(prefix) { continue; }
        let rest = line[prefix.len()..].trim();
        // Match: (libs.xxx) or libs.xxx
        let inner = rest.trim_start_matches('(').trim_end_matches(')').trim();
        if let Some(alias) = inner.strip_prefix("libs.") {
            let alias = alias.trim_end_matches(')').trim();
            if !alias.is_empty() && !alias.contains('\'') && !alias.contains('"') {
                return Some((scope, alias.to_string()));
            }
        }
    }
    None
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
    let trimmed = line.trim_start();
    let is_dep_line = prefixes.iter().any(|p| trimmed.starts_with(p));
    if !is_dep_line {
        return None;
    }
    let line = trimmed;

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
    println!("  {} Created root package.toml", style("✓").green());

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
        // a sibling module, convert it to a workspace module ref
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
                    deps.insert(parts[1].to_string(), DependencyValue::Detailed(DependencySpec {
                        workspace: Some(true),
                        ..Default::default()
                    }));
                }
            }
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
    let mut dependencies: BTreeMap<String, DependencyValue> = BTreeMap::new();

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
            let dep_optional = find_child_text(&dep, "optional");

            if let (Some(g), Some(a)) = (dep_group, dep_artifact) {
                let coord = format!("{}:{}", g, a);
                let ver = dep_version.unwrap_or_else(|| "LATEST".to_string());

                // Skip property references
                if ver.contains("${") {
                    continue;
                }

                // Skip optional dependencies
                if dep_optional.as_deref() == Some("true") {
                    eprintln!(
                        "  {} Skipped optional dependency: {} (add manually if needed)",
                        console::style("!").yellow(), coord
                    );
                    continue;
                }

                match dep_scope.as_deref() {
                    Some("test") => {
                        dependencies.insert(coord, DependencyValue::Detailed(DependencySpec {
                            version: Some(ver),
                            scope: Some("test".to_string()),
                            ..Default::default()
                        }));
                    }
                    Some("provided") => {
                        dependencies.insert(coord, DependencyValue::Detailed(DependencySpec {
                            version: Some(ver),
                            scope: Some("provided".to_string()),
                            ..Default::default()
                        }));
                    }
                    Some("runtime") => {
                        dependencies.insert(coord, DependencyValue::Detailed(DependencySpec {
                            version: Some(ver),
                            scope: Some("runtime".to_string()),
                            ..Default::default()
                        }));
                    }
                    Some("system") => {
                        eprintln!(
                            "  {} Skipped system-scoped dependency: {} (requires local JAR path, handle manually)",
                            console::style("!").yellow(), coord
                        );
                    }
                    _ => {
                        dependencies.insert(coord, DependencyValue::Simple(ver));
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
        // Detect Maven plugins and map to ym config
        detect_maven_plugins(&root, &mut config, &dependencies);
        config.dependencies = Some(dependencies);
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

/// Detect Maven plugins (spring-boot, annotation-processing, shade) and map to ym config.
fn detect_maven_plugins(
    root: &roxmltree::Node,
    config: &mut YmConfig,
    deps: &BTreeMap<String, DependencyValue>,
) {
    let mut hints: Vec<String> = Vec::new();

    // Scan <build><plugins> for known plugins
    for node in root.descendants() {
        if node.tag_name().name() != "plugin" {
            continue;
        }
        let artifact = find_child_text(&node, "artifactId").unwrap_or_default();

        match artifact.as_str() {
            "spring-boot-maven-plugin" => {
                if config.main.is_none() {
                    if let Some(pkg) = config.package.as_ref() {
                        config.main = Some(format!("{}.Application", pkg));
                    }
                }
                hints.push("Spring Boot Maven plugin detected".to_string());
            }
            "maven-compiler-plugin" => {
                // Check for annotation processor configuration
                for cfg_node in node.descendants() {
                    if cfg_node.tag_name().name() == "annotationProcessorPaths" {
                        let mut ap = Vec::new();
                        for path_node in cfg_node.children() {
                            if path_node.tag_name().name() == "path" || path_node.tag_name().name() == "annotationProcessorPath" {
                                let g = find_child_text(&path_node, "groupId");
                                let a = find_child_text(&path_node, "artifactId");
                                if let (Some(g), Some(a)) = (g, a) {
                                    ap.push(format!("{}:{}", g, a));
                                }
                            }
                        }
                        if !ap.is_empty() {
                            let compiler = config.compiler.get_or_insert_with(Default::default);
                            compiler.annotation_processors = Some(ap.clone());
                            hints.push(format!("Annotation processors: {}", ap.join(", ")));
                        }
                    }
                }
            }
            _ => {}
        }
    }

    // Auto-detect Lombok from dependencies
    let has_lombok = deps.keys().any(|k| k.contains("lombok"));
    if has_lombok && config.compiler.as_ref().and_then(|c| c.annotation_processors.as_ref()).is_none() {
        let compiler = config.compiler.get_or_insert_with(Default::default);
        compiler.annotation_processors = Some(vec!["org.projectlombok:lombok".to_string()]);
        hints.push("Lombok auto-detected as annotation processor".to_string());
    }

    for hint in &hints {
        println!("  {} {}", style("→").blue(), style(hint).dim());
    }
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

/// Resolve version for BOM-managed dependencies.
/// If version is empty and we know the Spring Boot version, use it for Spring Boot starters.
/// Otherwise warn and use "FIXME" placeholder.
fn resolve_bom_version(coord: &str, version: &str, spring_boot_version: &Option<String>) -> String {
    if !version.is_empty() {
        return version.to_string();
    }
    // Spring Boot BOM-managed dependencies
    if coord.starts_with("org.springframework.boot:") || coord.starts_with("org.springframework:") {
        if let Some(sb_ver) = spring_boot_version {
            return sb_ver.clone();
        }
    }
    eprintln!(
        "  {} No version for {} — add version manually",
        console::style("!").yellow(),
        coord
    );
    "FIXME".to_string()
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
    let trimmed = line.trim_start();
    if !trimmed.starts_with(prefix) {
        return None;
    }
    let line = trimmed;
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
    } else if parts.len() == 2 {
        // No version specified (e.g., Spring Boot BOM-managed dependency)
        let coord = format!("{}:{}", parts[0], parts[1]);
        Some((coord, String::new()))
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
        assert!(deps.contains_key("junit:junit"));
        // junit should have test scope
        match deps.get("junit:junit").unwrap() {
            DependencyValue::Detailed(spec) => assert_eq!(spec.scope.as_deref(), Some("test")),
            _ => panic!("Expected detailed dep for test scope"),
        }

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
        assert_eq!(cfg.name, "ym-gradle-migrate-test"); // name from dir (no settings.gradle rootProject.name)
        assert_eq!(cfg.group_id, "com.example");
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

    #[test]
    fn test_gradle_kotlin_dsl_dependency() {
        // Kotlin DSL uses implementation("group:artifact:version")
        let dep = parse_gradle_dependency(
            r#"    implementation("org.springframework.boot:spring-boot-starter-web:3.2.0")"#,
            "implementation",
        );
        assert!(dep.is_some());
        let (coord, ver) = dep.unwrap();
        assert_eq!(coord, "org.springframework.boot:spring-boot-starter-web");
        assert_eq!(ver, "3.2.0");
    }

    #[test]
    fn test_gradle_kotlin_dsl_test_dep() {
        let dep = parse_gradle_dependency(
            r#"    testImplementation("org.junit.jupiter:junit-jupiter:5.10.0")"#,
            "testImplementation",
        );
        assert!(dep.is_some());
        let (coord, ver) = dep.unwrap();
        assert_eq!(coord, "org.junit.jupiter:junit-jupiter");
        assert_eq!(ver, "5.10.0");
    }

    #[test]
    fn test_migrate_pom_multimodule_detection() {
        let tmpdir = std::env::temp_dir().join("ym-pom-multimod-test");
        let _ = std::fs::remove_dir_all(&tmpdir);
        std::fs::create_dir_all(&tmpdir).unwrap();

        let pom = r#"<?xml version="1.0"?>
<project>
    <groupId>com.example</groupId>
    <artifactId>parent</artifactId>
    <version>1.0.0</version>
    <packaging>pom</packaging>
    <modules>
        <module>core</module>
        <module>web</module>
    </modules>
</project>"#;
        std::fs::write(tmpdir.join("pom.xml"), pom).unwrap();

        let cfg = migrate_from_pom(&tmpdir.join("pom.xml")).unwrap();
        assert_eq!(cfg.private, Some(true));
        assert!(cfg.workspaces.is_some());
        let ws = cfg.workspaces.unwrap();
        assert!(ws.contains(&"core/*".to_string()));
        assert!(ws.contains(&"web/*".to_string()));

        let _ = std::fs::remove_dir_all(&tmpdir);
    }

    #[test]
    fn test_gradle_project_dep_kotlin_dsl() {
        // Kotlin DSL project dependency
        assert_eq!(
            parse_gradle_project_dependency(r#"    implementation(project(":common"))"#),
            Some(":common".to_string())
        );
    }

    #[test]
    fn test_gradle_project_dep_api_scope() {
        assert_eq!(
            parse_gradle_project_dependency("api project(':shared-lib')"),
            Some(":shared-lib".to_string())
        );
    }

    #[test]
    fn test_settings_gradle_separate_includes() {
        let tmpdir = std::env::temp_dir().join("ym-settings-sep-test");
        let _ = std::fs::remove_dir_all(&tmpdir);
        std::fs::create_dir_all(&tmpdir).unwrap();

        // Each include on separate line
        std::fs::write(
            tmpdir.join("settings.gradle"),
            "include ':core'\ninclude ':web'\ninclude ':api'\n",
        )
        .unwrap();

        let modules = parse_settings_gradle(&tmpdir.join("settings.gradle")).unwrap();
        assert_eq!(modules.len(), 3);
        assert!(modules.contains(&"core".to_string()));
        assert!(modules.contains(&"web".to_string()));
        assert!(modules.contains(&"api".to_string()));

        let _ = std::fs::remove_dir_all(&tmpdir);
    }

    #[test]
    fn test_extract_string_value_variants() {
        assert_eq!(extract_string_value("key = 'value'"), Some("value".to_string()));
        assert_eq!(extract_string_value("key = \"value\""), Some("value".to_string()));
        assert_eq!(extract_string_value("key = plain"), Some("plain".to_string()));
        assert_eq!(extract_string_value("no_equals"), None);
        assert_eq!(extract_string_value("key = "), None);
    }

    #[test]
    fn test_detect_java_version_from_properties() {
        let pom = r#"<?xml version="1.0"?>
<project>
    <properties>
        <java.version>21</java.version>
    </properties>
</project>"#;
        let doc = roxmltree::Document::parse(pom).unwrap();
        assert_eq!(detect_java_version(&doc.root_element()), Some("21".to_string()));
    }

    #[test]
    fn test_detect_main_class() {
        let pom = r#"<?xml version="1.0"?>
<project>
    <build>
        <plugins>
            <plugin>
                <configuration>
                    <mainClass>com.example.App</mainClass>
                </configuration>
            </plugin>
        </plugins>
    </build>
</project>"#;
        let doc = roxmltree::Document::parse(pom).unwrap();
        assert_eq!(detect_main_class(&doc.root_element()), Some("com.example.App".to_string()));
    }
}
