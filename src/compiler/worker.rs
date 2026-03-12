//! Compiler worker pool — manages long-running JVM processes that compile Java
//! via javax.tools.JavaCompiler, eliminating per-module JVM startup overhead.

use anyhow::{bail, Context, Result};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::{Condvar, Mutex};

use super::{CompileConfig, CompileOutcome, CompileResult};

/// Embedded Java worker source — compiled on first use to ~/.ym/worker/
const WORKER_JAVA_SOURCE: &str = include_str!("worker_src/YmCompilerWorker.java");

/// A single JVM worker process with piped stdin/stdout.
struct Worker {
    child: Child,
    stdin: BufWriter<ChildStdin>,
    stdout: BufReader<ChildStdout>,
}

impl Worker {
    fn start(worker_dir: &Path) -> Result<Self> {
        let mut child = Command::new("java")
            .arg("-cp")
            .arg(worker_dir)
            .arg("YmCompilerWorker")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .context("Failed to start compiler worker JVM")?;

        let stdin = BufWriter::new(child.stdin.take().unwrap());
        let stdout = BufReader::new(child.stdout.take().unwrap());

        Ok(Worker {
            child,
            stdin,
            stdout,
        })
    }

    /// Send a compile request and read the response.
    fn compile(&mut self, config: &CompileConfig, files: &[PathBuf]) -> Result<CompileResult> {
        let request = build_request_json(config, files);
        writeln!(self.stdin, "{}", request)?;
        self.stdin.flush()?;

        let mut response_line = String::new();
        let n = self.stdout.read_line(&mut response_line)?;
        if n == 0 {
            bail!("Worker process closed stdout (crashed?)");
        }

        parse_response(&response_line, files.len())
    }

    /// Health check: send PING, expect PONG.
    fn ping(&mut self) -> Result<()> {
        writeln!(self.stdin, "PING")?;
        self.stdin.flush()?;

        let mut line = String::new();
        let n = self.stdout.read_line(&mut line)?;
        if n == 0 {
            bail!("Worker did not respond to PING (process exited)");
        }
        if line.trim() != "PONG" {
            bail!("Worker returned unexpected response: {}", line.trim());
        }
        Ok(())
    }

    fn kill(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.kill();
    }
}

/// Pool of compiler worker JVM processes for parallel workspace compilation.
pub struct CompilerPool {
    workers: Mutex<Vec<Worker>>,
    available: Condvar,
    worker_dir: PathBuf,
}

impl CompilerPool {
    /// Create a new compiler pool. Workers are started eagerly and health-checked.
    pub fn new(pool_size: usize) -> Result<Self> {
        let worker_dir = ensure_worker_class()?;
        let mut workers: Vec<Worker> = Vec::with_capacity(pool_size);

        for i in 0..pool_size {
            match Worker::start(&worker_dir) {
                Ok(mut w) => {
                    // Health check
                    if let Err(e) = w.ping() {
                        // First worker failed — likely no JDK or javax.tools unavailable
                        for mut prev in workers {
                            prev.kill();
                        }
                        bail!("Worker {} health check failed: {}", i, e);
                    }
                    workers.push(w);
                }
                Err(e) => {
                    for mut prev in workers {
                        prev.kill();
                    }
                    return Err(e);
                }
            }
        }

        Ok(CompilerPool {
            workers: Mutex::new(workers),
            available: Condvar::new(),
            worker_dir,
        })
    }

    /// Compile files using a worker from the pool.
    /// Falls back to direct javac invocation if the worker fails.
    pub fn compile(&self, config: &CompileConfig, files: &[PathBuf]) -> Result<CompileResult> {
        let mut worker = self.acquire();

        match worker.compile(config, files) {
            Ok(result) => {
                self.release(worker);
                Ok(result)
            }
            Err(e) => {
                // Worker failed — try to restart
                worker.kill();
                match Worker::start(&self.worker_dir) {
                    Ok(new_worker) => self.release(new_worker),
                    Err(_) => {
                        // Can't restart, pool capacity shrinks by 1
                        self.available.notify_one();
                    }
                }
                // Fall back to direct javac for this request
                eprintln!(
                    "  {} worker compile failed ({}), falling back to javac",
                    console::style("!").yellow(),
                    e
                );
                super::incremental::compile_files_direct(config, files)
            }
        }
    }

    fn acquire(&self) -> Worker {
        let mut workers = self.workers.lock().unwrap();
        loop {
            if let Some(w) = workers.pop() {
                return w;
            }
            workers = self.available.wait(workers).unwrap();
        }
    }

    fn release(&self, worker: Worker) {
        let mut workers = self.workers.lock().unwrap();
        workers.push(worker);
        self.available.notify_one();
    }
}

impl Drop for CompilerPool {
    fn drop(&mut self) {
        if let Ok(mut workers) = self.workers.lock() {
            for w in workers.iter_mut() {
                w.kill();
            }
        }
    }
}

