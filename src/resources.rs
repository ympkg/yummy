use anyhow::Result;
use std::path::Path;

/// Copy resource files (non-.java) from source directories to output directory.
/// This includes .properties, .xml, .yml, .yaml, .json, .txt, .fxml, etc.
pub fn copy_resources(src_dir: &Path, output_dir: &Path) -> Result<usize> {
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
        if !is_resource_file(path) {
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
