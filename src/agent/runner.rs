//! Runs a local coding-agent CLI headless, answering with JSON that matches the build
//! schema.
//!
//! It works in the world's folder ([`super::workspace`]): it may read and search it, and
//! edit files there (the game's packages), and use the game's world tools, but nothing
//! else: no shell, no other MCP servers, and of the player's own config only which model
//! and provider to use ([`super::config`]).

use std::{
    io::{BufRead, BufReader},
    path::PathBuf,
    process::Stdio,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, RecvTimeoutError, Sender},
    },
    thread,
    time::{Duration, Instant},
};

use serde_json::Value;

use super::{
    config,
    mcp::{McpServer, SERVER_NAME},
    models::Choice,
    spec::{AgentAnswer, SCHEMA},
    stream::{Activity, Stream},
};

/// How long one request may take. A world game is designed, built and played through
/// with `test_game` before it's answered, which takes a while; Stop ends it sooner.
const TIMEOUT: Duration = Duration::from_secs(900);

/// How long the model may stay silent before its first word, thought or tool call. An
/// endpoint that takes the request and never answers would otherwise hold the chat for
/// the whole [`TIMEOUT`] while the CLI waits on it.
const FIRST_SIGN: Duration = Duration::from_secs(240);
/// The same, once a request to the model has failed and is being retried.
const FIRST_SIGN_AFTER_ERROR: Duration = Duration::from_secs(90);

/// Codex's reasoning level when nothing else names one: builds are many small steps,
/// so faster beats deeper.
pub const CODEX_EFFORT: &str = "low";

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
    /// Something it did or said while working, as it happens.
    Activity(Activity),
    Finished(Box<Result<AgentAnswer, RunError>>),
}

/// Why a request ended without an answer.
#[derive(Debug)]
pub enum RunError {
    /// It answered, but not with JSON that matches the build schema. Its work in the
    /// folder is done; it can be asked for the answer again.
    Malformed {
        /// What it answered, as it came.
        raw: String,
        /// What's wrong with it.
        why: String,
    },
    /// The player stopped it.
    Stopped,
    /// Anything else: it couldn't start, failed or timed out.
    Failed(String),
}

impl std::fmt::Display for RunError {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            RunError::Malformed { why, .. } => f.write_str(why),
            RunError::Stopped => f.write_str("stopped"),
            RunError::Failed(why) => f.write_str(why),
        }
    }
}

impl From<String> for RunError {
    fn from(why: String) -> Self {
        RunError::Failed(why)
    }
}

impl From<&str> for RunError {
    fn from(why: &str) -> Self {
        RunError::Failed(why.to_string())
    }
}

