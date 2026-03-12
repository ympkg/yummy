use anyhow::{bail, Result};
use console::style;
use std::path::Path;

use crate::config;

pub fn execute(target: Option<String>, registry: Option<&str>, dry_run: bool) -> Result<()> {
    let (config_path, cfg) = config::load_or_find_config()?;
    let project = config::project_dir(&config_path);

    // If target specified in workspace, redirect to that module
    if let Some(ref module_name) = target {
        if cfg.workspaces.is_some() {
            return publish_workspace_module(&config_path, &project, module_name, registry, dry_run);
        }
        bail!("--target only works in workspace mode. Current project is not a workspace.");
    }

    if cfg.private.unwrap_or(false) {
        bail!("Cannot publish a private package. Remove 'private = true' from package.toml.");
    }

    let version = cfg
        .version
        .as_deref()
        .unwrap_or("0.0.0");

    // Run prepublish script
    crate::scripts::run_script(&cfg, "prepublish", &project)?;

    // Build first
    super::build::execute(vec![], true)?;

    // Generate POM
    let pom_path = project.join("out").join("pom.xml");
    generate_pom(&project, &cfg, &pom_path)?;

    println!(
        "  {} Generated POM for {}@{}",
        style("✓").green(),
        style(&cfg.name).bold(),
        version
    );

    // Find registry: --registry flag → "default" key → single registry → error
    let registry_url = if let Some(name) = registry {
        cfg.registries
            .as_ref()
            .and_then(|r| r.get(name))
            .map(|v| v.url().to_string())
            .ok_or_else(|| anyhow::anyhow!("Registry '{}' not found in [registries]", name))?
    } else {
        let regs = cfg.registries.as_ref();
        match regs {
            None => bail!("No [registries] configured in package.toml. Add a registry to publish."),
            Some(r) if r.is_empty() => bail!("No [registries] configured in package.toml. Add a registry to publish."),
            Some(r) => {
                if let Some(v) = r.get("default") {
                    v.url().to_string()
                } else if r.len() == 1 {
                    r.values().next().unwrap().url().to_string()
                } else {
                    bail!(
                        "Multiple registries configured but no 'default' key. Use --registry <name> to specify.\nAvailable: {}",
                        r.keys().cloned().collect::<Vec<_>>().join(", ")
                    );
                }
            }
        }
    };

    let jar_path = find_output_jar(&project, &cfg)?;
    let jar_size = std::fs::metadata(&jar_path).map(|m| m.len()).unwrap_or(0);

    // Generate sources JAR
    let sources_jar = generate_sources_jar(&project, &cfg)?;

    // Generate javadoc JAR (non-fatal if fails)
    let javadoc_jar = generate_javadoc_jar(&project, &cfg)?;

    if dry_run {
        println!();
        println!("  {} dry run — would publish:", style("➜").green());
        println!("    Package:  {}@{}", style(&cfg.name).bold(), version);
        println!("    Registry: {}", style(&registry_url).dim());
        let sources_size = std::fs::metadata(&sources_jar).map(|m| m.len()).unwrap_or(0);
        println!("    JAR:      {} ({:.1} KB)", jar_path.display(), jar_size as f64 / 1024.0);
        println!("    Sources:  {} ({:.1} KB)", sources_jar.display(), sources_size as f64 / 1024.0);
        if let Some(ref jd) = javadoc_jar {
            let jd_size = std::fs::metadata(jd).map(|m| m.len()).unwrap_or(0);
            println!("    Javadoc:  {} ({:.1} KB)", jd.display(), jd_size as f64 / 1024.0);
        }
        println!("    POM:      {}", pom_path.display());
        println!("    Checksums: SHA-256 + MD5 for each artifact");
        let gpg = std::process::Command::new("gpg").arg("--version")
            .stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null())
            .status().map(|s| s.success()).unwrap_or(false);
        if gpg {
            println!("    GPG:      signatures will be generated");
        } else {
            println!("    GPG:      {} (gpg not found)", style("skipped").yellow());
        }
        println!();
        println!("  {} No artifacts were uploaded", style("!").yellow());
        return Ok(());
    }

    // Check credentials
    let creds_path = credentials_path();
    let creds = load_credentials_for_registry(&creds_path, &registry_url)?;

    println!(
        "  {} publishing {} to {}",
        style("➜").green(),
        style(&cfg.name).bold(),
        style(&registry_url).dim()
    );

    // Upload JAR + POM + sources JAR + javadoc JAR + checksums + GPG signatures
    upload_artifact(&jar_path, &pom_path, &sources_jar, javadoc_jar.as_deref(), &cfg, &registry_url, &creds)?;

    // Sonatype OSSRH staging flow: close + release if the URL looks like Sonatype
    if is_sonatype_url(&registry_url) {
        println!(
            "  {} Detected Sonatype OSSRH — attempting staging close + release",
            style("➜").green()
        );
        match sonatype_close_and_release(&registry_url, &cfg, &creds) {
            Ok(()) => {
                println!("  {} Staging repository released to Maven Central", style("✓").green());
            }
            Err(e) => {
                println!(
                    "  {} Staging close/release failed: {}",
                    style("!").yellow(),
                    e
                );
                println!("    Complete the release manually at https://oss.sonatype.org");
            }
        }
    }

    println!(
        "  {} Published {}@{}",
        style("✓").green(),
        style(&cfg.name).bold(),
        version
    );

    // Run postpublish script
    crate::scripts::run_script(&cfg, "postpublish", &project)?;

    Ok(())
}

