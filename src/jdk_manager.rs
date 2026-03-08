use anyhow::Result;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::process::Command;

/// A detected JDK installation.
#[derive(Clone, Debug)]
pub struct DetectedJdk {
    pub vendor: String,
    pub version: String,
    pub path: PathBuf,
    pub source: JdkSource,
    pub has_dcevm: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub enum JdkSource {
    YmManaged,
    JavaHome,
    IntelliJ,
    Sdkman,
    Jabba,
    System,
}

impl std::fmt::Display for JdkSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            JdkSource::YmManaged => write!(f, "ym"),
            JdkSource::JavaHome => write!(f, "JAVA_HOME"),
            JdkSource::IntelliJ => write!(f, "IntelliJ"),
            JdkSource::Sdkman => write!(f, "sdkman"),
            JdkSource::Jabba => write!(f, "jabba"),
            JdkSource::System => write!(f, "system"),
        }
    }
}

impl DetectedJdk {
    pub fn display_name(&self) -> String {
        format!("{} {}", self.vendor, self.version)
    }
}

/// Scan the system for installed JDKs.
pub fn scan_jdks() -> Vec<DetectedJdk> {
    let mut jdks = Vec::new();
    let mut seen_paths = std::collections::HashSet::new();

    let home = std::env::var("HOME").unwrap_or_default();

    // 1. ~/.ym/jdks/*
    scan_dir(&PathBuf::from(&home).join(".ym/jdks"), JdkSource::YmManaged, &mut jdks, &mut seen_paths);

    // 2. $JAVA_HOME
    if let Ok(java_home) = std::env::var("JAVA_HOME") {
        let p = PathBuf::from(&java_home);
        if p.exists() {
            try_add_jdk(&p, JdkSource::JavaHome, &mut jdks, &mut seen_paths);
        }
    }

    // 3. IntelliJ / JetBrains Toolbox paths
    let idea_patterns = intellij_jbr_paths(&home);
    for path in idea_patterns {
        try_add_jdk(&path, JdkSource::IntelliJ, &mut jdks, &mut seen_paths);
    }

    // 4. sdkman
    scan_dir(&PathBuf::from(&home).join(".sdkman/candidates/java"), JdkSource::Sdkman, &mut jdks, &mut seen_paths);

    // 5. jabba
    scan_dir(&PathBuf::from(&home).join(".jabba/jdk"), JdkSource::Jabba, &mut jdks, &mut seen_paths);

    // 6. System paths
    let system_dirs = [
        "/usr/lib/jvm",
        "/usr/java",
        "/Library/Java/JavaVirtualMachines",
    ];
    for dir in &system_dirs {
        scan_dir(&PathBuf::from(dir), JdkSource::System, &mut jdks, &mut seen_paths);
    }

    jdks
}

/// Find IntelliJ JBR paths by scanning Toolbox and standalone installs.
fn intellij_jbr_paths(home: &str) -> Vec<PathBuf> {
    let mut paths = Vec::new();

    // Toolbox (Linux)
    glob_collect(&format!("{}/.local/share/JetBrains/Toolbox/apps/IDEA-*/jbr", home), &mut paths);
    glob_collect(&format!("{}/.local/share/JetBrains/Toolbox/apps/intellij-idea-*/jbr", home), &mut paths);

    // Toolbox (macOS)
    glob_collect(&format!("{}/Library/Application Support/JetBrains/Toolbox/apps/IDEA-*/jbr", home), &mut paths);

    // Standalone (Linux)
    glob_collect("/opt/idea*/jbr", &mut paths);
    glob_collect(&format!("{}/idea*/jbr", home), &mut paths);
    glob_collect("/snap/intellij-idea-*/current/jbr", &mut paths);

    // Standalone (macOS)
    glob_collect("/Applications/IntelliJ IDEA*.app/Contents/jbr/Contents/Home", &mut paths);

    paths
}

fn glob_collect(pattern: &str, paths: &mut Vec<PathBuf>) {
    if let Ok(entries) = glob::glob(pattern) {
        for entry in entries.flatten() {
            if entry.exists() {
                paths.push(entry);
            }
        }
    }
}

