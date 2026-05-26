use anyhow::Result;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

const FINGERPRINT_FILE: &str = "fingerprints.json";
const BUILD_MANIFEST_FILE: &str = "build-manifest.json";

// Hash domain separator tags for cache key computation
mod tag {
    pub const SRC: &[u8] = b"src:";
    pub const RES: &[u8] = b"res:";
    pub const DEP: &[u8] = b"dep:";
    pub const MVN: &[u8] = b"mvn:";
    pub const CP: &[u8] = b"cp:";
    pub const AP: &[u8] = b"ap:";
    pub const VER: &[u8] = b"ver:";
    pub const ENC: &[u8] = b"enc:";
    pub const LINT: &[u8] = b"lint:";
    pub const ARG: &[u8] = b"arg:";
}

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
        for (path, rel_key, mtime) in walk_java_files(source_dirs)? {
            all.push(path.clone());
            if let Some(existing) = self.entries.get(&rel_key) {
                if existing.mtime_secs == mtime {
                    continue;
                }
                if hash_file(&path)? == existing.source_hash {
                    continue;
                }
            }
            changed.push(path);
        }
        Ok((changed, all))
    }

    /// Update fingerprint for a compiled file.
    pub fn update_source(&mut self, path: &Path, source_hash: &str, mtime_secs: u64) {
        let key = crate::normalize_cache_path(path);
        let entry = self.entries.entry(key).or_insert_with(|| FileEntry {
            source_hash: String::new(),
            abi_hash: None,
            mtime_secs: 0,
        });
        entry.source_hash = source_hash.to_string();
        entry.mtime_secs = mtime_secs;
    }

    /// Update ABI hash for a compiled class.
    pub fn update_abi(&mut self, source_path: &Path, abi_hash: &str) {
        let key = crate::normalize_cache_path(source_path);
        if let Some(entry) = self.entries.get_mut(&key) {
            entry.abi_hash = Some(abi_hash.to_string());
        }
    }

    /// Check if the ABI of a source file has changed.
    /// Returns true if ABI changed or if no previous ABI recorded.
    #[allow(dead_code)]
    pub fn abi_changed(&self, source_path: &Path, new_abi_hash: &str) -> bool {
        let key = crate::normalize_cache_path(source_path);
        match self.entries.get(&key) {
            Some(entry) => entry.abi_hash.as_deref() != Some(new_abi_hash),
            None => true,
        }
    }

    /// Remove entries for files that no longer exist.
    /// Returns the list of removed source paths.
    pub fn prune(&mut self, existing_files: &[PathBuf]) -> Vec<String> {
        let existing_keys: std::collections::HashSet<String> = existing_files
            .iter()
            .map(|p| crate::normalize_cache_path(p))
            .collect();
        let removed: Vec<String> = self.entries.keys()
            .filter(|k| !existing_keys.contains(k.as_str()))
            .cloned()
            .collect();
        self.entries.retain(|k, _| existing_keys.contains(k));
        removed
    }
}

fn file_mtime_secs(entry: &walkdir::DirEntry) -> u64 {
    entry
        .metadata()
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Aggregate a module-level ABI hash from per-file fingerprint entries.
/// Reuses already-computed per-file ABI hashes, avoiding re-reading .class files.
fn aggregate_abi_from_fingerprints(fingerprints: &Fingerprints) -> String {
    let mut entries: Vec<(&str, &str)> = fingerprints
        .entries
        .iter()
        .filter_map(|(k, e)| e.abi_hash.as_deref().map(|h| (k.as_str(), h)))
        .collect();
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    let mut hasher = Sha256::new();
    for (key, abi) in &entries {
        hasher.update(key.as_bytes());
        hasher.update(abi.as_bytes());
    }
    format!("{:x}", hasher.finalize())
}

fn cache_timestamp() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Walk source directories and collect all .java files with their normalized key and mtime.
fn walk_java_files(source_dirs: &[PathBuf]) -> Result<Vec<(PathBuf, String, u64)>> {
    let mut files = Vec::new();
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
            let rel_key = crate::normalize_cache_path(&path);
            let mtime = file_mtime_secs(&entry);
            files.push((path, rel_key, mtime));
        }
    }
    Ok(files)
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
///
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
/// Supports build cache sharing: on full recompilation, checks
/// ~/.ym/cache/build-cache/{input_hash}/ for cached .class files.
pub fn incremental_compile(
    config: &super::CompileConfig,
    cache_dir: &Path,
    pool: Option<&super::worker::CompilerPool>,
) -> Result<super::CompileResult> {
    // Use a per-output-dir fingerprint file so workspace modules don't conflict
    let fp_dir = fingerprint_dir_for(cache_dir, &config.output_dir);
    let mut fingerprints = Fingerprints::load(&fp_dir);
    let (changed, all_files) = fingerprints.get_changed_files(&config.source_dirs)?;

    // ADR-014: manifest fast-path. If the previous build wrote a completion
    // manifest AND every recorded class file still exists AND the source set
    // hasn't changed AND no source content changed, we KNOW the prior compile
    // is still valid — skip everything else, return UpToDate immediately.
    //
    // This is the single source of truth for "have we compiled before"; it
    // can't be fooled by resource files in output_dir, fingerprint residue
    // after `rm -rf out`, or any other shared-state pollution that the
    // ADR-013 has_classes heuristic still requires careful guarding against.
    if !all_files.is_empty() && changed.is_empty() {
        if let Some(manifest) = BuildManifest::load(&fp_dir) {
            if manifest.is_consistent_with(&all_files, &config.output_dir) {
                return Ok(super::CompileResult {
                    success: true,
                    outcome: super::CompileOutcome::UpToDate,
                    errors: String::new(),
                    module_abi_hash: Some(aggregate_abi_from_fingerprints(&fingerprints)),
                });
            }
        }
    }

    // A source deleted or renamed since the last build leaves its compiled
    // .class (plus nested / anonymous siblings) orphaned in output_dir. The
    // manifest fast-path above already bailed when the source set changed, so
    // reaching here means we must reconcile output_dir with the current source
    // set. Prune orphans UP FRONT — before the up-to-date early return below —
    // so every downstream path yields an output dir matching exactly the
    // current sources. Previously this ran only after a successful compile, so
    // a pure deletion (changed.is_empty(), nothing to recompile) returned
    // UpToDate with the orphan still on disk; packaging then shipped it (the
    // standard-task-core / entity-relation rename incidents).
    prune_orphan_classes(
        &mut fingerprints,
        &all_files,
        &config.source_dirs,
        &config.output_dir,
        &fp_dir,
    )?;

    // ADR-013: detect "have we compiled before" by looking for .class files
    // specifically, NOT by `dir is non-empty`. The build pipeline copies
    // resources (graphqls, properties, ...) into output_dir BEFORE invoking
    // incremental_compile, so a freshly-cleaned out/classes/ becomes non-empty
    // (resource files only, zero .class) by the time we get here.
    //
    // Old logic `dir.next().is_some()`:
    //   1. resources copied → out/classes/graphql/X.graphqls exists
    //   2. has_classes = true (dir non-empty, treated as "already compiled")
    //   3. fall through to else branch → fingerprint check from prior build
    //   4. all sources unchanged + cache fingerprints intact → "missing" check
    //   5. no .class for src + has fingerprint entry → "no-output module" path
    //   6. UpToDate returned, javac never invoked
    //   7. packaging produces a 0-class jar (see 2026-05-03 standard-task-core
    //      750B incident) which then gets published to the maven registry
    //
    // Correct check: "have we ACTUALLY compiled" = "is there at least one
    // .class file under output_dir?" Resource files do not count.
    let has_classes = config.output_dir.exists()
        && walkdir::WalkDir::new(&config.output_dir)
            .into_iter()
            .filter_map(|e| e.ok())
            .any(|e| e.path().extension().and_then(|s| s.to_str()) == Some("class"));

    let files_to_compile = if !has_classes {
        // Output directory missing or empty — clear stale fingerprints and force full compile.
        // Previous logic skipped recompile when all files had fingerprints (assuming "no-output
        // module"), but this also triggered when output was deleted (e.g. ym clean).
        if !fingerprints.entries.is_empty() {
            fingerprints.entries.clear();
            fingerprints.save(&fp_dir);
        }
        // Full compile needed — try build cache first
        if !all_files.is_empty() {
            if let Some(result) = try_restore_build_cache(config, &all_files, &mut fingerprints, &fp_dir)? {
                // ADR-014: cache restore succeeded — record the resulting state as a
                // valid completed build so the next call hits the manifest fast-path.
                if let Err(e) = BuildManifest::write(&fp_dir, &config.output_dir, &all_files) {
                    eprintln!("  Warning: failed to write build manifest after cache restore: {}", e);
                }
                return Ok(result);
            }
        }
        all_files.clone()
    } else if changed.is_empty() {
        // Check for missing .class files (e.g. user deleted out/classes/ contents)
        // But skip source files that were previously compiled successfully with no output
        // (e.g. entirely commented-out .java files that produce no .class)
        let missing: Vec<PathBuf> = all_files
            .iter()
            .filter(|src| {
                if find_class_for_source(src, &config.source_dirs, &config.output_dir).is_some() {
                    return false; // .class exists, not missing
                }
                // No .class file — check if this source was previously compiled successfully
                // (has a fingerprint entry). If so, it's a no-output file, skip it.
                let key = crate::normalize_cache_path(src);
                !fingerprints.entries.contains_key(&key)
            })
            .cloned()
            .collect();
        if missing.is_empty() {
            // ADR-014: write/refresh manifest so the next invocation can take
            // the manifest fast-path instead of recomputing this fingerprint
            // walk + missing check (and so a future caller that ONLY trusts
            // the manifest sees the truthful state).
            if let Err(e) = BuildManifest::write(&fp_dir, &config.output_dir, &all_files) {
                eprintln!("  Warning: failed to write build manifest on up-to-date path: {}", e);
            }
            return Ok(super::CompileResult {
                success: true,
                outcome: super::CompileOutcome::UpToDate,
                errors: String::new(),
                module_abi_hash: Some(aggregate_abi_from_fingerprints(&fingerprints)),
            });
        }
        missing
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
        resource_dirs: config.resource_dirs.clone(),
        output_dir: config.output_dir.clone(),
        classpath,
        java_version: config.java_version.clone(),
        encoding: config.encoding.clone(),
        annotation_processors: config.annotation_processors.clone(),
        lint: config.lint.clone(),
        extra_args: config.extra_args.clone(),
    };

    // A full compile regenerates a .class for every current source file. Any
    // .class already under output_dir that is NOT regenerated is an orphan —
    // left behind by a since-renamed or -deleted source. The prune()-based
    // cleanup below cannot catch these when fingerprints were cleared (module
    // renamed → fresh fingerprint dir, or `out/` carried stale classes): prune
    // has nothing to diff against. Left in place, orphans get snapshotted into
    // the content-addressed build cache and — because the cache key is purely
    // source-derived — poison every future restore of the same source. Purge
    // stale .class up front so a full compile always yields an output dir that
    // reflects exactly the current source set. Resources are left untouched.
    let is_full_compile = files_to_compile.len() == all_files.len();
    if is_full_compile && has_classes {
        purge_class_files(&config.output_dir);
    }

    let result = compile_files(&incremental_config, &files_to_compile, pool)?;

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
        // Orphan .class pruning for deleted/renamed sources already ran up front
        // (prune_orphan_classes, before the has_classes check) so it also covers
        // the up-to-date early-return path. Nothing left to prune here.
        fingerprints.save(&fp_dir)?;

        // Save to build cache after successful full compilation
        if is_full_compile && !files_to_compile.is_empty() {
            if let Err(e) = save_build_cache(config, &all_files) {
                eprintln!("  Warning: failed to save build cache: {}", e);
            }
        }

        // ADR-014: at this point fingerprints, .class files, and build cache
        // are all coherent — record the completed build state as the LAST
        // step. Next invocation can short-circuit on this manifest without
        // walking output_dir or guessing from fingerprint residue.
        if let Err(e) = BuildManifest::write(&fp_dir, &config.output_dir, &all_files) {
            eprintln!("  Warning: failed to write build manifest: {}", e);
        }
    }

    let abi = if result.success {
        Some(aggregate_abi_from_fingerprints(&fingerprints))
    } else {
        None
    };

    Ok(super::CompileResult {
        success: result.success,
        outcome: if files_to_compile.is_empty() {
            super::CompileOutcome::UpToDate
        } else {
            super::CompileOutcome::Compiled(files_to_compile.len())
        },
        errors: result.errors,
        module_abi_hash: abi,
    })
}