/// Publish a specific workspace module.
fn publish_workspace_module(
    config_path: &Path,
    workspace_root: &Path,
    module_name: &str,
    registry: Option<&str>,
    dry_run: bool,
) -> Result<()> {
    let ws = crate::workspace::graph::WorkspaceGraph::build(workspace_root)?;
    let pkg = ws.get_package(module_name)
        .ok_or_else(|| anyhow::anyhow!("Module '{}' not found in workspace", module_name))?;

    let module_cfg = &pkg.config;
    if module_cfg.private.unwrap_or(false) {
        bail!("Module '{}' is private. Remove 'private = true' to publish.", module_name);
    }

    let version = module_cfg.version.as_deref().unwrap_or("0.0.0");
    let module_path = &pkg.path;

    // Build the module
    super::build::execute(vec![module_name.to_string()], true)?;

    // Generate POM for the module
    let pom_path = module_path.join("out").join("pom.xml");
    generate_pom(module_path, module_cfg, &pom_path)?;

    println!(
        "  {} Generated POM for {}@{}",
        style("✓").green(),
        style(&module_cfg.name).bold(),
        version
    );

    // Get registry from root config (registries are defined at root level)
    let root_cfg = config::load_config(config_path)?;
    let registry_url = if let Some(name) = registry {
        root_cfg.registries
            .as_ref()
            .and_then(|r| r.get(name))
            .map(|v| v.url().to_string())
            .ok_or_else(|| anyhow::anyhow!("Registry '{}' not found in [registries]", name))?
    } else {
        let regs = root_cfg.registries.as_ref();
        match regs {
            None => bail!("No [registries] configured in root package.toml"),
            Some(r) if r.is_empty() => bail!("No [registries] configured in root package.toml"),
            Some(r) => {
                if let Some(v) = r.get("default") {
                    v.url().to_string()
                } else if r.len() == 1 {
                    r.values().next().unwrap().url().to_string()
                } else {
                    bail!(
                        "Multiple registries configured but no 'default' key. Use --registry <name>.\nAvailable: {}",
                        r.keys().cloned().collect::<Vec<_>>().join(", ")
                    );
                }
            }
        }
    };

    let jar_path = find_output_jar(module_path, module_cfg)?;
    let sources_jar = generate_sources_jar(module_path, module_cfg)?;
    let javadoc_jar = generate_javadoc_jar(module_path, module_cfg)?;

    if dry_run {
        let jar_size = std::fs::metadata(&jar_path).map(|m| m.len()).unwrap_or(0);
        println!();
        println!("  {} dry run — would publish module:", style("➜").green());
        println!("    Package:  {}@{}", style(&module_cfg.name).bold(), version);
        println!("    GroupId:  {}", module_cfg.group_id);
        println!("    Registry: {}", style(&registry_url).dim());
        println!("    JAR:      {} ({:.1} KB)", jar_path.display(), jar_size as f64 / 1024.0);
        println!();
        println!("  {} No artifacts were uploaded", style("!").yellow());
        return Ok(());
    }

    let creds_path = credentials_path();
    let creds = load_credentials_for_registry(&creds_path, &registry_url)?;

    println!(
        "  {} publishing module {} to {}",
        style("➜").green(),
        style(&module_cfg.name).bold(),
        style(&registry_url).dim()
    );

    upload_artifact(&jar_path, &pom_path, &sources_jar, javadoc_jar.as_deref(), module_cfg, &registry_url, &creds)?;

    if is_sonatype_url(&registry_url) {
        match sonatype_close_and_release(&registry_url, module_cfg, &creds) {
            Ok(()) => println!("  {} Staging repository released", style("✓").green()),
            Err(e) => println!("  {} Staging close/release failed: {}", style("!").yellow(), e),
        }
    }

    println!(
        "  {} Published {}@{}",
        style("✓").green(),
        style(&module_cfg.name).bold(),
        version
    );

    Ok(())
}

