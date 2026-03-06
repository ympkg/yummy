use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

use super::{CompileConfig, CompileResult};

/// Compile Java files using javac
pub fn compile(config: &CompileConfig) -> Result<CompileResult> {
    let mut java_files = Vec::new();
    for src_dir in &config.source_dirs {
        if src_dir.exists() {
            collect_java_files(src_dir, &mut java_files)?;
        }
    }

    if java_files.is_empty() {
        return Ok(CompileResult {
            success: true,
            files_compiled: 0,
            errors: String::new(),
        });
    }

    std::fs::create_dir_all(&config.output_dir)?;

    let mut cmd = Command::new("javac");
    cmd.arg("-d").arg(&config.output_dir);

    if let Some(ref ver) = config.java_version {
        cmd.arg("--release").arg(ver);
    }

    if let Some(ref enc) = config.encoding {
        cmd.arg("-encoding").arg(enc);
    }

    if !config.classpath.is_empty() {
        let sep = if cfg!(windows) { ";" } else { ":" };
        let cp = config
            .classpath
            .iter()
            .map(|p| p.to_string_lossy().to_string())
            .collect::<Vec<_>>()
            .join(sep);
        cmd.arg("-cp").arg(&cp);
    }

    if !config.annotation_processors.is_empty() {
        let sep = if cfg!(windows) { ";" } else { ":" };
        let ap = config
            .annotation_processors
            .iter()
            .map(|p| p.to_string_lossy().to_string())
            .collect::<Vec<_>>()
            .join(sep);
        cmd.arg("-processorpath").arg(&ap);
    }

    for lint_opt in &config.lint {
        cmd.arg(format!("-Xlint:{}", lint_opt));
    }

    for arg in &config.extra_args {
        cmd.arg(arg);
    }

    let file_count = java_files.len();
    for f in &java_files {
        cmd.arg(f);
    }

    let output = cmd
        .output()
        .context("Failed to run javac. Is JDK installed?")?;

    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    Ok(CompileResult {
        success: output.status.success(),
        files_compiled: file_count,
        errors: stderr,
    })
}

fn collect_java_files(dir: &Path, files: &mut Vec<PathBuf>) -> Result<()> {
    for entry in walkdir::WalkDir::new(dir) {
        let entry = entry?;
        if entry.path().extension().and_then(|e| e.to_str()) == Some("java") {
            files.push(entry.path().to_path_buf());
        }
    }
    Ok(())
}

/// Scan source files for classes containing a main method
pub fn scan_main_methods(src_dirs: &[PathBuf]) -> Result<Vec<String>> {
    let mut main_classes = Vec::new();

    for src_dir in src_dirs {
        if !src_dir.exists() {
            continue;
        }
        for entry in walkdir::WalkDir::new(src_dir) {
            let entry = entry?;
            if entry.path().extension().and_then(|e| e.to_str()) != Some("java") {
                continue;
            }
            let content = std::fs::read_to_string(entry.path())?;
            if content.contains("public static void main(String")
                || content.contains("public static void main (String")
            {
                let rel = entry.path().strip_prefix(src_dir)?;
                let class_name = rel
                    .to_string_lossy()
                    .replace('/', ".")
                    .replace('\\', ".")
                    .trim_end_matches(".java")
                    .to_string();
                main_classes.push(class_name);
            }
        }
    }

    Ok(main_classes)
}