/// Map a source .java file to its corresponding .class file in the output directory.
/// E.g. src/main/java/com/example/Foo.java → output_dir/com/example/Foo.class
///
/// ADR-010 Defense ③: a class file is only considered "present" if its content is valid
/// (size >= 8 bytes + 0xCAFEBABE magic header). 0-byte / truncated files left by an
/// interrupted javac would otherwise be treated as "already compiled" and the source
/// would be skipped on the next incremental build, propagating the corruption.
fn find_class_for_source(source: &Path, source_dirs: &[PathBuf], output_dir: &Path) -> Option<PathBuf> {
    for src_dir in source_dirs {
        if let Ok(rel) = source.strip_prefix(src_dir) {
            let class_rel = rel.with_extension("class");
            let class_file = output_dir.join(class_rel);
            if is_valid_class_file(&class_file) {
                return Some(class_file);
            }
        }
    }
    None
}

/// Prune `.class` families left behind by sources deleted or renamed since the
/// last build: every fingerprint entry whose source is no longer in `all_files`
/// is dropped, and its compiled `.class` (with nested / anonymous siblings) is
/// removed from `output_dir`. Persists the trimmed fingerprints when anything
/// was removed; a no-op (no orphans) touches no files.
///
/// Must run on EVERY incremental_compile path, not only after a recompile: a
/// pure deletion leaves `changed` empty and takes the up-to-date early return,
/// where post-compile cleanup never ran. Reaching that path also guarantees the
/// fingerprints are intact (every current source has a matching entry), so this
/// diff-based prune always has the deleted entry to act on — the "no prior
/// fingerprint to diff against" gap only afflicts the full-compile path, which
/// `purge_class_files` covers separately by clearing all `.class` outright.
fn prune_orphan_classes(
    fingerprints: &mut Fingerprints,
    all_files: &[PathBuf],
    source_dirs: &[PathBuf],
    output_dir: &Path,
    fp_dir: &Path,
) -> Result<()> {
    let removed = fingerprints.prune(all_files);
    if removed.is_empty() {
        return Ok(());
    }
    for removed_src in &removed {
        if let Some(class_file) =
            find_class_for_source(Path::new(removed_src), source_dirs, output_dir)
        {
            remove_class_family(&class_file);
        }
    }
    fingerprints.save(fp_dir)?;
    Ok(())
}

/// Remove every `.class` file under `dir`, recursively, leaving resources and
/// directory structure intact. Called before a full compile so the output dir
/// reflects exactly the current source set — see the call site in
/// `incremental_compile` for why fingerprint-diff orphan cleanup is insufficient.
fn purge_class_files(dir: &Path) {
    if !dir.exists() {
        return;
    }
    for entry in walkdir::WalkDir::new(dir).into_iter().filter_map(|e| e.ok()) {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) == Some("class") {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// Remove a primary `.class` together with its nested / anonymous siblings
/// (`Name.class` + `Name$*.class` in the same directory). A single `.java`
/// file compiles to many class files; deleting only the primary leaves
/// `Name$1.class`, `Name$Inner.class`, ... behind as orphans.
fn remove_class_family(primary_class: &Path) {
    let _ = std::fs::remove_file(primary_class);
    let parent = match primary_class.parent() {
        Some(p) => p,
        None => return,
    };
    let stem = match primary_class.file_stem().and_then(|s| s.to_str()) {
        Some(s) => s.to_string(),
        None => return,
    };
    let nested_prefix = format!("{}$", stem);
    if let Ok(entries) = std::fs::read_dir(parent) {
        for entry in entries.filter_map(|e| e.ok()) {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("class") {
                continue;
            }
            if let Some(name) = path.file_stem().and_then(|s| s.to_str()) {
                if name.starts_with(&nested_prefix) {
                    let _ = std::fs::remove_file(&path);
                }
            }
        }
    }
}

/// Validate a .class file by checking size + 0xCAFEBABE magic header.
/// Returns false if the file does not exist, is too small, or has an invalid magic.
/// This catches 0-byte / truncated files left by interrupted javac runs (see ADR-010).
fn is_valid_class_file(path: &Path) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else { return false };
    // Minimum class file = magic(4) + minor_version(2) + major_version(2) = 8 bytes
    if metadata.len() < 8 { return false; }
    let Ok(mut f) = std::fs::File::open(path) else { return false };
    let mut magic = [0u8; 4];
    use std::io::Read;
    if f.read_exact(&mut magic).is_err() { return false; }
    magic == [0xCA, 0xFE, 0xBA, 0xBE]
}

/// ADR-011: verify every .class file under `dir` (recursively) is valid before
/// trusting the cached output. Cache hits previously short-circuited on
/// `dir.exists()` alone, which let corrupt content (0-byte / truncated .class
/// from earlier interrupted builds) propagate into output_dir on every restore
/// — packaging then produced incomplete jars (see 2026-05-01 standard-task-core
/// incident: 18-entry jar with entity/repository class missing).
///
/// Empty dirs and dirs with only non-.class files (resources, graphqls) are
/// considered valid — placeholder modules legitimately have no .class.
fn is_cache_dir_valid(dir: &Path) -> bool {
    walkdir::WalkDir::new(dir)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().and_then(|s| s.to_str()) == Some("class"))
        .all(|e| is_valid_class_file(e.path()))
}

/// Derive a per-module fingerprint directory from the output dir path.
/// This ensures workspace modules have independent fingerprint files.
pub fn fingerprint_dir_for(cache_dir: &Path, output_dir: &Path) -> PathBuf {
    let hash = hash_bytes(crate::normalize_cache_path(output_dir).as_bytes());
    cache_dir.join("fingerprints").join(&hash[..16])
}

/// ADR-014: per-output-dir compilation completion record.
///
/// Single source of truth for "has this module's javac been fully run AND
/// produced these specific .class files?". Replaces fragile heuristics like
/// "is output_dir non-empty" (ADR-013) and "does fingerprint have an entry"
/// that get fooled by partial state (resources copied before javac, output
/// rm'd after fingerprint write, etc).
///
/// Lifecycle:
/// - **Write**: only at the very end of a successful full or incremental
///   compile. Atomic (sibling tmp + rename).
/// - **Read**: every incremental_compile entry, before any other staleness
///   check. If a valid manifest exists and is consistent with current sources
///   + on-disk class files, return UpToDate immediately — fastest path.
/// - **Invalidation**: any of (a) sources added/removed/renamed, (b) declared
///   class file missing from disk, (c) ym version changed → manifest no
///   longer trusted, fall through to fingerprint / cache restore / javac.
///
/// Stored at the same fingerprint directory as `fingerprints.json` (keyed by
/// output_dir hash), kept OUT of `output_dir` itself so that packaging walks
/// don't have to special-case it and `ym clean`-ing `out/` doesn't accidentally
/// orphan the manifest.
#[derive(Debug, Serialize, Deserialize)]
pub struct BuildManifest {
    pub ym_version: String,
    pub completed_at: u64,
    /// Source files compiled, normalized (matches `normalize_cache_path`).
    pub source_paths: Vec<String>,
    /// Class files produced, paths relative to `output_dir`.
    pub class_paths: Vec<String>,
}

impl BuildManifest {
    fn manifest_path(fp_dir: &Path) -> PathBuf {
        fp_dir.join(BUILD_MANIFEST_FILE)
    }

    pub fn load(fp_dir: &Path) -> Option<Self> {
        let path = Self::manifest_path(fp_dir);
        let content = std::fs::read_to_string(&path).ok()?;
        serde_json::from_str(&content).ok()
    }