// --- Worker class management ---

/// Ensure the Java worker class is compiled and up-to-date at ~/.ym/worker/.
fn ensure_worker_class() -> Result<PathBuf> {
    let dir = crate::home_dir().join(".ym").join("worker");
    let source_path = dir.join("YmCompilerWorker.java");
    let class_path = dir.join("YmCompilerWorker.class");
    let hash_path = dir.join(".source-hash");

    let current_hash = super::incremental::hash_bytes(WORKER_JAVA_SOURCE.as_bytes());

    // Check if recompilation is needed
    let needs_compile = if class_path.exists() {
        match std::fs::read_to_string(&hash_path) {
            Ok(stored) => stored.trim() != current_hash,
            Err(_) => true,
        }
    } else {
        true
    };

    if needs_compile {
        std::fs::create_dir_all(&dir)?;
        std::fs::write(&source_path, WORKER_JAVA_SOURCE)?;

        let output = Command::new("javac")
            .arg("-d")
            .arg(&dir)
            .arg(&source_path)
            .output()
            .context("Failed to compile compiler worker class")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!("Failed to compile compiler worker: {}", stderr);
        }

        std::fs::write(&hash_path, &current_hash)?;
    }

    Ok(dir)
}

// --- JSON protocol ---

fn build_request_json(config: &CompileConfig, files: &[PathBuf]) -> String {
    let sep = if cfg!(windows) { ";" } else { ":" };

    let classpath = config
        .classpath
        .iter()
        .map(|p| p.to_string_lossy().to_string())
        .collect::<Vec<_>>()
        .join(sep);

    let ap = config
        .annotation_processors
        .iter()
        .map(|p| p.to_string_lossy().to_string())
        .collect::<Vec<_>>()
        .join(sep);

    let files_json: Vec<String> = files
        .iter()
        .map(|f| format!("\"{}\"", json_escape(&f.to_string_lossy())))
        .collect();

    // Combine lint options and extra args
    let mut all_args: Vec<String> = config.lint.iter().map(|l| format!("-Xlint:{}", l)).collect();
    all_args.extend(config.extra_args.iter().cloned());

    let args_json: Vec<String> = all_args
        .iter()
        .map(|a| format!("\"{}\"", json_escape(a)))
        .collect();

    format!(
        "{{\"outputDir\":\"{}\",\"release\":\"{}\",\"encoding\":\"{}\",\"classpath\":\"{}\",\"processorPath\":\"{}\",\"args\":[{}],\"files\":[{}]}}",
        json_escape(&config.output_dir.to_string_lossy()),
        json_escape(config.java_version.as_deref().unwrap_or("")),
        json_escape(config.encoding.as_deref().unwrap_or("")),
        json_escape(&classpath),
        json_escape(&ap),
        args_json.join(","),
        files_json.join(","),
    )
}

fn json_escape(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => result.push_str("\\\""),
            '\\' => result.push_str("\\\\"),
            '\n' => result.push_str("\\n"),
            '\r' => result.push_str("\\r"),
            '\t' => result.push_str("\\t"),
            _ => result.push(c),
        }
    }
    result
}

fn parse_response(line: &str, expected_files: usize) -> Result<CompileResult> {
    let line = line.trim();
    if line.is_empty() {
        bail!("Empty response from compiler worker");
    }

    let success = line.contains("\"success\":true");
    let diagnostics = extract_json_string(line, "diagnostics").unwrap_or_default();
    let files_compiled = extract_json_number(line, "filesCompiled").unwrap_or(expected_files);

    Ok(CompileResult {
        success,
        outcome: CompileOutcome::Compiled(files_compiled),
        errors: diagnostics,
    })
}

fn extract_json_string(json: &str, key: &str) -> Option<String> {
    let pattern = format!("\"{}\":\"", key);
    let start = json.find(&pattern)? + pattern.len();
    let mut end = start;
    let bytes = json.as_bytes();
    while end < bytes.len() {
        if bytes[end] == b'\\' {
            end += 2;
            continue;
        }
        if bytes[end] == b'"' {
            break;
        }
        end += 1;
    }
    Some(json_unescape(&json[start..end]))
}

fn extract_json_number(json: &str, key: &str) -> Option<usize> {
    let pattern = format!("\"{}\":", key);
    let start = json.find(&pattern)? + pattern.len();
    let rest = &json[start..];
    let end = rest
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(rest.len());
    rest[..end].parse().ok()
}

fn json_unescape(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('"') => result.push('"'),
                Some('\\') => result.push('\\'),
                Some('n') => result.push('\n'),
                Some('r') => result.push('\r'),
                Some('t') => result.push('\t'),
                Some(c) => {
                    result.push('\\');
                    result.push(c);
                }
                None => result.push('\\'),
            }
        } else {
            result.push(c);
        }
    }
    result
}
