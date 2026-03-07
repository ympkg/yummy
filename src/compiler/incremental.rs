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

/// Compute an ABI hash from a .class file.
/// Parses the Java class file format and hashes everything except:
/// - Code attributes (method bodies)
/// - Private fields and methods
/// This means method body changes don't trigger dependent recompilation,
/// only signature/API changes do.
pub fn compute_class_abi_hash(class_file: &Path) -> Result<String> {
    let data = std::fs::read(class_file)?;
    match extract_abi_bytes(&data) {
        Some(abi) => Ok(hash_bytes(&abi)),
        None => Ok(hash_bytes(&data)), // fallback: hash entire file
    }
}

/// Parse a Java class file and extract ABI-relevant bytes (everything except
/// Code attributes and private members).
///
/// Java class file format (simplified):
///   magic(4) version(4) constant_pool fields methods attributes
fn extract_abi_bytes(data: &[u8]) -> Option<Vec<u8>> {
    let len = data.len();

    if len < 10 {
        return None;
    }

    // Magic number: 0xCAFEBABE
    if data[0..4] != [0xCA, 0xFE, 0xBA, 0xBE] {
        return None;
    }

    let mut abi = Vec::with_capacity(len);

    // Include magic + version (8 bytes)
    abi.extend_from_slice(&data[0..8]);
    let mut pos = 8;

    // Parse constant pool count (u16)
    let cp_count = read_u16(data, pos)? as usize;
    abi.extend_from_slice(&data[pos..pos + 2]);
    pos += 2;

    // Skip through constant pool entries (we include them all in ABI hash
    // since they contain type names, method signatures, etc.)
    let cp_start = pos;
    let mut i = 1; // constant pool is 1-indexed
    while i < cp_count {
        if pos >= len {
            return None;
        }
        let tag = data[pos];
        match tag {
            1 => {
                // CONSTANT_Utf8: u16 length + bytes
                if pos + 3 > len {
                    return None;
                }
                let str_len = read_u16(data, pos + 1)? as usize;
                pos += 3 + str_len;
            }
            3 | 4 => pos += 5,    // Integer, Float
            5 | 6 => {
                pos += 9; // Long, Double (takes 2 entries)
                i += 1;
            }
            7 | 8 | 16 | 19 | 20 => pos += 3, // Class, String, MethodType, Module, Package
            9 | 10 | 11 | 12 | 17 | 18 => pos += 5, // Fieldref, Methodref, InterfaceMethodref, NameAndType, Dynamic, InvokeDynamic
            15 => pos += 4, // MethodHandle
            _ => return None, // Unknown tag, bail out
        }
        i += 1;
    }
    // Include entire constant pool
    abi.extend_from_slice(&data[cp_start..pos]);

    // access_flags(2) + this_class(2) + super_class(2)
    if pos + 6 > len {
        return None;
    }
    abi.extend_from_slice(&data[pos..pos + 6]);
    pos += 6;

    // Interfaces count + interface indices
    if pos + 2 > len {
        return None;
    }
    let iface_count = read_u16(data, pos)? as usize;
    let iface_bytes = 2 + iface_count * 2;
    if pos + iface_bytes > len {
        return None;
    }
    abi.extend_from_slice(&data[pos..pos + iface_bytes]);
    pos += iface_bytes;

    // Fields
    pos = extract_members_abi(data, pos, &mut abi, false)?;

    // Methods — skip Code attributes
    pos = extract_members_abi(data, pos, &mut abi, true)?;

    // Class attributes (include all — SourceFile, InnerClasses, etc.)
    if pos + 2 <= len {
        abi.extend_from_slice(&data[pos..len.min(pos + (len - pos))]);
    }

    Some(abi)
}

const ACC_PRIVATE: u16 = 0x0002;