    /// Atomically write the manifest, populated from current source list +
    /// whatever .class files are currently under `output_dir`. Caller must
    /// only invoke this AFTER a successful compile (full or incremental) so
    /// the on-disk state truly matches the recorded state.
    pub fn write(fp_dir: &Path, output_dir: &Path, source_files: &[PathBuf]) -> Result<()> {
        std::fs::create_dir_all(fp_dir)?;

        let source_paths: Vec<String> = {
            let mut v: Vec<String> = source_files.iter()
                .map(|p| crate::normalize_cache_path(p))
                .collect();
            v.sort();
            v
        };

        let class_paths: Vec<String> = {
            let mut v: Vec<String> = walkdir::WalkDir::new(output_dir)
                .into_iter()
                .filter_map(|e| e.ok())
                .filter(|e| e.path().extension().and_then(|s| s.to_str()) == Some("class"))
                .filter_map(|e| {
                    e.path().strip_prefix(output_dir).ok()
                        .map(|p| p.to_string_lossy().replace('\\', "/"))
                })
                .collect();
            v.sort();
            v
        };

        let manifest = BuildManifest {
            ym_version: env!("CARGO_PKG_VERSION").to_string(),
            completed_at: cache_timestamp(),
            source_paths,
            class_paths,
        };

        let final_path = Self::manifest_path(fp_dir);
        let tmp_path = fp_dir.join(format!("{}.tmp", BUILD_MANIFEST_FILE));
        let _ = std::fs::remove_file(&tmp_path);
        let content = serde_json::to_string(&manifest)?;
        std::fs::write(&tmp_path, content)?;
        std::fs::rename(&tmp_path, &final_path)?;
        Ok(())
    }

    /// True if every recorded class file still exists on disk under
    /// `output_dir` AND the recorded source list matches `current_sources`
    /// (set equality after normalization). Either condition failing means
    /// the prior build's record no longer reflects reality — invalidate.
    ///
    /// Note: we do NOT validate .class CAFEBABE / size here — that's
    /// ADR-011's responsibility on the cache restore path. The manifest's
    /// invariant is only "the build I recorded is reproducible from disk
    /// state I can see". If a recorded .class got truncated externally
    /// after manifest write, ADR-011's is_valid_class_file (called from
    /// find_class_for_source) will catch it on the per-source verification.
    pub fn is_consistent_with(&self, current_sources: &[PathBuf], output_dir: &Path) -> bool {
        let curr: std::collections::HashSet<String> = current_sources.iter()
            .map(|p| crate::normalize_cache_path(p))
            .collect();
        let prior: std::collections::HashSet<&String> = self.source_paths.iter().collect();
        if curr.len() != prior.len() {
            return false;
        }
        for p in &curr {
            if !prior.contains(p) {
                return false;
            }
        }

        for class_path in &self.class_paths {
            let full = output_dir.join(class_path);
            if !full.exists() {
                return false;
            }
        }

        true
    }
}

/// Feed compiler configuration fields into a hasher (shared by both cache key functions).
fn feed_compiler_config(hasher: &mut Sha256, config: &super::CompileConfig) {
    if let Some(ref v) = config.java_version {
        hasher.update(tag::VER);
        hasher.update(v.as_bytes());
    }
    if let Some(ref e) = config.encoding {
        hasher.update(tag::ENC);
        hasher.update(e.as_bytes());
    }
    for l in &config.lint {
        hasher.update(tag::LINT);
        hasher.update(l.as_bytes());
    }
    for arg in &config.extra_args {
        hasher.update(tag::ARG);
        hasher.update(arg.as_bytes());
    }
}

/// Walk resource directories and return sorted `(normalized_path, content_hash)`
/// pairs for every non-`.java` file. Non-existent dirs are skipped; `.java`
/// files are skipped because they are compiled rather than packaged verbatim
/// (and are already covered by the source-file hashes).
///
/// Folded into the build cache key so a resource-only change yields a different
/// key — see `compute_build_cache_key`.
fn collect_resource_hashes(resource_dirs: &[PathBuf]) -> Result<Vec<(String, String)>> {
    let mut hashes: Vec<(String, String)> = Vec::new();
    for dir in resource_dirs {
        if !dir.exists() {
            continue;
        }
        for entry in walkdir::WalkDir::new(dir) {
            let entry = entry?;
            if !entry.file_type().is_file() {
                continue;
            }
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("java") {
                continue;
            }
            hashes.push((crate::normalize_cache_path(path), hash_file(path)?));
        }
    }
    hashes.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(hashes)
}

/// Compute a content-addressable key from all compilation inputs.
/// Used internally by incremental_compile for single-module cache.
fn compute_build_cache_key(config: &super::CompileConfig, source_files: &[PathBuf]) -> Result<String> {
    let mut hasher = Sha256::new();

    // Cache-format version — see compute_module_cache_key for the rationale.
    hasher.update(b"v3:");

    // Source content hashes (sorted for determinism)
    let mut source_hashes: Vec<(String, String)> = Vec::new();
    for f in source_files {
        let h = hash_file(f)?;
        let rel = crate::normalize_cache_path(f);
        source_hashes.push((rel, h));
    }
    source_hashes.sort_by(|a, b| a.0.cmp(&b.0));
    for (path, hash) in &source_hashes {
        hasher.update(path.as_bytes());
        hasher.update(hash.as_bytes());
    }

    // Resource content hashes. Resources (src/main/resources/**, plus non-.java
    // files under the source dir) are copied verbatim into output_dir and end
    // up inside the packaged jar. They are not compiled, but a resource-only
    // change MUST still invalidate this cache — otherwise try_restore_build_cache
    // restores a stale output_dir whose resources no longer match the source,
    // and packaging ships an outdated GraphQL schema / config file.
    // No resources → no bytes hashed → key unchanged (resource-less modules
    // keep their existing cache valid).
    for (path, hash) in &collect_resource_hashes(&config.resource_dirs)? {
        hasher.update(tag::RES);
        hasher.update(path.as_bytes());
        hasher.update(hash.as_bytes());
    }

    // Classpath (sorted paths)
    let mut cp: Vec<String> = config.classpath.iter()
        .map(|p| crate::normalize_cache_path(p))
        .collect();
    cp.sort();
    for p in &cp {
        hasher.update(tag::CP);
        hasher.update(p.as_bytes());
    }

    feed_compiler_config(&mut hasher, config);
    for ap in &config.annotation_processors {
        hasher.update(tag::AP);
        hasher.update(crate::normalize_cache_path(ap).as_bytes());
    }

    Ok(format!("{:x}", hasher.finalize()))
}

/// Build cache directory: ~/.ym/build-cache/{key}/
fn build_cache_dir(key: &str) -> PathBuf {
    crate::home_dir()
        .join(crate::config::CACHE_DIR)
        .join(crate::config::BUILD_CACHE_DIR)
        .join(key)
}

/// Try to restore compiled classes from the build cache.
/// Returns Some(CompileResult) on cache hit, None on miss.
fn try_restore_build_cache(
    config: &super::CompileConfig,
    source_files: &[PathBuf],
    fingerprints: &mut Fingerprints,
    fp_dir: &Path,
) -> Result<Option<super::CompileResult>> {
    let key = compute_build_cache_key(config, source_files)?;
    let cache_dir = build_cache_dir(&key);

    if !cache_dir.exists() {
        return Ok(None);
    }

    // ADR-011: invalidate corrupt cache rather than restoring 0-byte / truncated
    // .class into output_dir. Without this, a single bad cache entry propagates
    // forever — every restore copies the corrupt content, packaging produces
    // incomplete jars, and the only escape is manually deleting ~/.ym/build-cache.
    if !is_cache_dir_valid(&cache_dir) {
        eprintln!(
            "  Warning: invalidating corrupt build cache at {} (contains 0-byte / non-CAFEBABE .class)",
            cache_dir.display()
        );
        let _ = std::fs::remove_dir_all(&cache_dir);
        return Ok(None);
    }

    // Cache hit — restore .class files. Clear output_dir first so the restore
    // replaces rather than merges: a leftover .class / resource from a prior
    // build of since-renamed source must not survive into the restored output
    // (mirrors try_restore_module_cache, which already clears before restoring).
    if config.output_dir.exists() {
        let _ = std::fs::remove_dir_all(&config.output_dir);
    }
    std::fs::create_dir_all(&config.output_dir)?;
    copy_dir_recursive(&cache_dir, &config.output_dir)?;

    // Rebuild fingerprints from restored files
    for file in source_files {
        let hash = hash_file(file).unwrap_or_default();
        let mtime = std::fs::metadata(file)
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);
        fingerprints.update_source(file, &hash, mtime);

        if let Some(class_file) = find_class_for_source(file, &config.source_dirs, &config.output_dir) {
            if let Ok(abi_hash) = compute_class_abi_hash(&class_file) {
                fingerprints.update_abi(file, &abi_hash);
            }
        }
    }
    fingerprints.save(fp_dir)?;

    Ok(Some(super::CompileResult {
        success: true,
        outcome: super::CompileOutcome::Cached,
        errors: String::new(),
        module_abi_hash: Some(aggregate_abi_from_fingerprints(&fingerprints)),
    }))
}

/// Save compiled output to the build cache.
///
/// ADR-010 Defense ④: writes are atomic — copy into a sibling `.tmp` directory first,
/// then rename to the final cache_dir. Without this, an interrupt mid-copy would leave
/// `cache_dir` partially populated, and the next call's `cache_dir.exists()` check would
/// short-circuit with "already cached", causing future cache hits to restore corrupt
/// (or 0-byte) class files.
fn save_build_cache(config: &super::CompileConfig, source_files: &[PathBuf]) -> Result<()> {
    let key = compute_build_cache_key(config, source_files)?;
    let cache_dir = build_cache_dir(&key);

    if cache_dir.exists() {
        return Ok(()); // Already cached
    }

    // Sibling tmp dir under the same parent — keeps `rename` on the same filesystem
    // (POSIX guarantees rename within a filesystem is atomic).
    let parent = cache_dir.parent()
        .ok_or_else(|| anyhow::anyhow!("build cache dir has no parent: {}", cache_dir.display()))?;
    std::fs::create_dir_all(parent)?;

    let tmp_dir = parent.join(format!("{}.tmp",
        cache_dir.file_name().and_then(|s| s.to_str()).unwrap_or("cache")
    ));
    // Clean up any stale tmp from a previous interrupted run.
    if tmp_dir.exists() {
        std::fs::remove_dir_all(&tmp_dir)?;
    }
    std::fs::create_dir_all(&tmp_dir)?;
    copy_dir_recursive(&config.output_dir, &tmp_dir)?;

    // Atomic publish. If another process raced us and already created cache_dir,
    // discard our tmp and accept their version.
    match std::fs::rename(&tmp_dir, &cache_dir) {
        Ok(()) => Ok(()),
        Err(_) if cache_dir.exists() => {
            let _ = std::fs::remove_dir_all(&tmp_dir);
            Ok(())
        }
        Err(e) => Err(e.into()),
    }
}

