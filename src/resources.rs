use anyhow::Result;
use std::path::Path;

/// Copy resource files (non-.java) from source directories to output directory.
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
) -> Result<usize> {
    if !src_dir.exists() {
        return Ok(0);
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

    let mut count = 0;

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
            count += 1;
        }
    }

    Ok(count)
}
