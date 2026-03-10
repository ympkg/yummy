use anyhow::Result;
use console::style;
use std::process::Command;

/// Diagnose environment issues
pub fn execute(fix: bool) -> Result<()> {
    println!();
    println!("  {}", style("ym doctor").bold());
    println!();

    let mut ok = true;

    // Check Java
    ok &= check_command("java", &["-version"], "Java Runtime");

    // Check javac
    ok &= check_command("javac", &["-version"], "Java Compiler (javac)");

    // Check JAVA_HOME
    check_java_home();

    // Check JDK version matches target
    check_jdk_version_match(fix);

    // Check jar
    ok &= check_command("jar", &["--version"], "JAR tool");

    // Check git
    check_command("git", &["--version"], "Git");

    // Check package.toml
    check_config();

    // Check Maven cache
    check_maven_cache();

    // Check JAR integrity (SHA-256)
    ok &= check_jar_integrity(fix);

    // Check permissions
    check_permissions(fix);

    // Check project structure
    check_project_structure(fix);

    println!();
    if ok {
        println!(
            "  {} All critical checks passed",
            style("✓").green().bold()
        );
    } else {
        if fix {
            println!(
                "  {} Some issues were auto-fixed, but missing tools require manual installation",
                style("!").yellow().bold()
            );
        } else {
            println!(
                "  {} Some checks failed. Run {} to auto-fix or install missing tools.",
                style("✗").red().bold(),
                style("ym doctor --fix").cyan()
            );
        }
    }
    println!();

    Ok(())
}

fn check_project_structure(fix: bool) {
    let cwd = std::env::current_dir().unwrap_or_default();
    let config_path = crate::config::find_config(&cwd);

    if config_path.is_none() {
        return;
    }

    let project = config_path.as_ref().unwrap().parent().unwrap_or(&cwd);

    // Check src directory
    let src = project.join("src");
    if !src.exists() {
        if fix {
            let _ = std::fs::create_dir_all(&src);
            println!(
                "  {} Created missing src/ directory",
                style("✓").green()
            );
        } else {
            println!(
                "  {} src/ directory missing (run --fix to create)",
                style("!").yellow()
            );
        }
    } else {
        println!(
            "  {} src/ directory",
            style("✓").green()
        );
    }

    // Check .gitignore
    let gitignore = project.join(".gitignore");
    if !gitignore.exists() {
        if fix {
            let _ = std::fs::write(&gitignore, "out/\n.ym/\n.ym-sources.txt\n*.class\n");
            println!(
                "  {} Created .gitignore",
                style("✓").green()
            );
        } else {
            println!(
                "  {} .gitignore missing (run --fix to create)",
                style("!").yellow()
            );
        }
    } else {
        // Check if .ym/ is in gitignore
        let content = std::fs::read_to_string(&gitignore).unwrap_or_default();
        if !content.contains(".ym/") {
            if fix {
                let _ = std::fs::write(&gitignore, format!("{}\n.ym/\nout/\n", content));
                println!(
                    "  {} Added .ym/ and out/ to .gitignore",
                    style("✓").green()
                );
            } else {
                println!(
                    "  {} .gitignore exists but missing .ym/ entry (run --fix)",
                    style("!").yellow()
                );
            }
        } else {
            println!(
                "  {} .gitignore",
                style("✓").green()
            );
        }
    }
}

fn check_command(cmd: &str, args: &[&str], label: &str) -> bool {
    match Command::new(cmd).args(args).output() {
        Ok(output) if output.status.success() => {
            let ver = String::from_utf8_lossy(&output.stdout);
            let ver_err = String::from_utf8_lossy(&output.stderr);
            // java -version outputs to stderr
            let version_line = if ver.trim().is_empty() {
                ver_err.lines().next().unwrap_or("").trim().to_string()
            } else {
                ver.lines().next().unwrap_or("").trim().to_string()
            };
            println!(
                "  {} {}  {}",
                style("✓").green(),
                label,
                style(&version_line).dim()
            );
            true
        }
        _ => {
            println!(
                "  {} {}  {}",
                style("✗").red(),
                label,
                style("not found").red()
            );
            false
        }
    }
}

