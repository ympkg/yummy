use anyhow::{bail, Result};
use console::style;
use dialoguer::{Confirm, Input, MultiSelect, Select};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::config;
use crate::config::schema::{DependencyValue, YmConfig};
use crate::jdk_manager;

/// Entry point — aligned with spec 18-init.md
pub fn execute(
    name: Option<String>,
    interactive: bool,
    template: Option<String>,
    _yes: bool,
) -> Result<()> {
    let dir = resolve_dir(name.as_deref())?;

    // Pre-check: refuse if package.toml already exists
    let config_path = dir.join(config::CONFIG_FILE);
    if config_path.exists() {
        bail!("package.toml already exists in {}", dir.display());
    }

    if let Some(tpl) = template {
        return execute_from_template(&dir, &tpl);
    }

    if interactive {
        execute_interactive(&dir)
    } else {
        // Default: zero-question mode (like `bun init`)
        execute_defaults(&dir)
    }
}

/// Resolve the target directory path. Does NOT create the directory.
fn resolve_dir(name: Option<&str>) -> Result<PathBuf> {
    let cwd = std::env::current_dir()?;

    match name {
        None => Ok(cwd),
        Some(project_name) => Ok(cwd.join(project_name)),
    }
}

/// Zero-question mode — generate runnable project immediately
fn execute_defaults(dir: &Path) -> Result<()> {
    let dir_name = dir_name(dir);
    let pkg = default_package(&dir_name);

    let config = YmConfig {
        name: dir_name.clone(),
        group_id: "com.example".to_string(),
        version: Some("0.1.0".to_string()),
        target: Some("21".to_string()),
        package: Some(pkg.clone()),
        main: Some(format!("{}.Main", pkg)),
        dependencies: Some(BTreeMap::new()),
        scripts: Some(default_scripts()),
        ..Default::default()
    };

    write_project(dir, &config)?;

    // Run postinit hook if defined
    crate::scripts::run_script(&config.scripts, &config.env, "postinit", dir)?;

    let main_class = config.main.as_deref().unwrap_or("Main");
    let main_path = main_class.replace('.', "/");
    println!();
    println!("  {} Created package.toml", style("✓").green());
    println!("  {} Created src/main/java/{}.java", style("✓").green(), main_path);
    println!();
    println!("  Done! Next steps:");
    if dir != std::env::current_dir().unwrap_or_default() {
        println!("    cd {}", dir_name);
    }
    println!("    ym dev");
    println!();

    Ok(())
}