/// Parse fields or methods and add ABI-relevant bytes.
/// For methods with `skip_code=true`, Code attributes are excluded from the hash.
/// Private members are excluded entirely.
fn extract_members_abi(data: &[u8], mut pos: usize, abi: &mut Vec<u8>, skip_code: bool) -> Option<usize> {
    let len = data.len();
    if pos + 2 > len {
        return None;
    }
    let count = read_u16(data, pos)? as usize;
    // We'll write the actual count of non-private members later
    let count_pos = abi.len();
    abi.extend_from_slice(&[0, 0]); // placeholder
    pos += 2;

    let mut included_count: u16 = 0;

    for _ in 0..count {
        if pos + 8 > len {
            return None;
        }
        let access_flags = read_u16(data, pos)?;
        let _name_idx = read_u16(data, pos + 2)?;
        let _desc_idx = read_u16(data, pos + 4)?;
        let attr_count = read_u16(data, pos + 6)? as usize;

        let is_private = (access_flags & ACC_PRIVATE) != 0;

        if !is_private {
            // Include: access_flags + name_index + descriptor_index
            abi.extend_from_slice(&data[pos..pos + 6]);
            included_count += 1;
        }
        pos += 8;

        // We need to write attribute count for included members
        let attr_count_pos = abi.len();
        if !is_private {
            abi.extend_from_slice(&[0, 0]); // placeholder for attr count
        }
        let mut included_attrs: u16 = 0;

        // Parse attributes
        for _ in 0..attr_count {
            if pos + 6 > len {
                return None;
            }
            let attr_name_idx = read_u16(data, pos)?;
            let attr_len = read_u32(data, pos + 2)? as usize;
            let attr_end = pos + 6 + attr_len;
            if attr_end > len {
                return None;
            }

            if !is_private {
                // For methods, check if this is a Code attribute.
                // Code attribute has name_index pointing to "Code" in constant pool.
                // We can't easily resolve the name here without re-parsing the constant pool,
                // so we use a heuristic: we check if the attribute is a Code attribute
                // by looking up the constant pool entry.
                //
                // Actually, let's just check if skip_code is true and this is likely Code.
                // Code attributes are typically the largest method attributes.
                // But a reliable approach: we already parsed the constant pool,
                // so let's resolve the name index.
                //
                // For simplicity and reliability, we include the attribute name index
                // in the ABI. If skip_code, we check the name_idx against known Code positions.
                // Since we can't easily look up the constant pool here, we take a different approach:
                // we scan the constant pool for "Code" utf8 entry during parsing.
                //
                // Simpler approach: just skip large method attributes (Code is always the largest).
                // But that's not reliable.
                //
                // Best approach: we always include all attributes except Code for methods.
                // We detect Code by checking the constant pool string.
                // Since we already have the full data, let's resolve it.
                let is_code_attr = skip_code && is_utf8_constant(data, attr_name_idx, b"Code");

                if !is_code_attr {
                    abi.extend_from_slice(&data[pos..attr_end]);
                    included_attrs += 1;
                }
            }

            pos = attr_end;
        }

        // Patch attribute count
        if !is_private {
            abi[attr_count_pos] = (included_attrs >> 8) as u8;
            abi[attr_count_pos + 1] = (included_attrs & 0xFF) as u8;
        }
    }

    // Patch member count
    abi[count_pos] = (included_count >> 8) as u8;
    abi[count_pos + 1] = (included_count & 0xFF) as u8;

    Some(pos)
}

/// Check if a constant pool entry at the given index is a UTF-8 constant with the given value.
fn is_utf8_constant(data: &[u8], target_idx: u16, expected: &[u8]) -> bool {
    if data.len() < 10 {
        return false;
    }
    let cp_count = match read_u16(data, 8) {
        Some(c) => c as usize,
        None => return false,
    };
    let mut pos = 10;
    let mut idx: u16 = 1;
    while (idx as usize) < cp_count && pos < data.len() {
        let tag = data[pos];
        if idx == target_idx {
            if tag == 1 {
                // CONSTANT_Utf8
                if let Some(str_len) = read_u16(data, pos + 1) {
                    let str_start = pos + 3;
                    let str_end = str_start + str_len as usize;
                    if str_end <= data.len() {
                        return &data[str_start..str_end] == expected;
                    }
                }
            }
            return false;
        }
        match tag {
            1 => {
                let str_len = read_u16(data, pos + 1).unwrap_or(0) as usize;
                pos += 3 + str_len;
            }
            3 | 4 => pos += 5,
            5 | 6 => {
                pos += 9;
                idx += 1;
            }
            7 | 8 | 16 | 19 | 20 => pos += 3,
            9 | 10 | 11 | 12 | 17 | 18 => pos += 5,
            15 => pos += 4,
            _ => return false,
        }
        idx += 1;
    }
    false
}

fn read_u16(data: &[u8], pos: usize) -> Option<u16> {
    if pos + 2 > data.len() {
        return None;
    }
    Some(((data[pos] as u16) << 8) | data[pos + 1] as u16)
}