fn generate_pom(
    project: &Path,
    cfg: &config::schema::YmConfig,
    output: &Path,
) -> Result<()> {
    // Load workspace graph if we're in a workspace (for module dep mapping)
    let ws = config::find_workspace_root(project)
        .and_then(|root| crate::workspace::graph::WorkspaceGraph::build(&root).ok());

    // Build dependency XML, respecting scopes (skip test scope per spec)
    let mut dep_xml = String::new();
    if let Some(ref deps) = cfg.dependencies {
        for (coord, value) in deps {
            // Handle workspace module deps (not a Maven coordinate, workspace=true)
            if !crate::config::schema::is_maven_dep(coord) && value.is_workspace() {
                if let Some(ref ws) = ws {
                    if let Some(pkg) = ws.get_package(coord) {
                        let mod_version = pkg.config.version.as_deref().unwrap_or("0.0.0");
                        dep_xml.push_str(&format!(
                            r#"    <dependency>
      <groupId>{}</groupId>
      <artifactId>{}</artifactId>
      <version>{}</version>
    </dependency>
"#,
                            pkg.config.group_id, pkg.config.name, mod_version
                        ));
                    }
                }
                continue;
            }

            if !crate::config::schema::is_maven_dep(coord) || value.is_workspace() {
                continue;
            }
            let version = match value.version() {
                Some(v) => v,
                None => continue,
            };
            let scope = value.scope();
            // Test-scoped deps are NOT written to POM (per spec)
            if scope == "test" {
                continue;
            }
            let resolved = cfg.resolve_key(coord);
            let parts: Vec<&str> = resolved.split(':').collect();
            if parts.len() == 2 {
                let scope_xml = match scope {
                    "compile" => String::new(),
                    s => format!("\n      <scope>{}</scope>", s),
                };
                dep_xml.push_str(&format!(
                    r#"    <dependency>
      <groupId>{}</groupId>
      <artifactId>{}</artifactId>
      <version>{}</version>{scope_xml}
    </dependency>
"#,
                    parts[0], parts[1], version
                ));
            }
        }
    }

    let group_id = &cfg.group_id;
    let artifact_id = &cfg.name;

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

/// Generate a sources JAR containing all .java files from src/main/java/.
fn generate_sources_jar(project: &Path, cfg: &config::schema::YmConfig) -> Result<std::path::PathBuf> {
    let source_dir = config::source_dir(project);
    let jar_name = format!(
        "{}-{}-sources.jar",
        cfg.name,
        cfg.version.as_deref().unwrap_or("0.0.0")
    );
    let jar_path = project.join("out").join(&jar_name);

    if !source_dir.exists() {
        // No sources, create empty sources JAR
        let status = std::process::Command::new("jar")
            .arg("cf")
            .arg(&jar_path)
            .arg("-C")
            .arg(project.join("out"))
            .arg(".")
            .status()?;
        if !status.success() {
            bail!("Failed to create sources JAR");
        }
        return Ok(jar_path);
    }

    let status = std::process::Command::new("jar")
        .arg("cf")
        .arg(&jar_path)
        .arg("-C")
        .arg(&source_dir)
        .arg(".")
        .status()?;

    if !status.success() {
        bail!("Failed to create sources JAR");
    }

    println!(
        "  {} Generated sources JAR",
        style("✓").green()
    );

    Ok(jar_path)
}

/// Generate a Javadoc JAR by running `javadoc` on sources and packaging output.
fn generate_javadoc_jar(project: &Path, cfg: &config::schema::YmConfig) -> Result<Option<std::path::PathBuf>> {
    let source_dir = config::source_dir(project);
    if !source_dir.exists() {
        return Ok(None);
    }

    // Collect .java files
    let java_files: Vec<std::path::PathBuf> = walkdir::WalkDir::new(&source_dir)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("java"))
        .map(|e| e.path().to_path_buf())
        .collect();

    if java_files.is_empty() {
        return Ok(None);
    }

    let javadoc_dir = project.join("out").join("javadoc");
    let _ = std::fs::remove_dir_all(&javadoc_dir);
    std::fs::create_dir_all(&javadoc_dir)?;

    // Build classpath for javadoc (same as compile classpath)
    let cache = config::maven_cache_dir(project);
    let cp_str: String = walkdir::WalkDir::new(&cache)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("jar"))
        .map(|e| e.path().to_string_lossy().to_string())
        .collect::<Vec<_>>()
        .join(":");

    // Write file list to argfile
    let argfile = project.join("out").join("javadoc-files.txt");
    let file_list: String = java_files.iter()
        .map(|p| p.to_string_lossy().to_string())
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(&argfile, &file_list)?;

    let mut cmd = std::process::Command::new("javadoc");
    cmd.arg("-d").arg(&javadoc_dir)
        .arg("-quiet")
        .arg("-Xdoclint:none")
        .arg(format!("@{}", argfile.display()))
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());

    if !cp_str.is_empty() {
        cmd.arg("-classpath").arg(&cp_str);
    }

    // Detect source version
    let target = cfg.target.as_deref().unwrap_or("21");
    cmd.arg("-source").arg(target);

    let status = cmd.status();
    let _ = std::fs::remove_file(&argfile);

    // javadoc may fail (e.g. syntax issues) — treat as non-fatal
    match status {
        Ok(s) if s.success() => {}
        _ => {
            println!(
                "  {} Javadoc generation failed (non-fatal), skipping javadoc JAR",
                style("!").yellow()
            );
            return Ok(None);
        }
    }

    let jar_name = format!(
        "{}-{}-javadoc.jar",
        cfg.name,
        cfg.version.as_deref().unwrap_or("0.0.0")
    );
    let jar_path = project.join("out").join(&jar_name);

    let status = std::process::Command::new("jar")
        .arg("cf")
        .arg(&jar_path)
        .arg("-C")
        .arg(&javadoc_dir)
        .arg(".")
        .status()?;

    if !status.success() {
        println!(
            "  {} Failed to create javadoc JAR (non-fatal)",
            style("!").yellow()
        );
        return Ok(None);
    }

    println!("  {} Generated javadoc JAR", style("✓").green());
    Ok(Some(jar_path))
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
    crate::home_dir().join(".ym").join("credentials.json")
}