fn check_java_home() {
    match std::env::var("JAVA_HOME") {
        Ok(home) if !home.is_empty() => {
            let exists = std::path::Path::new(&home).exists();
            if exists {
                println!(
                    "  {} JAVA_HOME  {}",
                    style("✓").green(),
                    style(&home).dim()
                );
            } else {
                println!(
                    "  {} JAVA_HOME  {} (path does not exist)",
                    style("!").yellow(),
                    style(&home).dim()
                );
            }
        }
        _ => {
            println!(
                "  {} JAVA_HOME  {}",
                style("!").yellow(),
                style("not set (ym will use java from PATH)").dim()
            );
        }
    }
}

fn check_jdk_version_match(fix: bool) {
    let cwd = std::env::current_dir().unwrap_or_default();
    let config_path = crate::config::find_config(&cwd);
    let target = config_path
        .and_then(|p| crate::config::load_config(&p).ok())
        .and_then(|cfg| cfg.target.clone());

    if let Some(ref target) = target {
        // Get current java version
        let output = Command::new("java").arg("-version").output();
        if let Ok(out) = output {
            let stderr = String::from_utf8_lossy(&out.stderr);
            // Parse version from output like: openjdk version "21.0.1" or java version "1.8.0_xxx"
            let detected = stderr.lines().next().and_then(|line| {
                let start = line.find('"')?;
                let end = line[start + 1..].find('"')?;
                let ver = &line[start + 1..start + 1 + end];
                // Extract major version: "21.0.1" → "21", "1.8.0_xxx" → "8"
                if ver.starts_with("1.") {
                    ver.split('.').nth(1).map(|s| s.to_string())
                } else {
                    ver.split('.').next().map(|s| s.to_string())
                }
            });

            if let Some(ref major) = detected {
                if major == target {
                    println!(
                        "  {} JDK version  {} (matches target {})",
                        style("✓").green(),
                        style(major).dim(),
                        target
                    );
                } else if fix {
                    println!(
                        "  {} JDK version {} does not match target {}, downloading...",
                        style("➜").green(),
                        major,
                        target
                    );
                    match crate::jvm::ensure_jdk(target, None, true) {
                        Ok(path) => {
                            println!(
                                "  {} Downloaded JDK {} to {}",
                                style("✓").green(),
                                target,
                                style(path.display()).dim()
                            );
                        }
                        Err(e) => {
                            println!(
                                "  {} Failed to download JDK {}: {}",
                                style("✗").red(),
                                target,
                                e
                            );
                        }
                    }
                } else {
                    println!(
                        "  {} JDK version {} does not match target {} (run --fix to download)",
                        style("!").yellow(),
                        major,
                        target
                    );
                }
            }
        } else if fix {
            // No java found at all, try to download
            println!(
                "  {} No Java found, downloading JDK {}...",
                style("➜").green(),
                target
            );
            match crate::jvm::ensure_jdk(target, None, true) {
                Ok(path) => {
                    println!(
                        "  {} Downloaded JDK {} to {}",
                        style("✓").green(),
                        target,
                        style(path.display()).dim()
                    );
                }
                Err(e) => {
                    println!(
                        "  {} Failed to download JDK {}: {}",
                        style("✗").red(),
                        target,
                        e
                    );
                }
            }
        }
    }
}

fn check_config() {
    let cwd = std::env::current_dir().unwrap_or_default();
    if let Some(path) = crate::config::find_config(&cwd) {
        match crate::config::load_config(&path) {
            Ok(cfg) => {
                let project = crate::config::project_dir(&path);
                println!(
                    "  {} package.toml  {} ({})",
                    style("✓").green(),
                    style(&cfg.name).dim(),
                    style(path.display()).dim()
                );
                // Schema validation
                validate_config_schema(&cfg, &project);
            }
            Err(e) => {
                println!(
                    "  {} package.toml  {} ({})",
                    style("✗").red(),
                    style("parse error").red(),
                    style(e).dim()
                );
            }
        }
    } else {
        println!(
            "  {} package.toml  {}",
            style("-").dim(),
            style("not found in current directory tree").dim()
        );
    }
}

