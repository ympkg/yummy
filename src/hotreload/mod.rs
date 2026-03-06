use anyhow::{Context, Result};
use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Hot reload strategy used
#[derive(Debug)]
pub enum ReloadStrategy {
    HotSwap,     // L1: method body changes only
    ClassLoader, // L2: structural changes
    Restart,     // L3: ClassLoader failed
}

impl std::fmt::Display for ReloadStrategy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ReloadStrategy::HotSwap => write!(f, "Hot reloaded"),
            ReloadStrategy::ClassLoader => write!(f, "ClassLoader reloaded"),
            ReloadStrategy::Restart => write!(f, "Restart required"),
        }
    }
}

#[allow(dead_code)]
pub struct ReloadResult {
    pub success: bool,
    pub strategy: ReloadStrategy,
    pub time_ms: u64,
    pub error: Option<String>,
}

/// Client for the ym-agent running inside the target JVM.
pub struct AgentClient {
    port: u16,
}

impl AgentClient {
    pub fn new(port: u16) -> Self {
        Self { port }
    }

    /// Send a reload request to the agent.
    pub fn reload(&self, class_dir: &Path, class_names: &[String]) -> Result<ReloadResult> {
        let mut stream = TcpStream::connect(("127.0.0.1", self.port))
            .context("Failed to connect to ym-agent")?;
        stream.set_read_timeout(Some(Duration::from_secs(10)))?;

        let classes_json: Vec<String> = class_names
            .iter()
            .map(|c| format!("\"{}\"", c))
            .collect();

        let request = format!(
            r#"{{"method":"reload","params":{{"classDir":"{}","classes":[{}]}}}}"#,
            class_dir.to_string_lossy().replace('\\', "\\\\"),
            classes_json.join(",")
        );

        writeln!(stream, "{}", request)?;
        stream.flush()?;

        let mut reader = BufReader::new(&stream);
        let mut response_line = String::new();
        reader.read_line(&mut response_line)?;

        let resp: serde_json::Value = serde_json::from_str(&response_line)?;

        let success = resp["success"].as_bool().unwrap_or(false);
        let strategy_str = resp["strategy"].as_str().unwrap_or("restart");
        let time_ms = resp["timeMs"].as_u64().unwrap_or(0);
        let error = resp["error"].as_str().map(|s| s.to_string());

        let strategy = match strategy_str {
            "hotswap" => ReloadStrategy::HotSwap,
            "classloader" => ReloadStrategy::ClassLoader,
            _ => ReloadStrategy::Restart,
        };

        Ok(ReloadResult {
            success,
            strategy,
            time_ms,
            error,
        })
    }

    /// Check if the agent is reachable.
    #[allow(dead_code)]
    pub fn is_alive(&self) -> bool {
        TcpStream::connect(("127.0.0.1", self.port)).is_ok()
    }
}

/// Embedded ym-agent.jar bytes (7KB)
const AGENT_JAR_BYTES: &[u8] = include_bytes!("../../ym-agent/ym-agent.jar");

/// Find or extract the ym-agent.jar.
/// Checks standard locations first, then extracts the embedded jar to ~/.ym/.
pub fn find_agent_jar() -> Option<PathBuf> {
    let candidates = [
        std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|d| d.join("ym-agent.jar"))),
        std::env::current_dir()
            .ok()
            .map(|d| d.join(".ym").join("ym-agent.jar")),
        std::env::var("HOME")
            .ok()
            .map(|h| PathBuf::from(h).join(".ym").join("ym-agent.jar")),
    ];

    for candidate in candidates.into_iter().flatten() {
        if candidate.exists() {
            return Some(candidate);
        }
    }

    // Extract embedded jar to ~/.ym/ym-agent.jar
    if let Ok(home) = std::env::var("HOME") {
        let ym_dir = PathBuf::from(home).join(".ym");
        let jar_path = ym_dir.join("ym-agent.jar");
        if std::fs::create_dir_all(&ym_dir).is_ok()
            && std::fs::write(&jar_path, AGENT_JAR_BYTES).is_ok()
        {
            return Some(jar_path);
        }
    }

    None
}

/// Build JVM arguments for launching with the agent.
pub fn agent_jvm_args(agent_jar: &Path, port: u16) -> Vec<String> {
    vec![format!(
        "-javaagent:{}=port={}",
        agent_jar.to_string_lossy(),
        port
    )]
}

/// Find a free TCP port for agent communication.
pub fn find_free_port() -> Result<u16> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    Ok(listener.local_addr()?.port())
}
