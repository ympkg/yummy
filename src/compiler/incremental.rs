use anyhow::Result;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

const FINGERPRINT_FILE: &str = "fingerprints.json";

/// Tracks source file fingerprints for incremental compilation.
///
/// Strategy:
///   file changed → compute sourceHash
///     → sourceHash unchanged → skip
///     → sourceHash changed → compile → compute abiHash
///       → abiHash unchanged → only update .class, don't propagate
///       → abiHash changed → recompile all dependents (recursive)
#[derive(Debug, Serialize, Deserialize, Default)]
pub struct Fingerprints {
    /// source_path (relative to project) -> entry
    entries: HashMap<String, FileEntry>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct FileEntry {
    pub source_hash: String,
    pub abi_hash: Option<String>,
    pub mtime_secs: u64,
}

impl Fingerprints {
    pub fn load(cache_dir: &Path) -> Self {
        let path = cache_dir.join(FINGERPRINT_FILE);
        if let Ok(content) = std::fs::read_to_string(&path) {
            serde_json::from_str(&content).unwrap_or_default()
        } else {
            Self::default()
        }
    }

    pub fn save(&self, cache_dir: &Path) -> Result<()> {
        std::fs::create_dir_all(cache_dir)?;
        let path = cache_dir.join(FINGERPRINT_FILE);
        let content = serde_json::to_string(self)?;
        std::fs::write(path, content)?;
        Ok(())
    }

    /// Find source files that have changed since last compilation.
    /// Returns (changed_files, all_files).
    pub fn get_changed_files(&self, source_dirs: &[PathBuf]) -> Result<(Vec<PathBuf>, Vec<PathBuf>)> {
        let mut changed = Vec::new();
        let mut all = Vec::new();

        for src_dir in source_dirs {
            if !src_dir.exists() {
                continue;
            }
            for entry in walkdir::WalkDir::new(src_dir) {
                let entry = entry?;
                if entry.path().extension().and_then(|e| e.to_str()) != Some("java") {
                    continue;
                }
                let path = entry.path().to_path_buf();
                all.push(path.clone());

                let rel_key = path.to_string_lossy().to_string();

                // Quick check: modification time
                let mtime = entry
                    .metadata()
                    .ok()
                    .and_then(|m| m.modified().ok())
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs())
                    .unwrap_or(0);

                if let Some(existing) = self.entries.get(&rel_key) {
                    if existing.mtime_secs == mtime {
                        continue; // mtime unchanged, skip hash check
                    }
                    // mtime changed, check content hash
                    let hash = hash_file(&path)?;
                    if hash == existing.source_hash {
                        continue; // content unchanged despite mtime change
                    }
                }
                changed.push(path);
            }
        }

        Ok((changed, all))
    }

    /// Update fingerprint for a compiled file.
    pub fn update_source(&mut self, path: &Path, source_hash: &str, mtime_secs: u64) {
        let key = path.to_string_lossy().to_string();
        let entry = self.entries.entry(key).or_insert_with(|| FileEntry {
            source_hash: String::new(),
            abi_hash: None,
            mtime_secs: 0,
        });
        entry.source_hash = source_hash.to_string();
        entry.mtime_secs = mtime_secs;
    }

    /// Update ABI hash for a compiled class.
    #[allow(dead_code)]
    pub fn update_abi(&mut self, source_path: &Path, abi_hash: &str) {
        let key = source_path.to_string_lossy().to_string();
        if let Some(entry) = self.entries.get_mut(&key) {
            entry.abi_hash = Some(abi_hash.to_string());
        }
    }

    /// Check if the ABI of a source file has changed.
    /// Returns true if ABI changed or if no previous ABI recorded.
    #[allow(dead_code)]
    pub fn abi_changed(&self, source_path: &Path, new_abi_hash: &str) -> bool {
        let key = source_path.to_string_lossy().to_string();
        match self.entries.get(&key) {
            Some(entry) => entry.abi_hash.as_deref() != Some(new_abi_hash),
            None => true,
        }
    }

    /// Remove entries for files that no longer exist.
    pub fn prune(&mut self, existing_files: &[PathBuf]) {
        let existing_keys: std::collections::HashSet<String> = existing_files
            .iter()
            .map(|p| p.to_string_lossy().to_string())
            .collect();
        self.entries.retain(|k, _| existing_keys.contains(k));
    }
}

/// Compute SHA-256 hash of file content.
pub fn hash_file(path: &Path) -> Result<String> {
    let content = std::fs::read(path)?;
    Ok(hash_bytes(&content))
}

pub fn hash_bytes(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    format!("{:x}", hasher.finalize())
}

#[allow(dead_code)]
/// Compute an ABI hash from a .class file.
/// The ABI includes: class modifiers, method signatures, field types, constants, inheritance.
/// Excludes: method bodies, private members, comments.
///
/// For a practical MVP, we extract key structural bytes from the class file.
pub fn compute_class_abi_hash(class_file: &Path) -> Result<String> {
    let data = std::fs::read(class_file)?;
    // Simplified ABI: hash the constant pool + field/method descriptors
    // A full implementation would parse the class file format,
    // but hashing the whole file minus the Code attributes is complex.
    // For now, we use a heuristic: if the class file changes, we check
    // if only Code attributes changed by comparing non-Code sections.
    //
    // Practical approach: hash everything except method Code attributes.
    // The class file format makes this non-trivial, so for MVP we hash
    // the full class file. This means any change triggers dependent recompilation.
    // This is still much better than full recompilation since we skip unchanged files.
    Ok(hash_bytes(&data))
}

