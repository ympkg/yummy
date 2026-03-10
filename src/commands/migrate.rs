use anyhow::{bail, Result};
use console::style;
use std::collections::{BTreeMap, HashSet};
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
                "  {} migrating multi-module Gradle project ({} modules)...",
                style("➜").green(),
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
                "  {} migrating multi-module Maven project ({} modules)...",
                style("➜").green(),
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
        println!("  {} migrating from pom.xml...", style("➜").green());
        migrate_from_pom(&pom)?
    } else if gradle.exists() {
        println!("  {} migrating from build.gradle...", style("➜").green());
        migrate_from_gradle(&gradle)?
    } else if gradle_kts.exists() {
        println!(
            "  {} migrating from build.gradle.kts...",
            style("➜").green()
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
        style("➜").green()
    );

    match super::build::compile_only(None) {
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

// --- Gradle ext variable parsing ---

/// Parse `ext { key = 'value' }` block from a Gradle build file.
/// Returns a map of variable name → resolved value.
fn parse_ext_variables(content: &str) -> BTreeMap<String, String> {
    let mut variables = BTreeMap::new();
    let mut in_ext = false;
    let mut brace_depth = 0;

    for line in content.lines() {
        let trimmed = line.trim();

        if !in_ext {
            if trimmed.starts_with("ext {") || trimmed == "ext {" || trimmed.starts_with("ext{") {
                in_ext = true;
                brace_depth = 1;
                continue;
            }
            continue;
        }

        brace_depth += trimmed.chars().filter(|&c| c == '{').count() as i32;
        brace_depth -= trimmed.chars().filter(|&c| c == '}').count() as i32;
        if brace_depth <= 0 {
            in_ext = false;
            continue;
        }

        if trimmed.starts_with("//") {
            continue;
        }

        // Match: key = 'value' or key = "value"
        if let Some(eq_pos) = trimmed.find('=') {
            let key = trimmed[..eq_pos].trim();
            let val_part = trimmed[eq_pos + 1..].trim();

            // Extract quoted value
            if let Some(val) = extract_quoted_value(val_part) {
                if key.chars().all(|c| c.is_alphanumeric() || c == '_') {
                    variables.insert(key.to_string(), val);
                }
            }
            // Handle: applicationVersion = version (bare identifier referencing project version)
            else if val_part == "version" && key == "applicationVersion" {
                variables.insert(key.to_string(), "PROJECT_VERSION".to_string());
            }
        }
    }

    variables
}

/// Extract a quoted string value from a Gradle expression.
fn extract_quoted_value(s: &str) -> Option<String> {
    let s = s.trim();
    if (s.starts_with('\'') && s.ends_with('\'')) || (s.starts_with('"') && s.ends_with('"')) {
        Some(s[1..s.len() - 1].to_string())
    } else {
        None
    }
}

/// Resolve `${variableName}` references in a string using the ext variables map.
fn resolve_ext_refs(s: &str, ext_vars: &BTreeMap<String, String>, project_version: &str) -> String {
    let mut result = s.to_string();
    // Resolve all ${...} references
    while let Some(start) = result.find("${") {
        if let Some(end) = result[start..].find('}') {
            let var_name = &result[start + 2..start + end];
            let replacement = if var_name == "applicationVersion"
                || var_name == "rootProject.version"
                || var_name == "project.version"
            {
                project_version.to_string()
            } else if let Some(val) = ext_vars.get(var_name) {
                if val == "PROJECT_VERSION" {
                    project_version.to_string()
                } else {
                    val.clone()
                }
            } else {
                // Unresolved variable
                return result;
            };
            result = format!("{}{}{}", &result[..start], replacement, &result[start + end + 1..]);
        } else {
            break;
        }
    }
    result
}

/// Parse `allprojects { group "xxx" }` or `allprojects { group = "xxx" }` from root build.gradle.
/// Returns (group_id, version, compiler_args).
fn parse_allprojects(content: &str) -> (Option<String>, Option<String>, Vec<String>) {
    let mut group_id = None;
    let version = None;
    let mut compiler_args = Vec::new();
    let mut in_allprojects = false;
    let mut brace_depth = 0;

    for line in content.lines() {
        let trimmed = line.trim();

        if !in_allprojects {
            if trimmed.starts_with("allprojects") && trimmed.contains('{') {
                in_allprojects = true;
                brace_depth = 1;
                continue;
            }
            // Also check subprojects block
            if trimmed.starts_with("subprojects") && trimmed.contains('{') {
                in_allprojects = true;
                brace_depth = 1;
                continue;
            }
            continue;
        }

        brace_depth += trimmed.chars().filter(|&c| c == '{').count() as i32;
        brace_depth -= trimmed.chars().filter(|&c| c == '}').count() as i32;
        if brace_depth <= 0 {
            in_allprojects = false;
            continue;
        }

        // group "xxx" or group = "xxx" or group 'xxx'
        if trimmed.starts_with("group") && !trimmed.starts_with("groupId") {
            if let Some(val) = extract_string_value(trimmed) {
                group_id = Some(val);
            } else if let Some(val) = extract_string_arg(trimmed) {
                group_id = Some(val);
            }
        }

        // compilerArgs.add('-parameters') or compilerArgs << '-parameters'
        if trimmed.contains("compilerArgs") && trimmed.contains("-parameters") {
            compiler_args.push("-parameters".to_string());
        }
    }

    (group_id, version, compiler_args)
}

/// Scan filesystem for build.gradle files when settings.gradle uses dynamic fileTree discovery.
fn scan_gradle_modules(root: &Path) -> Vec<String> {
    let mut modules = Vec::new();
    scan_gradle_modules_recursive(root, root, &mut modules);
    modules
}

fn scan_gradle_modules_recursive(root: &Path, dir: &Path, modules: &mut Vec<String>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }

        let name = entry.file_name().to_string_lossy().to_string();
        // Skip common non-module directories
        if matches!(
            name.as_str(),
            "build" | ".gradle" | ".idea" | "buildSrc" | "gradle" | ".git" | "node_modules" | "specs"
        ) {
            continue;
        }

        let gradle_file = path.join("build.gradle");
        let gradle_kts_file = path.join("build.gradle.kts");
        if gradle_file.exists() || gradle_kts_file.exists() {
            if let Ok(rel) = path.strip_prefix(root) {
                let rel_str = rel.to_string_lossy().replace('\\', "/");
                modules.push(rel_str);
            }
        }

        // Recurse into subdirectories
        scan_gradle_modules_recursive(root, &path, modules);
    }
}