fn load_credentials_for_registry(path: &Path, registry_url: &str) -> Result<Credentials> {
    // Priority 1: Environment variables
    if let (Ok(username), Ok(password)) = (
        std::env::var("YM_REGISTRY_USERNAME"),
        std::env::var("YM_REGISTRY_PASSWORD"),
    ) {
        return Ok(Credentials { username, password });
    }
    if let Ok(token) = std::env::var("YM_REGISTRY_TOKEN") {
        return Ok(Credentials { username: token, password: String::new() });
    }

    // Priority 1.5: GitHub Token for GitHub Packages
    if registry_url.contains("maven.pkg.github.com") {
        if let Ok(token) = std::env::var("GITHUB_TOKEN") {
            return Ok(Credentials { username: "github-actions".to_string(), password: token });
        }
    }

    // Priority 2: credentials.json file
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => bail!(
            "No credentials found for '{}'. Run 'ym login' first, or set YM_REGISTRY_USERNAME/YM_REGISTRY_PASSWORD env vars.",
            registry_url
        ),
    };
    let map: std::collections::BTreeMap<String, serde_json::Value> = serde_json::from_str(&content)?;

    let normalized = registry_url.trim_end_matches('/');

    let entry = map.get(normalized)
        .or_else(|| map.get(&format!("{}/", normalized)));

    match entry {
        Some(v) => {
            // Support both {"username","password"} and {"token"} formats
            if let Some(token) = v.get("token").and_then(|t| t.as_str()) {
                Ok(Credentials { username: token.to_string(), password: String::new() })
            } else {
                Ok(Credentials {
                    username: v["username"].as_str().unwrap_or("").to_string(),
                    password: v["password"].as_str().unwrap_or("").to_string(),
                })
            }
        }
        None => bail!(
            "No credentials found for '{}'. Run 'ym login' first, or set YM_REGISTRY_USERNAME/YM_REGISTRY_PASSWORD env vars.",
            registry_url
        ),
    }
}