/// Scan a directory for JDK subdirectories.
fn scan_dir(dir: &Path, source: JdkSource, jdks: &mut Vec<DetectedJdk>, seen: &mut std::collections::HashSet<PathBuf>) {
    if !dir.exists() {
        return;
    }
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                try_add_jdk(&path, source.clone(), jdks, seen);
            }
        }
    }
}

/// Try to identify a JDK at the given path and add it to the list.
fn try_add_jdk(path: &Path, source: JdkSource, jdks: &mut Vec<DetectedJdk>, seen: &mut std::collections::HashSet<PathBuf>) {
    let canonical = match std::fs::canonicalize(path) {
        Ok(c) => c,
        Err(_) => path.to_path_buf(),
    };

    if seen.contains(&canonical) {
        return;
    }

    // Check for java binary
    let java_bin = find_java_binary(path);
    if java_bin.is_none() {
        return;
    }

    if let Some((vendor, version)) = detect_jdk_info(path) {
        let has_dcevm = vendor.contains("JBR") || vendor.contains("JetBrains");
        seen.insert(canonical);
        jdks.push(DetectedJdk {
            vendor,
            version,
            path: path.to_path_buf(),
            source,
            has_dcevm,
        });
    }
}

/// Find the java binary in a JDK directory.
fn find_java_binary(jdk_path: &Path) -> Option<PathBuf> {
    let candidates = [
        jdk_path.join("bin/java"),
        jdk_path.join("bin/java.exe"),
        jdk_path.join("Contents/Home/bin/java"), // macOS
    ];
    candidates.into_iter().find(|p| p.exists())
}

/// Run `java -version` to detect vendor and version.
fn detect_jdk_info(jdk_path: &Path) -> Option<(String, String)> {
    let java_bin = find_java_binary(jdk_path)?;

    let output = Command::new(&java_bin)
        .arg("-version")
        .output()
        .ok()?;

    // java -version outputs to stderr
    let stderr = String::from_utf8_lossy(&output.stderr);
    parse_java_version(&stderr)
}

/// Parse the output of `java -version`.
fn parse_java_version(output: &str) -> Option<(String, String)> {
    let lines: Vec<&str> = output.lines().collect();
    if lines.is_empty() {
        return None;
    }

    let first = lines[0];

    // Extract version: "openjdk version \"21.0.2\"" or "java version \"1.8.0_392\""
    let version = first
        .split('"')
        .nth(1)
        .unwrap_or("unknown")
        .to_string();

    // Extract major version
    let major = if version.starts_with("1.") {
        // Java 8 style: 1.8.0_392 → 8
        version.split('.').nth(1).unwrap_or(&version).to_string()
    } else {
        // Java 9+ style: 21.0.2 → 21
        version.split('.').next().unwrap_or(&version).to_string()
    };

    // Detect vendor from java -version output
    let all_text = output.to_lowercase();
    let vendor = if all_text.contains("jetbrains") || all_text.contains("jbr") {
        format!("JBR {}", major)
    } else if all_text.contains("graalvm") {
        format!("GraalVM {}", major)
    } else if all_text.contains("corretto") {
        format!("Corretto {}", major)
    } else if all_text.contains("temurin") || all_text.contains("adoptium") {
        format!("Temurin {}", major)
    } else if all_text.contains("zulu") {
        format!("Zulu {}", major)
    } else if all_text.contains("semeru") || all_text.contains("openj9") {
        format!("Semeru {}", major)
    } else if all_text.contains("sapmachine") {
        format!("SapMachine {}", major)
    } else if all_text.contains("liberica") || all_text.contains("bellsoft") {
        format!("Liberica {}", major)
    } else if all_text.contains("microsoft") {
        format!("Microsoft {}", major)
    } else if all_text.contains("dragonwell") {
        format!("Dragonwell {}", major)
    } else if all_text.contains("java(tm)") || all_text.contains("hotspot(tm)") {
        format!("Oracle {}", major)
    } else if all_text.contains("openjdk") {
        format!("OpenJDK {}", major)
    } else {
        format!("Java {}", major)
    };

    Some((vendor, version))
}