/// Interactive mode (`-i`) — ask package name, Java version, template, JDK
fn execute_interactive(dir: &Path) -> Result<()> {
    let default_name = dir_name(dir);

    // 1. Package name
    let pkg_input: String = Input::new()
        .with_prompt("package name")
        .default(default_package(&default_name))
        .interact_text()?;

    // 2. Java version — scan installed JDKs
    println!();
    let term = console::Term::stderr();
    let _ = term.write_line(&format!("  {} Scanning JDKs...", style("→").blue()));
    let jdks = jdk_manager::scan_jdks();
    let _ = term.clear_last_lines(1);

    let java_version = select_java_version(&jdks)?;

    // 3. Template selection
    let template_items = ["app", "lib", "spring-boot"];
    let template_idx = Select::new()
        .with_prompt("template")
        .items(&template_items)
        .default(0)
        .interact()?;
    let template = template_items[template_idx];

    // 4. DEV JDK selection (recommend JBR for hot reload)
    let dev_jdk_path = select_jdk("Select DEV JDK (for hot reload)", &jdks, true)?;

    // Build config
    let dir_name = dir_name(dir);
    let mut config = YmConfig {
        name: dir_name.clone(),
        group_id: "com.example".to_string(),
        version: Some("0.1.0".to_string()),
        target: Some(java_version),
        package: Some(pkg_input.clone()),
        dependencies: Some(BTreeMap::new()),
        scripts: Some(default_scripts()),
        ..Default::default()
    };

    // Set up env with DEV_JAVA_HOME if JDK was selected
    if let Some(ref path) = dev_jdk_path {
        let mut env = BTreeMap::new();
        env.insert("DEV_JAVA_HOME".to_string(), shorten_home(path));
        config.env = Some(env);
        // Update scripts to use DEV_JAVA_HOME
        use config::schema::ScriptValue;
        let mut scripts = BTreeMap::new();
        scripts.insert("dev".to_string(), ScriptValue::Simple("JAVA_HOME=$DEV_JAVA_HOME ymc dev".to_string()));
        scripts.insert("build".to_string(), ScriptValue::Simple("ymc build".to_string()));
        scripts.insert("test".to_string(), ScriptValue::Simple("ymc test".to_string()));
        config.scripts = Some(scripts);
    }

    // Apply template-specific config
    match template {
        "spring-boot" => {
            let mut deps = BTreeMap::new();
            deps.insert(
                "org.springframework.boot:spring-boot-starter-web".to_string(),
                DependencyValue::Simple("3.4.0".to_string()),
            );
            config.dependencies = Some(deps);
            config.main = Some(format!("{}.Application", pkg_input));
        }
        "lib" => {
            let mut deps = BTreeMap::new();
            deps.insert(
                "org.junit.jupiter:junit-jupiter".to_string(),
                DependencyValue::Detailed(crate::config::schema::DependencySpec {
                    version: Some("5.11.0".to_string()),
                    scope: Some("test".to_string()),
                    ..Default::default()
                }),
            );
            deps.insert(
                "org.junit.platform:junit-platform-console-standalone".to_string(),
                DependencyValue::Detailed(crate::config::schema::DependencySpec {
                    version: Some("1.11.0".to_string()),
                    scope: Some("test".to_string()),
                    ..Default::default()
                }),
            );
            config.dependencies = Some(deps);
            config.main = None; // lib has no main
        }
        _ => {
            // app (default)
            config.main = Some(format!("{}.Main", pkg_input));
        }
    }

    // 5. Optional dependency selection (checkbox)
    let extra_deps = select_optional_deps(template)?;
    if !extra_deps.is_empty() {
        let deps = config.dependencies.get_or_insert_with(BTreeMap::new);
        for (coord, value) in extra_deps {
            deps.insert(coord, value);
        }
    }

    write_project_for_template(dir, &config, template)?;

    // Run postinit hook if defined
    crate::scripts::run_script(&config.scripts, &config.env, "postinit", dir)?;

    println!();
    println!(
        "  {} Created {} project",
        style("✓").green(),
        style(template).cyan()
    );
    println!();
    println!("  Done! Next steps:");
    if dir != std::env::current_dir().unwrap_or_default() {
        println!("    cd {}", dir_name);
    }
    println!("    ym dev");
    println!();

    Ok(())
}

fn select_java_version(_jdks: &[jdk_manager::DetectedJdk]) -> Result<String> {
    let version_items = ["25 (latest)", "21 (LTS)", "17 (LTS)"];
    let version_idx = Select::new()
        .with_prompt("Java version")
        .items(&version_items)
        .default(0)
        .interact()?;
    Ok(match version_idx {
        0 => "25".to_string(),
        1 => "21".to_string(),
        2 => "17".to_string(),
        _ => "21".to_string(),
    })
}

