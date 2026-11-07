//! Runs a local coding-agent CLI headless, locked down to answering with a build spec.
//!
//! Neither CLI gets tools: no shell, no file edits, no MCP servers, no user config. They
//! run in an empty scratch directory and must return JSON matching the build schema.

use std::{
    io::{BufRead, BufReader},
    path::PathBuf,
    process::{Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, RecvTimeoutError, Sender},
    },
    thread,
    time::{Duration, Instant},
};

use serde_json::Value;

use super::{
    mcp::{McpServer, SERVER_NAME},
    spec::{AgentAnswer, SCHEMA},
};

const TIMEOUT: Duration = Duration::from_secs(300);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Provider {
    Claude,
    Codex,
}

impl Provider {
    pub const ALL: [Provider; 2] = [Provider::Claude, Provider::Codex];

    pub fn label(self) -> &'static str {
        match self {
            Provider::Claude => "Claude Code",
            Provider::Codex => "Codex CLI",
        }
    }
}

pub enum AgentEvent {
    Status(String),
    Finished(Result<AgentAnswer, String>),
}

/// A running agent request. Dropping it doesn't stop the process; call [`AgentJob::cancel`].
pub struct AgentJob {
    pub events: Receiver<AgentEvent>,
    cancel: Arc<AtomicBool>,
    pub started: Instant,
}

