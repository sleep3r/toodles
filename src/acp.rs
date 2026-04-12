//! ACP (Agent Client Protocol) client for `gemini --acp`.
//!
//! Implements a custom JSON-RPC 2.0 client over stdio to communicate with
//! gemini-cli in ACP mode. This avoids the `!Send` constraint of the official
//! `agent-client-protocol` crate, keeping all types `Send + Sync`.

use std::collections::HashMap;
use std::path::Path;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{mpsc, oneshot, Mutex};
use tracing::{debug, error, info, warn};

// ──────────────────────────────────────────────────────────────────────────────
// ACP Events — structured events from the agent
// ──────────────────────────────────────────────────────────────────────────────

/// Events sent from the ACP agent via `session/update` notifications.
#[derive(Debug, Clone)]
pub enum AcpEvent {
    /// Streaming text chunk from the agent's response.
    TextChunk(String),
    /// A tool call has been initiated.
    ToolCall { title: String, status: String },
    /// Progress update for an ongoing tool call.
    ToolCallUpdate {
        status: String,
        content: Option<String>,
    },
    /// Agent's plan for the task.
    Plan(Vec<PlanEntry>),
    /// Usage statistics update.
    Usage {
        input_tokens: Option<u64>,
        output_tokens: Option<u64>,
    },
    /// An error occurred.
    Error(String),
}

/// A single entry in the agent's plan.
#[derive(Debug, Clone)]
pub struct PlanEntry {
    pub content: String,
    pub status: String,
}

// ──────────────────────────────────────────────────────────────────────────────
// JSON-RPC types
// ──────────────────────────────────────────────────────────────────────────────

#[derive(Serialize)]
struct JsonRpcRequest {
    jsonrpc: &'static str,
    id: u64,
    method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    params: Option<Value>,
}

/// A notification has no `id` field.
#[derive(Serialize)]
struct JsonRpcNotification {
    jsonrpc: &'static str,
    method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    params: Option<Value>,
}

/// Incoming JSON-RPC message (could be response, notification, or agent request).
#[derive(Deserialize, Debug)]
struct JsonRpcMessage {
    #[allow(dead_code)]
    jsonrpc: Option<String>,
    id: Option<Value>,
    method: Option<String>,
    params: Option<Value>,
    result: Option<Value>,
    error: Option<JsonRpcError>,
}

#[derive(Deserialize, Debug)]
struct JsonRpcError {
    #[allow(dead_code)]
    code: i64,
    message: String,
}

// ──────────────────────────────────────────────────────────────────────────────
// ACP Connection
// ──────────────────────────────────────────────────────────────────────────────

/// A connection to a `gemini --acp` process.
///
/// Manages the JSON-RPC communication, request/response matching, and
/// notification dispatch. All types are `Send + Sync`.
pub struct AcpConnection {
    stdin: Arc<Mutex<tokio::process::ChildStdin>>,
    next_id: AtomicU64,
    pending: Arc<tokio::sync::Mutex<HashMap<u64, oneshot::Sender<Result<Value>>>>>,
    _child: Child,
}