/// A running agent request. Dropping it doesn't stop the process; call [`AgentJob::cancel`].
pub struct AgentJob {
    pub events: Receiver<AgentEvent>,
    cancel: Arc<AtomicBool>,
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

/// Starts a request, in `folder` (the world's, which it may read and edit) or an empty
/// scratch folder.
pub fn start(
    provider: Provider,
    choice: Choice,
    system: String,
    prompt: String,
    tools: Option<&McpServer>,
    folder: Option<PathBuf>,
) -> AgentJob {
    let (tx, rx) = mpsc::channel();
    let cancel = Arc::new(AtomicBool::new(false));
    let flag = cancel.clone();
    let tools = tools.map(|t| ToolServer {
        url: t.url.clone(),
        token: t.token.clone(),
    });
    thread::spawn(move || {
        let result = run(
            provider,
            &choice,
            &system,
            &prompt,
            tools.as_ref(),
            folder,
            &tx,
            &flag,
        );
        let _ = tx.send(AgentEvent::Finished(Box::new(result)));
    });
    AgentJob { events: rx, cancel }
}

/// The agent CLI's full path, or what to do about it missing.
fn program(name: &str, provider: Provider) -> Result<PathBuf, String> {
    crate::platform::find_program(name).ok_or_else(|| {
        let install = match provider {
            Provider::Claude => "https://code.claude.com/docs/en/setup",
            Provider::Codex => "https://github.com/openai/codex",
        };
        format!("{} isn't installed: `{name}` isn't on your PATH. Install it ({install}), sign in once from a terminal, then try again.", provider.label())
    })
}

fn scratch_dir() -> Result<PathBuf, String> {
    let dir = std::env::temp_dir().join("genverse-agent");
    std::fs::create_dir_all(&dir).map_err(|e| format!("can't create {}: {e}", dir.display()))?;
    Ok(dir)
}

fn run(
    provider: Provider,
    choice: &Choice,
    system: &str,
    prompt: &str,
    tools: Option<&ToolServer>,
    folder: Option<PathBuf>,
    tx: &Sender<AgentEvent>,
    cancel: &AtomicBool,
) -> Result<AgentAnswer, RunError> {
    let scratch = scratch_dir()?;
    let files = folder.is_some();
    let dir = folder.unwrap_or_else(|| scratch.clone());
    let mut cmd;
    let last_message = scratch.join("codex-last-message.json");
    // With the world tools, the answer is handed in with `submit_answer`, which refuses a
    // malformed one while the agent can still fix it: not only JSON that breaks the schema
    // but anything the game can't read as an answer. Its last message is then a line.
    // Without them, the CLI is held to the schema (`--json-schema`, `--output-schema`).
    let handing_in = tools.is_some();
    let system = if handing_in {
        format!("{system}\n\n{}", super::prompt::SUBMIT_NOTE)
    } else {
        system.to_string()
    };
    let system = system.as_str();
    match provider {
        Provider::Claude => {
            cmd = crate::platform::command(&program("claude", provider)?);
            // Partial messages: what it says arrives as it's written, for the live feed.
            cmd.args([
                "-p",
                prompt,
                "--output-format",
                "stream-json",
                "--verbose",
                "--include-partial-messages",
            ])
            .args(["--system-prompt", system])
            .args(["--strict-mcp-config", "--setting-sources", ""])
            .args(["--disable-slash-commands", "--no-session-persistence"]);
            // Of the player's settings, only how to reach their model.
            if !handing_in {
                cmd.args(["--json-schema", SCHEMA]);
            }
            if let Some(settings) = config::claude().settings {
                cmd.args(["--settings", &settings.to_string()]);
            }
            if let Some(model) = &choice.model {
                cmd.args(["--model", model]);
            }
            if let Some(effort) = &choice.effort {
                cmd.args(["--effort", effort]);
            }
            // In the world's folder: reading and searching it, and editing files in it
            // (edits are accepted only inside the working folder).
            if files {
                cmd.args([
                    "--tools",
                    "Read,Glob,Grep,Edit,Write",
                    "--permission-mode",
                    "acceptEdits",
                ]);
            } else {
                cmd.args(["--tools", ""]);
            }
            // Only the game's own tools; no other MCP servers load.
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
            let schema = scratch.join("build_spec.schema.json");
            std::fs::write(&schema, SCHEMA).map_err(|e| e.to_string())?;
            let _ = std::fs::remove_file(&last_message);
            cmd = crate::platform::command(&program("codex", provider)?);
            cmd.args([
                "exec",
                "--json",
                "--ignore-user-config",
                "--ephemeral",
                "--skip-git-repo-check",
            ])
            .args([
                "--sandbox",
                if files {
                    "workspace-write"
                } else {
                    "read-only"
                },
            ]);
            // The player's config is left out, all but how to reach their model.
            let user = config::codex();
            for setting in &user.overrides {
                cmd.arg("-c").arg(setting);
            }
            if let Some(model) = &choice.model {
                cmd.arg("-m").arg(model);
            }
            // On their own endpoint, the level they set for it (or the model's own), since
            // it may not take ours.
            let effort = choice.effort.clone().or(match user.custom_provider {
                Some(_) => user.effort,
                None => Some(CODEX_EFFORT.into()),
            });
            if let Some(effort) = effort {
                cmd.arg("-c").arg(format!(
                    "model_reasoning_effort={}",
                    toml::Value::String(effort)
                ));
            }
            if let Some(tools) = tools {
                cmd.arg("-c")
                    .arg(format!("mcp_servers.{SERVER_NAME}.url=\"{}\"", tools.url))
                    .arg("-c")
                    .arg(format!(
                        "mcp_servers.{SERVER_NAME}.bearer_token_env_var=\"GENVERSE_MCP_TOKEN\""
                    ))
                    // Headless runs can't ask for approval; the game asks the player itself
                    // about the tools that need it.
                    .arg("-c")
                    .arg(format!(
                        "mcp_servers.{SERVER_NAME}.default_tools_approval_mode=\"approve\""
                    ))
                    .env("GENVERSE_MCP_TOKEN", &tools.token);
            }
            // Codex doesn't hold its last message to `--output-schema` itself (that's up to
            // the model's provider), so it's only the fallback without the world tools.
            if !handing_in {
                cmd.arg("--output-schema").arg(&schema);
            }
            cmd.arg("-o")
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
        .map_err(|e| {
            format!(
                "couldn't start {}: {e}. Is it installed and on PATH?",
                provider.label()
            )
        })?;

    // Read stdout on its own thread so we can enforce the timeout and cancellation.
    let stdout = child.stdout.take().expect("stdout is piped");
    // Drain stderr too, or a chatty CLI could fill the pipe and stall.
    // Its tail is shared rather than joined: a CLI started through a wrapper (npm's
    // `codex`) leaves the real program holding the pipe open after the wrapper is killed.
    let stderr = child.stderr.take().expect("stderr is piped");
    let tail = Arc::new(Mutex::new(String::new()));
    let stderr = {
        let tail = tail.clone();
        thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                let Ok(mut tail) = tail.lock() else { break };
                tail.push_str(&line);
                tail.push('\n');
                if tail.len() > 4096 {
                    let cut = tail.ceil_char_boundary(tail.len() - 4096);
                    tail.drain(..cut);
                }
            }
        })
    };
    let (line_tx, lines) = mpsc::channel::<String>();
    thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if line_tx.send(line).is_err() {
                break;
            }
        }
    });

    let started = Instant::now();
    let deadline = started + TIMEOUT;
    let mut answer: Option<Value> = None;
    // Claude's last message, when it wasn't held to the schema.
    let mut final_text: Option<String> = None;
    let mut failure: Option<String> = None;
    // The latest retry or error, to say why if it never gets through.
    let mut warning: Option<String> = None;
    let mut retrying = false;
    // The model has answered something: a word, a thought or a tool call.
    let mut heard = false;
    let mut stream = Stream::new(&dir);
    loop {
        let silent = !heard
            && started.elapsed()
                > if retrying {
                    FIRST_SIGN_AFTER_ERROR
                } else {
                    FIRST_SIGN
                };
        if cancel.load(Ordering::Relaxed) || Instant::now() > deadline || silent {
            let _ = child.kill();
            let _ = child.wait();
            if cancel.load(Ordering::Relaxed) {
                return Err(RunError::Stopped);
            }
            let stderr = tail.lock().map(|t| t.clone()).unwrap_or_default();
            let why = warning
                .or_else(|| stderr.trim().lines().last().map(str::to_string))
                .map(|w| format!(" Last: {w}"))
                .unwrap_or_default();
            return Err(RunError::Failed(if silent {
                format!(
                    "no answer from the model after {}s.{why} Check that {} can reach the model you use (its provider, endpoint and key).",
                    started.elapsed().as_secs(),
                    provider.label()
                )
            } else {
                format!("timed out.{why}")
            }));
        }
        let line = match lines.recv_timeout(Duration::from_millis(200)) {
            Ok(line) => line,
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => break,
        };
        let Ok(event) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let activities = match provider {
            Provider::Claude => stream.claude(&event),
            Provider::Codex => stream.codex(&event),
        };
        for activity in activities {
            match &activity {
                Activity::Warning { text, retry } => {
                    warning = Some(text.clone());
                    retrying |= retry;
                }
                _ => heard = true,
            }
            let _ = tx.send(AgentEvent::Activity(activity));
        }
        match (provider, event["type"].as_str()) {
            (Provider::Claude, Some("result")) => {
                if event["is_error"].as_bool() == Some(true) {
                    let why = event["result"]
                        .as_str()
                        .map(str::to_string)
                        .unwrap_or_else(|| {
                            let kind = event["subtype"].as_str().unwrap_or("error");
                            format!(
                                "Claude reported an error ({kind}: {})",
                                event["errors"]
                                    .to_string()
                                    .chars()
                                    .take(300)
                                    .collect::<String>()
                            )
                        });
                    failure = Some(why);
                } else if !event["structured_output"].is_null() {
                    answer = Some(event["structured_output"].clone());
                } else {
                    // No schema to hold it to (it handed its answer in): its last words.
                    final_text = event["result"].as_str().map(str::to_string);
                }
            }
            // An `error` alone may be a retry that gets through; it's kept as the warning.
            (Provider::Codex, Some("turn.failed")) => {
                let message = event["error"]["message"]
                    .as_str()
                    .or(event["message"].as_str());
                failure = Some(message.unwrap_or("Codex reported an error").to_string());
            }
            _ => {}
        }
    }
    let status = child.wait().map_err(|e| e.to_string())?;
    if let Some(failure) = failure {
        return Err(RunError::Failed(failure));
    }
    if provider == Provider::Codex {
        final_text = Some(std::fs::read_to_string(&last_message).map_err(|_| {
            let why = warning
                .as_deref()
                .map(|w| format!(": {w}"))
                .unwrap_or_default();
            format!("Codex exited ({status}) without an answer{why}")
        })?);
    }
    if answer.is_none()
        && let Some(text) = final_text
    {
        answer = Some(
            serde_json::from_str(&text).map_err(|e| RunError::Malformed {
                why: format!("{} answer wasn't JSON: {e}", provider.label()),
                raw: text.clone(),
            })?,
        );
    }
    let answer = answer.ok_or_else(|| {
        let _ = stderr.join();
        let stderr = tail.lock().map(|t| t.clone()).unwrap_or_default();
        format!(
            "{} exited ({status}) without an answer. {}",
            provider.label(),
            stderr.trim()
        )
    })?;
    let raw = answer.to_string();
    serde_json::from_value(answer).map_err(|e| RunError::Malformed {
        why: format!("answer didn't match the build schema: {e}"),
        raw,
    })
}
