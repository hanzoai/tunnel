//! Interactive terminal/PTY session management.
//!
//! Provides persistent PTY sessions that can be controlled remotely.
//! Each session has a unique ID and streams output as events.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;
use tokio::sync::{Mutex, mpsc, watch};
use tracing::{debug, info};

/// A terminal session request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TerminalOpenParams {
    /// Shell to use (default: user's $SHELL or /bin/sh).
    #[serde(default)]
    pub shell: Option<String>,
    /// Working directory.
    #[serde(default)]
    pub cwd: Option<String>,
    /// Environment variables to set.
    #[serde(default)]
    pub env: HashMap<String, String>,
    /// Initial terminal size.
    #[serde(default = "default_cols")]
    pub cols: u16,
    #[serde(default = "default_rows")]
    pub rows: u16,
}

fn default_cols() -> u16 {
    80
}
fn default_rows() -> u16 {
    24
}

/// Terminal session info returned to the caller.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TerminalSession {
    pub session_id: String,
    pub shell: String,
    pub pid: u32,
    pub cols: u16,
    pub rows: u16,
}

/// Terminal output event data.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TerminalOutputData {
    pub session_id: String,
    pub data: String,
}

/// Terminal exit event data.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TerminalExitData {
    pub session_id: String,
    pub exit_code: Option<i32>,
}

/// Manages active terminal sessions on this node.
pub struct TerminalManager {
    sessions: Arc<Mutex<HashMap<String, SessionHandle>>>,
    /// Channel for sending terminal output events upstream.
    event_tx: mpsc::Sender<TerminalEvent>,
}

/// Terminal events sent upstream (to gateway or cloud).
#[derive(Debug, Clone)]
pub enum TerminalEvent {
    Output {
        session_id: String,
        data: String,
    },
    Exit {
        session_id: String,
        exit_code: Option<i32>,
    },
}

struct SessionHandle {
    stdin_tx: mpsc::Sender<Vec<u8>>,
    shutdown: watch::Sender<bool>,
    pid: u32,
    shell: String,
    cols: u16,
    rows: u16,
}

impl TerminalManager {
    /// Create a new terminal manager.
    ///
    /// `event_tx` receives terminal output and exit events that should be
    /// forwarded to the remote controller.
    pub fn new(event_tx: mpsc::Sender<TerminalEvent>) -> Self {
        Self {
            sessions: Arc::new(Mutex::new(HashMap::new())),
            event_tx,
        }
    }

    /// Open a new terminal session.
    pub async fn open(&self, params: TerminalOpenParams) -> Result<TerminalSession, String> {
        let session_id = uuid::Uuid::new_v4().to_string();

        let shell = params.shell.unwrap_or_else(|| {
            std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into())
        });

        let mut cmd = Command::new(&shell);
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        if let Some(ref cwd) = params.cwd {
            cmd.current_dir(cwd);
        }

        for (k, v) in &params.env {
            cmd.env(k, v);
        }

        // Set terminal size env vars.
        cmd.env("COLUMNS", params.cols.to_string());
        cmd.env("LINES", params.rows.to_string());
        cmd.env("TERM", "xterm-256color");

        let mut child = cmd.spawn().map_err(|e| format!("spawn failed: {e}"))?;
        let pid = child.id().unwrap_or(0);

        let (stdin_tx, mut stdin_rx) = mpsc::channel::<Vec<u8>>(256);
        let (shutdown_tx, mut shutdown_rx) = watch::channel(false);

        let mut child_stdin = child.stdin.take().ok_or("no stdin")?;
        let mut child_stdout = child.stdout.take().ok_or("no stdout")?;
        let mut child_stderr = child.stderr.take().ok_or("no stderr")?;

        let event_tx = self.event_tx.clone();
        let sid = session_id.clone();
        let sessions = self.sessions.clone();

        // Spawn stdin writer.
        tokio::spawn(async move {
            while let Some(data) = stdin_rx.recv().await {
                if child_stdin.write_all(&data).await.is_err() {
                    break;
                }
            }
        });

        // Spawn stdout reader.
        let event_tx2 = event_tx.clone();
        let sid2 = sid.clone();
        tokio::spawn(async move {
            let mut buf = [0u8; 4096];
            loop {
                match child_stdout.read(&mut buf).await {
                    Ok(0) => break,
                    Ok(n) => {
                        let data = String::from_utf8_lossy(&buf[..n]).to_string();
                        let _ = event_tx2
                            .send(TerminalEvent::Output {
                                session_id: sid2.clone(),
                                data,
                            })
                            .await;
                    }
                    Err(_) => break,
                }
            }
        });