fn validate_config_schema(cfg: &crate::config::schema::YmConfig, project: &std::path::Path) {
    // Build workspace graph if this is a workspace root (for module existence checks)
    let ws = if cfg.workspaces.is_some() {
        crate::workspace::graph::WorkspaceGraph::build(project).ok()
    } else {
        None
    };
    let ws_packages: std::collections::HashSet<String> = ws
        .as_ref()
        .map(|w| w.all_packages().into_iter().collect())
        .unwrap_or_default();

    // Load root config for { workspace = true } Maven dep validation
    let root_cfg = crate::config::find_workspace_root(project)
        .and_then(|root| {
            if root != project {
                crate::config::load_config(&root.join(crate::config::CONFIG_FILE)).ok()
            } else {
                None
            }
        });

    // Validate dependency coordinate format
    if let Some(ref deps) = cfg.dependencies {
        for (key, value) in deps {
            if key.contains(':') {
                // Maven coordinate: should be groupId:artifactId
                let parts: Vec<&str> = key.split(':').collect();
                if parts.len() != 2 || parts[0].is_empty() || parts[1].is_empty() {
                    println!(
                        "  {} Invalid coordinate format: '{}' (expected groupId:artifactId)",
                        style("!").yellow(),
                        key
                    );
                }
                // Validate scope value
                let scope = value.scope();
                if !["compile", "runtime", "provided", "test"].contains(&scope) {
                    println!(
                        "  {} Invalid scope '{}' for dependency '{}'",
                        style("!").yellow(),
                        scope,
                        key
                    );
                }
                // Validate { workspace = true } references root version
                if value.is_workspace() {
                    if let Some(ref root) = root_cfg {
                        let found = root.dependencies.as_ref()
                            .and_then(|d| d.get(key))
                            .and_then(|v| v.version())
                            .is_some();
                        if !found {
                            println!(
                                "  {} Dependency '{}' uses {{workspace = true}} but root has no version for it",
                                style("!").yellow(),
                                key
                            );
                        }
                    }
                }
            } else {
                // Module reference: must have workspace = true
                if !value.is_workspace() {
                    println!(
                        "  {} Dependency '{}' has no colon but no workspace = true",
                        style("!").yellow(),
                        key
                    );
                } else if !ws_packages.is_empty() && !ws_packages.contains(key) {
                    // Check if referenced module exists in workspace
                    println!(
                        "  {} Module dependency '{}' not found in workspace",
                        style("!").yellow(),
                        key
                    );
                }
            }
        }
    }

    // Validate target value
    if let Some(ref target) = cfg.target {
        if target.parse::<u32>().is_err() {
            println!(
                "  {} target '{}' is not a valid Java version number",
                style("!").yellow(),
                target
            );
        }
    }
}

fn check_maven_cache() {
    let cwd = std::env::current_dir().unwrap_or_default();
    let cache = crate::config::maven_cache_dir(&cwd);
    if cache.exists() {
        let size = dir_size(&cache);
        let jar_count = count_files_with_ext(&cache, "jar");
        println!(
            "  {} Maven cache  {} ({} JARs)",
            style("✓").green(),
            style(format_size(size)).dim(),
            jar_count
        );
    } else {
        println!(
            "  {} Maven cache  {}",
            style("-").dim(),
            style("empty").dim()
        );
    }
}