/// Recursively hardlink directory contents, falling back to copy on cross-filesystem.
/// Hardlink files when possible (same filesystem), fall back to copy.
/// Detects cross-filesystem (EXDEV) on first failure and switches to copy-only.
fn hardlink_or_copy_dir(src: &Path, dst: &Path) -> Result<()> {
    let mut use_hardlink = true;
    for entry in walkdir::WalkDir::new(src) {
        let entry = entry?;
        let rel = entry.path().strip_prefix(src)?;
        let dest = dst.join(rel);
        if entry.file_type().is_dir() {
            std::fs::create_dir_all(&dest)?;
        } else {
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent)?;
            }
            if use_hardlink {
                match std::fs::hard_link(entry.path(), &dest) {
                    Ok(()) => continue,
                    Err(_) => {
                        use_hardlink = false;
                        std::fs::copy(entry.path(), &dest)?;
                    }
                }
            } else {
                std::fs::copy(entry.path(), &dest)?;
            }
        }
    }
    Ok(())
}

/// Recursively copy directory contents.
pub fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<()> {
    for entry in walkdir::WalkDir::new(src) {
        let entry = entry?;
        let rel = entry.path().strip_prefix(src)?;
        let dest = dst.join(rel);
        if entry.file_type().is_dir() {
            std::fs::create_dir_all(&dest)?;
        } else {
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::copy(entry.path(), &dest)?;
        }
    }
    Ok(())
}

/// Compile specific files using javac (or worker pool if available).
fn compile_files(
    config: &super::CompileConfig,
    files: &[PathBuf],
    pool: Option<&super::worker::CompilerPool>,
) -> Result<super::CompileResult> {
    if files.is_empty() {
        return Ok(super::CompileResult {
            success: true,
            outcome: super::CompileOutcome::UpToDate,
            errors: String::new(),
            module_abi_hash: None,
        });
    }

    std::fs::create_dir_all(&config.output_dir)?;

    if let Some(pool) = pool {
        pool.compile(config, files)
    } else {
        compile_with_javac(config, files)
    }
}

/// Direct javac compilation (public for worker fallback).
pub fn compile_files_direct(
    config: &super::CompileConfig,
    files: &[PathBuf],
) -> Result<super::CompileResult> {
    if files.is_empty() {
        return Ok(super::CompileResult {
            success: true,
            outcome: super::CompileOutcome::UpToDate,
            errors: String::new(),
            module_abi_hash: None,
        });
    }
    std::fs::create_dir_all(&config.output_dir)?;
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
            _cp_argfile_guard = Some(ArgfileCleanup(cp_file));
        } else {
            _cp_argfile_guard = None;
            cmd.arg("-cp").arg(&cp);
        }
    } else {
        _cp_argfile_guard = None;
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
    } else {
        cmd.arg("-proc:none");
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
        outcome: super::CompileOutcome::Compiled(files.len()),
        errors: stderr,
        module_abi_hash: None,
    })
}

/// RAII guard to clean up argfile after compilation
pub(crate) struct ArgfileCleanup(pub(crate) PathBuf);
impl Drop for ArgfileCleanup {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

// ═══════════════════════════════════════════════════════════════════════
// Content-addressed build cache: public API for workspace wave scheduling
// ═══════════════════════════════════════════════════════════════════════

/// Compute content hashes for all source files in the given directories,
/// using mtime fast path to avoid rehashing unchanged files.
///
/// Returns sorted Vec<(relative_path, content_sha256)>.
pub fn compute_source_content_hashes(
    source_dirs: &[PathBuf],
    cache_dir: &Path,
    output_dir: &Path,
) -> Result<Vec<(String, String)>> {
    let fp_dir = fingerprint_dir_for(cache_dir, output_dir);
    let fingerprints = Fingerprints::load(&fp_dir);

    let mut hashes: Vec<(String, String)> = Vec::new();
    for (path, rel_key, mtime) in walk_java_files(source_dirs)? {
        let content_hash = match fingerprints.entries.get(&rel_key) {
            Some(e) if e.mtime_secs == mtime => e.source_hash.clone(),
            _ => hash_file(&path)?,
        };
        hashes.push((rel_key, content_hash));
    }
    hashes.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(hashes)
}

/// Compute a module-level ABI hash by aggregating ABI hashes of all .class files
/// in the output directory.
///
/// Module ABI hash = SHA-256(sorted(class_file_path + abi_hash))
pub fn compute_module_abi_hash(output_dir: &Path) -> Result<String> {
    let mut entries: Vec<(String, String)> = Vec::new();

    if !output_dir.exists() {
        return Ok(hash_bytes(b"empty"));
    }

    for entry in walkdir::WalkDir::new(output_dir) {
        let entry = entry?;
        if entry.path().extension().and_then(|e| e.to_str()) != Some("class") {
            continue;
        }
        let rel = entry
            .path()
            .strip_prefix(output_dir)
            .unwrap_or(entry.path())
            .to_string_lossy()
            .to_string();
        let abi_hash = compute_class_abi_hash(entry.path()).unwrap_or_else(|_| {
            hash_file(entry.path()).unwrap_or_else(|_| hash_bytes(b"unreadable"))
        });
        entries.push((rel, abi_hash));
    }

    entries.sort_by(|a, b| a.0.cmp(&b.0));

    let mut hasher = Sha256::new();
    for (path, hash) in &entries {
        hasher.update(path.as_bytes());
        hasher.update(hash.as_bytes());
    }
    Ok(format!("{:x}", hasher.finalize()))
}

/// Inputs for computing a content-addressed module cache key.
/// All slice fields must be sorted by first element for deterministic hashing.
pub struct ModuleCacheInput<'a> {
    pub source_hashes: &'a [(String, String)],
    pub dep_abi_hashes: &'a [(String, String)],
    pub maven_jar_sha256s: &'a [(String, String)],
    pub config: &'a super::CompileConfig,
    pub ap_jar_sha256s: &'a [(String, String)],
}

/// Compute a content-addressed module cache key for workspace wave scheduling.
pub fn compute_module_cache_key(input: &ModuleCacheInput) -> String {
    let mut hasher = Sha256::new();

    // Cache-format version. Bumped v1 → v2 to abandon entries with orphan
    // .class files from a since-renamed package; v2 → v3 to abandon entries
    // that snapshotted orphan *resource* files (resource copying was additive
    // until resources::sync_resources started pruning). The cache key is
    // source-derived, so a corrected rebuild of the same source hashes to the
    // same key; without a version bump it would keep restoring the poisoned
    // pre-fix entry.
    hasher.update(b"v3:");

    // All input slices are pre-sorted by caller (see ModuleCacheInput doc)
    for (path, hash) in input.source_hashes {
        hasher.update(tag::SRC);
        hasher.update(path.as_bytes());
        hasher.update(hash.as_bytes());
    }
    for (name, abi) in input.dep_abi_hashes {
        hasher.update(tag::DEP);
        hasher.update(name.as_bytes());
        hasher.update(abi.as_bytes());
    }
    for (coord, sha) in input.maven_jar_sha256s {
        hasher.update(tag::MVN);
        hasher.update(coord.as_bytes());
        hasher.update(sha.as_bytes());
    }
    feed_compiler_config(&mut hasher, input.config);
    for (path, sha) in input.ap_jar_sha256s {
        hasher.update(tag::AP);
        hasher.update(path.as_bytes());
        hasher.update(sha.as_bytes());
    }

    format!("{:x}", hasher.finalize())
}

/// Try to restore a module from the content-addressed build cache.
/// Returns Some(abi_hash) on cache hit, None on miss.
pub fn try_restore_module_cache(
    cache_key: &str,
    output_dir: &Path,
) -> Result<Option<String>> {
    let cache_dir = build_cache_dir(cache_key);
    let classes_dir = cache_dir.join("classes");

    if !classes_dir.exists() {
        return Ok(None);
    }

    // ADR-011: invalidate corrupt cache. See is_cache_dir_valid doc comment for context.
    if !is_cache_dir_valid(&classes_dir) {
        eprintln!(
            "  Warning: invalidating corrupt module cache at {} (contains 0-byte / non-CAFEBABE .class)",
            cache_dir.display()
        );
        let _ = std::fs::remove_dir_all(&cache_dir);
        return Ok(None);
    }

    // Clear stale output before restoring to avoid leftover .class files from previous builds
    if output_dir.exists() {
        let _ = std::fs::remove_dir_all(output_dir);
    }
    std::fs::create_dir_all(output_dir)?;
    copy_dir_recursive(&classes_dir, output_dir)?;

    // Read stored ABI hash
    let abi_path = cache_dir.join("abi_hash");
    let abi_hash = std::fs::read_to_string(&abi_path)
        .map(|s| s.trim().to_string())
        .unwrap_or_default();

    // Touch meta.json mtime for LRU eviction (avoids JSON parse overhead on hot path)
    let meta_path = cache_dir.join("meta.json");
    let _ = std::fs::OpenOptions::new().write(true).open(&meta_path);

    Ok(Some(abi_hash))
}

/// Save a module's compilation output to the content-addressed build cache.
pub fn save_module_cache(
    cache_key: &str,
    output_dir: &Path,
    abi_hash: &str,
    module_name: &str,
) -> Result<()> {
    let cache_dir = build_cache_dir(cache_key);
    let classes_dir = cache_dir.join("classes");

    if classes_dir.exists() {
        return Ok(()); // Already cached
    }

    std::fs::create_dir_all(&classes_dir)?;
    hardlink_or_copy_dir(output_dir, &classes_dir)?;

    // Write ABI hash
    std::fs::write(cache_dir.join("abi_hash"), abi_hash)?;

    let now = cache_timestamp();
    let meta = serde_json::json!({
        "created_at": now,
        "last_accessed": now,
        "module": module_name,
    });
    std::fs::write(cache_dir.join("meta.json"), meta.to_string())?;

    Ok(())
}