// --- Gradle multi-module migration ---

/// Parse settings.gradle(.kts) to extract included module paths.
fn parse_settings_gradle(path: &Path) -> Result<Vec<String>> {
    let content = std::fs::read_to_string(path)?;
    let mut modules = Vec::new();

    // Track brace depth to skip fileTree/pluginManagement blocks
    let mut skip_depth: i32 = 0;
    let mut in_skippable_block = false;

    for line in content.lines() {
        let trimmed = line.trim();

        // Detect blocks we should skip (fileTree, pluginManagement, etc.)
        if !in_skippable_block && (trimmed.starts_with("fileTree") || trimmed.starts_with("pluginManagement")) {
            in_skippable_block = true;
            skip_depth = 0;
        }

        if in_skippable_block {
            skip_depth += trimmed.chars().filter(|&c| c == '{').count() as i32;
            skip_depth -= trimmed.chars().filter(|&c| c == '}').count() as i32;
            if skip_depth <= 0 && trimmed.contains('}') {
                in_skippable_block = false;
            }
            continue;
        }

        // Match: include 'module-a', ':module-b', ':parent:child'
        // Match: include("module-a", ":module-b")
        if !trimmed.starts_with("include") {
            continue;
        }

        // Extract all quoted strings from the line
        let mut i = 0;
        let chars: Vec<char> = trimmed.chars().collect();
        while i < chars.len() {
            if chars[i] == '\'' || chars[i] == '"' {
                let quote = chars[i];
                i += 1;
                let start = i;
                while i < chars.len() && chars[i] != quote {
                    i += 1;
                }
                if i < chars.len() {
                    let module = &trimmed[start..i];
                    // Strip leading colon and convert : to /
                    let module_path = module.trim_start_matches(':').replace(':', "/");
                    // Skip glob patterns (these are file includes, not module includes)
                    if !module_path.is_empty() && !module_path.contains('*') {
                        modules.push(module_path);
                    }
                }
            }
            i += 1;
        }
    }

    // Fallback: if no module include statements found, check for dynamic fileTree scanning
    if modules.is_empty() && content.contains("fileTree") {
        if let Some(root_dir) = path.parent() {
            let scanned = scan_gradle_modules(root_dir);
            if !scanned.is_empty() {
                println!(
                    "  {} Detected dynamic fileTree module discovery, scanning filesystem ({} modules found)",
                    style("➜").green(),
                    scanned.len()
                );
                return Ok(scanned);
            }
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

    // Parse root build.gradle for ext variables, allprojects, and shared settings
    let root_content = root_gradle_path
        .and_then(|p| std::fs::read_to_string(p).ok())
        .unwrap_or_default();

    let ext_vars = parse_ext_variables(&root_content);
    let (all_group, _all_version, compiler_args) = parse_allprojects(&root_content);

    // Extract project version from root build.gradle
    let mut project_version = String::new();
    for line in root_content.lines() {
        let trimmed = line.trim();
        // Match: version "4.0.9" or version = "4.0.9" or version '4.0.9'
        if trimmed.starts_with("version") && !trimmed.starts_with("version(") {
            if let Some(val) = extract_string_value(trimmed) {
                project_version = val;
                break;
            }
            if let Some(val) = extract_string_arg(trimmed) {
                project_version = val;
                break;
            }
        }
    }

    if !ext_vars.is_empty() {
        println!(
            "  {} Parsed {} ext variables from root build.gradle",
            style("✓").green(),
            ext_vars.len()
        );
    }

    // Parse root build.gradle for basic config (name, etc.)
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

    // Apply allprojects group
    if let Some(ref group) = all_group {
        if root_cfg.group_id == "com.example" || root_cfg.group_id.is_empty() {
            root_cfg.group_id.clone_from(group);
        }
    }

    // Apply compiler args
    if !compiler_args.is_empty() {
        let compiler = root_cfg.compiler.get_or_insert_with(Default::default);
        compiler.args = Some(compiler_args);
    }

    // Set version
    if !project_version.is_empty() {
        root_cfg.version = Some(project_version.clone());
    }

    // Determine workspace patterns from module paths (use smart glob patterns)
    let workspace_patterns = compute_workspace_patterns(modules);
    root_cfg.workspaces = Some(workspace_patterns);

    // All module names for inter-module dependency detection
    let all_module_names: HashSet<String> = modules
        .iter()
        .map(|m| {
            Path::new(m)
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string()
        })
        .collect();
    let all_module_names_vec: Vec<String> = all_module_names.iter().cloned().collect();

    // First pass: collect all external dependencies with their versions for root catalog
    let mut all_ext_deps: BTreeMap<String, String> = BTreeMap::new();
    let mut bom_managed_deps: HashSet<String> = HashSet::new(); // deps with no version (BOM managed)
    for module_path in modules {
        let module_dir = root.join(module_path);
        let gradle_path = find_gradle_file(&module_dir);
        let gradle_path = match gradle_path {
            Some(p) => p,
            None => continue,
        };
        let content = match std::fs::read_to_string(&gradle_path) {
            Ok(c) => c,
            Err(_) => continue,
        };
        collect_ext_deps_from_gradle(&content, &ext_vars, &project_version, &all_module_names, &mut all_ext_deps, &mut bom_managed_deps);
    }

    // Build root dependencies from ext variables + collected deps
    let mut root_deps: BTreeMap<String, DependencyValue> = BTreeMap::new();
    for (coord, version) in &all_ext_deps {
        if coord.starts_with("com.summer.jarvis:") {
            continue; // External jarvis deps stay per-module
        }
        root_deps.insert(
            coord.clone(),
            DependencyValue::Simple(version.clone()),
        );
    }

    // Pre-scan for Spring Boot plugin version from ext vars or submodules
    let mut spring_boot_version: Option<String> = ext_vars.get("springBootVersion").cloned();
    if spring_boot_version.is_none() {
        // Scan root and submodule build.gradle files for Spring Boot plugin version
        let all_gradle_contents: Vec<String> = std::iter::once(root_content.clone())
            .chain(modules.iter().filter_map(|m| {
                let gp = find_gradle_file(&root.join(m));
                gp.and_then(|p| std::fs::read_to_string(p).ok())
            }))
            .collect();
        for content in &all_gradle_contents {
            for line in content.lines() {
                let line = line.trim();
                if line.contains("org.springframework.boot") && line.contains("version") {
                    let resolved = resolve_ext_refs(line, &ext_vars, &project_version);
                    if let Some(start) = resolved.rfind('\'').or_else(|| resolved.rfind('"')) {
                        let before = &resolved[..start];
                        if let Some(vstart) = before.rfind('\'').or_else(|| before.rfind('"')) {
                            let ver = &resolved[vstart + 1..start];
                            if !ver.is_empty() && ver.chars().next().is_some_and(|c| c.is_ascii_digit()) {
                                spring_boot_version = Some(ver.to_string());
                            }
                        }
                    }
                }
            }
            if spring_boot_version.is_some() { break; }
        }
    }

    // Add BOM-managed deps to root with resolved versions
    if spring_boot_version.is_some() {
        for coord in &bom_managed_deps {
            if !root_deps.contains_key(coord) {
                let version = resolve_bom_version(coord, "", &spring_boot_version);
                root_deps.insert(coord.clone(), DependencyValue::Simple(version));
            }
        }
    } else {
        let sb_version = ext_vars.get("springBootVersion").cloned().unwrap_or_default();
        if !sb_version.is_empty() {
            for coord in &bom_managed_deps {
                if !root_deps.contains_key(coord) {
                    root_deps.insert(coord.clone(), DependencyValue::Simple(sb_version.clone()));
                }
            }
        }
    }

    // Lombok → provided scope
    // Check ext vars first, then scan for io.freefair.lombok plugin
    let lombok_ver = ext_vars.get("lombokVersion").cloned();
    let has_freefair_lombok = root_content.contains("io.freefair.lombok")
        || modules.iter().any(|m| {
            find_gradle_file(&root.join(m))
                .and_then(|p| std::fs::read_to_string(p).ok())
                .is_some_and(|c| c.contains("io.freefair.lombok"))
        });
    if lombok_ver.is_some() || has_freefair_lombok {
        let ver = lombok_ver.unwrap_or_else(|| "1.18.36".to_string()); // latest stable default
        root_deps.insert(
            "org.projectlombok:lombok".to_string(),
            DependencyValue::Detailed(DependencySpec {
                version: Some(ver),
                scope: Some("provided".to_string()),
                ..Default::default()
            }),
        );
        if has_freefair_lombok {
            println!(
                "  {} io.freefair.lombok plugin detected → added Lombok as provided dependency",
                style("✓").green()
            );
        }
    }

    if !root_deps.is_empty() {
        root_cfg.dependencies = Some(root_deps.clone());
        println!(
            "  {} Root dependencies: {} shared entries",
            style("✓").green(),
            root_deps.len()
        );
    }

    let root_dep_coords: HashSet<String> = root_deps.keys().cloned().collect();

    let root_config_path = root.join(config::CONFIG_FILE);
    config::save_config(&root_config_path, &root_cfg)?;
    println!("  {} Created root package.toml", style("✓").green());

    // Second pass: migrate each submodule
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

        let gradle_path = match find_gradle_file(&module_dir) {
            Some(p) => p,
            None => continue,
        };

        let mut module_cfg = migrate_from_gradle_ext(
            &gradle_path,
            &all_module_names_vec,
            &ext_vars,
            &project_version,
            &root_dep_coords,
            true, // quiet: suppress per-module hints in multi-module migration
        )?;

        // Use directory name as module name if not set
        if module_cfg.name.is_empty() {
            module_cfg.name = module_dir
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("module")
                .to_string();
        }

        // Inherit group from allprojects
        if let Some(ref group) = all_group {
            if module_cfg.group_id == "com.example" || module_cfg.group_id.is_empty() {
                module_cfg.group_id.clone_from(group);
            }
        }

        // Save with workspace inheritance for version and target
        save_module_config(&module_config_path, &module_cfg)?;
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

/// Save a sub-module config with `version = { workspace = true }` and `target = { workspace = true }`.
fn save_module_config(path: &Path, cfg: &YmConfig) -> Result<()> {
    // Separate workspace deps from non-workspace deps
    let mut workspace_deps: BTreeMap<String, ()> = BTreeMap::new();
    let mut temp_cfg = cfg.clone();
    temp_cfg.version = None;
    temp_cfg.target = None;

    if let Some(ref mut deps) = temp_cfg.dependencies {
        let mut non_ws = BTreeMap::new();
        for (k, v) in deps.iter() {
            if v.is_workspace() {
                workspace_deps.insert(k.clone(), ());
            } else {
                non_ws.insert(k.clone(), v.clone());
            }
        }
        if non_ws.is_empty() {
            temp_cfg.dependencies = None;
        } else {
            *deps = non_ws;
        }
    }

    // Save non-workspace deps via serde
    config::save_config(path, &temp_cfg)?;

    // Patch in workspace inheritance for version and target
    let content = std::fs::read_to_string(path)?;
    let mut lines: Vec<String> = content.lines().map(|l| l.to_string()).collect();

    // Find the name line and insert version/target after it
    if let Some(name_idx) = lines.iter().position(|l| l.starts_with("name")) {
        let insert_at = name_idx + 1;
        let insert_at = if insert_at < lines.len() && lines[insert_at].starts_with("group_id") {
            insert_at + 1
        } else {
            insert_at
        };
        lines.insert(insert_at, "version = { workspace = true }".to_string());
        lines.insert(insert_at + 1, "target = { workspace = true }".to_string());
    }

    // Append workspace deps as inline format under [dependencies]
    if !workspace_deps.is_empty() {
        // Ensure [dependencies] section exists
        if !lines.iter().any(|l| l.trim() == "[dependencies]") {
            lines.push(String::new());
            lines.push("[dependencies]".to_string());
        }
        // Find the [dependencies] line and insert after it
        if let Some(dep_idx) = lines.iter().position(|l| l.trim() == "[dependencies]") {
            let mut insert_at = dep_idx + 1;
            // Skip existing key = value lines
            while insert_at < lines.len() {
                let l = lines[insert_at].trim();
                if l.is_empty() || l.starts_with('[') {
                    break;
                }
                insert_at += 1;
            }
            for key in workspace_deps.keys() {
                let needs_quote = key.contains(':');
                let entry = if needs_quote {
                    format!("\"{}\" = {{ workspace = true }}", key)
                } else {
                    format!("{} = {{ workspace = true }}", key)
                };
                lines.insert(insert_at, entry);
                insert_at += 1;
            }
        }
    }

    std::fs::write(path, lines.join("\n"))?;
    Ok(())
}

/// Find build.gradle or build.gradle.kts in a directory.
fn find_gradle_file(dir: &Path) -> Option<std::path::PathBuf> {
    let kts = dir.join("build.gradle.kts");
    if kts.exists() {
        return Some(kts);
    }
    let groovy = dir.join("build.gradle");
    if groovy.exists() {
        return Some(groovy);
    }
    None
}

/// Compute smart workspace patterns from module paths.
/// Groups modules under common parent directories to minimize pattern count.
fn compute_workspace_patterns(modules: &[String]) -> Vec<String> {
    // Group by top-level directories and find optimal glob depth
    let mut patterns: Vec<String> = Vec::new();
    let mut seen_prefixes: HashSet<String> = HashSet::new();

    // Sort modules by path for grouping
    let mut sorted: Vec<&String> = modules.iter().collect();
    sorted.sort();

    for module in &sorted {
        let parts: Vec<&str> = module.split('/').collect();
        if parts.len() <= 1 {
            // Top-level module
            let pattern = format!("{}/*", parts[0]);
            if seen_prefixes.insert(pattern.clone()) {
                // Check if this single dir actually contains the module or is the module
                // For top-level, use direct pattern
                patterns.push(format!("./*"));
            }
            continue;
        }

        // For deeper paths, try to find the shallowest common prefix that uses **/*
        // e.g., project/jarvis-utils/utils-xxx → project/jarvis-utils/*
        // e.g., project/jarvis-infra/account/xxx/yyy → project/jarvis-infra/**/*
        let mut found = false;
        for depth in (1..parts.len()).rev() {
            let prefix: String = parts[..depth].join("/");
            let pattern = if depth == parts.len() - 1 {
                format!("{}/*", prefix)
            } else {
                format!("{}/**/*", prefix)
            };
            if seen_prefixes.contains(&pattern) {
                found = true;
                break;
            }
        }

        if !found {
            // Find the best prefix depth: check if all siblings share same parent depth
            let prefix: String = parts[..parts.len() - 1].join("/");
            let pattern = format!("{}/*", prefix);

            // Check if any other module has more nesting under the same top-2 prefix
            let top_prefix = if parts.len() >= 2 {
                parts[..2].join("/")
            } else {
                parts[0].to_string()
            };

            let has_deeper = sorted.iter().any(|m| {
                m.starts_with(&top_prefix) && m.split('/').count() > parts.len()
            });

            if has_deeper {
                let deep_pattern = format!("{}/**/*", top_prefix);
                if seen_prefixes.insert(deep_pattern.clone()) {
                    patterns.push(deep_pattern);
                }
            } else {
                if seen_prefixes.insert(pattern.clone()) {
                    patterns.push(pattern);
                }
            }
        }
    }

    // Deduplicate: remove patterns that are subsumed by **/* patterns
    let glob_patterns: Vec<String> = patterns.iter().filter(|p| p.contains("**")).cloned().collect();
    patterns.retain(|p| {
        if p.contains("**") {
            return true;
        }
        // Check if any **/* pattern already covers this
        !glob_patterns.iter().any(|gp| {
            let gp_prefix = gp.trim_end_matches("/**/*");
            p.starts_with(gp_prefix)
        })
    });

    if patterns.is_empty() {
        patterns.push("./*".to_string());
    }

    patterns
}

/// Collect all external dependency coordinates+versions from a build.gradle
/// (first pass, for building root dependency catalog).
fn collect_ext_deps_from_gradle(
    content: &str,
    ext_vars: &BTreeMap<String, String>,
    project_version: &str,
    all_module_names: &HashSet<String>,
    deps: &mut BTreeMap<String, String>,
    bom_managed: &mut HashSet<String>,
) {
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("//") {
            continue;
        }

        // Skip project() dependencies
        if trimmed.contains("project(") {
            continue;
        }

        // Skip platform() dependencies
        if trimmed.contains("platform(") {
            continue;
        }

        // Parse dependency line
        if let Some((coord, version)) = parse_gradle_dep_raw(trimmed) {
            // Resolve ext variable references
            let resolved_coord = resolve_ext_refs(&coord, ext_vars, project_version);
            let resolved_version = resolve_ext_refs(&version, ext_vars, project_version);

            if resolved_coord.contains("${") {
                continue; // Unresolvable coordinate
            }

            let parts: Vec<&str> = resolved_coord.split(':').collect();
            if parts.len() < 2 {
                continue;
            }

            let full_coord = if parts.len() >= 3 {
                format!("{}:{}", parts[0], parts[1])
            } else {
                resolved_coord.clone()
            };
            let ver = if parts.len() >= 3 {
                parts[2].to_string()
            } else if !resolved_version.is_empty() && !resolved_version.contains("${") {
                resolved_version
            } else {
                String::new()
            };

            // Skip com.summer.jarvis internal modules
            if full_coord.starts_with("com.summer.jarvis:") {
                let artifact = full_coord.split(':').nth(1).unwrap_or("");
                if all_module_names.contains(artifact) {
                    continue;
                }
                // External jarvis deps: don't add to root catalog
                continue;
            }

            if ver.is_empty() {
                // BOM-managed dependency (no version)
                bom_managed.insert(full_coord);
            } else {
                deps.entry(full_coord).or_insert(ver);
            }
        }
    }
}

/// Raw parse a Gradle dependency line, returning the full dependency string and version.
fn parse_gradle_dep_raw(line: &str) -> Option<(String, String)> {
    let scope_prefixes = [
        "implementation", "api", "compile", "testImplementation",
        "testCompile", "compileOnly", "runtimeOnly", "annotationProcessor",
    ];

    let trimmed = line.trim();
    if !scope_prefixes.iter().any(|p| trimmed.starts_with(p)) {
        return None;
    }

    // Skip project() references
    if trimmed.contains("project(") || trimmed.contains("platform(") {
        return None;
    }

    // Extract quoted dependency string
    let start = trimmed.find('\'').or_else(|| trimmed.find('"'))?;
    let quote_char = trimmed.chars().nth(start)?;
    let rest = &trimmed[start + 1..];
    let end = rest.rfind(quote_char)?;
    let dep_str = &rest[..end];

    // Split into coordinate parts
    let parts: Vec<&str> = dep_str.split(':').collect();
    match parts.len() {
        3 => Some((dep_str.to_string(), parts[2].to_string())),
        2 => Some((dep_str.to_string(), String::new())),
        _ => None,
    }
}

/// Migrate a Gradle build file with ext variable resolution and workspace awareness.
fn migrate_from_gradle_ext(
    gradle_path: &Path,
    all_module_names: &[String],
    ext_vars: &BTreeMap<String, String>,
    project_version: &str,
    root_dep_coords: &HashSet<String>,
    quiet: bool,
) -> Result<YmConfig> {
    let content = std::fs::read_to_string(gradle_path)?;

    let project_dir = gradle_path.parent().unwrap_or(Path::new("."));
    let catalog = parse_version_catalog(project_dir);

    let mut config = YmConfig::default();
    let mut dependencies: BTreeMap<String, DependencyValue> = BTreeMap::new();

    // Pre-scan for Spring Boot plugin version
    let mut spring_boot_version: Option<String> = None;
    for line in content.lines() {
        let line = line.trim();
        if line.contains("org.springframework.boot") && line.contains("version") {
            let resolved = resolve_ext_refs(line, ext_vars, project_version);
            if let Some(start) = resolved.rfind('\'').or_else(|| resolved.rfind('"')) {
                let before = &resolved[..start];
                if let Some(vstart) = before.rfind('\'').or_else(|| before.rfind('"')) {
                    let ver = &resolved[vstart + 1..start];
                    if !ver.is_empty() && ver.chars().next().is_some_and(|c| c.is_ascii_digit()) {
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

    let all_module_set: HashSet<&str> = all_module_names.iter().map(|s| s.as_str()).collect();

    for line in content.lines() {
        let line = line.trim();

        // Skip comments
        if line.starts_with("//") {
            continue;
        }

        // Extract group/version
        if line.starts_with("group") && !line.starts_with("groupId") {
            if let Some(val) = extract_string_value(line) {
                config.group_id = val.clone();
                if config.package.is_none() {
                    config.package = Some(val);
                }
            }
        }
        if line.starts_with("version") && line.contains('=') && !line.contains("${") {
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
            // Take the last segment of the project path as module name
            let dep_name = proj_dep.split(':').last().unwrap_or(&proj_dep).to_string();
            if all_module_set.contains(dep_name.as_str()) {
                let scope = detect_scope(line);
                let dep_val = if let Some(s) = scope {
                    DependencyValue::Detailed(DependencySpec {
                        workspace: Some(true),
                        scope: Some(s),
                        ..Default::default()
                    })
                } else {
                    DependencyValue::Detailed(DependencySpec {
                        workspace: Some(true),
                        ..Default::default()
                    })
                };
                dependencies.entry(dep_name).or_insert(dep_val);
                continue;
            }
        }

        // Skip platform() BOM imports (just log)
        if line.contains("platform(") {
            // Detected but not converted — user should handle manually
            continue;
        }

        // External dependencies (with ext variable resolution)
        if let Some((raw_coord, _raw_ver)) = parse_gradle_dep_raw(line) {
            let resolved = resolve_ext_refs(&raw_coord, ext_vars, project_version);
            let parts: Vec<&str> = resolved.split(':').collect();
            if parts.len() < 2 {
                continue;
            }

            let coord = if parts.len() >= 3 {
                format!("{}:{}", parts[0], parts[1])
            } else {
                resolved.clone()
            };
            let version = if parts.len() >= 3 {
                parts[2].to_string()
            } else {
                // BOM-managed: no version
                String::new()
            };

            let scope = detect_scope(line);

            // Check if it's a com.summer.jarvis reference
            if coord.starts_with("com.summer.jarvis:") {
                let artifact = coord.split(':').nth(1).unwrap_or("");
                if all_module_set.contains(artifact) {
                    // Internal module → workspace reference
                    let dep_val = if let Some(s) = scope {
                        DependencyValue::Detailed(DependencySpec {
                            workspace: Some(true),
                            scope: Some(s),
                            ..Default::default()
                        })
                    } else {
                        DependencyValue::Detailed(DependencySpec {
                            workspace: Some(true),
                            ..Default::default()
                        })
                    };
                    dependencies.entry(artifact.to_string()).or_insert(dep_val);
                    continue;
                }
                // External jarvis dep → keep as external with version
                let ver = if version.is_empty() { project_version.to_string() } else { version };
                let dep_val = if let Some(s) = scope {
                    DependencyValue::Detailed(DependencySpec {
                        version: Some(ver),
                        scope: Some(s),
                        ..Default::default()
                    })
                } else {
                    DependencyValue::Simple(ver)
                };
                dependencies.entry(coord).or_insert(dep_val);
                continue;
            }

            // Regular external dep: use workspace = true if in root catalog
            if root_dep_coords.contains(&coord) {
                let dep_val = if let Some(s) = scope {
                    DependencyValue::Detailed(DependencySpec {
                        workspace: Some(true),
                        scope: Some(s),
                        ..Default::default()
                    })
                } else {
                    DependencyValue::Detailed(DependencySpec {
                        workspace: Some(true),
                        ..Default::default()
                    })
                };
                dependencies.entry(coord).or_insert(dep_val);
            } else {
                // Not in root → include version directly
                let ver = if version.is_empty() {
                    resolve_bom_version(&coord, "", &spring_boot_version)
                } else {
                    version
                };
                let dep_val = if let Some(s) = scope {
                    DependencyValue::Detailed(DependencySpec {
                        version: Some(ver),
                        scope: Some(s),
                        ..Default::default()
                    })
                } else {
                    DependencyValue::Simple(ver)
                };
                dependencies.entry(coord).or_insert(dep_val);
            }

            continue;
        }

        // Version Catalog references
        if !catalog.is_empty() {
            if let Some((scope_str, alias)) = parse_catalog_reference(line) {
                let normalized = alias.replace('.', "-");
                if let Some((module, version)) = catalog.get(&normalized) {
                    let dep_val = match scope_str {
                        "compile" => DependencyValue::Simple(version.clone()),
                        _ => DependencyValue::Detailed(DependencySpec {
                            version: Some(version.clone()),
                            scope: Some(scope_str.to_string()),
                            ..Default::default()
                        }),
                    };
                    dependencies.insert(module.clone(), dep_val);
                }
            }
        }
    }

    // Detect common Gradle plugins
    detect_gradle_plugins(&content, &mut config, &mut dependencies, quiet);

    // Post-process: convert deps that exist in root catalog to { workspace = true }
    for (coord, dep_val) in dependencies.iter_mut() {
        if root_dep_coords.contains(coord) {
            // Preserve scope if any, but switch to workspace reference
            let scope = match dep_val {
                DependencyValue::Detailed(spec) => spec.scope.clone(),
                _ => None,
            };
            if let Some(s) = scope {
                *dep_val = DependencyValue::Detailed(DependencySpec {
                    workspace: Some(true),
                    scope: Some(s),
                    ..Default::default()
                });
            } else {
                *dep_val = DependencyValue::Detailed(DependencySpec {
                    workspace: Some(true),
                    ..Default::default()
                });
            }
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

    Ok(config)
}

/// Detect Gradle scope from a dependency line.
fn detect_scope(line: &str) -> Option<String> {
    let trimmed = line.trim();
    if trimmed.starts_with("testImplementation") || trimmed.starts_with("testCompile") {
        Some("test".to_string())
    } else if trimmed.starts_with("compileOnly") {
        Some("provided".to_string())
    } else if trimmed.starts_with("runtimeOnly") {
        Some("runtime".to_string())
    } else {
        None
    }
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
                    if !ver.is_empty() && ver.chars().next().is_some_and(|c| c.is_ascii_digit()) {
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
            if all_module_names.contains(&dep_name) {
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
    detect_gradle_plugins(&content, &mut config, &mut dependencies, false);

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
    deps: &mut BTreeMap<String, DependencyValue>,
    quiet: bool,
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
    let has_lombok_dep = deps.keys().any(|k| k.contains("lombok"));
    // io.freefair.lombok plugin implicitly adds Lombok as compileOnly + annotationProcessor
    let has_lombok_plugin = content.contains("io.freefair.lombok");
    let has_lombok = has_lombok_dep || has_lombok_plugin;
    let has_mapstruct = deps.keys().any(|k| k.contains("mapstruct"));
    if has_lombok || has_mapstruct {
        let mut ap = Vec::new();
        if has_lombok {
            ap.push("org.projectlombok:lombok".to_string());
            // If Lombok came from plugin (not explicitly in deps), add it as provided dependency
            if has_lombok_plugin && !has_lombok_dep {
                // freefair plugin version != Lombok library version (8.x → 1.18.x)
                // Use latest stable Lombok version as default
                let lombok_ver = "1.18.36".to_string();
                deps.insert(
                    "org.projectlombok:lombok".to_string(),
                    DependencyValue::Detailed(DependencySpec {
                        version: Some(lombok_ver),
                        scope: Some("provided".to_string()),
                        ..Default::default()
                    }),
                );
                hints.push("io.freefair.lombok plugin → added Lombok as provided dependency".to_string());
            }
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

    // Print detected plugin hints (skip in quiet mode for sub-modules)
    if !quiet {
        for hint in &hints {
            println!("  {} {}", style("➜").green(), style(hint).dim());
        }
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
        println!("  {} {}", style("➜").green(), style(hint).dim());
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
/// Downloads and parses Spring Boot BOM to get exact managed versions.
fn resolve_bom_version(coord: &str, version: &str, spring_boot_version: &Option<String>) -> String {
    if !version.is_empty() {
        return version.to_string();
    }
    if let Some(sb_ver) = spring_boot_version {
        // Spring Boot BOM-managed: same version as Spring Boot itself
        if coord.starts_with("org.springframework.boot:") {
            return sb_ver.clone();
        }
        // Try to resolve from Spring Boot BOM (cached)
        let bom_versions = get_spring_boot_bom_versions(sb_ver);
        if let Some(resolved) = bom_versions.get(coord) {
            return resolved.clone();
        }
        // Spring Framework core: derive from Spring Boot version
        if coord.starts_with("org.springframework:") {
            if let Some(v) = spring_boot_to_framework_version(sb_ver) {
                return v;
            }
        }
    }
    eprintln!(
        "  {} No version for {} — add version manually",
        console::style("!").yellow(),
        coord
    );
    "FIXME".to_string()
}

/// Download and parse Spring Boot Dependencies BOM to extract managed versions.
/// Results are cached in a thread-local to avoid repeated downloads.
fn get_spring_boot_bom_versions(boot_version: &str) -> std::collections::HashMap<String, String> {
    use std::sync::Mutex;
    use std::collections::HashMap;

    // Simple static cache
    static BOM_CACHE: std::sync::LazyLock<Mutex<HashMap<String, HashMap<String, String>>>> =
        std::sync::LazyLock::new(|| Mutex::new(HashMap::new()));

    let mut cache = BOM_CACHE.lock().unwrap();
    if let Some(versions) = cache.get(boot_version) {
        return versions.clone();
    }

    let versions = fetch_spring_boot_bom_versions(boot_version);
    cache.insert(boot_version.to_string(), versions.clone());
    versions
}

fn fetch_spring_boot_bom_versions(boot_version: &str) -> std::collections::HashMap<String, String> {
    use std::collections::HashMap;

    let url = format!(
        "https://repo1.maven.org/maven2/org/springframework/boot/spring-boot-dependencies/{}/spring-boot-dependencies-{}.pom",
        boot_version, boot_version
    );

    eprintln!(
        "  {} Fetching Spring Boot BOM {} for version resolution...",
        console::style("↓").blue(),
        boot_version
    );

    let client = match reqwest::blocking::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(10))
        .build()
    {
        Ok(c) => c,
        Err(_) => return HashMap::new(),
    };

    let mut versions = HashMap::new();
    let mut bom_imports: Vec<(String, String, String)> = Vec::new(); // (groupId, artifactId, version)

    // Parse the main BOM
    if let Some((props, _)) = fetch_and_parse_bom_pom(&client, &url) {
        extract_managed_versions_from_url(&client, &url, &props, &mut versions, &mut bom_imports);
    }

    // Recursively resolve BOM imports (max 2 levels deep for Spring ecosystem)
    for (group_id, artifact_id, version) in &bom_imports {
        let nested_url = format!(
            "https://repo1.maven.org/maven2/{}/{}/{}/{}-{}.pom",
            group_id.replace('.', "/"),
            artifact_id,
            version,
            artifact_id,
            version
        );
        let mut nested_bom_imports = Vec::new();
        if let Some((nested_props, _)) = fetch_and_parse_bom_pom(&client, &nested_url) {
            extract_managed_versions_from_url(
                &client, &nested_url, &nested_props, &mut versions, &mut nested_bom_imports,
            );
        }
    }

    if !versions.is_empty() {
        eprintln!(
            "  {} Spring Boot BOM: {} managed versions extracted (including nested BOMs)",
            console::style("✓").green(),
            versions.len()
        );
    }

    versions
}

/// Fetch and parse a POM file, returning (properties, document_body).
fn fetch_and_parse_bom_pom(
    client: &reqwest::blocking::Client,
    url: &str,
) -> Option<(std::collections::HashMap<String, String>, String)> {
    use std::collections::HashMap;

    let response = client.get(url).send().ok()?;
    if !response.status().is_success() {
        return None;
    }
    let body = response.text().ok()?;
    let doc = roxmltree::Document::parse(&body).ok()?;

    let root = doc.root_element();
    let mut props: HashMap<String, String> = HashMap::new();
    for node in root.children() {
        if node.tag_name().name() == "properties" {
            for child in node.children() {
                if child.is_element() {
                    if let Some(val) = child.text() {
                        props.insert(child.tag_name().name().to_string(), val.trim().to_string());
                    }
                }
            }
        }
    }

    Some((props, body))
}

/// Extract managed versions from a BOM POM, collecting direct versions and BOM imports.
fn extract_managed_versions_from_url(
    _client: &reqwest::blocking::Client,
    url: &str,
    parent_props: &std::collections::HashMap<String, String>,
    versions: &mut std::collections::HashMap<String, String>,
    bom_imports: &mut Vec<(String, String, String)>,
) {
    use std::collections::HashMap;

    let client2 = match reqwest::blocking::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(10))
        .build()
    {
        Ok(c) => c,
        Err(_) => return,
    };

    let response = match client2.get(url).send() {
        Ok(r) if r.status().is_success() => r,
        _ => return,
    };

    let body = match response.text() {
        Ok(b) => b,
        Err(_) => return,
    };

    let doc = match roxmltree::Document::parse(&body) {
        Ok(d) => d,
        Err(_) => return,
    };

    let root = doc.root_element();

    // Collect this POM's own properties, merging with parent
    let mut props: HashMap<String, String> = parent_props.clone();
    for node in root.children() {
        if node.tag_name().name() == "properties" {
            for child in node.children() {
                if child.is_element() {
                    if let Some(val) = child.text() {
                        props.insert(child.tag_name().name().to_string(), val.trim().to_string());
                    }
                }
            }
        }
    }

    for node in root.descendants() {
        if node.tag_name().name() == "dependencyManagement" {
            for deps_node in node.descendants() {
                if deps_node.tag_name().name() == "dependency" {
                    let mut group_id = String::new();
                    let mut artifact_id = String::new();
                    let mut version = String::new();
                    let mut scope = String::new();
                    let mut dep_type = String::new();

                    for child in deps_node.children() {
                        match child.tag_name().name() {
                            "groupId" => group_id = child.text().unwrap_or("").trim().to_string(),
                            "artifactId" => artifact_id = child.text().unwrap_or("").trim().to_string(),
                            "version" => version = child.text().unwrap_or("").trim().to_string(),
                            "scope" => scope = child.text().unwrap_or("").trim().to_string(),
                            "type" => dep_type = child.text().unwrap_or("").trim().to_string(),
                            _ => {}
                        }
                    }

                    if group_id.is_empty() || artifact_id.is_empty() || version.is_empty() {
                        continue;
                    }

                    let resolved_version = resolve_pom_properties(&version, &props);

                    if scope == "import" && dep_type == "pom" {
                        // BOM import — collect for recursive resolution
                        bom_imports.push((
                            resolve_pom_properties(&group_id, &props),
                            resolve_pom_properties(&artifact_id, &props),
                            resolved_version,
                        ));
                    } else {
                        let coord = format!("{}:{}", group_id, artifact_id);
                        versions.entry(coord).or_insert(resolved_version);
                    }
                }
            }
        }
    }
}

/// Resolve ${property} references in a POM version string.
fn resolve_pom_properties(value: &str, props: &std::collections::HashMap<String, String>) -> String {
    let mut result = value.to_string();
    for _ in 0..10 {
        if !result.contains("${") {
            break;
        }
        let mut new_result = result.clone();
        while let Some(start) = new_result.find("${") {
            if let Some(end) = new_result[start..].find('}') {
                let key = &new_result[start + 2..start + end];
                if let Some(val) = props.get(key) {
                    new_result = format!("{}{}{}", &new_result[..start], val, &new_result[start + end + 1..]);
                } else {
                    break;
                }
            } else {
                break;
            }
        }
        if new_result == result {
            break;
        }
        result = new_result;
    }
    result
}

/// Derive Spring Framework version from Spring Boot version.
/// Spring Boot 4.0.x → Spring Framework 7.0.x
/// Spring Boot 3.4.x → Spring Framework 6.2.x
/// Returns None if version pattern is unrecognized.
fn spring_boot_to_framework_version(boot_ver: &str) -> Option<String> {
    let parts: Vec<&str> = boot_ver.split('.').collect();
    if parts.len() < 2 { return None; }
    let major: u32 = parts[0].parse().ok()?;
    let minor: u32 = parts[1].parse().ok()?;
    // Mapping: Boot 4.0 → Framework 7.0, Boot 3.4 → Framework 6.2, Boot 3.3 → Framework 6.1
    let (fw_major, fw_minor) = match (major, minor) {
        (4, m) => (7, m),          // Boot 4.x → Framework 7.x
        (3, 4) => (6, 2),          // Boot 3.4 → Framework 6.2
        (3, 3) => (6, 1),          // Boot 3.3 → Framework 6.1
        (3, m) => (6, m.saturating_sub(1)), // approximate
        _ => return None,
    };
    let patch = if parts.len() >= 3 { parts[2] } else { "0" };
    Some(format!("{}.{}.{}", fw_major, fw_minor, patch))
}

fn extract_string_value(line: &str) -> Option<String> {
    // Handle: key = 'value' or key = "value" or key = value
    let parts: Vec<&str> = line.splitn(2, '=').collect();
    if parts.len() != 2 {
        return None;
    }
    let val = parts[1].trim().trim_matches('\'').trim_matches('"').trim();
    if val.is_empty() || val.contains("${") {
        None
    } else {
        Some(val.to_string())
    }
}

/// Extract string argument from a Gradle method call: `group "com.example"` or `group 'com.example'`
fn extract_string_arg(line: &str) -> Option<String> {
    // Find first quoted string after a space (method-call style: `group "value"`)
    let trimmed = line.trim();
    for (i, c) in trimmed.char_indices() {
        if c == '\'' || c == '"' {
            let rest = &trimmed[i + 1..];
            if let Some(end) = rest.find(c) {
                let val = &rest[..end];
                if !val.is_empty() && !val.contains("${") {
                    return Some(val.to_string());
                }
            }
            break;
        }
    }
    None
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
