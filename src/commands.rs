//! Node command dispatcher.
//!
//! Routes incoming `node.invoke` requests to the appropriate handler.
//! Built-in commands for terminal, dev session, and system operations.

use crate::gateway::{GatewayConnection, GatewayError, NodeInvokeRequest};
use crate::protocol::{CommandPayload, EventPayload, Frame, ResponsePayload};
use crate::terminal::{TerminalEvent, TerminalManager, TerminalOpenParams};
use crate::TunnelConnection;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{debug, error, info};

/// Result of handling a command.
pub struct CommandResult {
    pub ok: bool,
    pub payload: Option<Value>,
    pub error: Option<GatewayError>,
}

impl CommandResult {
    pub fn success(payload: Value) -> Self {
        Self {
            ok: true,
            payload: Some(payload),
            error: None,
        }
    }

    pub fn error(code: &str, message: &str) -> Self {
        Self {
            ok: false,
            payload: None,
            error: Some(GatewayError {
                code: code.into(),
                message: message.into(),
                details: None,
            }),
        }
    }
}

/// A command handler function.
pub type CommandHandler =
    Box<dyn Fn(Value) -> std::pin::Pin<Box<dyn std::future::Future<Output = CommandResult> + Send>> + Send + Sync>;

/// Dispatches node.invoke commands to registered handlers.
pub struct CommandDispatcher {
    handlers: HashMap<String, CommandHandler>,
    terminal: Arc<TerminalManager>,
}

impl CommandDispatcher {
    /// Create a new dispatcher with built-in terminal commands.
    pub fn new(terminal: Arc<TerminalManager>) -> Self {
        let mut dispatcher = Self {
            handlers: HashMap::new(),
            terminal,
        };
        dispatcher.register_builtins();
        dispatcher
    }

    /// Register a custom command handler.
    pub fn register<F, Fut>(&mut self, command: &str, handler: F)
    where
        F: Fn(Value) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = CommandResult> + Send + 'static,
    {
        self.handlers.insert(
            command.into(),
            Box::new(move |params| Box::pin(handler(params))),
        );
    }

    /// Dispatch a node.invoke request.
    pub async fn dispatch(&self, req: &NodeInvokeRequest) -> CommandResult {
        let params = req.params.clone().unwrap_or(Value::Null);

        // Check custom handlers first.
        if let Some(handler) = self.handlers.get(&req.command) {
            return handler(params).await;
        }

        // Built-in commands.
        match req.command.as_str() {
            "terminal.open" => self.handle_terminal_open(params).await,
            "terminal.input" => self.handle_terminal_input(params).await,
            "terminal.close" => self.handle_terminal_close(params).await,
            "terminal.resize" => self.handle_terminal_resize(params).await,
            "terminal.list" => self.handle_terminal_list().await,
            "system.info" => self.handle_system_info().await,
            "system.run" => self.handle_system_run(params).await,
            "dev.launch" => self.handle_dev_launch(params).await,
            "dev.status" => self.handle_dev_status().await,
            _ => CommandResult::error(
                "UNKNOWN_COMMAND",
                &format!("unknown command: {}", req.command),
            ),
        }
    }

    /// Get the list of supported commands.
    pub fn supported_commands(&self) -> Vec<String> {
        let mut commands: Vec<String> = self.handlers.keys().cloned().collect();
        commands.extend([
            "terminal.open".into(),
            "terminal.input".into(),
            "terminal.close".into(),
            "terminal.resize".into(),
            "terminal.list".into(),
            "system.info".into(),
            "system.run".into(),
            "dev.launch".into(),
            "dev.status".into(),
        ]);
        commands.sort();
        commands.dedup();
        commands
    }

    fn register_builtins(&mut self) {
        // Built-ins are handled in dispatch() directly, not via the handler map.
    }

    async fn handle_terminal_open(&self, params: Value) -> CommandResult {
        let open_params: TerminalOpenParams = match serde_json::from_value(params) {
            Ok(p) => p,
            Err(e) => return CommandResult::error("INVALID_PARAMS", &e.to_string()),
        };

        match self.terminal.open(open_params).await {
            Ok(session) => CommandResult::success(serde_json::to_value(&session).unwrap_or_default()),
            Err(e) => CommandResult::error("SPAWN_FAILED", &e),
        }
    }

