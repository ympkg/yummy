use anyhow::{bail, Result};
use console::style;
use std::path::Path;

use crate::config;

pub fn execute(dry_run: bool) -> Result<()> {
    let (config_path, cfg) = config::load_or_find_config()?;
    let project = config::project_dir(&config_path);

    if cfg.private.unwrap_or(false) {
        bail!("Cannot publish a private package. Remove \"private\": true from ym.json.");
    }

    let version = cfg
        .version
        .as_deref()
        .unwrap_or("0.0.0");

    // Build first
    super::build::execute(None, true)?;

    // Generate POM
    let pom_path = project.join("out").join("pom.xml");
    generate_pom(&project, &cfg, &pom_path)?;

    println!(
        "  {} Generated POM for {}@{}",
        style("✓").green(),
        style(&cfg.name).bold(),
        version
    );

    // Find registry
    let registry = cfg
        .registries
        .as_ref()
        .and_then(|r| r.get("default"))
        .cloned()
        .unwrap_or_else(|| "https://repo1.maven.org/maven2".to_string());

    let jar_path = find_output_jar(&project, &cfg)?;
    let jar_size = std::fs::metadata(&jar_path).map(|m| m.len()).unwrap_or(0);

    if dry_run {
        println!();
        println!("  {} Dry run — would publish:", style("→").blue());
        println!("    Package:  {}@{}", style(&cfg.name).bold(), version);
        println!("    Registry: {}", style(&registry).dim());
        println!("    JAR:      {} ({:.1} KB)", jar_path.display(), jar_size as f64 / 1024.0);
        println!("    POM:      {}", pom_path.display());
        println!();
        println!("  {} No artifacts were uploaded", style("!").yellow());
        return Ok(());
    }

    // Check credentials
    let creds_path = credentials_path();
    if !creds_path.exists() {
        bail!("Not logged in. Run 'ym login' first.");
    }

    println!(
        "  {} Publishing {} to {}",
        style("→").blue(),
        style(&cfg.name).bold(),
        style(&registry).dim()
    );

    // Upload JAR + POM
    let creds = load_credentials(&creds_path)?;
    upload_artifact(&jar_path, &pom_path, &cfg, &registry, &creds)?;

    println!(
        "  {} Published {}@{}",
        style("✓").green(),
        style(&cfg.name).bold(),
        version
    );

    Ok(())
}

fn generate_pom(
    _project: &Path,
    cfg: &config::schema::YmConfig,
    output: &Path,
) -> Result<()> {
    let deps = cfg.dependencies.as_ref().cloned().unwrap_or_default();

    let mut dep_xml = String::new();
    for (coord, version) in &deps {
        let parts: Vec<&str> = coord.split(':').collect();
        if parts.len() == 2 {
            dep_xml.push_str(&format!(
                r#"    <dependency>
      <groupId>{}</groupId>
      <artifactId>{}</artifactId>
      <version>{}</version>
    </dependency>
"#,
                parts[0], parts[1], version
            ));
        }
    }

    // Extract groupId from name or use default
    let (group_id, artifact_id) = if cfg.name.contains(':') {
        let parts: Vec<&str> = cfg.name.split(':').collect();
        (parts[0].to_string(), parts[1].to_string())
    } else {
        ("com.example".to_string(), cfg.name.clone())
    };

    let pom = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<project xmlns="http://maven.apache.org/POM/4.0.0"
         xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance"
         xsi:schemaLocation="http://maven.apache.org/POM/4.0.0 http://maven.apache.org/xsd/maven-4.0.0.xsd">
  <modelVersion>4.0.0</modelVersion>

  <groupId>{group_id}</groupId>
  <artifactId>{artifact_id}</artifactId>
  <version>{version}</version>
  <packaging>jar</packaging>

  <dependencies>
{dep_xml}  </dependencies>
</project>
"#,
        version = cfg.version.as_deref().unwrap_or("0.0.0")
    );

    if let Some(parent) = output.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(output, pom)?;
    Ok(())
}

fn find_output_jar(project: &Path, cfg: &config::schema::YmConfig) -> Result<std::path::PathBuf> {
    // For release builds, we should create a JAR from out/classes
    let classes_dir = config::output_classes_dir(project);
    let jar_name = format!(
        "{}-{}.jar",
        cfg.name,
        cfg.version.as_deref().unwrap_or("0.0.0")
    );
    let jar_path = project.join("out").join(&jar_name);

    // Create JAR using jar command
    let status = std::process::Command::new("jar")
        .arg("cf")
        .arg(&jar_path)
        .arg("-C")
        .arg(&classes_dir)
        .arg(".")
        .status()?;

    if !status.success() {
        bail!("Failed to create JAR");
    }

    Ok(jar_path)
}

struct Credentials {
    username: String,
    password: String,
}

fn credentials_path() -> std::path::PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    std::path::PathBuf::from(home).join(".ym").join("credentials.json")
}

fn load_credentials(path: &Path) -> Result<Credentials> {
    let content = std::fs::read_to_string(path)?;
    let v: serde_json::Value = serde_json::from_str(&content)?;
    Ok(Credentials {
        username: v["username"].as_str().unwrap_or("").to_string(),
        password: v["password"].as_str().unwrap_or("").to_string(),
    })
}

fn upload_artifact(
    jar_path: &Path,
    pom_path: &Path,
    cfg: &config::schema::YmConfig,
    registry: &str,
    creds: &Credentials,
) -> Result<()> {
    let (group_id, artifact_id) = if cfg.name.contains(':') {
        let parts: Vec<&str> = cfg.name.split(':').collect();
        (parts[0].to_string(), parts[1].to_string())
    } else {
        ("com.example".to_string(), cfg.name.clone())
    };

    let version = cfg.version.as_deref().unwrap_or("0.0.0");
    let group_path = group_id.replace('.', "/");
    let base_url = format!(
        "{}/{}/{}/{}",
        registry, group_path, artifact_id, version
    );

    let client = reqwest::blocking::Client::builder()
        .user_agent("ym/0.1.0")
        .build()?;

    // Upload POM
    let pom_bytes = std::fs::read(pom_path)?;
    let pom_url = format!("{}/{}-{}.pom", base_url, artifact_id, version);
    let resp = client
        .put(&pom_url)
        .basic_auth(&creds.username, Some(&creds.password))
        .body(pom_bytes)
        .send()?;

    if !resp.status().is_success() {
        bail!("Failed to upload POM: HTTP {}", resp.status());
    }

    // Upload JAR
    let jar_bytes = std::fs::read(jar_path)?;
    let jar_url = format!("{}/{}-{}.jar", base_url, artifact_id, version);
    let resp = client
        .put(&jar_url)
        .basic_auth(&creds.username, Some(&creds.password))
        .body(jar_bytes)
        .send()?;

    if !resp.status().is_success() {
        bail!("Failed to upload JAR: HTTP {}", resp.status());
    }

    Ok(())
}