/// Compute SHA-256 hash of a file, return hex string.
fn sha256_of_file(path: &Path) -> Result<String> {
    let bytes = std::fs::read(path)?;
    Ok(crate::compiler::incremental::hash_bytes(&bytes))
}

/// Compute MD5 hash of a file, return hex string.
fn md5_of_file(path: &Path) -> Result<String> {
    use std::io::Read;
    let mut file = std::fs::File::open(path)?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf)?;
    let digest = md5::compute(&buf);
    Ok(format!("{:x}", digest))
}

/// Try to GPG-sign a file. Returns the .asc path if successful, None otherwise.
fn gpg_sign_file(path: &Path) -> Option<std::path::PathBuf> {
    let asc_path = path.with_extension(
        format!("{}.asc", path.extension().unwrap_or_default().to_string_lossy())
    );
    let status = std::process::Command::new("gpg")
        .arg("--batch")
        .arg("--yes")
        .arg("-ab")
        .arg(path)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();

    match status {
        Ok(s) if s.success() && asc_path.exists() => Some(asc_path),
        _ => None,
    }
}

/// Upload a single file to a URL with basic auth.
fn upload_file(
    client: &reqwest::blocking::Client,
    url: &str,
    path: &Path,
    creds: &Credentials,
) -> Result<()> {
    let bytes = std::fs::read(path)?;
    let resp = client
        .put(url)
        .basic_auth(&creds.username, Some(&creds.password))
        .body(bytes)
        .send()?;
    if !resp.status().is_success() {
        bail!("Failed to upload {}: HTTP {}", path.display(), resp.status());
    }
    Ok(())
}

/// Upload checksum string to a URL.
fn upload_checksum(
    client: &reqwest::blocking::Client,
    url: &str,
    checksum: &str,
    creds: &Credentials,
) -> Result<()> {
    let resp = client
        .put(url)
        .basic_auth(&creds.username, Some(&creds.password))
        .body(checksum.to_string())
        .send()?;
    if !resp.status().is_success() {
        bail!("Failed to upload checksum to {}: HTTP {}", url, resp.status());
    }
    Ok(())
}