    async fn handle_terminal_input(&self, params: Value) -> CommandResult {
        let session_id = match params.get("session_id").and_then(|v| v.as_str()) {
            Some(id) => id,
            None => return CommandResult::error("INVALID_PARAMS", "missing session_id"),
        };
        let data = match params.get("data").and_then(|v| v.as_str()) {
            Some(d) => d,
            None => return CommandResult::error("INVALID_PARAMS", "missing data"),
        };

        match self.terminal.input(session_id, data.as_bytes()).await {
            Ok(()) => CommandResult::success(serde_json::json!({"ok": true})),
            Err(e) => CommandResult::error("INPUT_FAILED", &e),
        }
    }

    async fn handle_terminal_close(&self, params: Value) -> CommandResult {
        let session_id = match params.get("session_id").and_then(|v| v.as_str()) {
            Some(id) => id,
            None => return CommandResult::error("INVALID_PARAMS", "missing session_id"),
        };

        match self.terminal.close(session_id).await {
            Ok(()) => CommandResult::success(serde_json::json!({"ok": true})),
            Err(e) => CommandResult::error("CLOSE_FAILED", &e),
        }
    }

    async fn handle_terminal_resize(&self, params: Value) -> CommandResult {
        let session_id = match params.get("session_id").and_then(|v| v.as_str()) {
            Some(id) => id,
            None => return CommandResult::error("INVALID_PARAMS", "missing session_id"),
        };
        let cols = params.get("cols").and_then(|v| v.as_u64()).unwrap_or(80) as u16;
        let rows = params.get("rows").and_then(|v| v.as_u64()).unwrap_or(24) as u16;

        match self.terminal.resize(session_id, cols, rows).await {
            Ok(()) => CommandResult::success(serde_json::json!({"ok": true})),
            Err(e) => CommandResult::error("RESIZE_FAILED", &e),
        }
    }

    async fn handle_terminal_list(&self) -> CommandResult {
        let sessions = self.terminal.list().await;
        CommandResult::success(serde_json::to_value(&sessions).unwrap_or_default())
    }

    async fn handle_system_info(&self) -> CommandResult {
        CommandResult::success(serde_json::json!({
            "os": std::env::consts::OS,
            "arch": std::env::consts::ARCH,
            "hostname": std::env::var("HOSTNAME").or_else(|_| std::env::var("HOST")).unwrap_or_default(),
            "home": std::env::var("HOME").unwrap_or_default(),
            "cwd": std::env::current_dir().map(|p| p.display().to_string()).unwrap_or_default(),
            "shell": std::env::var("SHELL").unwrap_or_default(),
            "user": std::env::var("USER").unwrap_or_default(),
        }))
    }

    async fn handle_system_run(&self, params: Value) -> CommandResult {
        let cmd = match params.get("cmd").and_then(|v| v.as_str()) {
            Some(c) => c.to_string(),
            None => match params.get("command").and_then(|v| v.as_str()) {
                Some(c) => c.to_string(),
                None => return CommandResult::error("INVALID_PARAMS", "missing cmd"),
            },
        };

        let cwd = params
            .get("cwd")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        let timeout_ms = params
            .get("timeout_ms")
            .and_then(|v| v.as_u64())
            .unwrap_or(30_000);

        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
        let mut command = tokio::process::Command::new(&shell);
        command
            .arg("-c")
            .arg(&cmd)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());

        if let Some(ref dir) = cwd {
            command.current_dir(dir);
        }

        let result = tokio::time::timeout(
            std::time::Duration::from_millis(timeout_ms),
            command.output(),
        )
        .await;

