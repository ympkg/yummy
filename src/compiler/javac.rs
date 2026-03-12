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
            outcome: super::CompileOutcome::UpToDate,
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

    let _cp_argfile_guard;
    if !config.classpath.is_empty() {
        let sep = if cfg!(windows) { ";" } else { ":" };
        let cp = config
            .classpath
            .iter()
            .map(|p| p.to_string_lossy().to_string())
            .collect::<Vec<_>>()
            .join(sep);
        // Use @argfile for very long classpaths (OS command line limits)
        if cp.len() > 8000 {
            let cp_file = config.output_dir.join(".ym-classpath.txt");
            std::fs::write(&cp_file, format!("-cp\n{}", cp))?;
            cmd.arg(format!("@{}", cp_file.display()));
            _cp_argfile_guard = Some(super::incremental::ArgfileCleanup(cp_file));
        } else {
            _cp_argfile_guard = None;
            cmd.arg("-cp").arg(&cp);
        }
    } else {
        _cp_argfile_guard = None;
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
    let _src_argfile_guard;
    if file_count > 50 {
        let argfile = config.output_dir.join(".ym-sources.txt");
        let content = java_files
            .iter()
            .map(|f| f.to_string_lossy().to_string())
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(&argfile, &content)?;
        cmd.arg(format!("@{}", argfile.display()));
        _src_argfile_guard = Some(super::incremental::ArgfileCleanup(argfile));
    } else {
        _src_argfile_guard = None;
        for f in &java_files {
            cmd.arg(f);
        }
    }

    let output = cmd
        .output()
        .context("Failed to run javac. Is JDK installed?")?;

    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    Ok(CompileResult {
        success: output.status.success(),
        outcome: super::CompileOutcome::Compiled(file_count),
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