/// Get the ym JDK storage directory.
pub fn jdk_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(home).join(".ym/jdks")
}

/// Download a JDK. Returns the installed path.
pub fn download_jdk(vendor: &str, major_version: &str) -> Result<PathBuf> {
    let target_dir = jdk_dir();
    std::fs::create_dir_all(&target_dir)?;

    let (url, dir_name) = resolve_download_url(vendor, major_version)?;

    let install_path = target_dir.join(&dir_name);
    if install_path.exists() {
        return Ok(install_path);
    }

    println!("  Downloading {} {}...", vendor, major_version);
    println!("  {}", url);

    let tmp_archive = target_dir.join("_download.tar.gz");
    download_with_progress(&url, &tmp_archive)?;

    // Extract
    let status = std::process::Command::new("tar")
        .args(["xzf", &tmp_archive.to_string_lossy(), "-C", &target_dir.to_string_lossy()])
        .status()?;

    std::fs::remove_file(&tmp_archive).ok();

    if !status.success() {
        anyhow::bail!("Failed to extract JDK archive");
    }

    // Find the extracted directory and rename it
    if !install_path.exists() {
        // Try to find the extracted dir (often has a long name)
        if let Some(extracted) = find_extracted_jdk_dir(&target_dir, &dir_name)? {
            std::fs::rename(&extracted, &install_path)?;
        }
    }

    if !install_path.exists() {
        anyhow::bail!("JDK installation failed: {} not found", install_path.display());
    }

    println!("  Installed: {}", install_path.display());

    Ok(install_path)
}

/// Resolve download URL for a JDK vendor and version.
fn resolve_download_url(vendor: &str, major: &str) -> Result<(String, String)> {
    let os = if cfg!(target_os = "macos") { "mac" } else { "linux" };
    let arch = if cfg!(target_arch = "aarch64") { "aarch64" } else { "x64" };

    match vendor.to_lowercase().as_str() {
        "temurin" | "openjdk" => {
            let dir_name = format!("temurin-{}", major);
            let url = format!(
                "https://api.adoptium.net/v3/binary/latest/{}/ga/{}/{}/jdk/hotspot/normal/eclipse",
                major, os, arch
            );
            Ok((url, dir_name))
        }
        "jbr" | "jetbrains" => {
            let dir_name = format!("jbr-{}", major);
            let url = resolve_jbr_url(major, os, arch)?;
            Ok((url, dir_name))
        }
        "graalvm" => {
            let dir_name = format!("graalvm-{}", major);
            let url = format!(
                "https://download.oracle.com/graalvm/{}/latest/graalvm-jdk-{}_{}-{}_bin.tar.gz",
                major, major, os, arch
            );
            Ok((url, dir_name))
        }
        "corretto" => {
            let dir_name = format!("corretto-{}", major);
            let url = format!(
                "https://corretto.aws/downloads/latest/amazon-corretto-{}-{}-{}-jdk.tar.gz",
                major, arch, os
            );
            Ok((url, dir_name))
        }
        _ => anyhow::bail!("Unknown JDK vendor: {}", vendor),
    }
}