fn check_jar_integrity(fix: bool) -> bool {
    let cwd = std::env::current_dir().unwrap_or_default();
    let resolved = match crate::config::load_resolved_cache(&cwd) {
        Ok(r) => r,
        Err(_) => return true, // No resolved.json, nothing to check
    };

    if resolved.dependencies.is_empty() {
        return true;
    }

    let cache = crate::config::maven_cache_dir(&cwd);
    let mut all_ok = true;
    let mut missing = 0;
    let mut corrupt = 0;

    for (key, entry) in &resolved.dependencies {
        let parts: Vec<&str> = key.split(':').collect();
        if parts.len() != 3 {
            continue;
        }
        let jar_path = cache
            .join(parts[0])
            .join(parts[1])
            .join(parts[2])
            .join(format!("{}-{}.jar", parts[1], parts[2]));

        if !jar_path.exists() {
            missing += 1;
            all_ok = false;
            continue;
        }

        if let Some(ref expected_sha) = entry.sha256 {
            if let Ok(data) = std::fs::read(&jar_path) {
                let actual = crate::compiler::incremental::hash_bytes(&data);
                if &actual != expected_sha {
                    corrupt += 1;
                    all_ok = false;
                }
            }
        }
    }

    if all_ok {
        println!(
            "  {} Dependency integrity  {} deps verified",
            style("✓").green(),
            resolved.dependencies.len()
        );
    } else {
        if fix {
            // Delete corrupt/missing JARs and clear resolved cache so next build re-downloads
            let resolved_path = cwd.join(".ym").join("resolved.json");
            let _ = std::fs::remove_file(&resolved_path);
            println!(
                "  {} Cleared resolved cache ({} missing, {} corrupt). Next build will re-download.",
                style("✓").green(),
                missing,
                corrupt
            );
        } else {
            println!(
                "  {} Dependency integrity  {} missing, {} corrupt (run --fix to re-download)",
                style("!").yellow(),
                missing,
                corrupt
            );
        }
    }

    all_ok
}

fn count_files_with_ext(dir: &std::path::Path, ext: &str) -> usize {
    walkdir::WalkDir::new(dir)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some(ext))
        .count()
}

#[allow(unused_variables)]
fn check_permissions(fix: bool) {
    // Check ~/.ym/credentials.json permissions (should be 0o600)
    let home = std::env::var("HOME").unwrap_or_default();
    let creds_path = std::path::PathBuf::from(&home).join(".ym").join("credentials.json");
    if creds_path.exists() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Ok(meta) = std::fs::metadata(&creds_path) {
                let mode = meta.permissions().mode() & 0o777;
                if mode != 0o600 {
                    if fix {
                        let _ = std::fs::set_permissions(
                            &creds_path,
                            std::fs::Permissions::from_mode(0o600),
                        );
                        println!(
                            "  {} Fixed credentials.json permissions (was {:o}, now 600)",
                            style("✓").green(),
                            mode
                        );
                    } else {
                        println!(
                            "  {} credentials.json permissions {:o} (should be 600, run --fix)",
                            style("!").yellow(),
                            mode
                        );
                    }
                } else {
                    println!(
                        "  {} credentials.json permissions",
                        style("✓").green()
                    );
                }
            }
        }
        #[cfg(not(unix))]
        {
            println!(
                "  {} credentials.json exists",
                style("✓").green()
            );
        }
    }

    // Check .ym/ directory permissions
    let cwd = std::env::current_dir().unwrap_or_default();
    let ym_dir = cwd.join(".ym");
    if ym_dir.exists() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Ok(meta) = std::fs::metadata(&ym_dir) {
                let mode = meta.permissions().mode() & 0o777;
                if mode != 0o755 {
                    if fix {
                        let _ = std::fs::set_permissions(
                            &ym_dir,
                            std::fs::Permissions::from_mode(0o755),
                        );
                        println!(
                            "  {} Fixed .ym/ directory permissions (was {:o}, now 755)",
                            style("✓").green(),
                            mode
                        );
                    } else {
                        println!(
                            "  {} .ym/ directory permissions {:o} (should be 755, run --fix)",
                            style("!").yellow(),
                            mode
                        );
                    }
                } else {
                    println!(
                        "  {} .ym/ directory permissions",
                        style("✓").green()
                    );
                }
            }
        }
    }
}

fn dir_size(path: &std::path::Path) -> u64 {
    walkdir::WalkDir::new(path)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .filter_map(|e| e.metadata().ok())
        .map(|m| m.len())
        .sum()
}

fn format_size(bytes: u64) -> String {
    if bytes >= 1_073_741_824 {
        format!("{:.1} GB", bytes as f64 / 1_073_741_824.0)
    } else if bytes >= 1_048_576 {
        format!("{:.1} MB", bytes as f64 / 1_048_576.0)
    } else if bytes >= 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{} B", bytes)
    }
}