impl AgentJob {
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

/// Where the game's world tools are served, if they are.
#[derive(Clone)]
struct ToolServer {
    url: String,
    token: String,
}

pub fn start(provider: Provider, system: String, prompt: String, tools: Option<&McpServer>) -> AgentJob {
    let (tx, rx) = mpsc::channel();
    let cancel = Arc::new(AtomicBool::new(false));
    let flag = cancel.clone();
    let tools = tools.map(|t| ToolServer { url: t.url.clone(), token: t.token.clone() });
    thread::spawn(move || {
        let result = run(provider, &system, &prompt, tools.as_ref(), &tx, &flag);
        let _ = tx.send(AgentEvent::Finished(result));
    });
    AgentJob { events: rx, cancel, started: Instant::now() }
}

fn scratch_dir() -> Result<PathBuf, String> {
    let dir = std::env::temp_dir().join("cubewish-agent");
    std::fs::create_dir_all(&dir).map_err(|e| format!("can't create {}: {e}", dir.display()))?;
    Ok(dir)
}

fn run(
    provider: Provider,
    system: &str,
    prompt: &str,
    tools: Option<&ToolServer>,
    tx: &Sender<AgentEvent>,
    cancel: &AtomicBool,
) -> Result<AgentAnswer, String> {
    let dir = scratch_dir()?;
    let mut cmd;
    let last_message = dir.join("codex-last-message.json");
    match provider {
        Provider::Claude => {
            cmd = Command::new("claude");
            cmd.args(["-p", prompt, "--output-format", "stream-json", "--verbose"])
                .args(["--json-schema", SCHEMA, "--system-prompt", system])
                .args(["--tools", "", "--strict-mcp-config", "--setting-sources", ""])
                .args(["--disable-slash-commands", "--no-session-persistence"]);
            // Only the game's own read-only tools; no other MCP servers load.
            if let Some(tools) = tools {
                let config = serde_json::json!({ "mcpServers": { SERVER_NAME: {
                    "type": "http",
                    "url": tools.url,
                    "headers": { "Authorization": format!("Bearer {}", tools.token) },
                }}});
                cmd.args(["--mcp-config", &config.to_string()])
                    .args(["--allowedTools", &format!("mcp__{SERVER_NAME}")]);
            }
        }
        Provider::Codex => {
            let schema = dir.join("build_spec.schema.json");
            std::fs::write(&schema, SCHEMA).map_err(|e| e.to_string())?;
            let _ = std::fs::remove_file(&last_message);
            cmd = Command::new("codex");
            cmd.args(["exec", "--json", "--ignore-user-config", "--ephemeral", "--skip-git-repo-check"])
                .args(["--sandbox", "read-only", "-c", "model_reasoning_effort=\"low\""]);
            if let Some(tools) = tools {
                cmd.arg("-c")
                    .arg(format!("mcp_servers.{SERVER_NAME}.url=\"{}\"", tools.url))
                    .arg("-c")
                    .arg(format!("mcp_servers.{SERVER_NAME}.bearer_token_env_var=\"CUBEWISH_MCP_TOKEN\""))
                    // Headless runs can't ask for approval; these tools are read-only.
                    .arg("-c")
                    .arg(format!("mcp_servers.{SERVER_NAME}.default_tools_approval_mode=\"approve\""))
                    .env("CUBEWISH_MCP_TOKEN", &tools.token);
            }
            cmd
                .arg("--output-schema")
                .arg(&schema)
                .arg("-o")
                .arg(&last_message)
                .arg("-C")
                .arg(&dir)
                // Codex has no system-prompt flag, so instructions lead the prompt.
                .arg(format!("{system}\n\n---\n\n{prompt}"));
        }
    }
    let mut child = cmd
        .current_dir(&dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("couldn't start {}: {e}. Is it installed and on PATH?", provider.label()))?;

    // Read stdout on its own thread so we can enforce the timeout and cancellation.
    let stdout = child.stdout.take().expect("stdout is piped");
    // Drain stderr too, or a chatty CLI could fill the pipe and stall.
    let stderr = child.stderr.take().expect("stderr is piped");
    let stderr = thread::spawn(move || {
        let mut tail = String::new();
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            tail.push_str(&line);
            tail.push('\n');
            if tail.len() > 4096 {
                let cut = tail.ceil_char_boundary(tail.len() - 4096);
                tail.drain(..cut);
            }
        }
        tail
    });
    let (line_tx, lines) = mpsc::channel::<String>();
    thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if line_tx.send(line).is_err() {
                break;
            }
        }
    });

    let deadline = Instant::now() + TIMEOUT;
    let mut answer: Option<Value> = None;
    let mut failure: Option<String> = None;
    loop {
        if cancel.load(Ordering::Relaxed) || Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(if cancel.load(Ordering::Relaxed) { "stopped".into() } else { "timed out".into() });
        }
        let line = match lines.recv_timeout(Duration::from_millis(200)) {
            Ok(line) => line,
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => break,
        };
        let Ok(event) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        match provider {
            Provider::Claude => match event["type"].as_str() {
                Some("assistant") => {
                    let tool = event["message"]["content"]
                        .as_array()
                        .and_then(|c| c.iter().find(|b| b["type"] == "tool_use"))
                        .and_then(|b| b["name"].as_str());
                    let status = match tool {
                        Some(name) if name.starts_with("mcp__") => {
                            format!("using {}…", name.rsplit("__").next().unwrap_or(name))
                        }
                        _ => "designing…".into(),
                    };
                    let _ = tx.send(AgentEvent::Status(status));
                }
                Some("result") => {
                    if event["is_error"].as_bool() == Some(true) {
                        failure = Some(event["result"].as_str().unwrap_or("Claude reported an error").to_string());
                    } else {
                        answer = Some(event["structured_output"].clone());
                    }
                }
                _ => {}
            },
            Provider::Codex => match event["type"].as_str() {
                Some("item.completed") if event["item"]["type"] == "reasoning" => {
                    let _ = tx.send(AgentEvent::Status("thinking…".into()));
                }
                Some("item.started") if event["item"]["type"] == "mcp_tool_call" => {
                    let tool = event["item"]["tool"].as_str().unwrap_or("a tool");
                    let _ = tx.send(AgentEvent::Status(format!("using {tool}…")));
                }
                Some("turn.failed") | Some("error") => {
                    let message = event["error"]["message"].as_str().or(event["message"].as_str());
                    failure = Some(message.unwrap_or("Codex reported an error").to_string());
                }
                _ => {}
            },
        }
    }
    let status = child.wait().map_err(|e| e.to_string())?;
    if let Some(failure) = failure {
        return Err(failure);
    }
    if provider == Provider::Codex {
        let text = std::fs::read_to_string(&last_message)
            .map_err(|_| format!("Codex exited ({status}) without an answer"))?;
        answer = Some(serde_json::from_str(&text).map_err(|e| format!("Codex answer wasn't JSON: {e}"))?);
    }
    let answer = answer.ok_or_else(|| {
        let stderr = stderr.join().unwrap_or_default();
        format!("{} exited ({status}) without an answer. {}", provider.label(), stderr.trim())
    })?;
    serde_json::from_value(answer).map_err(|e| format!("answer didn't match the build schema: {e}"))
}