impl AcpConnection {
    /// Spawn an ACP agent process and establish the ACP connection.
    ///
    /// Returns the connection and a receiver for ACP events.
    pub async fn spawn(
        agent_cmd: &str,
        working_dir: Option<&Path>,
    ) -> Result<(Self, mpsc::UnboundedReceiver<AcpEvent>)> {
        info!(cmd = %agent_cmd, "Spawning ACP agent process");

        // Wrap command with exec and env TERM=dumb NO_COLOR=1 so bash is fully replaced by the gemini-cli
        // process, ensuring proper cleanup (no orphaned zombie node processes) when the Child is dropped,
        // and suppressing ANSI terminal queries that block stdout.
        let wrapped_cmd = format!("exec env TERM=dumb NO_COLOR=1 {}", agent_cmd);

        let mut cmd = Command::new("bash");
        cmd.arg("-lc")
            .arg(&wrapped_cmd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        if let Some(dir) = working_dir {
            cmd.current_dir(dir);
        }

        let mut child = cmd
            .spawn()
            .with_context(|| format!("Failed to spawn '{}'", agent_cmd))?;

        let stdin = child
            .stdin
            .take()
            .context("Failed to capture gemini-cli stdin")?;
        let stdout = child
            .stdout
            .take()
            .context("Failed to capture gemini-cli stdout")?;

        // Drain stderr in the background.
        let stderr = child.stderr.take();
        tokio::spawn(async move {
            if let Some(err_stream) = stderr {
                let mut reader = BufReader::new(err_stream);
                let mut line = String::new();
                loop {
                    line.clear();
                    match reader.read_line(&mut line).await {
                        Ok(0) => break,
                        Ok(_) => {
                            let trimmed = line.trim();
                            if !trimmed.is_empty() {
                                debug!("gemini-cli stderr: {}", trimmed);
                            }
                        }
                        Err(_) => break,
                    }
                }
            }
        });

        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let pending: Arc<tokio::sync::Mutex<HashMap<u64, oneshot::Sender<Result<Value>>>>> =
            Arc::new(tokio::sync::Mutex::new(HashMap::new()));

        let stdin = Arc::new(Mutex::new(stdin));

        // Spawn stdout reader task — routes responses and notifications.
        let pending_clone = pending.clone();
        let event_tx_clone = event_tx.clone();
        let stdin_clone = stdin.clone();
        tokio::spawn(async move {
            let mut reader = BufReader::new(stdout);
            let mut line = String::new();
            loop {
                line.clear();
                match reader.read_line(&mut line).await {
                    Ok(0) => {
                        debug!("ACP stdout closed (EOF)");
                        break;
                    }
                    Ok(_) => {
                        let trimmed = line.trim();
                        if trimmed.is_empty() {
                            continue;
                        }
                        // gemini-cli may emit ANSI terminal queries (e.g.
                        // \x1b]11;?\x1b\\ ) before JSON on stdout.
                        // Find the first '{' to locate the JSON payload.
                        let json_str = match trimmed.find('{') {
                            Some(idx) => &trimmed[idx..],
                            None => {
                                debug!("Non-JSON ACP output: {trimmed}");
                                continue;
                            }
                        };
                        match serde_json::from_str::<JsonRpcMessage>(json_str) {
                            Ok(msg) => {
                                handle_incoming_message(
                                    msg,
                                    &pending_clone,
                                    &event_tx_clone,
                                    &stdin_clone,
                                )
                                .await;
                            }
                            Err(e) => {
                                debug!("Failed to parse ACP message: {e} — json: {json_str}");
                            }
                        }
                    }
                    Err(e) => {
                        error!("Error reading ACP stdout: {e}");
                        break;
                    }
                }
            }
            // Signal that the connection is closed.
            event_tx_clone
                .send(AcpEvent::Error("ACP connection closed".into()))
                .ok();
        });

        Ok((
            AcpConnection {
                stdin,
                next_id: AtomicU64::new(1),
                pending,
                _child: child,
            },
            event_rx,
        ))
    }

    // ── Protocol methods ─────────────────────────────────────────────────

    /// Send a JSON-RPC request and wait for the response.
    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        self.request_with_timeout(method, params, Duration::from_secs(60))
            .await
    }

    /// Send a JSON-RPC request and wait for the response with a custom timeout.
    async fn request_with_timeout(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let request = JsonRpcRequest {
            jsonrpc: "2.0",
            id,
            method: method.to_string(),
            params: Some(params),
        };

        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);

