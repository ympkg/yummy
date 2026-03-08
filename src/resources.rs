use anyhow::Result;
use std::path::Path;

/// Copy resource files (non-.java) from source directories to output directory.
/// If `custom_extensions` is provided, uses that list instead of the default.
/// Extensions can be with or without leading dot (e.g., ".xml" or "xml").
pub fn copy_resources_with_extensions(
    src_dir: &Path,
    output_dir: &Path,
    custom_extensions: Option<&[String]>,
) -> Result<usize> {
    copy_resources_inner(src_dir, output_dir, custom_extensions)
}


fn copy_resources_inner(
    src_dir: &Path,
    output_dir: &Path,
    custom_extensions: Option<&[String]>,
) -> Result<usize> {
    if !src_dir.exists() {
        return Ok(0);
    }

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

        // Check if this is a resource file we should copy
        let is_resource = match custom_extensions {
            Some(exts) => {
                let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
                exts.iter().any(|e| {
                    let e = e.strip_prefix('.').unwrap_or(e);
                    e == ext
                })
            }
            None => is_resource_file(path),
        };
        if !is_resource {
            continue;
        }

        let rel = path.strip_prefix(src_dir)?;
        let dest = output_dir.join(rel);

        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }

        // Only copy if source is newer than dest
        let should_copy = if dest.exists() {
            let src_mtime = std::fs::metadata(path)?.modified()?;
            let dst_mtime = std::fs::metadata(&dest)?.modified()?;
            src_mtime > dst_mtime
        } else {
            true
        };

        if should_copy {
            std::fs::copy(path, &dest)?;
            count += 1;
        }
    }

    Ok(count)
}

fn is_resource_file(path: &Path) -> bool {
    let ext = match path.extension().and_then(|e| e.to_str()) {
        Some(e) => e,
        None => return false,
    };

    matches!(
        ext,
        "properties"
            | "xml"
            | "yml"
            | "yaml"
            | "json"
            | "txt"
            | "csv"
            | "sql"
            | "fxml"
            | "css"
            | "html"
            | "conf"
            | "cfg"
            | "ini"
            | "toml"
            | "graphql"
            | "graphqls"
            | "proto"
            | "ftl"
            | "mustache"
    )
}