        match result {
            Ok(Ok(output)) => {
                let stdout = String::from_utf8_lossy(&output.stdout).to_string();
                let stderr = String::from_utf8_lossy(&output.stderr).to_string();
                CommandResult::success(serde_json::json!({
                    "exit_code": output.status.code(),
                    "stdout": stdout,
                    "stderr": stderr,
                }))
            }
            Ok(Err(e)) => CommandResult::error("EXEC_FAILED", &e.to_string()),
            Err(_) => CommandResult::error("TIMEOUT", "command timed out"),
        }
    }

    async fn handle_dev_launch(&self, params: Value) -> CommandResult {
        let message = params
            .get("message")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let cwd = params
            .get("cwd")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let model = params
            .get("model")
            .and_then(|v| v.as_str())
            .unwrap_or("claude-sonnet-4-6");

        // Find the dev binary.
        let dev_bin = which_dev();

        let mut cmd = tokio::process::Command::new(&dev_bin);
        cmd.arg("--model").arg(model);

        if !message.is_empty() {
            cmd.arg(message);
        }

        if let Some(ref dir) = cwd {
            cmd.current_dir(dir);
        }

        cmd.stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());

        match cmd.spawn() {
            Ok(child) => {
                let pid = child.id().unwrap_or(0);
                info!(pid = pid, dev_bin = %dev_bin, "dev session launched");
                CommandResult::success(serde_json::json!({
                    "pid": pid,
                    "binary": dev_bin,
                    "model": model,
                    "cwd": cwd,
                }))
            }
            Err(e) => CommandResult::error("LAUNCH_FAILED", &e.to_string()),
        }
    }

    async fn handle_dev_status(&self) -> CommandResult {
        // Check for running dev processes.
        let output = tokio::process::Command::new("pgrep")
            .args(["-f", "dev.*--model"])
            .output()
            .await;

        let pids: Vec<String> = match output {
            Ok(o) => String::from_utf8_lossy(&o.stdout)
                .lines()
                .map(|l| l.trim().to_string())
                .filter(|l| !l.is_empty())
                .collect(),
            Err(_) => vec![],
        };

        CommandResult::success(serde_json::json!({
            "running": !pids.is_empty(),
            "pids": pids,
        }))
    }
}

/// Find the dev binary.
fn which_dev() -> String {
    // Check common locations.
    for candidate in &["dev", "hanzo-dev", "codex"] {
        if let Ok(output) = std::process::Command::new("which")
            .arg(candidate)
            .output()
        {
            if output.status.success() {
                return String::from_utf8_lossy(&output.stdout).trim().to_string();
            }
        }
    }
    "dev".into()
}

/// Run the command dispatch loop.
///
/// Receives node.invoke requests from the gateway connection, dispatches them
/// to the command dispatcher, and sends results back. Also forwards terminal
/// events as node events.
pub async fn run_dispatch_loop(
    conn: Arc<GatewayConnection>,
    dispatcher: Arc<CommandDispatcher>,
    mut terminal_events: mpsc::Receiver<TerminalEvent>,
) {
    let conn2 = conn.clone();

    // Forward terminal events as node events.
    tokio::spawn(async move {
        while let Some(event) = terminal_events.recv().await {
            match event {
                TerminalEvent::Output { session_id, data } => {
                    let _ = conn2
                        .send_node_event(
                            "terminal.output",
                            Some(serde_json::json!({
                                "session_id": session_id,
                                "data": data,
                            })),
                        )
                        .await;
                }
                TerminalEvent::Exit {
                    session_id,
                    exit_code,
                } => {
                    let _ = conn2
                        .send_node_event(
                            "terminal.exit",
                            Some(serde_json::json!({
                                "session_id": session_id,
                                "exit_code": exit_code,
                            })),
                        )
                        .await;
                }
            }
        }
    });

    // Dispatch loop.
    loop {
        match conn.recv_invoke().await {
            Some(req) => {
                debug!(command = %req.command, id = %req.id, "dispatching node.invoke");
                let result = dispatcher.dispatch(&req).await;
                if let Err(e) = conn
                    .send_invoke_result(&req.id, result.ok, result.payload, result.error)
                    .await
                {
                    error!(error = %e, "failed to send invoke result");
                    break;
                }
            }
            None => {
                info!("gateway connection closed, stopping dispatch loop");
                break;
            }
        }
    }
}