        let msg = serde_json::to_string(&request)? + "\n";
        debug!("ACP → {}", msg.trim());
        {
            let mut stdin = self.stdin.lock().await;
            stdin.write_all(msg.as_bytes()).await?;
            stdin.flush().await?;
        } // Drop stdin lock before waiting.

        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(anyhow::anyhow!("ACP response channel closed")),
            Err(_) => {
                // Clean up the pending entry.
                self.pending.lock().await.remove(&id);
                Err(anyhow::anyhow!(
                    "ACP request timed out after {}s: {method}",
                    timeout.as_secs()
                ))
            }
        }
    }

    /// Send a JSON-RPC notification (no response expected).
    async fn notify(&self, method: &str, params: Value) -> Result<()> {
        let notification = JsonRpcNotification {
            jsonrpc: "2.0",
            method: method.to_string(),
            params: Some(params),
        };
        let msg = serde_json::to_string(&notification)? + "\n";
        debug!("ACP → {}", msg.trim());
        let mut stdin = self.stdin.lock().await;
        stdin.write_all(msg.as_bytes()).await?;
        stdin.flush().await?;
        Ok(())
    }

    /// Initialize the ACP connection.
    pub async fn initialize(&self) -> Result<Value> {
        self.request_with_timeout(
            "initialize",
            serde_json::json!({
                "protocolVersion": 1,
                "clientCapabilities": {},
                "clientInfo": {
                    "name": "toodles",
                    "title": "Toodles Bot",
                    "version": env!("CARGO_PKG_VERSION")
                }
            }),
            Duration::from_secs(180),
        )
        .await
    }

    /// Create a new ACP session.
    pub async fn new_session(&self, cwd: &Path) -> Result<String> {
        let result = self
            .request_with_timeout(
                "session/new",
                serde_json::json!({
                    "cwd": cwd.to_string_lossy(),
                    "mcpServers": []
                }),
                Duration::from_secs(120),
            )
            .await?;
        let session_id = result["sessionId"]
            .as_str()
            .context("Missing sessionId in session/new response")?
            .to_string();
        info!(session_id = %session_id, "ACP session created");
        Ok(session_id)
    }

    /// Send a prompt to the agent. The response comes after all streaming
    /// events have been sent via `session/update` notifications.
    pub async fn prompt(&self, session_id: &str, content: Vec<ContentBlock>) -> Result<Value> {
        let prompt: Vec<Value> = content.into_iter().map(|c| c.to_json()).collect();
        self.request_with_timeout(
            "session/prompt",
            serde_json::json!({
                "sessionId": session_id,
                "prompt": prompt
            }),
            Duration::from_secs(30 * 60),
        )
        .await
    }

    /// Cancel an ongoing prompt.
    pub async fn cancel(&self, session_id: &str) -> Result<()> {
        self.notify(
            "session/cancel",
            serde_json::json!({
                "sessionId": session_id
            }),
        )
        .await
    }

    /// Set session mode (e.g., auto-approve for yolo mode).
    pub async fn set_session_mode(&self, session_id: &str, mode_id: &str) -> Result<Value> {
        self.request(
            "session/set-mode",
            serde_json::json!({
                "sessionId": session_id,
                "modeId": mode_id
            }),
        )
        .await
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Content blocks for prompts
// ──────────────────────────────────────────────────────────────────────────────

/// Content block in a prompt message.
pub enum ContentBlock {
    Text(String),
}

impl ContentBlock {
    fn to_json(self) -> Value {
        match self {
            ContentBlock::Text(text) => serde_json::json!({
                "type": "text",
                "text": text
            }),
        }
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Incoming message handler
// ──────────────────────────────────────────────────────────────────────────────

/// Route an incoming JSON-RPC message to the correct handler.
async fn handle_incoming_message(
    msg: JsonRpcMessage,
    pending: &tokio::sync::Mutex<HashMap<u64, oneshot::Sender<Result<Value>>>>,
    event_tx: &mpsc::UnboundedSender<AcpEvent>,
    stdin: &Mutex<tokio::process::ChildStdin>,
) {
    // Determine if this is a response, notification, or agent request.
    let has_id = msg.id.is_some();
    let has_method = msg.method.is_some();

    if has_id && !has_method {
        // Response to one of our requests.
        let id = match &msg.id {
            Some(Value::Number(n)) => n.as_u64().unwrap_or(0),
            _ => return,
        };
        let mut pending = pending.lock().await;
        if let Some(tx) = pending.remove(&id) {
            if let Some(error) = msg.error {
                tx.send(Err(anyhow::anyhow!("ACP error: {}", error.message)))
                    .ok();
            } else {
                tx.send(Ok(msg.result.unwrap_or(Value::Null))).ok();
            }
        }
    } else if has_id && has_method {
        // Request FROM the agent (e.g., session/request_permission).
        let id = match &msg.id {
            Some(Value::Number(n)) => n.as_u64().unwrap_or(0),
            _ => return,
        };
        let method = msg.method.as_deref().unwrap_or("");
        handle_agent_request(id, method, msg.params.as_ref(), stdin).await;
    } else if has_method {
        // Notification from the agent.
        let method = msg.method.as_deref().unwrap_or("");
        handle_notification(method, msg.params.as_ref(), event_tx);
    }
}

/// Handle a request from the agent (agent → client).
async fn handle_agent_request(
    id: u64,
    method: &str,
    params: Option<&Value>,
    stdin: &Mutex<tokio::process::ChildStdin>,
) {
    match method {
        "session/request_permission" => {
            // Auto-approve: find the first "allow" option.
            let option_id = params
                .and_then(|p| p["options"].as_array())
                .and_then(|opts| {
                    opts.iter()
                        .find(|o| {
                            let kind = o["kind"].as_str().unwrap_or("");
                            kind == "allowAlways" || kind == "allowOnce"
                        })
                        .or(opts.first())
                })
                .and_then(|o| o["optionId"].as_str())
                .unwrap_or("allow_always");

            debug!("Auto-approving permission request with option: {option_id}");

            let response = serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "outcome": {
                        "type": "selected",
                        "optionId": option_id
                    }
                }
            });
            let msg = serde_json::to_string(&response).unwrap() + "\n";
            let mut stdin = stdin.lock().await;
            stdin.write_all(msg.as_bytes()).await.ok();
            stdin.flush().await.ok();
        }
        _ => {
            // Unknown method — send an error response.
            warn!("Unknown agent request: {method}");
            let response = serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {
                    "code": -32601,
                    "message": format!("Method not supported: {method}")
                }
            });
            let msg = serde_json::to_string(&response).unwrap() + "\n";
            let mut stdin = stdin.lock().await;
            stdin.write_all(msg.as_bytes()).await.ok();
            stdin.flush().await.ok();
        }
    }
}