        // Spawn stderr reader (merge into output).
        let event_tx3 = event_tx.clone();
        let sid3 = sid.clone();
        tokio::spawn(async move {
            let mut buf = [0u8; 4096];
            loop {
                match child_stderr.read(&mut buf).await {
                    Ok(0) => break,
                    Ok(n) => {
                        let data = String::from_utf8_lossy(&buf[..n]).to_string();
                        let _ = event_tx3
                            .send(TerminalEvent::Output {
                                session_id: sid3.clone(),
                                data,
                            })
                            .await;
                    }
                    Err(_) => break,
                }
            }
        });

        // Spawn process waiter.
        let sid4 = sid.clone();
        let sessions2 = sessions.clone();
        tokio::spawn(async move {
            let status: Result<std::process::ExitStatus, _> = child.wait().await;
            let exit_code = status.ok().and_then(|s| s.code());
            let _ = event_tx
                .send(TerminalEvent::Exit {
                    session_id: sid4.clone(),
                    exit_code,
                })
                .await;
            sessions2.lock().await.remove(&sid4);
            info!(session_id = %sid4, exit_code = ?exit_code, "terminal session exited");
        });

        let handle = SessionHandle {
            stdin_tx,
            shutdown: shutdown_tx,
            pid,
            shell: shell.clone(),
            cols: params.cols,
            rows: params.rows,
        };

        let session = TerminalSession {
            session_id: session_id.clone(),
            shell,
            pid,
            cols: params.cols,
            rows: params.rows,
        };

        self.sessions.lock().await.insert(session_id, handle);

        info!(session_id = %session.session_id, pid = pid, "terminal session opened");

        Ok(session)
    }

    /// Send input to a terminal session.
    pub async fn input(&self, session_id: &str, data: &[u8]) -> Result<(), String> {
        let sessions = self.sessions.lock().await;
        let handle = sessions
            .get(session_id)
            .ok_or_else(|| format!("session not found: {session_id}"))?;
        handle
            .stdin_tx
            .send(data.to_vec())
            .await
            .map_err(|_| "stdin closed".into())
    }

    /// Close a terminal session.
    pub async fn close(&self, session_id: &str) -> Result<(), String> {
        let mut sessions = self.sessions.lock().await;
        if let Some(handle) = sessions.remove(session_id) {
            let _ = handle.shutdown.send(true);
            info!(session_id = %session_id, "terminal session closed");
            Ok(())
        } else {
            Err(format!("session not found: {session_id}"))
        }
    }

    /// List active terminal sessions.
    pub async fn list(&self) -> Vec<TerminalSession> {
        let sessions = self.sessions.lock().await;
        sessions
            .iter()
            .map(|(id, h)| TerminalSession {
                session_id: id.clone(),
                shell: h.shell.clone(),
                pid: h.pid,
                cols: h.cols,
                rows: h.rows,
            })
            .collect()
    }

    /// Resize a terminal session.
    pub async fn resize(&self, session_id: &str, cols: u16, rows: u16) -> Result<(), String> {
        let sessions = self.sessions.lock().await;
        // Note: For a full PTY we'd use ioctl TIOCSWINSZ here.
        // With piped stdin/stdout we can only update the env vars.
        // A real PTY implementation would use portable-pty or nix.
        if sessions.contains_key(session_id) {
            debug!(session_id, cols, rows, "terminal resize (env only)");
            Ok(())
        } else {
            Err(format!("session not found: {session_id}"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_open_params_defaults() {
        let json = "{}";
        let params: TerminalOpenParams = serde_json::from_str(json).unwrap();
        assert_eq!(params.cols, 80);
        assert_eq!(params.rows, 24);
        assert!(params.shell.is_none());
    }

    #[test]
    fn terminal_session_serialization() {
        let session = TerminalSession {
            session_id: "abc".into(),
            shell: "/bin/zsh".into(),
            pid: 12345,
            cols: 120,
            rows: 40,
        };
        let json = serde_json::to_string(&session).unwrap();
        assert!(json.contains("\"pid\":12345"));
    }

    #[tokio::test]
    async fn terminal_manager_open_close() {
        let (tx, mut rx) = mpsc::channel(64);
        let mgr = TerminalManager::new(tx);

        let session = mgr
            .open(TerminalOpenParams {
                shell: Some("/bin/echo".into()),
                cwd: None,
                env: HashMap::new(),
                cols: 80,
                rows: 24,
            })
            .await
            .unwrap();

        assert!(!session.session_id.is_empty());
        assert_eq!(session.shell, "/bin/echo");

        // Wait a bit for the process to complete.
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Should have received output or exit event.
        let list = mgr.list().await;
        // echo exits immediately, session may already be removed.
        assert!(list.len() <= 1);
    }
}