/// Run command dispatch over a tunnel protocol connection.
///
/// Works with `TunnelConnection` (used for both cloud relay and bot gateway's
/// `/v1/tunnel` endpoint). Receives `Frame::Command` frames, dispatches them
/// via the `CommandDispatcher`, and sends `Frame::Response` back.
pub async fn run_tunnel_dispatch_loop(
    conn: Arc<TunnelConnection>,
    dispatcher: Arc<CommandDispatcher>,
    mut terminal_events: mpsc::Receiver<TerminalEvent>,
) {
    let conn2 = conn.clone();

    // Forward terminal events as tunnel event frames.
    tokio::spawn(async move {
        while let Some(event) = terminal_events.recv().await {
            let (event_name, data) = match event {
                TerminalEvent::Output { session_id, data } => (
                    "terminal.output",
                    serde_json::json!({
                        "session_id": session_id,
                        "data": data,
                    }),
                ),
                TerminalEvent::Exit {
                    session_id,
                    exit_code,
                } => (
                    "terminal.exit",
                    serde_json::json!({
                        "session_id": session_id,
                        "exit_code": exit_code,
                    }),
                ),
            };
            if let Err(e) = conn2.send_event(event_name, data).await {
                debug!(error = %e, "failed to send terminal event (connection may be closed)");
                break;
            }
        }
    });

    // Dispatch loop — receive Command frames, dispatch, send Response frames.
    loop {
        match conn.recv_command().await {
            Some(Frame::Command(CommandPayload { id, method, params })) => {
                let req = NodeInvokeRequest {
                    id: id.clone(),
                    node_id: conn.instance_id.clone(),
                    command: method.clone(),
                    params: Some(params),
                    timeout_ms: None,
                };
                debug!(command = %method, id = %id, "dispatching tunnel command");
                let result = dispatcher.dispatch(&req).await;

                let send_result = if result.ok {
                    conn.respond(&id, result.payload).await
                } else {
                    let error_msg = result
                        .error
                        .map(|e| e.message)
                        .unwrap_or_else(|| "unknown error".into());
                    conn.respond_error(&id, &error_msg).await
                };

                if let Err(e) = send_result {
                    error!(error = %e, "failed to send response");
                    break;
                }
            }
            Some(Frame::Ping) => {
                // Handled by transport layer, but log at debug.
                debug!("received ping (handled by transport)");
            }
            Some(_) => {
                // Ignore non-command frames (registered, events, etc.)
            }
            None => {
                info!("tunnel connection closed, stopping dispatch loop");
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_result_success() {
        let r = CommandResult::success(serde_json::json!({"ok": true}));
        assert!(r.ok);
        assert!(r.error.is_none());
    }

    #[test]
    fn command_result_error() {
        let r = CommandResult::error("NOT_FOUND", "session not found");
        assert!(!r.ok);
        assert_eq!(r.error.as_ref().unwrap().code, "NOT_FOUND");
    }

    #[tokio::test]
    async fn dispatcher_supported_commands() {
        let (tx, _rx) = mpsc::channel(1);
        let terminal = Arc::new(TerminalManager::new(tx));
        let dispatcher = CommandDispatcher::new(terminal);
        let commands = dispatcher.supported_commands();
        assert!(commands.contains(&"terminal.open".to_string()));
        assert!(commands.contains(&"system.run".to_string()));
        assert!(commands.contains(&"dev.launch".to_string()));
    }

    #[tokio::test]
    async fn dispatcher_system_info() {
        let (tx, _rx) = mpsc::channel(1);
        let terminal = Arc::new(TerminalManager::new(tx));
        let dispatcher = CommandDispatcher::new(terminal);
        let req = NodeInvokeRequest {
            id: "test".into(),
            node_id: "n1".into(),
            command: "system.info".into(),
            params: None,
            timeout_ms: None,
        };
        let result = dispatcher.dispatch(&req).await;
        assert!(result.ok);
        let payload = result.payload.unwrap();
        assert!(payload.get("os").is_some());
        assert!(payload.get("arch").is_some());
    }

    #[tokio::test]
    async fn dispatcher_unknown_command() {
        let (tx, _rx) = mpsc::channel(1);
        let terminal = Arc::new(TerminalManager::new(tx));
        let dispatcher = CommandDispatcher::new(terminal);
        let req = NodeInvokeRequest {
            id: "test".into(),
            node_id: "n1".into(),
            command: "does.not.exist".into(),
            params: None,
            timeout_ms: None,
        };
        let result = dispatcher.dispatch(&req).await;
        assert!(!result.ok);
        assert_eq!(result.error.unwrap().code, "UNKNOWN_COMMAND");
    }
}
