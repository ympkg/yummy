use anyhow::Result;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// Filename of the per-output-dir resource manifest, stored alongside the
/// incremental-compiler fingerprints (see `incremental::fingerprint_dir_for`).
const RESOURCE_MANIFEST_FILE: &str = "resource-manifest.json";

/// Copy resource files (non-.java) from one source directory into the output
/// directory, returning the relative path of every file that belongs in the
/// output — i.e. every file that matched the copy criteria, whether or not the
/// mtime check actually re-copied it this run.
///
/// Mode selection:
/// - If `custom_extensions` is provided → whitelist mode (only copy matching extensions)
/// - Otherwise → copy all non-`.java` files, applying `exclude_patterns` if provided
///
/// `exclude_patterns` are regexes matched against the file's relative path (from `src_dir`).
pub fn copy_resources_with_extensions(
    src_dir: &Path,
    output_dir: &Path,
    custom_extensions: Option<&[String]>,
    exclude_patterns: Option<&[String]>,
) -> Result<Vec<PathBuf>> {
    let mut copied: Vec<PathBuf> = Vec::new();

    if !src_dir.exists() {
        return Ok(copied);
    }

    // Pre-compile exclude regexes (only used when custom_extensions is None)
    let exclude_regexes = if custom_extensions.is_none() {
        match exclude_patterns {
            Some(patterns) => {
                let mut regexes = Vec::with_capacity(patterns.len());
                for p in patterns {
                    regexes.push(regex::Regex::new(p)?);
                }
                Some(regexes)
            }
            None => None,
        }
    } else {
        None
    };

    for entry in walkdir::WalkDir::new(src_dir) {
        let entry = entry?;
        if !entry.file_type().is_file() {
            continue;
        }

        let path = entry.path();

        // Skip Java source files
        if path.extension().and_then(|e| e.to_str()) == Some("java") {
            continue;
        }

        let should_copy = match custom_extensions {
            Some(exts) => {
                // Whitelist mode: only copy files matching given extensions
                let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
                exts.iter().any(|e| {
                    let e = e.strip_prefix('.').unwrap_or(e);
                    e == ext
                })
            }
            None => {
                // Default mode: copy all non-.java, check exclude patterns
                if let Some(ref regexes) = exclude_regexes {
                    let rel = path.strip_prefix(src_dir).unwrap_or(path);
                    let rel_str = rel.to_string_lossy();
                    !regexes.iter().any(|re| re.is_match(&rel_str))
                } else {
                    true
                }
            }
        };

        if !should_copy {
            continue;
        }

        let rel = path.strip_prefix(src_dir)?;
        let dest = output_dir.join(rel);

        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }

        // Only copy if source is newer than dest
        let needs_copy = if dest.exists() {
            let src_mtime = std::fs::metadata(path)?.modified()?;
            let dst_mtime = std::fs::metadata(&dest)?.modified()?;
            src_mtime > dst_mtime
        } else {
            true
        };

        if needs_copy {
            std::fs::copy(path, &dest)?;
        }

        copied.push(rel.to_path_buf());
    }

    Ok(copied)
}

/// Sync resources from every root in `resource_roots` into `output_dir`.
///
/// Beyond copying the current resources, this removes files that a *previous*
/// sync copied but whose source has since been deleted or renamed. That closes
/// the ghost-file gap: `copy_resources_with_extensions` only ever *adds* files,
/// so a resource removed from `src` lingered in `output_dir`, got packaged into
/// the jar, and was snapshotted into the build cache indefinitely.
///
/// Pruning is scoped to this module's resource manifest — only files a previous
/// `sync_resources` recorded are ever removed. Compiler `.class` output and
/// annotation-processor-generated resources (e.g.
/// `META-INF/spring-configuration-metadata.json`), which have no source-tree
/// counterpart, are never touched.
pub fn sync_resources(
    resource_roots: &[PathBuf],
    output_dir: &Path,
    manifest_dir: &Path,
    custom_extensions: Option<&[String]>,
    exclude_patterns: Option<&[String]>,
) -> Result<()> {
    // Copy current resources from every root; collect the union of relative paths.
    let mut current: BTreeSet<String> = BTreeSet::new();
    for root in resource_roots {
        for rel in copy_resources_with_extensions(root, output_dir, custom_extensions, exclude_patterns)? {
            current.insert(rel.to_string_lossy().to_string());
        }
    }

    // Remove resources a previous sync copied that are no longer in any root.
    let manifest_path = manifest_dir.join(RESOURCE_MANIFEST_FILE);
    for rel in load_resource_manifest(&manifest_path) {
        if !current.contains(&rel) {
            let _ = std::fs::remove_file(output_dir.join(&rel));
        }
    }

    // Record the current set so the next sync can diff against it.
    write_resource_manifest(&manifest_path, &current)?;
    Ok(())
}

/// Load the previously synced resource paths. A missing or unreadable manifest
/// yields an empty list — the first sync simply has nothing to prune.
fn load_resource_manifest(path: &Path) -> Vec<String> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str::<Vec<String>>(&s).ok())
        .unwrap_or_default()
}

fn write_resource_manifest(path: &Path, entries: &BTreeSet<String>) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let list: Vec<&String> = entries.iter().collect();
    std::fs::write(path, serde_json::to_string(&list)?)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sync_resources_prunes_orphan_keeps_live_and_untracked() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let res_dir = root.join("src/main/resources");
        let out = root.join("out/classes");
        let manifest_dir = root.join("cache/fp");
        std::fs::create_dir_all(res_dir.join("graphql")).unwrap();

        // First sync: Issue + StandardIssue both present in src.
        std::fs::write(res_dir.join("graphql/Issue.graphqls"), "type Issue").unwrap();
        std::fs::write(res_dir.join("graphql/StandardIssue.graphqls"), "type StandardIssue").unwrap();
        sync_resources(&[res_dir.clone()], &out, &manifest_dir, None, None).unwrap();
        assert!(out.join("graphql/Issue.graphqls").exists());
        assert!(out.join("graphql/StandardIssue.graphqls").exists());

        // StandardIssue removed from src — the next sync must prune the ghost.
        std::fs::remove_file(res_dir.join("graphql/StandardIssue.graphqls")).unwrap();
        sync_resources(&[res_dir.clone()], &out, &manifest_dir, None, None).unwrap();
        assert!(out.join("graphql/Issue.graphqls").exists(), "live resource kept");
        assert!(
            !out.join("graphql/StandardIssue.graphqls").exists(),
            "orphan resource must be pruned"
        );

        // A file this sync never copied (e.g. annotation-processor output)
        // must survive — pruning is scoped to the manifest.
        std::fs::write(out.join("generated.json"), "{}").unwrap();
        sync_resources(&[res_dir.clone()], &out, &manifest_dir, None, None).unwrap();
        assert!(
            out.join("generated.json").exists(),
            "untracked (AP-generated) file must not be pruned"
        );
    }
}