fn upload_artifact(
    jar_path: &Path,
    pom_path: &Path,
    sources_jar_path: &Path,
    javadoc_jar_path: Option<&Path>,
    cfg: &config::schema::YmConfig,
    registry: &str,
    creds: &Credentials,
) -> Result<()> {
    let group_id = &cfg.group_id;
    let artifact_id = &cfg.name;
    let version = cfg.version.as_deref().unwrap_or("0.0.0");
    let group_path = group_id.replace('.', "/");
    let base_url = format!(
        "{}/{}/{}/{}",
        registry, group_path, artifact_id, version
    );

    let client = reqwest::blocking::Client::builder()
        .user_agent(concat!("ym/", env!("CARGO_PKG_VERSION")))
        .build()?;

    // Check if GPG is available for signing
    let gpg_available = std::process::Command::new("gpg")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);

    // Upload POM + checksums + signature
    let pom_url = format!("{}/{}-{}.pom", base_url, artifact_id, version);
    upload_file(&client, &pom_url, pom_path, creds)?;
    upload_checksums(&client, &pom_url, pom_path, creds)?;
    if gpg_available {
        upload_gpg_signature(&client, &pom_url, pom_path, creds)?;
    }

    // Upload JAR + checksums + signature
    let jar_url = format!("{}/{}-{}.jar", base_url, artifact_id, version);
    upload_file(&client, &jar_url, jar_path, creds)?;
    upload_checksums(&client, &jar_url, jar_path, creds)?;
    if gpg_available {
        upload_gpg_signature(&client, &jar_url, jar_path, creds)?;
    }

    // Upload sources JAR + checksums + signature
    let sources_url = format!("{}/{}-{}-sources.jar", base_url, artifact_id, version);
    upload_file(&client, &sources_url, sources_jar_path, creds)?;
    upload_checksums(&client, &sources_url, sources_jar_path, creds)?;
    if gpg_available {
        upload_gpg_signature(&client, &sources_url, sources_jar_path, creds)?;
    }

    // Upload javadoc JAR + checksums + signature (if available)
    if let Some(javadoc_path) = javadoc_jar_path {
        let javadoc_url = format!("{}/{}-{}-javadoc.jar", base_url, artifact_id, version);
        upload_file(&client, &javadoc_url, javadoc_path, creds)?;
        upload_checksums(&client, &javadoc_url, javadoc_path, creds)?;
        if gpg_available {
            upload_gpg_signature(&client, &javadoc_url, javadoc_path, creds)?;
        }
    }

    if gpg_available {
        println!("  {} GPG signatures uploaded", style("✓").green());
    }
    println!("  {} SHA-256/MD5 checksums uploaded", style("✓").green());

    Ok(())
}

/// Upload SHA-256 and MD5 checksum files for an artifact.
fn upload_checksums(
    client: &reqwest::blocking::Client,
    artifact_url: &str,
    artifact_path: &Path,
    creds: &Credentials,
) -> Result<()> {
    let sha = sha256_of_file(artifact_path)?;
    upload_checksum(client, &format!("{}.sha256", artifact_url), &sha, creds)?;

    let md5 = md5_of_file(artifact_path)?;
    upload_checksum(client, &format!("{}.md5", artifact_url), &md5, creds)?;

    Ok(())
}

/// Check if a registry URL looks like Sonatype OSSRH.
fn is_sonatype_url(url: &str) -> bool {
    url.contains("oss.sonatype.org") || url.contains("s01.oss.sonatype.org")
}

