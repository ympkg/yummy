pub mod ecj_service;
pub mod incremental;
pub mod javac;

use std::path::PathBuf;

pub struct CompileResult {
    pub success: bool,
    pub files_compiled: usize,
    pub errors: String,
}

pub struct CompileConfig {
    pub source_dirs: Vec<PathBuf>,
    pub output_dir: PathBuf,
    pub classpath: Vec<PathBuf>,
    pub java_version: Option<String>,
    pub encoding: Option<String>,
    pub annotation_processors: Vec<PathBuf>,
    pub lint: Vec<String>,
    pub extra_args: Vec<String>,
}

/// Colorize javac/ecj compiler error output for better readability
pub fn colorize_errors(raw: &str) -> String {
    use console::style;
    let mut result = String::new();
    for line in raw.lines() {
        if line.contains(": error:") {
            // file.java:10: error: message
            if let Some(idx) = line.find(": error:") {
                let location = &line[..idx];
                let message = &line[idx + 8..];
                result.push_str(&format!(
                    "{}: {}{}\n",
                    style(location).dim(),
                    style("error:").red().bold(),
                    message
                ));
                continue;
            }
        } else if line.contains(": warning:") {
            if let Some(idx) = line.find(": warning:") {
                let location = &line[..idx];
                let message = &line[idx + 10..];
                result.push_str(&format!(
                    "{}: {}{}\n",
                    style(location).dim(),
                    style("warning:").yellow(),
                    message
                ));
                continue;
            }
        } else if line.trim() == "^" {
            result.push_str(&format!("{}\n", style(line).green().bold()));
            continue;
        } else if line.starts_with("  symbol:") || line.starts_with("  location:") {
            result.push_str(&format!("{}\n", style(line).dim()));
            continue;
        }
        result.push_str(line);
        result.push('\n');
    }
    result
}

/// Compiler engine selection
#[derive(Debug, Clone, PartialEq)]
pub enum CompilerEngine {
    Javac,
    Ecj,
}

impl CompilerEngine {
    pub fn from_config(engine: Option<&str>) -> Self {
        match engine {
            Some("ecj") | Some("ECJ") => CompilerEngine::Ecj,
            _ => CompilerEngine::Javac,
        }
    }
}