/// Interactive JDK selection with arrow keys.
fn select_jdk(label: &str, jdks: &[jdk_manager::DetectedJdk], prefer_dcevm: bool) -> Result<Option<PathBuf>> {
    println!();
    println!("  {}", style(label).bold());

    if jdks.is_empty() {
        println!("  No JDKs found locally.");
        let download = Confirm::new()
            .with_prompt("  Download a JDK?")
            .default(true)
            .interact()?;

        if download {
            return download_jdk_interactive();
        }
        return Ok(None);
    }

    // Sort: when prefer_dcevm, put JBR (has_dcevm) entries first
    let sorted_jdks: Vec<&jdk_manager::DetectedJdk> = if prefer_dcevm {
        let mut dcevm: Vec<_> = jdks.iter().filter(|j| j.has_dcevm).collect();
        let mut rest: Vec<_> = jdks.iter().filter(|j| !j.has_dcevm).collect();
        dcevm.append(&mut rest);
        dcevm
    } else {
        jdks.iter().collect()
    };

    let mut items: Vec<String> = Vec::new();
    // Add "Skip" as first option
    items.push(format!("{}", style("Skip").dim()));

    for jdk in &sorted_jdks {
        let mut label = format!(
            "{:<20} {}  ({})",
            jdk.display_name(),
            style(jdk.path.display()).dim(),
            jdk.source,
        );
        if prefer_dcevm && jdk.has_dcevm {
            label = format!("{} {}", label, style("★ hot reload").yellow());
        }
        items.push(label);
    }
    items.push(format!("{}", style("Download other...").cyan()));

    let default = if prefer_dcevm && sorted_jdks.first().map(|j| j.has_dcevm).unwrap_or(false) {
        1 // First JBR
    } else {
        0 // Skip
    };

    let selection = Select::new()
        .items(&items)
        .default(default)
        .interact()?;

    if selection == 0 {
        return Ok(None); // Skip
    }

    // "Download other..." is the last item
    if selection == items.len() - 1 {
        return download_jdk_interactive();
    }

    let selected = sorted_jdks[selection - 1];
    println!(
        "  {} {}",
        style("✓").green(),
        selected.display_name()
    );

    Ok(Some(selected.path.clone()))
}

/// Interactive JDK download: select vendor then version.
fn download_jdk_interactive() -> Result<Option<PathBuf>> {
    // Step 1: Select provider (JBR recommended)
    let mut vendor_labels: Vec<String> = vec![
        format!("{} {}", "JetBrains Runtime (JBR)", style("★ recommended").yellow()),
    ];
    for (label, _) in jdk_manager::DOWNLOAD_VENDORS.iter().skip(0) {
        if !label.to_lowercase().contains("jetbrains") && !label.to_lowercase().contains("jbr") {
            vendor_labels.push(label.to_string());
        }
    }
    vendor_labels.push(format!("{}", style("Skip").dim()));

    println!();
    let vendor_idx = Select::new()
        .with_prompt("  Provider")
        .items(&vendor_labels)
        .default(0)
        .interact()?;

    // Last item = Skip
    if vendor_idx == vendor_labels.len() - 1 {
        return Ok(None);
    }

    // Step 2: Select version (25 default)
    let version_labels = ["25 (latest)", "21 (LTS)", "17 (LTS)"];
    let version_idx = Select::new()
        .with_prompt("  Version")
        .items(&version_labels)
        .default(0)
        .interact()?;

    let version_key = match version_idx {
        0 => "25",
        1 => "21",
        2 => "17",
        _ => "25",
    };

    // Map vendor selection to vendor key
    let vendor_key = if vendor_idx == 0 {
        "jbr"
    } else {
        // Find the matching vendor key from DOWNLOAD_VENDORS
        jdk_manager::DOWNLOAD_VENDORS
            .iter()
            .find(|(label, _)| vendor_labels.get(vendor_idx).map(|l| l == *label).unwrap_or(false))
            .map(|(_, key)| *key)
            .unwrap_or("temurin")
    };

    let path = jdk_manager::download_jdk(vendor_key, version_key)?;
    Ok(Some(path))
}

/// Replace $HOME prefix with ~ for shorter, portable paths.
fn shorten_home(path: &Path) -> String {
    if let Ok(home) = std::env::var("HOME") {
        let s = path.display().to_string();
        if let Some(rest) = s.strip_prefix(&home) {
            return format!("~{}", rest);
        }
    }
    path.display().to_string()
}

/// Default scripts for package.toml
fn default_scripts() -> BTreeMap<String, config::schema::ScriptValue> {
    use config::schema::ScriptValue;
    let mut scripts = BTreeMap::new();
    scripts.insert("dev".to_string(), ScriptValue::Simple("ymc dev".to_string()));
    scripts.insert("build".to_string(), ScriptValue::Simple("ymc build".to_string()));
    scripts.insert("test".to_string(), ScriptValue::Simple("ymc test".to_string()));
    scripts
}

