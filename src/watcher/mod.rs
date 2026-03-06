use anyhow::Result;
use notify::{Event, RecommendedWatcher, RecursiveMode, Watcher};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// Watch directories for file changes, with debouncing.
pub struct FileWatcher {
    _watcher: RecommendedWatcher,
    rx: mpsc::Receiver<Result<Event, notify::Error>>,
    extensions: Vec<String>,
}

impl FileWatcher {
    pub fn new(dirs: &[PathBuf], extensions: Vec<String>) -> Result<Self> {
        let (tx, rx) = mpsc::channel();

        let mut watcher = RecommendedWatcher::new(
            move |res| {
                let _ = tx.send(res);
            },
            notify::Config::default(),
        )?;

        for dir in dirs {
            if dir.exists() {
                watcher.watch(dir, RecursiveMode::Recursive)?;
            }
        }

        Ok(FileWatcher {
            _watcher: watcher,
            rx,
            extensions,
        })
    }

    /// Wait for file changes. Returns changed file paths after debouncing.
    /// Uses timeout so callers can periodically check for shutdown signals.
    pub fn wait_for_changes(&self, debounce: Duration) -> Vec<PathBuf> {
        let mut changed = Vec::new();

        // Poll with timeout so the caller can check shutdown flags
        match self.rx.recv_timeout(debounce) {
            Ok(Ok(event)) => {
                let relevant: Vec<PathBuf> = event
                    .paths
                    .into_iter()
                    .filter(|p| self.is_relevant(p))
                    .collect();
                if relevant.is_empty() {
                    return changed;
                }
                changed.extend(relevant);
            }
            _ => return changed,
        }

        // Debounce: collect more events within the window
        let deadline = Instant::now() + debounce;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            match self.rx.recv_timeout(remaining) {
                Ok(Ok(event)) => {
                    let relevant: Vec<PathBuf> = event
                        .paths
                        .into_iter()
                        .filter(|p| self.is_relevant(p))
                        .collect();
                    changed.extend(relevant);
                }
                _ => break,
            }
        }

        // Deduplicate
        changed.sort();
        changed.dedup();
        changed
    }

    fn is_relevant(&self, path: &Path) -> bool {
        if self.extensions.is_empty() {
            return true;
        }
        if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
            let dot_ext = format!(".{}", ext);
            self.extensions.iter().any(|e| e == &dot_ext || e == ext)
        } else {
            false
        }
    }
}