fn read_u32(data: &[u8], pos: usize) -> Option<u32> {
    if pos + 4 > data.len() {
        return None;
    }
    Some(
        ((data[pos] as u32) << 24)
            | ((data[pos + 1] as u32) << 16)
            | ((data[pos + 2] as u32) << 8)
            | data[pos + 3] as u32,
    )
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

            // Compute ABI hash from corresponding .class file
            // Source: src/main/java/com/Foo.java → Class: output_dir/com/Foo.class
            if let Some(class_file) = find_class_for_source(file, &config.source_dirs, &config.output_dir) {
                if let Ok(abi_hash) = compute_class_abi_hash(&class_file) {
                    fingerprints.update_abi(file, &abi_hash);
                }
            }
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

/// Map a source .java file to its corresponding .class file in the output directory.
/// E.g. src/main/java/com/example/Foo.java → output_dir/com/example/Foo.class
fn find_class_for_source(source: &Path, source_dirs: &[PathBuf], output_dir: &Path) -> Option<PathBuf> {
    for src_dir in source_dirs {
        if let Ok(rel) = source.strip_prefix(src_dir) {
            let class_rel = rel.with_extension("class");
            let class_file = output_dir.join(class_rel);
            if class_file.exists() {
                return Some(class_file);
            }
        }
    }
    None
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal valid Java class file for testing.
    /// Class: public class Test { public void hello() { ... } }
    fn build_test_class(method_code: &[u8]) -> Vec<u8> {
        let mut data = Vec::new();

        // Magic
        data.extend_from_slice(&[0xCA, 0xFE, 0xBA, 0xBE]);
        // Version: Java 8 (52.0)
        data.extend_from_slice(&[0x00, 0x00, 0x00, 0x34]);

        // Constant pool - 10 entries (count = 11, 1-indexed)
        data.extend_from_slice(&[0x00, 0x0B]); // cp count = 11

        // #1 CONSTANT_Utf8 "Test"
        data.push(1);
        data.extend_from_slice(&[0x00, 0x04]);
        data.extend_from_slice(b"Test");

        // #2 CONSTANT_Class -> #1
        data.push(7);
        data.extend_from_slice(&[0x00, 0x01]);

        // #3 CONSTANT_Utf8 "java/lang/Object"
        data.push(1);
        data.extend_from_slice(&[0x00, 0x10]);
        data.extend_from_slice(b"java/lang/Object");

        // #4 CONSTANT_Class -> #3
        data.push(7);
        data.extend_from_slice(&[0x00, 0x03]);

        // #5 CONSTANT_Utf8 "hello"
        data.push(1);
        data.extend_from_slice(&[0x00, 0x05]);
        data.extend_from_slice(b"hello");

        // #6 CONSTANT_Utf8 "()V"
        data.push(1);
        data.extend_from_slice(&[0x00, 0x03]);
        data.extend_from_slice(b"()V");

        // #7 CONSTANT_Utf8 "Code"
        data.push(1);
        data.extend_from_slice(&[0x00, 0x04]);
        data.extend_from_slice(b"Code");

        // #8 CONSTANT_Utf8 "SourceFile"
        data.push(1);
        data.extend_from_slice(&[0x00, 0x0A]);
        data.extend_from_slice(b"SourceFile");

        // #9 CONSTANT_Utf8 "Test.java"
        data.push(1);
        data.extend_from_slice(&[0x00, 0x09]);
        data.extend_from_slice(b"Test.java");

        // #10 CONSTANT_Utf8 "Exceptions"
        data.push(1);
        data.extend_from_slice(&[0x00, 0x0A]);
        data.extend_from_slice(b"Exceptions");

        // access_flags: ACC_PUBLIC (0x0001)
        data.extend_from_slice(&[0x00, 0x01]);
        // this_class: #2
        data.extend_from_slice(&[0x00, 0x02]);
        // super_class: #4
        data.extend_from_slice(&[0x00, 0x04]);

        // interfaces_count: 0
        data.extend_from_slice(&[0x00, 0x00]);

        // fields_count: 0
        data.extend_from_slice(&[0x00, 0x00]);

        // methods_count: 1
        data.extend_from_slice(&[0x00, 0x01]);

        // Method: public void hello()
        // access_flags: ACC_PUBLIC
        data.extend_from_slice(&[0x00, 0x01]);
        // name_index: #5 "hello"
        data.extend_from_slice(&[0x00, 0x05]);
        // descriptor_index: #6 "()V"
        data.extend_from_slice(&[0x00, 0x06]);
        // attributes_count: 1 (Code)
        data.extend_from_slice(&[0x00, 0x01]);

        // Code attribute
        // attribute_name_index: #7 "Code"
        data.extend_from_slice(&[0x00, 0x07]);
        // attribute_length
        let code_len = method_code.len() as u32 + 12; // max_stack(2)+max_locals(2)+code_length(4)+code+exception_table_length(2)+attributes_count(2)
        data.extend_from_slice(&code_len.to_be_bytes());
        // max_stack: 1
        data.extend_from_slice(&[0x00, 0x01]);
        // max_locals: 1
        data.extend_from_slice(&[0x00, 0x01]);
        // code_length
        data.extend_from_slice(&(method_code.len() as u32).to_be_bytes());
        // code bytes
        data.extend_from_slice(method_code);
        // exception_table_length: 0
        data.extend_from_slice(&[0x00, 0x00]);
        // code attributes_count: 0
        data.extend_from_slice(&[0x00, 0x00]);

        // Class attributes_count: 0
        data.extend_from_slice(&[0x00, 0x00]);

        data
    }

    #[test]
    fn test_extract_abi_bytes_valid_class() {
        let class_data = build_test_class(&[0xB1]); // return void
        let abi = extract_abi_bytes(&class_data);
        assert!(abi.is_some());
    }

    #[test]
    fn test_extract_abi_bytes_invalid_magic() {
        let data = vec![0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00];
        assert!(extract_abi_bytes(&data).is_none());
    }

    #[test]
    fn test_extract_abi_bytes_too_short() {
        let data = vec![0xCA, 0xFE, 0xBA, 0xBE];
        assert!(extract_abi_bytes(&data).is_none());
    }

    #[test]
    fn test_abi_unchanged_when_method_body_changes() {
        // Two class files with same signature but different method body
        let class1 = build_test_class(&[0xB1]); // return
        let class2 = build_test_class(&[0x03, 0x57, 0xB1]); // iconst_0, pop, return

        let abi1 = extract_abi_bytes(&class1).unwrap();
        let abi2 = extract_abi_bytes(&class2).unwrap();

        // ABI should be identical since only the Code attribute differs
        assert_eq!(hash_bytes(&abi1), hash_bytes(&abi2));
    }

    #[test]
    fn test_abi_changes_when_signature_changes() {
        let class1 = build_test_class(&[0xB1]);
        let mut class2 = class1.clone();

        // Change the descriptor from "()V" to "()I" by modifying constant pool entry #6
        for i in 0..class2.len() - 2 {
            if &class2[i..i + 3] == b"()V" {
                class2[i + 2] = b'I';
                break;
            }
        }

        let abi1 = extract_abi_bytes(&class1).unwrap();
        let abi2 = extract_abi_bytes(&class2).unwrap();

        // ABI should differ since the method signature changed
        assert_ne!(hash_bytes(&abi1), hash_bytes(&abi2));
    }

    #[test]
    fn test_abi_excludes_private_members() {
        let class1 = build_test_class(&[0xB1]);
        let mut class2 = class1.clone();

        // Change the method from public (0x0001) to private (0x0002)
        // Find: fields_count(00 00) methods_count(00 01) access_flags(00 01)
        for i in 0..class2.len() - 6 {
            if class2[i] == 0x00 && class2[i+1] == 0x00
                && class2[i+2] == 0x00 && class2[i+3] == 0x01
                && class2[i+4] == 0x00 && class2[i+5] == 0x01
            {
                class2[i+5] = 0x02; // Change to PRIVATE
                break;
            }
        }

        let abi1 = extract_abi_bytes(&class1).unwrap();
        let abi2 = extract_abi_bytes(&class2).unwrap();

        // ABI should differ: public method included vs private method excluded
        assert_ne!(hash_bytes(&abi1), hash_bytes(&abi2));
    }

    #[test]
    fn test_is_utf8_constant_lookup() {
        let class_data = build_test_class(&[0xB1]);
        assert!(is_utf8_constant(&class_data, 7, b"Code"));
        assert!(is_utf8_constant(&class_data, 5, b"hello"));
        assert!(is_utf8_constant(&class_data, 1, b"Test"));
        assert!(!is_utf8_constant(&class_data, 1, b"Nope"));
        // CP #2 is a Class ref, not Utf8
        assert!(!is_utf8_constant(&class_data, 2, b"Test"));
    }

    #[test]
    fn test_read_u16_u32_helpers() {
        let data = [0x01, 0x02, 0x03, 0x04];
        assert_eq!(read_u16(&data, 0), Some(0x0102));
        assert_eq!(read_u16(&data, 2), Some(0x0304));
        assert_eq!(read_u32(&data, 0), Some(0x01020304));
        assert_eq!(read_u16(&data, 3), None);
        assert_eq!(read_u32(&data, 2), None);
    }

    #[test]
    fn test_fingerprints_abi_tracking() {
        let mut fp = Fingerprints::default();
        let path = Path::new("src/Foo.java");

        fp.update_source(path, "hash1", 1000);
        fp.update_abi(path, "abi_v1");

        assert!(!fp.abi_changed(path, "abi_v1"));
        assert!(fp.abi_changed(path, "abi_v2"));
        assert!(fp.abi_changed(Path::new("src/Bar.java"), "abi_v1"));
    }
}