/// Non-interactive init from template
fn execute_from_template(dir: &Path, template: &str) -> Result<()> {
    // Check if template is a Git URL or local directory path
    if is_custom_template(template) {
        return execute_from_custom_template(dir, template);
    }

    let dir_name = dir_name(dir);
    let pkg = default_package(&dir_name);

    let mut config = YmConfig {
        name: dir_name.clone(),
        group_id: "com.example".to_string(),
        version: Some("0.1.0".to_string()),
        target: Some("21".to_string()),
        package: Some(pkg.clone()),
        dependencies: Some(BTreeMap::new()),
        scripts: Some(default_scripts()),
        ..Default::default()
    };

    match template {
        "spring-boot" | "spring" => {
            let mut deps = BTreeMap::new();
            deps.insert(
                "org.springframework.boot:spring-boot-starter-web".to_string(),
                DependencyValue::Simple("3.4.0".to_string()),
            );
            config.dependencies = Some(deps);
            config.main = Some(format!("{}.Application", pkg));
        }
        "lib" | "library" => {
            let mut deps = BTreeMap::new();
            deps.insert(
                "org.junit.jupiter:junit-jupiter".to_string(),
                DependencyValue::Detailed(crate::config::schema::DependencySpec {
                    version: Some("5.11.0".to_string()),
                    scope: Some("test".to_string()),
                    ..Default::default()
                }),
            );
            deps.insert(
                "org.junit.platform:junit-platform-console-standalone".to_string(),
                DependencyValue::Detailed(crate::config::schema::DependencySpec {
                    version: Some("1.11.0".to_string()),
                    scope: Some("test".to_string()),
                    ..Default::default()
                }),
            );
            config.dependencies = Some(deps);
            config.main = None;
        }
        _ => {
            // "app" default
            config.main = Some(format!("{}.Main", pkg));
        }
    }

    write_project_for_template(dir, &config, template)?;

    // Run postinit hook if defined
    crate::scripts::run_script(&config.scripts, &config.env, "postinit", dir)?;

    println!();
    println!(
        "  {} Created {} project from '{}' template",
        style("✓").green(),
        style(&dir_name).bold(),
        style(template).cyan()
    );
    println!();
    println!("  Done! Next steps:");
    if dir != std::env::current_dir().unwrap_or_default() {
        println!("    cd {}", dir_name);
    }
    println!("    ym dev");
    println!();

    Ok(())
}

/// Check if template string is a custom template (Git URL or local path)
fn is_custom_template(template: &str) -> bool {
    template.starts_with("https://")
        || template.starts_with("http://")
        || template.starts_with("git@")
        || template.starts_with("./")
        || template.starts_with("../")
        || template.starts_with('/')
}

/// Init from a custom template: Git URL or local directory
fn execute_from_custom_template(dir: &Path, template: &str) -> Result<()> {
    std::fs::create_dir_all(dir)?;

    if template.starts_with("https://") || template.starts_with("http://") || template.starts_with("git@") {
        // Clone Git repo to temp dir, then copy contents
        println!(
            "  {} Cloning template from {}",
            style("→").blue(),
            style(template).dim()
        );

        let tmp = tempfile::tempdir()?;
        let status = std::process::Command::new("git")
            .args(["clone", "--depth", "1", template, tmp.path().to_str().unwrap()])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .status()?;

        if !status.success() {
            bail!("Failed to clone template from {}", template);
        }

        copy_dir_contents(tmp.path(), dir)?;
    } else {
        // Local directory template
        let src = Path::new(template);
        if !src.is_dir() {
            bail!("Template directory not found: {}", template);
        }

        println!(
            "  {} Copying template from {}",
            style("→").blue(),
            style(template).dim()
        );

        copy_dir_contents(src, dir)?;
    }

    // Load and display the generated config if it exists
    let config_path = dir.join(config::CONFIG_FILE);
    if config_path.exists() {
        // Run postinit hook if defined in the template's package.toml
        if let Ok(cfg) = config::load_config(&config_path) {
            crate::scripts::run_script(&cfg.scripts, &cfg.env, "postinit", dir)?;
        }
    }

    let dn = dir_name(dir);
    println!();
    println!(
        "  {} Created project from custom template",
        style("✓").green(),
    );
    println!();
    println!("  Done! Next steps:");
    if dir != std::env::current_dir().unwrap_or_default() {
        println!("    cd {}", dn);
    }
    println!("    ym dev");
    println!();

    Ok(())
}