/// Sonatype OSSRH staging close + release flow.
///
/// 1. Find the open staging repository
/// 2. Close it (triggers validation)
/// 3. Release it (promotes to Maven Central)
fn sonatype_close_and_release(
    registry_url: &str,
    cfg: &config::schema::YmConfig,
    creds: &Credentials,
) -> Result<()> {
    // Derive the Sonatype base URL from the registry URL
    let base_url = if registry_url.contains("s01.oss.sonatype.org") {
        "https://s01.oss.sonatype.org"
    } else {
        "https://oss.sonatype.org"
    };

    let client = reqwest::blocking::Client::builder()
        .user_agent(concat!("ym/", env!("CARGO_PKG_VERSION")))
        .timeout(std::time::Duration::from_secs(60))
        .build()?;

    // Find open staging repository for our group
    let profile_url = format!(
        "{}/service/local/staging/profile_evaluate?type=deployed&g={}&a={}&v={}",
        base_url, cfg.group_id, cfg.name,
        cfg.version.as_deref().unwrap_or("0.0.0")
    );
    let resp = client.get(&profile_url)
        .basic_auth(&creds.username, Some(&creds.password))
        .header("Accept", "application/json")
        .send()?;

    if !resp.status().is_success() {
        bail!("Failed to query staging profiles: HTTP {}", resp.status());
    }

    let body: serde_json::Value = resp.json()?;
    let profile_id = body["data"][0]["id"].as_str()
        .or_else(|| body["data"][0]["profileId"].as_str());

    let profile_id = match profile_id {
        Some(id) => id.to_string(),
        None => bail!("No staging profile found for {}", cfg.group_id),
    };

    // List open staging repos
    let repos_url = format!(
        "{}/service/local/staging/profile_repositories/{}",
        base_url, profile_id
    );
    let resp = client.get(&repos_url)
        .basic_auth(&creds.username, Some(&creds.password))
        .header("Accept", "application/json")
        .send()?;

    if !resp.status().is_success() {
        bail!("Failed to list staging repositories: HTTP {}", resp.status());
    }

    let body: serde_json::Value = resp.json()?;
    let repos = body["data"].as_array();
    let repo_id = repos
        .and_then(|arr| {
            arr.iter()
                .rfind(|r| r["type"].as_str() == Some("open"))
                .and_then(|r| r["repositoryId"].as_str())
        });

    let repo_id = match repo_id {
        Some(id) => id.to_string(),
        None => bail!("No open staging repository found. Artifacts may not have been uploaded correctly."),
    };

    println!("  {} Found staging repo: {}", style("✓").green(), &repo_id);

    // Close staging repository
    let close_url = format!(
        "{}/service/local/staging/profiles/{}/finish",
        base_url, profile_id
    );
    let close_body = serde_json::json!({
        "data": {
            "stagedRepositoryId": repo_id,
            "description": format!("Close {}@{}", cfg.name, cfg.version.as_deref().unwrap_or("0.0.0"))
        }
    });

    let resp = client.post(&close_url)
        .basic_auth(&creds.username, Some(&creds.password))
        .header("Content-Type", "application/json")
        .json(&close_body)
        .send()?;

    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().unwrap_or_default();
        bail!("Failed to close staging repo: HTTP {} — {}", status, text);
    }

    println!("  {} Staging repo closed, waiting for validation...", style("➜").green());

    // Poll for close completion (up to 2 minutes)
    let activity_url = format!(
        "{}/service/local/staging/repository/{}/activity",
        base_url, repo_id
    );
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
    let mut closed = false;
    while std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_secs(5));
        if let Ok(resp) = client.get(&activity_url)
            .basic_auth(&creds.username, Some(&creds.password))
            .header("Accept", "application/json")
            .send()
        {
            if let Ok(body) = resp.json::<serde_json::Value>() {
                if let Some(activities) = body.as_array() {
                    let has_close = activities.iter().any(|a| {
                        a["name"].as_str() == Some("close")
                            && a["events"].as_array().is_some_and(|events| {
                                events.iter().any(|e| e["name"].as_str() == Some("repositoryClosed"))
                            })
                    });
                    if has_close {
                        closed = true;
                        break;
                    }
                    // Check for close failure
                    let has_fail = activities.iter().any(|a| {
                        a["name"].as_str() == Some("close")
                            && a["events"].as_array().is_some_and(|events| {
                                events.iter().any(|e| e["name"].as_str() == Some("repositoryCloseFailed"))
                            })
                    });
                    if has_fail {
                        bail!("Staging validation failed. Check Sonatype for details.");
                    }
                }
            }
        }
    }

    if !closed {
        bail!("Staging close timed out after 120s. Check Sonatype manually.");
    }

    println!("  {} Validation passed", style("✓").green());

    // Release staging repository
    let release_url = format!(
        "{}/service/local/staging/profiles/{}/promote",
        base_url, profile_id
    );
    let release_body = serde_json::json!({
        "data": {
            "stagedRepositoryId": repo_id,
            "description": format!("Release {}@{}", cfg.name, cfg.version.as_deref().unwrap_or("0.0.0"))
        }
    });

    let resp = client.post(&release_url)
        .basic_auth(&creds.username, Some(&creds.password))
        .header("Content-Type", "application/json")
        .json(&release_body)
        .send()?;

    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().unwrap_or_default();
        bail!("Failed to release staging repo: HTTP {} — {}", status, text);
    }

    Ok(())
}

/// GPG-sign an artifact and upload the .asc file.
fn upload_gpg_signature(
    client: &reqwest::blocking::Client,
    artifact_url: &str,
    artifact_path: &Path,
    creds: &Credentials,
) -> Result<()> {
    if let Some(asc_path) = gpg_sign_file(artifact_path) {
        upload_file(client, &format!("{}.asc", artifact_url), &asc_path, creds)?;
        let _ = std::fs::remove_file(&asc_path);
    }
    Ok(())
}