/// Resolve JBR download URL.
/// 1. Query GitHub Releases API for latest tag matching major version
/// 2. Parse tag "jbr-release-{version}b{build}" into version + build
/// 3. Construct CDN URL and verify with HEAD request
fn resolve_jbr_url(major: &str, os: &str, arch: &str) -> Result<String> {
    let api_url = "https://api.github.com/repos/JetBrains/JetBrainsRuntime/releases?per_page=30";

    let client = reqwest::blocking::Client::builder()
        .user_agent(concat!("ym/", env!("CARGO_PKG_VERSION")))
        .redirect(reqwest::redirect::Policy::none())
        .build()?;

    let resp = client.get(api_url).send()?;
    if !resp.status().is_success() {
        anyhow::bail!("Failed to query JBR releases: HTTP {}", resp.status());
    }

    let releases: Vec<serde_json::Value> = resp.json()?;

    // Find first tag matching major version: "jbr-release-{major}.x.xb..."
    let prefix = format!("jbr-release-{}.", major);
    let tag = releases.iter()
        .filter_map(|r| r["tag_name"].as_str())
        .find(|t| t.starts_with(&prefix))
        .ok_or_else(|| anyhow::anyhow!(
            "No JBR {} release found. Check https://github.com/JetBrains/JetBrainsRuntime/releases",
            major
        ))?;

    // Parse tag: "jbr-release-25.0.2b329.72" → version="25.0.2", build="b329.72"
    let version_build = tag.strip_prefix("jbr-release-").unwrap_or(tag);
    let (version, build) = version_build
        .find('b')
        .map(|pos| (&version_build[..pos], &version_build[pos..]))
        .ok_or_else(|| anyhow::anyhow!("Failed to parse JBR tag: {}", tag))?;

    let cdn_base = "https://cache-redirector.jetbrains.com/intellij-jbr";

    // Try variants in order: jbrsdk (full SDK), jbr (runtime only)
    let variants = ["jbrsdk", "jbr"];
    for variant in &variants {
        let url = format!("{}/{}-{}-{}-{}-{}.tar.gz", cdn_base, variant, version, os, arch, build);
        let head = client.head(&url).send();
        if let Ok(resp) = head {
            let status = resp.status().as_u16();
            // 302 = redirect to actual CDN (valid), 200 = direct
            if status == 200 || status == 302 {
                return Ok(url);
            }
        }
    }

    anyhow::bail!(
        "No JBR {} binary found for {}-{} (tag: {}). Check https://github.com/JetBrains/JetBrainsRuntime/releases",
        major, os, arch, tag
    )
}

/// Find extracted JDK directory after tar extraction.
fn find_extracted_jdk_dir(parent: &Path, expected_name: &str) -> Result<Option<PathBuf>> {
    let expected = parent.join(expected_name);
    if expected.exists() {
        return Ok(Some(expected));
    }

    // Look for recently created directories that look like JDKs
    if let Ok(entries) = std::fs::read_dir(parent) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() && path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default() != expected_name {
                // Check if it looks like a JDK (has bin/java)
                if find_java_binary(&path).is_some() && path.file_name().unwrap().to_string_lossy().starts_with('_') == false {
                    return Ok(Some(path));
                }
            }
        }
    }

    Ok(None)
}

/// Available vendors for download.
pub const DOWNLOAD_VENDORS: &[(&str, &str)] = &[
    ("Temurin", "temurin"),
    ("JetBrains Runtime", "jbr"),
    ("GraalVM", "graalvm"),
    ("Amazon Corretto", "corretto"),
];

/// Download a file with progress bar. Respects HTTP_PROXY/HTTPS_PROXY env vars.
fn download_with_progress(url: &str, dest: &Path) -> Result<()> {
    use indicatif::{ProgressBar, ProgressStyle};
    use std::time::Duration;

    let client = reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(30))
        .build()?;

    let mut response = client.get(url).send()?;
    if !response.status().is_success() {
        anyhow::bail!("Download failed: HTTP {} from {}", response.status(), url);
    }

    let total = response.content_length().unwrap_or(0);
    let pb = if total > 0 {
        let pb = ProgressBar::new(total);
        pb.set_style(
            ProgressStyle::default_bar()
                .template("  [{bar:40.cyan/dim}] {bytes}/{total_bytes} remaining {eta}")
                .unwrap()
                .progress_chars("█▉▊▋▌▍▎▏ "),
        );
        pb
    } else {
        let pb = ProgressBar::new_spinner();
        pb.set_style(
            ProgressStyle::default_spinner()
                .template("  {spinner} {bytes} downloaded...")
                .unwrap(),
        );
        pb
    };

    let mut file = std::fs::File::create(dest)?;
    let mut buf = [0u8; 8192];
    loop {
        let n = response.read(&mut buf)?;
        if n == 0 {
            break;
        }
        std::io::Write::write_all(&mut file, &buf[..n])?;
        pb.inc(n as u64);
    }

    pb.finish_and_clear();
    Ok(())
}