/// Recursively copy directory contents, skipping .git
fn copy_dir_contents(src: &Path, dst: &Path) -> Result<()> {
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let name = entry.file_name();
        let name_str = name.to_string_lossy();

        // Skip .git directory
        if name_str == ".git" {
            continue;
        }

        let src_path = entry.path();
        let dst_path = dst.join(&name);

        if src_path.is_dir() {
            std::fs::create_dir_all(&dst_path)?;
            copy_dir_contents(&src_path, &dst_path)?;
        } else {
            std::fs::copy(&src_path, &dst_path)?;
        }
    }
    Ok(())
}

/// Write package.toml + create directories/files (default app template)
fn write_project(dir: &Path, config: &YmConfig) -> Result<()> {
    std::fs::create_dir_all(dir)?;

    let config_path = dir.join(config::CONFIG_FILE);
    config::save_config(&config_path, config)?;

    let src_dir = dir.join("src").join("main").join("java");
    std::fs::create_dir_all(&src_dir)?;
    create_sample_main(&src_dir, config)?;

    let resources_dir = dir.join("src").join("main").join("resources");
    std::fs::create_dir_all(&resources_dir)?;

    let test_dir = dir.join("src").join("test").join("java");
    std::fs::create_dir_all(&test_dir)?;

    write_gitignore(dir)?;

    Ok(())
}

/// Write project with template-specific source files
fn write_project_for_template(dir: &Path, config: &YmConfig, template: &str) -> Result<()> {
    std::fs::create_dir_all(dir)?;

    let config_path = dir.join(config::CONFIG_FILE);
    config::save_config(&config_path, config)?;

    let src_dir = dir.join("src").join("main").join("java");
    std::fs::create_dir_all(&src_dir)?;

    let resources_dir = dir.join("src").join("main").join("resources");
    std::fs::create_dir_all(&resources_dir)?;

    let test_java_dir = dir.join("src").join("test").join("java");
    std::fs::create_dir_all(&test_java_dir)?;

    let pkg = config.package.as_deref().unwrap_or("com.example");
    let pkg_dir = src_dir.join(pkg.replace('.', "/"));
    std::fs::create_dir_all(&pkg_dir)?;

    match template {
        "spring-boot" | "spring" => {
            let main_content = format!(
                r#"package {};

import org.springframework.boot.SpringApplication;
import org.springframework.boot.autoconfigure.SpringBootApplication;

@SpringBootApplication
public class Application {{
    public static void main(String[] args) {{
        SpringApplication.run(Application.class, args);
    }}
}}
"#,
                pkg
            );
            std::fs::write(pkg_dir.join("Application.java"), main_content)?;

            // Create application.yml
            std::fs::write(resources_dir.join("application.yml"), "server:\n  port: 8080\n")?;
        }
        "lib" | "library" => {
            let class_name = to_class_name(&config.name);
            let lib_content = format!(
                r#"package {};

public class {} {{
    public String greet(String name) {{
        return "Hello, " + name + "!";
    }}
}}
"#,
                pkg, class_name
            );
            std::fs::write(pkg_dir.join(format!("{}.java", class_name)), lib_content)?;

            // Create test file
            let test_pkg_dir = test_java_dir.join(pkg.replace('.', "/"));
            std::fs::create_dir_all(&test_pkg_dir)?;
            let test_content = format!(
                r#"package {};

import org.junit.jupiter.api.Test;
import static org.junit.jupiter.api.Assertions.*;

class {}Test {{
    @Test
    void testGreet() {{
        {} lib = new {}();
        assertEquals("Hello, World!", lib.greet("World"));
    }}
}}
"#,
                pkg, class_name, class_name, class_name
            );
            std::fs::write(
                test_pkg_dir.join(format!("{}Test.java", class_name)),
                test_content,
            )?;
        }
        _ => {
            // app (default)
            create_sample_main(&src_dir, config)?;
        }
    }

    write_gitignore(dir)?;

    Ok(())
}