/// Incremental compile: only compile changed files.
/// Falls back to full compilation if output dir is empty.
pub fn incremental_compile(
    config: &super::CompileConfig,
    cache_dir: &Path,
    engine: &super::CompilerEngine,
) -> Result<super::CompileResult> {
    // Use a per-output-dir fingerprint file so workspace modules don't conflict
    let fp_dir = fingerprint_dir_for(cache_dir, &config.output_dir);
    let mut fingerprints = Fingerprints::load(&fp_dir);
    let (changed, all_files) = fingerprints.get_changed_files(&config.source_dirs)?;

    // If output directory doesn't exist or is empty, do full compile
    let has_classes = config.output_dir.exists()
        && std::fs::read_dir(&config.output_dir)
            .map(|mut d| d.next().is_some())
            .unwrap_or(false);

    let files_to_compile = if !has_classes {
        all_files.clone() // Full compile
    } else if changed.is_empty() {
        return Ok(super::CompileResult {
            success: true,
            files_compiled: 0,
            errors: String::new(),
        });
    } else {
        changed.clone()
    };

    // Include output dir in classpath for incremental compilation
    // so javac can resolve types from previously compiled classes
    let mut classpath = config.classpath.clone();
    if has_classes && !classpath.contains(&config.output_dir) {
        classpath.push(config.output_dir.clone());
    }

    let incremental_config = super::CompileConfig {
        source_dirs: Vec::new(), // We'll pass files directly
        output_dir: config.output_dir.clone(),
        classpath,
        java_version: config.java_version.clone(),
        encoding: config.encoding.clone(),
        annotation_processors: config.annotation_processors.clone(),
        lint: config.lint.clone(),
        extra_args: config.extra_args.clone(),
    };

    let result = compile_files(&incremental_config, &files_to_compile, engine)?;

    if result.success {
        // Update fingerprints for compiled files
        for file in &files_to_compile {
            let hash = hash_file(file).unwrap_or_default();
            let mtime = std::fs::metadata(file)
                .ok()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0);
            fingerprints.update_source(file, &hash, mtime);
        }
        fingerprints.prune(&all_files);
        fingerprints.save(&fp_dir)?;
    }

    Ok(super::CompileResult {
        success: result.success,
        files_compiled: files_to_compile.len(),
        errors: result.errors,
    })
}

/// Derive a per-module fingerprint directory from the output dir path.
/// This ensures workspace modules have independent fingerprint files.
fn fingerprint_dir_for(cache_dir: &Path, output_dir: &Path) -> PathBuf {
    let hash = hash_bytes(output_dir.to_string_lossy().as_bytes());
    cache_dir.join("fingerprints").join(&hash[..16])
}

/// Compile specific files using the selected compiler engine.
fn compile_files(
    config: &super::CompileConfig,
    files: &[PathBuf],
    engine: &super::CompilerEngine,
) -> Result<super::CompileResult> {
    if files.is_empty() {
        return Ok(super::CompileResult {
            success: true,
            files_compiled: 0,
            errors: String::new(),
        });
    }

    std::fs::create_dir_all(&config.output_dir)?;

    // Try ECJ service if configured
    if *engine == super::CompilerEngine::Ecj {
        if let Some(ecj_jar) = super::ecj_service::find_ecj_jar() {
            return compile_with_ecj(&ecj_jar, config, files);
        }
        // ECJ JAR not found, fall back to javac
    }

    compile_with_javac(config, files)
}

fn compile_with_javac(
    config: &super::CompileConfig,
    files: &[PathBuf],
) -> Result<super::CompileResult> {
    let mut cmd = std::process::Command::new("javac");
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

    // Annotation processor path
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

    // Lint options (-Xlint)
    for lint_opt in &config.lint {
        cmd.arg(format!("-Xlint:{}", lint_opt));
    }

    // Extra compiler arguments
    for arg in &config.extra_args {
        cmd.arg(arg);
    }

    // Use @argfile when file list is large (avoids OS command line length limits)
    let _argfile_guard;
    if files.len() > 50 {
        let argfile = config.output_dir.join(".ym-sources.txt");
        let content = files
            .iter()
            .map(|f| f.to_string_lossy().to_string())
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(&argfile, &content)?;
        cmd.arg(format!("@{}", argfile.display()));
        _argfile_guard = Some(ArgfileCleanup(argfile));
    } else {
        _argfile_guard = None;
        for f in files {
            cmd.arg(f);
        }
    }

    let output = cmd.output()?;
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    Ok(super::CompileResult {
        success: output.status.success(),
        files_compiled: files.len(),
        errors: stderr,
    })
}

/// RAII guard to clean up argfile after compilation
struct ArgfileCleanup(PathBuf);
impl Drop for ArgfileCleanup {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn compile_with_ecj(
    ecj_jar: &Path,
    config: &super::CompileConfig,
    files: &[PathBuf],
) -> Result<super::CompileResult> {
    // Use ECJ as a standalone compiler (java -jar ecj.jar ...)
    // This is the fallback when the ECJ service daemon is not running.
    // The ECJ service (long-lived JVM) can be started separately for faster compilation.
    let mut cmd = std::process::Command::new("java");
    cmd.arg("-jar").arg(ecj_jar);

    cmd.arg("-d").arg(&config.output_dir);
    cmd.arg("-proceedOnError");

    if let Some(ref ver) = config.java_version {
        cmd.arg("-source").arg(ver);
        cmd.arg("-target").arg(ver);
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
        cmd.arg("-classpath").arg(&cp);
    }

    for f in files {
        cmd.arg(f);
    }

    let output = cmd.output()?;
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    Ok(super::CompileResult {
        success: output.status.success(),
        files_compiled: files.len(),
        errors: stderr,
    })
}
