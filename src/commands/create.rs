use anyhow::{bail, Result};
use console::style;
use std::collections::BTreeMap;

use crate::config;
use crate::config::schema::YmConfig;

/// Create a new module in a workspace.
pub fn execute(name: String, template: Option<String>, include_deps: bool) -> Result<()> {
    let (config_path, cfg) = config::load_or_find_config()?;
    let project = config::project_dir(&config_path);

    if cfg.workspaces.is_none() {
        bail!("Not a workspace project. Use 'ym init' to create a standalone project.");
    }

    let template = template.as_deref().unwrap_or("app");

    // Determine target directory based on template
    let dir = match template {
        "lib" | "library" => project.join("libs").join(&name),
        _ => project.join("apps").join(&name),
    };

    if dir.exists() {
        bail!("Directory '{}' already exists", dir.display());
    }

    // Create directory structure
    let pkg = super::init::default_package(&name);
    let pkg_path = pkg.replace('.', "/");

    let src_dir = dir.join("src").join("main").join("java");
    std::fs::create_dir_all(&src_dir)?;

    let resources_dir = dir.join("src").join("main").join("resources");
    std::fs::create_dir_all(&resources_dir)?;

    let test_java_dir = dir.join("src").join("test").join("java");
    std::fs::create_dir_all(&test_java_dir)?;

    // Create ym.json
    let java_version = cfg.target.clone().unwrap_or_else(|| "21".to_string());

    let mut module_config = YmConfig {
        name: name.clone(),
        version: Some("1.0.0".to_string()),
        target: Some(java_version),
        ..Default::default()
    };
    module_config.package = Some(pkg.clone());

    let mut deps = BTreeMap::new();
    let mut dev_deps = BTreeMap::new();

    let pkg_dir = src_dir.join(&pkg_path);
    std::fs::create_dir_all(&pkg_dir)?;

    match template {
        "lib" | "library" => {
            if include_deps {
                dev_deps.insert("org.junit.jupiter:junit-jupiter".to_string(), "5.11.0".to_string());
            }
            let class_name = to_class_name(&name);
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

            let test_pkg_dir = test_java_dir.join(&pkg_path);
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
            module_config.main = Some(format!("{}.Main", pkg));
            if include_deps {
                deps.insert("com.google.guava:guava".to_string(), "33.4.0-jre".to_string());
                dev_deps.insert("org.junit.jupiter:junit-jupiter".to_string(), "5.11.0".to_string());
            }
            let main_content = format!(
                r#"package {};

public class Main {{
    public static void main(String[] args) {{
        System.out.println("Hello from {}!");
    }}
}}
"#,
                pkg, name
            );
            std::fs::write(pkg_dir.join("Main.java"), main_content)?;
        }
    }

    module_config.dependencies = Some(deps);
    if !dev_deps.is_empty() {
        module_config.dev_dependencies = Some(dev_deps);
    }

    let config_path = dir.join(config::CONFIG_FILE);
    config::save_config(&config_path, &module_config)?;

    println!();
    println!(
        "  {} Created module {} ({})",
        style("✓").green(),
        style(&name).bold(),
        style(template).dim()
    );
    println!("    {}", style(dir.display()).dim());
    println!();
    println!(
        "  Run {} to start developing",
        style(format!("ym dev {}", name)).cyan()
    );

    Ok(())
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
