use anyhow::{bail, Context, Result};
use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// Client for the ECJ (Eclipse Compiler for Java) compile service.
/// The service runs as a long-lived JVM process, accepting compile requests
/// via a TCP socket with JSON-RPC protocol.
#[allow(dead_code)]
pub struct EcjService {
    process: Child,
    port: u16,
}

/// Request sent to the ECJ service
#[allow(dead_code)]
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct CompileRequest {
    method: String,
    id: u32,
    params: CompileParams,
}

#[allow(dead_code)]
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct CompileParams {
    changed_files: Vec<String>,
    source_dirs: Vec<String>,
    classpath: Vec<String>,
    output_dir: String,
    source_version: Option<String>,
    encoding: Option<String>,
}

/// Response from the ECJ service
#[allow(dead_code)]
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompileResponse {
    pub success: bool,
    pub compiled_files: usize,
    pub errors: Vec<String>,
    pub warnings: Vec<String>,
    pub time_ms: u64,
}

#[allow(dead_code)]
impl EcjService {
    /// Start the ECJ compile service.
    /// Looks for ym-ecj-service.jar in standard locations.
    pub fn start(ecj_jar: &Path) -> Result<Self> {
        // Find a free port
        let port = find_free_port()?;

        let process = Command::new("java")
            .arg("-jar")
            .arg(ecj_jar)
            .arg("--port")
            .arg(port.to_string())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("Failed to start ECJ service")?;

        let service = EcjService { process, port };

        // Wait for service to be ready
        service.wait_ready(Duration::from_secs(10))?;

        Ok(service)
    }

    fn wait_ready(&self, timeout: Duration) -> Result<()> {
        let start = Instant::now();
        while start.elapsed() < timeout {
            if TcpStream::connect(("127.0.0.1", self.port)).is_ok() {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        bail!("ECJ service failed to start within {:?}", timeout)
    }

    /// Send a compile request to the ECJ service.
    pub fn compile(
        &self,
        changed_files: &[PathBuf],
        source_dirs: &[PathBuf],
        classpath: &[PathBuf],
        output_dir: &Path,
        source_version: Option<&str>,
        encoding: Option<&str>,
    ) -> Result<CompileResponse> {
        let request = CompileRequest {
            method: "compile".to_string(),
            id: 1,
            params: CompileParams {
                changed_files: changed_files.iter().map(|p| p.to_string_lossy().to_string()).collect(),
                source_dirs: source_dirs.iter().map(|p| p.to_string_lossy().to_string()).collect(),
                classpath: classpath.iter().map(|p| p.to_string_lossy().to_string()).collect(),
                output_dir: output_dir.to_string_lossy().to_string(),
                source_version: source_version.map(|s| s.to_string()),
                encoding: encoding.map(|s| s.to_string()),
            },
        };

        let json = serde_json::to_string(&request)?;
        let mut stream = TcpStream::connect(("127.0.0.1", self.port))?;
        stream.set_read_timeout(Some(Duration::from_secs(60)))?;

        writeln!(stream, "{}", json)?;
        stream.flush()?;

        let mut reader = BufReader::new(&stream);
        let mut response_line = String::new();
        reader.read_line(&mut response_line)?;

        let response: CompileResponse = serde_json::from_str(&response_line)?;
        Ok(response)
    }

    /// Shutdown the ECJ service.
    pub fn shutdown(&mut self) -> Result<()> {
        // Send shutdown command
        if let Ok(mut stream) = TcpStream::connect(("127.0.0.1", self.port)) {
            let _ = writeln!(stream, r#"{{"method":"shutdown","id":0,"params":{{}}}}"#);
        }
        let _ = self.process.kill();
        let _ = self.process.wait();
        Ok(())
    }
}

impl Drop for EcjService {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

#[allow(dead_code)]
fn find_free_port() -> Result<u16> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    Ok(listener.local_addr()?.port())
}

/// Find the ECJ service JAR in standard locations.
pub fn find_ecj_jar() -> Option<PathBuf> {
    let candidates = [
        // Next to the ym binary
        std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|d| d.join("ym-ecj-service.jar"))),
        // In the project's .ym directory
        std::env::current_dir()
            .ok()
            .map(|d| d.join(".ym").join("ym-ecj-service.jar")),
        // In user's home
        dirs_path().map(|d| d.join("ym-ecj-service.jar")),
    ];

    for candidate in candidates.into_iter().flatten() {
        if candidate.exists() {
            return Some(candidate);
        }
    }
    None
}

fn dirs_path() -> Option<PathBuf> {
    std::env::var("HOME")
        .ok()
        .map(|h| PathBuf::from(h).join(".ym"))
}
