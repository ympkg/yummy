use anyhow::{bail, Context, Result};
use console::style;
use indicatif::{ProgressBar, ProgressStyle};
use std::path::{Path, PathBuf};

/// JVM auto-download manager.
/// Downloads JDK from Adoptium (Eclipse Temurin) if not found locally.

const ADOPTIUM_API: &str = "https://api.adoptium.net/v3";

/// Find or download a JDK for the given version.
/// Returns the JAVA_HOME path.
pub fn ensure_jdk(version: &str, vendor: Option<&str>, auto_download: bool) -> Result<PathBuf> {
    // 1. Check JAVA_HOME
    if let Ok(java_home) = std::env::var("JAVA_HOME") {
        let java_home = PathBuf::from(&java_home);
        if java_home.join("bin").join("javac").exists()
            || java_home.join("bin").join("javac.exe").exists()
        {
            return Ok(java_home);
        }
    }

    // 2. Check if javac is on PATH
    if which_javac().is_some() {
        // javac exists, use system JDK
        return Ok(PathBuf::from("system"));
    }

    // 3. Check cached JDKs
    let jdk_dir = jdk_cache_dir();
    let cached = find_cached_jdk(&jdk_dir, version);
    if let Some(path) = cached {
        return Ok(path);
    }

    // 4. Auto-download if enabled
    if !auto_download {
        bail!(
            "JDK {} not found. Install it or set jvm.autoDownload: true in package.json",
            version
        );
    }

    download_jdk(version, vendor.unwrap_or("temurin"), &jdk_dir)
}

fn which_javac() -> Option<PathBuf> {
    let cmd = if cfg!(windows) { "where" } else { "which" };
    std::process::Command::new(cmd)
        .arg("javac")
        .output()
        .ok()
        .and_then(|o| {
            if o.status.success() {
                let path = String::from_utf8_lossy(&o.stdout).trim().to_string();
                if !path.is_empty() {
                    Some(PathBuf::from(path))
                } else {
                    None
                }
            } else {
                None
            }
        })
}

fn jdk_cache_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home).join(".ym").join("jdks")
}

fn find_cached_jdk(jdk_dir: &Path, version: &str) -> Option<PathBuf> {
    if !jdk_dir.exists() {
        return None;
    }
    // Look for directories matching the version
    if let Ok(entries) = std::fs::read_dir(jdk_dir) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.contains(version) {
                let path = entry.path();
                // Check for bin/javac inside
                if path.join("bin").join("javac").exists() {
                    return Some(path);
                }
                // Sometimes there's an extra directory level
                if let Ok(inner) = std::fs::read_dir(&path) {
                    for inner_entry in inner.flatten() {
                        if inner_entry.path().join("bin").join("javac").exists() {
                            return Some(inner_entry.path());
                        }
                    }
                }
            }
        }
    }
    None
}

fn download_jdk(version: &str, _vendor: &str, jdk_dir: &Path) -> Result<PathBuf> {
    let os = if cfg!(target_os = "linux") {
        "linux"
    } else if cfg!(target_os = "macos") {
        "mac"
    } else if cfg!(target_os = "windows") {
        "windows"
    } else {
        bail!("Unsupported OS for JDK auto-download");
    };

    let arch = if cfg!(target_arch = "x86_64") {
        "x64"
    } else if cfg!(target_arch = "aarch64") {
        "aarch64"
    } else {
        bail!("Unsupported architecture for JDK auto-download");
    };

    let ext = if cfg!(target_os = "windows") {
        "zip"
    } else {
        "tar.gz"
    };

    // Adoptium API: get latest release for this feature version
    let url = format!(
        "{}/binary/latest/{}/ga/{}/{}/jdk/hotspot/normal/eclipse?project=jdk",
        ADOPTIUM_API, version, os, arch
    );

    println!(
        "  {} Downloading JDK {}...",
        style("→").blue(),
        version
    );

    let client = reqwest::blocking::Client::builder()
        .user_agent(concat!("ym/", env!("CARGO_PKG_VERSION")))
        .timeout(std::time::Duration::from_secs(300))
        .build()?;

    let response = client.get(&url).send()?;
    if !response.status().is_success() {
        bail!(
            "Failed to download JDK {}: HTTP {}",
            version,
            response.status()
        );
    }

    let total_size = response.content_length().unwrap_or(0);
    let pb = ProgressBar::new(total_size);
    pb.set_style(
        ProgressStyle::default_bar()
            .template("  [{bar:40.cyan/dim}] {bytes}/{total_bytes} ({eta})")
            .unwrap()
            .progress_chars("=> "),
    );

    let bytes = response.bytes()?;
    pb.finish_and_clear();

    // Extract
    std::fs::create_dir_all(jdk_dir)?;
    let archive_path = jdk_dir.join(format!("jdk-{}.{}", version, ext));
    std::fs::write(&archive_path, &bytes)?;

    println!(
        "  {} Extracting JDK {}...",
        style("→").blue(),
        version
    );

    if ext == "tar.gz" {
        let status = std::process::Command::new("tar")
            .arg("xf")
            .arg(&archive_path)
            .arg("-C")
            .arg(jdk_dir)
            .status()
            .context("Failed to extract JDK archive")?;

        if !status.success() {
            bail!("Failed to extract JDK archive");
        }
    } else {
        // zip extraction for Windows
        let status = std::process::Command::new("unzip")
            .arg("-q")
            .arg(&archive_path)
            .arg("-d")
            .arg(jdk_dir)
            .status()
            .context("Failed to extract JDK archive")?;

        if !status.success() {
            bail!("Failed to extract JDK archive");
        }
    }

    // Clean up archive
    let _ = std::fs::remove_file(&archive_path);

    // Find the extracted directory
    let java_home = find_cached_jdk(jdk_dir, version)
        .context("JDK extracted but couldn't find JAVA_HOME")?;

    println!(
        "  {} JDK {} installed at {}",
        style("✓").green(),
        version,
        java_home.display()
    );

    Ok(java_home)
}