const CACHE_MAX_AGE_DAYS: u64 = 30;

/// Evict build cache entries not accessed in the last N days.
/// Runs after successful builds; errors are silently ignored to never block compilation.
pub fn evict_stale_build_cache() {
    let cache_root = crate::home_dir()
        .join(crate::config::CACHE_DIR)
        .join(crate::config::BUILD_CACHE_DIR);

    let entries = match std::fs::read_dir(&cache_root) {
        Ok(e) => e,
        Err(_) => return,
    };

    let cutoff = cache_timestamp().saturating_sub(CACHE_MAX_AGE_DAYS * 86400);

    for entry in entries.filter_map(|e| e.ok()) {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        // Use meta.json mtime as last-accessed indicator
        let meta = path.join("meta.json");
        let mtime = std::fs::metadata(&meta)
            .or_else(|_| std::fs::metadata(&path))
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);

        if mtime < cutoff {
            let _ = std::fs::remove_dir_all(&path);
        }
    }
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
    fn test_purge_class_files_removes_class_keeps_resources() {
        let tmp = tempfile::tempdir().unwrap();
        let out = tmp.path();
        let pkg = out.join("com/example");
        std::fs::create_dir_all(&pkg).unwrap();
        std::fs::write(pkg.join("Foo.class"), build_test_class(&[0xB1])).unwrap();
        std::fs::write(pkg.join("Foo$1.class"), build_test_class(&[0xB1])).unwrap();
        // Resources are copied into output_dir before compilation — must survive.
        std::fs::write(pkg.join("schema.graphqls"), "type Query").unwrap();
        std::fs::write(out.join("application.properties"), "k=v").unwrap();

        purge_class_files(out);

        assert!(!pkg.join("Foo.class").exists(), "primary .class must be purged");
        assert!(!pkg.join("Foo$1.class").exists(), "nested .class must be purged");
        assert!(pkg.join("schema.graphqls").exists(), "resource must survive purge");
        assert!(out.join("application.properties").exists(), "resource must survive purge");
    }

    #[test]
    fn test_remove_class_family_removes_nested_and_anonymous() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let primary = dir.join("Service.class");
        std::fs::write(&primary, build_test_class(&[0xB1])).unwrap();
        std::fs::write(dir.join("Service$Inner.class"), build_test_class(&[0xB1])).unwrap();
        std::fs::write(dir.join("Service$1.class"), build_test_class(&[0xB1])).unwrap();
        // A different top-level class sharing a name prefix must NOT be removed.
        std::fs::write(dir.join("ServiceHelper.class"), build_test_class(&[0xB1])).unwrap();

        remove_class_family(&primary);

        assert!(!primary.exists(), "primary .class removed");
        assert!(!dir.join("Service$Inner.class").exists(), "nested .class removed");
        assert!(!dir.join("Service$1.class").exists(), "anonymous .class removed");
        assert!(dir.join("ServiceHelper.class").exists(), "unrelated sibling kept");
    }

    /// Regression for the orphan-`.class` leak on the up-to-date path.
    ///
    /// When a source is deleted/renamed and NO surviving source changed,
    /// `incremental_compile` takes the up-to-date early return (changed empty,
    /// nothing missing) — javac is never invoked. Orphan-`.class` cleanup used
    /// to run only AFTER a real compile, so the deleted source's `.class` (plus
    /// its nested / anonymous siblings) stayed in `output_dir` and got packaged
    /// into the jar (the standard-task-core / entity-relation rename incidents).
    /// Pruning now runs up front on every path, so the orphan is gone even
    /// though this path compiles nothing.
    #[test]
    fn test_incremental_compile_prunes_orphan_class_when_source_deleted() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = tmp.path().join("cache");
        let src_dir = tmp.path().join("src");
        let output_dir = tmp.path().join("out");
        let pkg = output_dir.join("com/example");
        std::fs::create_dir_all(src_dir.join("com/example")).unwrap();
        std::fs::create_dir_all(&pkg).unwrap();

        // Surviving source A + its compiled class.
        let a_java = src_dir.join("com/example/A.java");
        std::fs::write(&a_java, "package com.example; class A {}").unwrap();
        std::fs::write(pkg.join("A.class"), build_test_class(&[0xB1])).unwrap();

        // B was deleted from src after the last build, but its class family
        // (primary + nested) still sits in output_dir as an orphan.
        let b_java = src_dir.join("com/example/B.java");
        std::fs::write(pkg.join("B.class"), build_test_class(&[0xB1])).unwrap();
        std::fs::write(pkg.join("B$Inner.class"), build_test_class(&[0xB1])).unwrap();

        let config = super::super::CompileConfig {
            source_dirs: vec![src_dir.clone()],
            resource_dirs: vec![],
            output_dir: output_dir.clone(),
            classpath: vec![],
            java_version: Some("21".to_string()),
            encoding: None,
            annotation_processors: vec![],
            lint: vec![],
            extra_args: vec![],
        };

        // Seed fingerprints + manifest as the prior build (A + B) left them:
        // A matches the on-disk source (so `changed` stays empty), B is stale.
        let fp_dir = fingerprint_dir_for(&cache, &output_dir);
        let mut fp = Fingerprints::default();
        let a_hash = hash_file(&a_java).unwrap();
        let a_mtime = std::fs::metadata(&a_java).unwrap().modified().unwrap()
            .duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
        fp.update_source(&a_java, &a_hash, a_mtime);
        fp.update_source(&b_java, "stale-b-hash", 1);
        fp.save(&fp_dir).unwrap();
        BuildManifest::write(&fp_dir, &output_dir, &[a_java.clone(), b_java.clone()]).unwrap();

        let result = incremental_compile(&config, &cache, None).unwrap();

        // No surviving source changed → up-to-date, javac never invoked.
        assert!(result.success);
        assert_eq!(result.outcome, super::super::CompileOutcome::UpToDate);
        // Live class kept; orphan family (incl. nested) pruned.
        assert!(pkg.join("A.class").exists(), "live class must survive");
        assert!(!pkg.join("B.class").exists(), "orphan primary .class must be pruned");
        assert!(!pkg.join("B$Inner.class").exists(), "orphan nested .class must be pruned");
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

    /// ADR-010 Defense ③: a class file with valid CAFEBABE magic is recognized.
    #[test]
    fn test_is_valid_class_file_valid_magic() {
        let tmp = tempfile::tempdir().unwrap();
        let class_file = tmp.path().join("Foo.class");
        std::fs::write(&class_file, b"\xCA\xFE\xBA\xBE\x00\x00\x00\x42").unwrap();
        assert!(is_valid_class_file(&class_file));
    }

    /// ADR-010 Defense ③: a 0-byte class file (interrupted javac) returns false.
    #[test]
    fn test_is_valid_class_file_rejects_zero_byte() {
        let tmp = tempfile::tempdir().unwrap();
        let class_file = tmp.path().join("Broken.class");
        std::fs::File::create(&class_file).unwrap();
        assert_eq!(std::fs::metadata(&class_file).unwrap().len(), 0);
        assert!(!is_valid_class_file(&class_file), "0-byte class must be invalid");
    }

    /// ADR-010 Defense ③: a truncated class file (size < 8) returns false.
    #[test]
    fn test_is_valid_class_file_rejects_truncated() {
        let tmp = tempfile::tempdir().unwrap();
        let class_file = tmp.path().join("Trunc.class");
        std::fs::write(&class_file, b"\xCA\xFE\xBA").unwrap();
        assert!(!is_valid_class_file(&class_file));
    }

    /// ADR-010 Defense ③: a file without CAFEBABE magic is invalid even if size is large.
    #[test]
    fn test_is_valid_class_file_rejects_wrong_magic() {
        let tmp = tempfile::tempdir().unwrap();
        let class_file = tmp.path().join("Garbage.class");
        std::fs::write(&class_file, b"\x00\x00\x00\x00\x00\x00\x00\x00").unwrap();
        assert!(!is_valid_class_file(&class_file));
    }

    /// ADR-010 Defense ③: missing file returns false (not an error).
    #[test]
    fn test_is_valid_class_file_missing() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(!is_valid_class_file(&tmp.path().join("nope.class")));
    }

    /// ADR-010 Defense ③: find_class_for_source must skip 0-byte class files,
    /// forcing the source to be recompiled.
    #[test]
    fn test_find_class_for_source_skips_zero_byte() {
        let tmp = tempfile::tempdir().unwrap();
        let src_dir = tmp.path().join("src");
        let pkg_dir = src_dir.join("com").join("example");
        std::fs::create_dir_all(&pkg_dir).unwrap();
        let source = pkg_dir.join("Foo.java");
        std::fs::write(&source, "package com.example; class Foo {}").unwrap();

        let out_dir = tmp.path().join("out");
        let out_pkg = out_dir.join("com").join("example");
        std::fs::create_dir_all(&out_pkg).unwrap();
        // Simulate interrupted javac: 0-byte .class.
        std::fs::File::create(out_pkg.join("Foo.class")).unwrap();

        let result = find_class_for_source(&source, &[src_dir], &out_dir);
        assert!(result.is_none(),
            "0-byte class must be treated as missing → triggers recompilation");
    }

    /// ADR-010 Defense ④: save_build_cache must be atomic — sibling tmp + rename.
    /// Verify that after a successful save, both the cache_dir exists and no .tmp leaked.
    #[test]
    fn test_save_build_cache_atomic_no_tmp_leak() {
        let tmp = tempfile::tempdir().unwrap();
        let output_dir = tmp.path().join("out");
        std::fs::create_dir_all(&output_dir).unwrap();
        std::fs::write(output_dir.join("Foo.class"), b"\xCA\xFE\xBA\xBE\x00\x00\x00\x42").unwrap();

        let config = super::super::CompileConfig {
            source_dirs: vec![],
            resource_dirs: vec![],
            output_dir: output_dir.clone(),
            classpath: vec![],
            java_version: Some("17".to_string()),
            encoding: None,
            annotation_processors: vec![],
            lint: vec![],
            extra_args: vec![],
        };
        let source_files = vec![tmp.path().join("Foo.java")];
        std::fs::write(&source_files[0], "class Foo {}").unwrap();

        save_build_cache(&config, &source_files).expect("save_build_cache must succeed");

        // Verify cache_dir exists and contains the class file
        let key = compute_build_cache_key(&config, &source_files).unwrap();
        let cache_dir = build_cache_dir(&key);
        assert!(cache_dir.exists(), "cache_dir must exist after save");
        assert!(cache_dir.join("Foo.class").exists(), "Foo.class must be in cache");

        // Verify no sibling .tmp leaked
        let tmp_sibling = cache_dir.parent().unwrap()
            .join(format!("{}.tmp", cache_dir.file_name().unwrap().to_str().unwrap()));
        assert!(!tmp_sibling.exists(), ".tmp sibling must not leak after rename");

        // Cleanup global cache dir we created
        let _ = std::fs::remove_dir_all(&cache_dir);
    }

    /// ADR-010 Defense ④: a stale .tmp from a prior interrupted run must be cleaned up,
    /// not block the next save.
    #[test]
    fn test_save_build_cache_clears_stale_tmp() {
        let tmp = tempfile::tempdir().unwrap();
        let output_dir = tmp.path().join("out");
        std::fs::create_dir_all(&output_dir).unwrap();
        std::fs::write(output_dir.join("Foo.class"), b"\xCA\xFE\xBA\xBE\x00\x00\x00\x42").unwrap();

        let config = super::super::CompileConfig {
            source_dirs: vec![],
            resource_dirs: vec![],
            output_dir: output_dir.clone(),
            classpath: vec![],
            java_version: Some("21".to_string()),  // distinct version → distinct cache key
            encoding: None,
            annotation_processors: vec![],
            lint: vec![],
            extra_args: vec![],
        };
        let source_files = vec![tmp.path().join("StaleTmp.java")];
        std::fs::write(&source_files[0], "class StaleTmp {}").unwrap();

        // Pre-create a stale .tmp (as if a previous run was interrupted mid-copy)
        let key = compute_build_cache_key(&config, &source_files).unwrap();
        let cache_dir = build_cache_dir(&key);
        let parent = cache_dir.parent().unwrap();
        std::fs::create_dir_all(parent).unwrap();
        let stale_tmp = parent.join(format!("{}.tmp", cache_dir.file_name().unwrap().to_str().unwrap()));
        std::fs::create_dir_all(&stale_tmp).unwrap();
        std::fs::write(stale_tmp.join("garbage"), b"residue").unwrap();

        save_build_cache(&config, &source_files).expect("must clear stale tmp and succeed");

        assert!(cache_dir.exists(), "cache_dir must exist");
        assert!(!cache_dir.join("garbage").exists(),
            "stale residue from old tmp must NOT appear in final cache");
        assert!(cache_dir.join("Foo.class").exists(), "fresh content must be in cache");

        let _ = std::fs::remove_dir_all(&cache_dir);
    }

    /// Resource-staleness regression: a change to a packaged resource file
    /// (src/main/resources/**) with NO .java change must produce a different
    /// build cache key.
    ///
    /// Before this was fixed, compute_build_cache_key hashed only .java
    /// sources. A resource-only edit kept the key identical, so
    /// try_restore_build_cache restored a stale output_dir (old .class + old
    /// resources) — the outdated resource then got packaged into the jar. The
    /// whole compile→CI chain stayed green while shipping an artifact whose
    /// GraphQL schema / application.yml did not match the committed source.
    #[test]
    fn test_build_cache_key_tracks_resource_changes() {
        let tmp = tempfile::tempdir().unwrap();

        // src/main/java/Foo.java — the .java source, kept constant throughout.
        let java_dir = tmp.path().join("src").join("main").join("java");
        std::fs::create_dir_all(&java_dir).unwrap();
        let java_file = java_dir.join("Foo.java");
        std::fs::write(&java_file, "class Foo {}").unwrap();

        // src/main/resources/schema.graphqls — the resource we will mutate.
        let res_dir = tmp.path().join("src").join("main").join("resources");
        std::fs::create_dir_all(&res_dir).unwrap();
        let res_file = res_dir.join("schema.graphqls");
        std::fs::write(&res_file, "type Query { a: String }").unwrap();

        let make_config = || super::super::CompileConfig {
            source_dirs: vec![java_dir.clone()],
            resource_dirs: vec![java_dir.clone(), res_dir.clone()],
            output_dir: tmp.path().join("out"),
            classpath: vec![],
            java_version: Some("17".to_string()),
            encoding: None,
            annotation_processors: vec![],
            lint: vec![],
            extra_args: vec![],
        };
        let source_files = vec![java_file.clone()];

        let key_before = compute_build_cache_key(&make_config(), &source_files).unwrap();
        // Determinism: identical inputs → identical key.
        assert_eq!(
            key_before,
            compute_build_cache_key(&make_config(), &source_files).unwrap(),
            "cache key must be deterministic for identical inputs"
        );

        // Mutate ONLY the resource file — the .java file is untouched.
        std::fs::write(&res_file, "type Query { a: String, b: Int }").unwrap();
        let key_after_edit = compute_build_cache_key(&make_config(), &source_files).unwrap();
        assert_ne!(
            key_before, key_after_edit,
            "editing a resource file must change the build cache key"
        );

        // Adding a brand-new resource file must also change the key.
        std::fs::write(res_dir.join("application.yml"), b"server:\n  port: 8080\n").unwrap();
        let key_after_add = compute_build_cache_key(&make_config(), &source_files).unwrap();
        assert_ne!(
            key_after_edit, key_after_add,
            "adding a resource file must change the build cache key"
        );
    }

    /// A module with no resource files must compute the same key whether
    /// resource_dirs is empty or points at non-existent directories — so
    /// resource-less modules keep their existing build cache valid (and a
    /// .java source change still drives the key, as before).
    #[test]
    fn test_build_cache_key_no_resources_is_stable() {
        let tmp = tempfile::tempdir().unwrap();
        let java_dir = tmp.path().join("src").join("main").join("java");
        std::fs::create_dir_all(&java_dir).unwrap();
        let java_file = java_dir.join("Bar.java");
        std::fs::write(&java_file, "class Bar {}").unwrap();
        let source_files = vec![java_file.clone()];

        let base = |resource_dirs: Vec<std::path::PathBuf>| super::super::CompileConfig {
            source_dirs: vec![java_dir.clone()],
            resource_dirs,
            output_dir: tmp.path().join("out"),
            classpath: vec![],
            java_version: Some("17".to_string()),
            encoding: None,
            annotation_processors: vec![],
            lint: vec![],
            extra_args: vec![],
        };

        let key_empty = compute_build_cache_key(&base(vec![]), &source_files).unwrap();
        let key_missing_dir = compute_build_cache_key(
            &base(vec![tmp.path().join("does-not-exist")]), &source_files,
        ).unwrap();
        assert_eq!(
            key_empty, key_missing_dir,
            "empty / non-existent resource dirs must not perturb the cache key"
        );

        // A .java content change still changes the key.
        std::fs::write(&java_file, "class Bar { int x; }").unwrap();
        let key_java_changed = compute_build_cache_key(&base(vec![]), &source_files).unwrap();
        assert_ne!(
            key_empty, key_java_changed,
            "a .java source change must still change the build cache key"
        );
    }

    /// ADR-013 root-cause regression: simulate the standard-task-core 750B
    /// incident pre-conditions and assert incremental_compile recognises that
    /// no compilation has actually happened (despite resources being present
    /// in output_dir).
    ///
    /// Pre-conditions of the bug:
    ///   1. output_dir was previously cleaned (no .class files)
    ///   2. resource files (e.g. graphqls) WERE copied into output_dir before
    ///      incremental_compile runs (this is what build.rs:2740 does)
    ///   3. fingerprints from a previous successful build still exist
    ///
    /// With OLD `has_classes = dir.next().is_some()`: misjudges as "already
    /// compiled", goes to UpToDate path, never invokes javac.
    /// With NEW `has_classes = any .class file`: correctly reports false,
    /// triggers full compile.
    #[test]
    fn test_has_classes_ignores_resources_only_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let output_dir = tmp.path().join("classes");
        std::fs::create_dir_all(&output_dir).unwrap();
        // Simulate "resources copied but no .class compiled yet" — exactly the
        // state build.rs leaves output_dir in just before calling
        // incremental_compile.
        let res_dir = output_dir.join("graphql");
        std::fs::create_dir_all(&res_dir).unwrap();
        std::fs::write(res_dir.join("Schema.graphqls"), b"type Q {}").unwrap();
        std::fs::write(output_dir.join("application.yml"), b"port: 8080").unwrap();

        // Inline the (small) has_classes check we use in incremental_compile to
        // pin the behaviour down without hauling in the whole compile config.
        let has_classes = output_dir.exists()
            && walkdir::WalkDir::new(&output_dir)
                .into_iter()
                .filter_map(|e| e.ok())
                .any(|e| e.path().extension().and_then(|s| s.to_str()) == Some("class"));

        assert!(!has_classes,
            "resources-only output_dir must NOT count as 'has compiled classes' \
             — otherwise incremental_compile skips javac and packaging produces \
             a 0-class jar (regression of 2026-05-03 standard-task-core)");
    }

    /// Sanity: actual .class file under output_dir does count.
    #[test]
    fn test_has_classes_detects_real_class() {
        let tmp = tempfile::tempdir().unwrap();
        let output_dir = tmp.path().join("classes");
        std::fs::create_dir_all(output_dir.join("com").join("example")).unwrap();
        std::fs::write(output_dir.join("com").join("example").join("Foo.class"),
            b"\xCA\xFE\xBA\xBE\x00\x00\x00\x42").unwrap();
        // Mix in a resource for good measure.
        std::fs::write(output_dir.join("application.yml"), b"port: 8080").unwrap();

        let has_classes = output_dir.exists()
            && walkdir::WalkDir::new(&output_dir)
                .into_iter()
                .filter_map(|e| e.ok())
                .any(|e| e.path().extension().and_then(|s| s.to_str()) == Some("class"));

        assert!(has_classes,
            "output_dir with at least one .class file must count as having compiled classes");
    }

    // ─────────────────────────────────────────────────────────────────────
    // ADR-014: BuildManifest tests
    // ─────────────────────────────────────────────────────────────────────

    /// Round-trip: write a manifest, load it, fields preserved.
    #[test]
    fn test_build_manifest_write_load_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let fp_dir = tmp.path().join("fp");
        let output_dir = tmp.path().join("out");
        std::fs::create_dir_all(&output_dir).unwrap();
        std::fs::write(output_dir.join("Foo.class"),
            b"\xCA\xFE\xBA\xBE\x00\x00\x00\x42").unwrap();
        let nested = output_dir.join("com").join("example");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(nested.join("Bar.class"),
            b"\xCA\xFE\xBA\xBE\x00\x00\x00\x42").unwrap();

        let sources = vec![
            tmp.path().join("Foo.java"),
            tmp.path().join("com/example/Bar.java"),
        ];

        BuildManifest::write(&fp_dir, &output_dir, &sources).unwrap();

        let loaded = BuildManifest::load(&fp_dir).expect("manifest must load");
        assert_eq!(loaded.ym_version, env!("CARGO_PKG_VERSION"));
        assert_eq!(loaded.source_paths.len(), 2, "two sources recorded");
        assert_eq!(loaded.class_paths.len(), 2, "two .class files recorded");
        // class paths are sorted, normalized to forward slashes
        assert!(loaded.class_paths.iter().any(|p| p == "Foo.class"));
        assert!(loaded.class_paths.iter().any(|p| p == "com/example/Bar.class"));
    }

    /// Manifest write goes through tmp + rename — no .json.tmp leaks after.
    #[test]
    fn test_build_manifest_write_atomic_no_tmp_leak() {
        let tmp = tempfile::tempdir().unwrap();
        let fp_dir = tmp.path().join("fp");
        let output_dir = tmp.path().join("out");
        std::fs::create_dir_all(&output_dir).unwrap();

        BuildManifest::write(&fp_dir, &output_dir, &[]).unwrap();

        assert!(fp_dir.join("build-manifest.json").exists(), "manifest must exist");
        assert!(!fp_dir.join("build-manifest.json.tmp").exists(),
            ".tmp must not leak after rename");
    }

    /// is_consistent_with returns true when source set + recorded class files all match.
    #[test]
    fn test_build_manifest_consistent_when_all_match() {
        let tmp = tempfile::tempdir().unwrap();
        let fp_dir = tmp.path().join("fp");
        let output_dir = tmp.path().join("out");
        std::fs::create_dir_all(&output_dir).unwrap();
        std::fs::write(output_dir.join("Foo.class"),
            b"\xCA\xFE\xBA\xBE\x00\x00\x00\x42").unwrap();

        let sources = vec![tmp.path().join("Foo.java")];
        BuildManifest::write(&fp_dir, &output_dir, &sources).unwrap();
        let manifest = BuildManifest::load(&fp_dir).unwrap();

        assert!(manifest.is_consistent_with(&sources, &output_dir),
            "freshly written manifest must be consistent with the same source list");
    }

    /// is_consistent_with returns false when a recorded .class is gone (user rm'd out/).
    #[test]
    fn test_build_manifest_invalid_when_class_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let fp_dir = tmp.path().join("fp");
        let output_dir = tmp.path().join("out");
        std::fs::create_dir_all(&output_dir).unwrap();
        std::fs::write(output_dir.join("Foo.class"),
            b"\xCA\xFE\xBA\xBE\x00\x00\x00\x42").unwrap();

        let sources = vec![tmp.path().join("Foo.java")];
        BuildManifest::write(&fp_dir, &output_dir, &sources).unwrap();
        let manifest = BuildManifest::load(&fp_dir).unwrap();

        // Simulate user `rm -rf out/`: class file vanishes
        std::fs::remove_file(output_dir.join("Foo.class")).unwrap();

        assert!(!manifest.is_consistent_with(&sources, &output_dir),
            "manifest must invalidate when a recorded .class file is missing — \
             prevents 'cache says we built it but it's not on disk' silent failure");
    }

    /// is_consistent_with returns false when source list changes (user added/removed .java).
    #[test]
    fn test_build_manifest_invalid_when_sources_change() {
        let tmp = tempfile::tempdir().unwrap();
        let fp_dir = tmp.path().join("fp");
        let output_dir = tmp.path().join("out");
        std::fs::create_dir_all(&output_dir).unwrap();
        std::fs::write(output_dir.join("Foo.class"),
            b"\xCA\xFE\xBA\xBE\x00\x00\x00\x42").unwrap();

        let sources_v1 = vec![tmp.path().join("Foo.java")];
        BuildManifest::write(&fp_dir, &output_dir, &sources_v1).unwrap();
        let manifest = BuildManifest::load(&fp_dir).unwrap();

        // Simulate adding a new .java file
        let sources_v2 = vec![
            tmp.path().join("Foo.java"),
            tmp.path().join("Bar.java"),
        ];
        assert!(!manifest.is_consistent_with(&sources_v2, &output_dir),
            "manifest must invalidate when source set grew");

        // Simulate removing a source
        let sources_v3: Vec<PathBuf> = vec![];
        assert!(!manifest.is_consistent_with(&sources_v3, &output_dir),
            "manifest must invalidate when source set shrank");
    }

    /// Critical regression: manifest fast-path does NOT trip on resource-only
    /// output_dir (the standard-task-core 750B incident scenario at the
    /// fingerprint+manifest layer instead of the has_classes layer).
    ///
    /// Before manifest: a fresh build with stale fingerprints would silently
    /// skip javac. With manifest: no manifest exists yet → no fast-path
    /// shortcut → falls through to has_classes / cache restore / javac.
    #[test]
    fn test_build_manifest_absent_means_no_fastpath() {
        let tmp = tempfile::tempdir().unwrap();
        let fp_dir = tmp.path().join("fp");

        // No manifest written.
        assert!(BuildManifest::load(&fp_dir).is_none(),
            "absent manifest → load returns None → no fast-path → falls through");
    }

    // ========================================================================
    // 04-compiler.md「增量编译契约」路径×不变式矩阵的运行时实证。
    //
    // spec 强制规则(L148-158):矩阵每个 ✓ 必须 grep 到对应测试,且测试必须打到
    // 「会短路的那条路径本身」(non-helper)。本批覆盖:
    //   - manifest fast-path 路径:触发短路 + 2 个绕过场景(源集变化 / 记录的
    //     class 缺失)
    //   - up-to-date 路径:Stage 4 写 manifest 不变式(补 ADR-019 已有的 Stage
    //     1 prune orphan 测试)
    //   - 跨 build 不变式:up-to-date → fast-path 链路 e2e(spec L156 强制
    //     「跨 build 不变式必须 e2e 集成测试」)
    //
    // 未覆盖(留 follow-up):
    //   - cache restore 路径:需 `~/.ym/build-cache` 注入,目前 `build_cache_dir`
    //     直接读 `crate::home_dir()`,unit test 注入需引入 HOME env var 重定向
    //     + 测试串行化机制
    //   - 全量编译 / 增量编译 Stage 3 真编译路径:需 JDK 中的 javac 可用
    // ========================================================================

    /// 构造「前次构建成功后留下的状态」:写源、写假合法 .class、写匹配的
    /// fingerprint(source_hash + mtime 与源一致,确保下一次跑 `get_changed_files`
    /// 返回 `changed = []`)。**不写 manifest** —— 让调用方按需决定是否 seed
    /// manifest,以触发 fast-path 或落入下游 up-to-date 路径。
    fn seed_prior_build(
        src_dir: &Path,
        output_dir: &Path,
        fp_dir: &Path,
        rel_source: &str,
    ) -> PathBuf {
        let java = src_dir.join(rel_source);
        std::fs::create_dir_all(java.parent().unwrap()).unwrap();
        let stem = java.file_stem().unwrap().to_string_lossy().to_string();
        std::fs::write(&java, format!("package x; class {} {{}}", stem)).unwrap();

        let class_rel = std::path::Path::new(rel_source).with_extension("class");
        let class = output_dir.join(&class_rel);
        std::fs::create_dir_all(class.parent().unwrap()).unwrap();
        std::fs::write(&class, build_test_class(&[0xB1])).unwrap();

        let mut fp = Fingerprints::load(fp_dir);
        let hash = hash_file(&java).unwrap();
        let mtime = std::fs::metadata(&java).unwrap().modified().unwrap()
            .duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
        fp.update_source(&java, &hash, mtime);
        fp.save(fp_dir).unwrap();

        java
    }

    fn minimal_config(src_dir: &Path, output_dir: &Path) -> super::super::CompileConfig {
        super::super::CompileConfig {
            source_dirs: vec![src_dir.to_path_buf()],
            resource_dirs: vec![],
            output_dir: output_dir.to_path_buf(),
            classpath: vec![],
            java_version: Some("21".to_string()),
            encoding: None,
            annotation_processors: vec![],
            lint: vec![],
            extra_args: vec![],
        }
    }

    /// 路径 = manifest fast-path / 不变式 = 触发并 early-return UpToDate
    ///
    /// 前次构建写了 manifest + fingerprint + .class 都和现源对齐 → 本次入口直
    /// 接走 fast-path(L497-508),javac 永不调到(pool=None 时若真进入编译
    /// 路径会因 javac 不可用 panic;成功返回 UpToDate 即等价于路径短路)。
    /// 强不变式:fast-path early-return 在 `prune_orphan_classes` 与
    /// `BuildManifest::write` 之前 —— 故 manifest 字节不变。
    #[test]
    fn test_manifest_fastpath_short_circuits_when_prior_manifest_consistent() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = tmp.path().join("cache");
        let src_dir = tmp.path().join("src");
        let output_dir = tmp.path().join("out");
        let fp_dir = fingerprint_dir_for(&cache, &output_dir);

        let java = seed_prior_build(&src_dir, &output_dir, &fp_dir, "com/example/A.java");
        BuildManifest::write(&fp_dir, &output_dir, &[java]).unwrap();

        let manifest_before = std::fs::read(fp_dir.join(BUILD_MANIFEST_FILE)).unwrap();
        let config = minimal_config(&src_dir, &output_dir);

        let result = incremental_compile(&config, &cache, None).unwrap();

        assert!(result.success);
        assert_eq!(
            result.outcome,
            super::super::CompileOutcome::UpToDate,
            "fast-path 触发时必须 early-return UpToDate"
        );
        let manifest_after = std::fs::read(fp_dir.join(BUILD_MANIFEST_FILE)).unwrap();
        assert_eq!(
            manifest_before, manifest_after,
            "fast-path early-return 不应触达 Stage 4 manifest write — 字节必须不变"
        );
    }

    /// 路径 = manifest fast-path / 不变式 = 源集变化时绕过 fast-path
    ///
    /// `is_consistent_with` 第一关:`curr.len() != prior.len()` 即返回 false
    /// (L947-949)。manifest 记录 [A, B] 而现源仅 A → fast-path 绕过 → 落入
    /// `prune_orphan_classes`(Stage 1)清掉 B.class orphan → 进入 has_classes
    /// 分支 → `changed=[]` + `missing=[]` → 走 up-to-date(L572-603)→ Stage 4
    /// 复写新 manifest 仅含 A。
    ///
    /// 验证两条不变式:① fast-path 真的绕过(manifest 被复写为单源)②
    /// PrePrune 在下游路径前真的跑了(orphan B.class 被清掉)。
    #[test]
    fn test_manifest_fastpath_bypassed_when_source_set_shrinks() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = tmp.path().join("cache");
        let src_dir = tmp.path().join("src");
        let output_dir = tmp.path().join("out");
        let fp_dir = fingerprint_dir_for(&cache, &output_dir);

        // 现源:仅 A.java + A.class
        let a = seed_prior_build(&src_dir, &output_dir, &fp_dir, "com/example/A.java");

        // 前次构建留下的 B.java 已删除,但 B.class 还在 output_dir(orphan),
        // 同时 fingerprints 内还有 B 的旧条目(prior build 记录过 B)。
        let b_phantom = src_dir.join("com/example/B.java");
        let b_class = output_dir.join("com/example/B.class");
        std::fs::write(&b_class, build_test_class(&[0xB1])).unwrap();
        let mut fp = Fingerprints::load(&fp_dir);
        fp.update_source(&b_phantom, "stale-b-hash", 1);
        fp.save(&fp_dir).unwrap();

        // manifest 记录的源集是 [A, B],但 B.java 物理上不存在
        BuildManifest::write(&fp_dir, &output_dir, &[a, b_phantom.clone()]).unwrap();
        assert!(!b_phantom.exists());

        let config = minimal_config(&src_dir, &output_dir);
        let result = incremental_compile(&config, &cache, None).unwrap();

        assert!(result.success);
        assert_eq!(result.outcome, super::super::CompileOutcome::UpToDate);

        let manifest = BuildManifest::load(&fp_dir).unwrap();
        assert_eq!(
            manifest.source_paths.len(),
            1,
            "fast-path 绕过后,下游 up-to-date 路径必须复写 manifest 反映现源集"
        );
        assert!(manifest.source_paths[0].ends_with("A.java"));

        assert!(
            !b_class.exists(),
            "PrePrune (ADR-019) 必须在每条非 fast-path 路径上清掉 B.class orphan"
        );
    }

    /// 路径 = manifest fast-path / 不变式 = 记录的 .class 缺失时绕过 fast-path
    ///
    /// `is_consistent_with` 第二关:遍历 `class_paths`,任一文件不存在即返回
    /// false(L956-961)。模拟用户 `rm -rf out/` 但 fingerprint 没清(老版本 ym
    /// 或测试场景),fast-path 必须绕过避免「manifest 说我编译过但 .class 已不
    /// 在」的静默失败。
    ///
    /// 绕过后:`has_classes=false` → 进 cache-restore 分支 → 测试环境 cache miss
    /// → 必然进入 `compile_files` 调 javac。pool=None + 测试环境无 javac 时
    /// `compile_files` 会因 `Command::new("javac")` spawn 失败抛错,这也算 fast-
    /// path 被绕过的反向实证(任何非 UpToDate / Cached 的结局都说明走到了编
    /// 译路径)。
    #[test]
    fn test_manifest_fastpath_bypassed_when_recorded_class_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = tmp.path().join("cache");
        let src_dir = tmp.path().join("src");
        let output_dir = tmp.path().join("out");
        let fp_dir = fingerprint_dir_for(&cache, &output_dir);

        let java = seed_prior_build(&src_dir, &output_dir, &fp_dir, "com/example/A.java");
        BuildManifest::write(&fp_dir, &output_dir, &[java]).unwrap();

        // 用户外部清掉 A.class —— manifest.class_paths 与磁盘不一致
        let a_class = output_dir.join("com/example/A.class");
        std::fs::remove_file(&a_class).unwrap();

        let config = minimal_config(&src_dir, &output_dir);
        let result = incremental_compile(&config, &cache, None);

        match result {
            Ok(r) => {
                assert_ne!(
                    r.outcome,
                    super::super::CompileOutcome::UpToDate,
                    "manifest 记录的 class 缺失时 fast-path 必须绕过,不能返回 UpToDate"
                );
            }
            Err(_) => {
                // 测试环境无 javac → compile_files 抛错,同样证明走到了编译路径
            }
        }
    }

    /// 路径 = up-to-date / 不变式 = Stage 4 写 manifest 供下次 fast-path
    ///
    /// 前次构建留下 fingerprint + .class 但没写 manifest(模拟 ADR-014 之前的
    /// 老版本 ym 升级场景)。本次跑:fast-path load manifest 返回 None → 绕过
    /// → has_classes=true + changed=[] + missing=[] → 进入 up-to-date(L589-
    /// 603)。Stage 4 必须落盘 manifest(L594-596),否则下次跑还是 up-to-date,
    /// fast-path 永远命不中。
    #[test]
    fn test_up_to_date_path_writes_manifest_for_next_fastpath() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = tmp.path().join("cache");
        let src_dir = tmp.path().join("src");
        let output_dir = tmp.path().join("out");
        let fp_dir = fingerprint_dir_for(&cache, &output_dir);

        seed_prior_build(&src_dir, &output_dir, &fp_dir, "com/example/A.java");
        assert!(
            !fp_dir.join(BUILD_MANIFEST_FILE).exists(),
            "测试前提:fp_dir 中不能有 manifest,否则第一次跑就走 fast-path 了"
        );

        let config = minimal_config(&src_dir, &output_dir);
        let result = incremental_compile(&config, &cache, None).unwrap();

        assert!(result.success);
        assert_eq!(
            result.outcome,
            super::super::CompileOutcome::UpToDate,
            "源 + class + fingerprint 全一致 → 应走 up-to-date 路径"
        );
        assert!(
            fp_dir.join(BUILD_MANIFEST_FILE).exists(),
            "up-to-date 路径 Stage 4 必须落盘 manifest,否则下次跑无法走 fast-path"
        );
    }

    /// 跨 build e2e:up-to-date → fast-path 链路幂等
    ///
    /// spec 04-compiler.md L156 强制「manifest 落盘 + 下次 fast-path 命中」这
    /// 类跨多次 build 才能观察的不变式必须 e2e 集成测试。
    ///
    /// Round 1:无 manifest → 走 up-to-date → Stage 4 写 manifest
    /// Round 2:有 manifest → 走 fast-path → early-return,*不复写* manifest
    ///
    /// 两个 outcome 都是 UpToDate,但走的是不同路径。通过「Round 2 后 manifest
    /// 字节与 Round 1 后完全一致」反推:Round 2 命中的是 fast-path(若 Round 2
    /// 又走 up-to-date 会调 `BuildManifest::write`,后者带新的 `completed_at`
    /// 时间戳,字节必变)。
    #[test]
    fn test_up_to_date_then_fastpath_chain_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = tmp.path().join("cache");
        let src_dir = tmp.path().join("src");
        let output_dir = tmp.path().join("out");
        let fp_dir = fingerprint_dir_for(&cache, &output_dir);

        seed_prior_build(&src_dir, &output_dir, &fp_dir, "com/example/A.java");
        let config = minimal_config(&src_dir, &output_dir);

        // Round 1 — up-to-date 路径
        let r1 = incremental_compile(&config, &cache, None).unwrap();
        assert_eq!(r1.outcome, super::super::CompileOutcome::UpToDate);
        let manifest_after_r1 = std::fs::read(fp_dir.join(BUILD_MANIFEST_FILE)).unwrap();

        // 确保两次跑之间 `completed_at` 真有可能不同 —— manifest 用秒级时间戳,
        // 同一秒内复写字节仍可能相同,这会让本测试无法区分 fast-path / up-to-
        // date。睡 1.1 秒强制跨秒边界,让 up-to-date(若误命中)会写出不同字节。
        std::thread::sleep(std::time::Duration::from_millis(1100));

        // Round 2 — 应走 fast-path
        let r2 = incremental_compile(&config, &cache, None).unwrap();
        assert_eq!(r2.outcome, super::super::CompileOutcome::UpToDate);
        let manifest_after_r2 = std::fs::read(fp_dir.join(BUILD_MANIFEST_FILE)).unwrap();

        assert_eq!(
            manifest_after_r1, manifest_after_r2,
            "Round 2 必须走 fast-path(不复写 manifest)而非 up-to-date(会复写 \
             带新时间戳的 manifest)。字节相等是 fast-path 命中的反向实证"
        );
    }

    /// 路径 = up-to-date / 不变式 = Stage 1 PrePrune 必须在 early-return 之前跑
    ///
    /// ADR-019 主回归测试,作为路径矩阵中 up-to-date 行 Stage 1 列 ✓ 的实证锚
    /// 点。具体测试在文件上方的
    /// `test_incremental_compile_prunes_orphan_class_when_source_deleted`,本注释
    /// 仅为矩阵 grep 时能定位到。
    #[test]
    fn test_up_to_date_path_pre_prune_runs_before_early_return_anchor() {
        // 真正的实证测试是 `test_incremental_compile_prunes_orphan_class_when_source_deleted`
        // (ADR-019 主回归)。本测试只是确认锚点存在 —— 如果未来有人误删上面那个
        // 测试,本测试名 grep 能让 reviewer 立刻发现 ADR-019 / 路径矩阵中
        // up-to-date × Stage 1 PrePrune 这一格丢了实证。
        //
        // 这是 spec L154 「测试命名应反映路径+不变式两个维度,便于 grep 倒查」
        // 的具体实现策略。
        let _ = test_incremental_compile_prunes_orphan_class_when_source_deleted;
    }
}