/// Handle a notification from the agent (session/update).
fn handle_notification(
    method: &str,
    params: Option<&Value>,
    event_tx: &mpsc::UnboundedSender<AcpEvent>,
) {
    if method != "session/update" {
        debug!("Ignoring notification: {method}");
        return;
    }

    let Some(params) = params else { return };
    let update = &params["update"];
    let update_type = update["sessionUpdate"].as_str().unwrap_or("");
    debug!(
        "ACP ← [{update_type}] {}",
        serde_json::to_string(update).unwrap_or_default()
    );

    match update_type {
        "agent_message_chunk" | "agent_thought_chunk" => {
            // Try multiple paths for text content — the structure may vary.
            let text = update["content"]["text"]
                .as_str()
                .or_else(|| update["text"].as_str())
                .or_else(|| update["content"].as_str());
            if let Some(text) = text {
                event_tx.send(AcpEvent::TextChunk(text.to_string())).ok();
            } else {
                debug!(
                    "Message chunk without text: {}",
                    serde_json::to_string(&update).unwrap_or_default()
                );
            }
        }
        "tool_call" => {
            let title = update["title"].as_str().unwrap_or("unknown").to_string();
            let status = update["status"].as_str().unwrap_or("pending").to_string();
            event_tx.send(AcpEvent::ToolCall { title, status }).ok();
        }
        "tool_call_update" => {
            let status = update["status"].as_str().unwrap_or("").to_string();
            let content = update["content"]
                .as_array()
                .and_then(|arr| arr.first())
                .and_then(|c| c["content"]["text"].as_str())
                .map(|s| s.to_string());
            event_tx
                .send(AcpEvent::ToolCallUpdate { status, content })
                .ok();
        }
        "plan" => {
            let entries = update["entries"]
                .as_array()
                .map(|arr| {
                    arr.iter()
                        .map(|e| PlanEntry {
                            content: e["content"].as_str().unwrap_or("").to_string(),
                            status: e["status"].as_str().unwrap_or("pending").to_string(),
                        })
                        .collect()
                })
                .unwrap_or_default();
            event_tx.send(AcpEvent::Plan(entries)).ok();
        }
        "usage_update" => {
            let input_tokens = update["inputTokens"].as_u64();
            let output_tokens = update["outputTokens"].as_u64();
            event_tx
                .send(AcpEvent::Usage {
                    input_tokens,
                    output_tokens,
                })
                .ok();
        }
        // Known informational updates we don't act on.
        "available_commands_update" | "mode_update" | "model_update" => {}
        _ => {
            debug!("Unknown session update type: {update_type}");
        }
    }
}