fn write_gitignore(dir: &Path) -> Result<()> {
    let gitignore_path = dir.join(".gitignore");
    if !gitignore_path.exists() {
        std::fs::write(&gitignore_path, "out/\n.ym/\n.ym-sources.txt\n*.class\n")?;
    }
    Ok(())
}

/// my-app → myapp
pub fn sanitize_package_name(name: &str) -> String {
    name.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect::<String>()
        .to_lowercase()
}

/// my-app → com.example.myapp
pub fn default_package(name: &str) -> String {
    format!("com.example.{}", sanitize_package_name(name))
}

fn dir_name(dir: &Path) -> String {
    dir.file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("my-app")
        .to_string()
}

fn to_class_name(name: &str) -> String {
    name.split(['-', '_'])
        .map(|part| {
            let mut c = part.chars();
            match c.next() {
                None => String::new(),
                Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
            }
        })
        .collect()
}

fn create_sample_main(src_dir: &Path, config: &YmConfig) -> Result<()> {
    let main_class = config.main.as_deref().unwrap_or("Main");

    let (pkg, class_name) = if let Some(idx) = main_class.rfind('.') {
        (Some(&main_class[..idx]), &main_class[idx + 1..])
    } else {
        (config.package.as_deref(), main_class)
    };

    let pkg_dir = if let Some(p) = pkg {
        src_dir.join(p.replace('.', "/"))
    } else {
        src_dir.to_path_buf()
    };

    std::fs::create_dir_all(&pkg_dir)?;
    let main_file = pkg_dir.join(format!("{}.java", class_name));
    if main_file.exists() {
        return Ok(());
    }

    let package_decl = if let Some(p) = pkg {
        format!("package {};\n\n", p)
    } else {
        String::new()
    };

    let content = format!(
        r#"{}public class {} {{
    public static void main(String[] args) {{
        System.out.println("Hello, World!");
    }}
}}
"#,
        package_decl, class_name
    );

    std::fs::write(&main_file, content)?;
    Ok(())
}

/// Common dependency catalog for interactive selection
const COMMON_DEPS: &[(&str, &str, &str, &str)] = &[
    // (display_label, coordinate, version, scope)
    ("Jackson (JSON)",                     "com.fasterxml.jackson.core:jackson-databind", "2.18.2", "compile"),
    ("Lombok",                             "org.projectlombok:lombok",                    "1.18.36", "provided"),
    ("SLF4J + Logback",                    "ch.qos.logback:logback-classic",              "1.5.16", "compile"),
    ("Google Guava",                       "com.google.guava:guava",                      "33.4.0-jre", "compile"),
    ("Apache Commons Lang",                "org.apache.commons:commons-lang3",            "3.17.0", "compile"),
    ("JUnit Jupiter (test)",               "org.junit.jupiter:junit-jupiter",             "5.11.4", "test"),
    ("Mockito (test)",                     "org.mockito:mockito-core",                    "5.14.2", "test"),
    ("AssertJ (test)",                     "org.assertj:assertj-core",                    "3.27.3", "test"),
];

/// Interactive dependency selection via checkbox
fn select_optional_deps(template: &str) -> Result<BTreeMap<String, DependencyValue>> {
    // Skip for spring-boot (already has its own deps) and lib (already has test deps)
    if template == "spring-boot" || template == "lib" {
        return Ok(BTreeMap::new());
    }

    println!();
    let labels: Vec<&str> = COMMON_DEPS.iter().map(|(l, _, _, _)| *l).collect();
    let selections = MultiSelect::new()
        .with_prompt("Add dependencies (space to select, enter to confirm)")
        .items(&labels)
        .interact()?;

    let mut deps = BTreeMap::new();
    for idx in selections {
        let (_, coord, version, scope) = COMMON_DEPS[idx];
        let value = if scope == "compile" {
            DependencyValue::Simple(version.to_string())
        } else {
            DependencyValue::Detailed(crate::config::schema::DependencySpec {
                version: Some(version.to_string()),
                scope: Some(scope.to_string()),
                ..Default::default()
            })
        };
        deps.insert(coord.to_string(), value);
    }

    Ok(deps)
}
