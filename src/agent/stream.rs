//! Reads what an agent CLI prints while it works (one JSON event a line) as the same few
//! activities whichever CLI it is, for the chat's live feed.

use std::{
    collections::HashSet,
    path::{Path, PathBuf},
};

use serde_json::Value;

/// What a tool call is for; the feed gives each its own icon.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// Reading or searching the world's files.
    Look,
    /// Changing the world's files.
    Edit,
    /// One of the game's own world tools.
    World,
    /// A shell command.
    Run,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Activity {
    Thinking,
    /// Something went wrong that it may get past. `retry`: a request to the model failed
    /// and is being tried again.
    Warning {
        text: String,
        retry: bool,
    },
    /// The agent starts saying something to the player.
    Note(String),
    /// More of what it's saying, as it's written.
    NoteMore(String),
    /// A tool call began: `what` is the file, search, tool or command.
    Tool {
        id: String,
        kind: Kind,
        what: String,
    },
    ToolDone {
        id: String,
        ok: bool,
    },
}

/// The reader for one run.
pub struct Stream {
    /// The working folder, as given and as the system spells it; paths show relative to it.
    roots: Vec<PathBuf>,
    /// Claude: the next piece of text starts a new note.
    fresh: bool,
    /// Codex: the last message, kept back until something follows it. The last one of
    /// all is the answer, which the chat shows itself.
    held: Option<String>,
    /// Codex: the calls announced as started.
    open: HashSet<String>,
}

impl Stream {
    pub fn new(dir: &Path) -> Self {
        let mut roots = vec![dir.to_path_buf()];
        roots.extend(dir.canonicalize().ok().filter(|real| real != dir));
        Self {
            roots,
            fresh: true,
            held: None,
            open: HashSet::new(),
        }
    }

    /// One event of `claude -p --output-format stream-json --include-partial-messages`.
    pub fn claude(&mut self, event: &Value) -> Vec<Activity> {
        let mut out = Vec::new();
        match event["type"].as_str() {
            // A request to the model failed and is tried again.
            Some("system") if event["subtype"] == "api_retry" => {
                let status = event["error_status"]
                    .as_u64()
                    .map(|s| format!("HTTP {s}, "))
                    .unwrap_or_default();
                let error = event["error"].as_str().unwrap_or("error");
                out.push(Activity::Warning {
                    text: format!(
                        "Retrying the model ({}/{}): {status}{error}",
                        event["attempt"].as_u64().unwrap_or(0),
                        event["max_retries"].as_u64().unwrap_or(0),
                    ),
                    retry: true,
                });
            }
            // Text arrives a few words at a time.
            Some("stream_event") => {
                let event = &event["event"];
                match (
                    event["type"].as_str(),
                    event["content_block"]["type"].as_str(),
                    event["delta"]["type"].as_str(),
                ) {
                    (Some("content_block_start"), Some("text"), _) => self.fresh = true,
                    (Some("content_block_start"), Some("thinking"), _) => {
                        out.push(Activity::Thinking)
                    }
                    (Some("content_block_delta"), _, Some("text_delta")) => {
                        let text = event["delta"]["text"]
                            .as_str()
                            .unwrap_or_default()
                            .to_string();
                        if !text.is_empty() {
                            out.push(if self.fresh {
                                Activity::Note(text)
                            } else {
                                Activity::NoteMore(text)
                            });
                            self.fresh = false;
                        }
                    }
                    _ => {}
                }
            }
            // The whole message, once written: its tool calls, with their inputs.
            Some("assistant") => {
                for block in event["message"]["content"].as_array().into_iter().flatten() {
                    let (Some("tool_use"), Some(id), Some(name)) = (
                        block["type"].as_str(),
                        block["id"].as_str(),
                        block["name"].as_str(),
                    ) else {
                        continue;
                    };
                    let input = &block["input"];
                    let text = |key: &str| input[key].as_str().unwrap_or_default();
                    let (kind, what) = match name {
                        "Read" => (Kind::Look, self.relative(text("file_path"))),
                        "Glob" => (Kind::Look, text("pattern").to_string()),
                        "Grep" => (Kind::Look, format!("“{}”", text("pattern"))),
                        "Edit" | "Write" => (Kind::Edit, self.relative(text("file_path"))),
                        // How the answer is handed in, not a step of the work.
                        "StructuredOutput" => continue,
                        name => (
                            Kind::World,
                            name.rsplit("__").next().unwrap_or(name).to_string(),
                        ),
                    };
                    out.push(Activity::Tool {
                        id: id.to_string(),
                        kind,
                        what: clip(&what),
                    });
                }
            }
            Some("user") => {
                for block in event["message"]["content"].as_array().into_iter().flatten() {
                    if let (Some("tool_result"), Some(id)) =
                        (block["type"].as_str(), block["tool_use_id"].as_str())
                    {
                        out.push(Activity::ToolDone {
                            id: id.to_string(),
                            ok: block["is_error"].as_bool() != Some(true),
                        });
                    }
                }
            }
            _ => {}
        }
        out
    }

    /// One event of `codex exec --json`. It reports whole items, not text as it's written.
    pub fn codex(&mut self, event: &Value) -> Vec<Activity> {
        let mut out = Vec::new();
        let started = match event["type"].as_str() {
            Some("item.started") => true,
            Some("item.completed") => false,
            // Reconnecting, or what ended the turn.
            Some("error") => {
                out.extend(event["message"].as_str().map(|m| warning(m, true)));
                return out;
            }
            _ => return out,
        };
        // Something follows the message kept back, so it wasn't the answer.
        out.extend(self.held.take().map(Activity::Note));
        let item = &event["item"];
        let id = item["id"].as_str().unwrap_or_default().to_string();
        let (kind, what, ok) = match item["type"].as_str() {
            Some("agent_message") => {
                if !started {
                    self.held = Some(said(item["text"].as_str().unwrap_or_default()))
                        .filter(|text| !text.is_empty());
                }
                return out;
            }
            Some("error") => {
                out.extend(item["message"].as_str().map(|m| warning(m, false)));
                return out;
            }
            Some("reasoning") => {
                out.push(Activity::Thinking);
                return out;
            }
            Some("command_execution") => (
                Kind::Run,
                command(item["command"].as_str().unwrap_or_default()),
                item["exit_code"].as_i64() == Some(0),
            ),
            Some("file_change") => {
                let paths: Vec<String> = item["changes"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|change| change["path"].as_str())
                    .map(|path| self.relative(path))
                    .collect();
                (Kind::Edit, paths.join(", "), item["status"] == "completed")
            }
            Some("mcp_tool_call") => (
                Kind::World,
                item["tool"].as_str().unwrap_or("a tool").to_string(),
                item["status"] == "completed",
            ),
            _ => return out,
        };
        // Some items are only reported once they're done.
        if started || !self.open.remove(&id) {
            out.push(Activity::Tool {
                id: id.clone(),
                kind,
                what: clip(&what),
            });
        }
        if started {
            self.open.insert(id);
        } else {
            out.push(Activity::ToolDone { id, ok });
        }
        out
    }

    /// A path as the player knows it: within the world's folder.
    fn relative(&self, path: &str) -> String {
        let path = Path::new(path);
        let inside = self
            .roots
            .iter()
            .find_map(|root| path.strip_prefix(root).ok());
        let shown = inside
            .or_else(|| path.file_name().map(Path::new))
            .unwrap_or(path);
        shown.display().to_string()
    }
}

/// What a Codex message says. With an answer schema, every message is JSON in the
/// answer's shape, with the words in "reply".
fn said(text: &str) -> String {
    let reply = serde_json::from_str::<Value>(text)
        .ok()
        .and_then(|json| json["reply"].as_str().map(str::to_string));
    reply.unwrap_or_else(|| text.to_string()).trim().to_string()
}

/// A command without the shell Codex wraps it in (`/bin/zsh -lc '…'`).
fn command(line: &str) -> String {
    let Some((_, inner)) = line.split_once(" -lc ") else {
        return line.to_string();
    };
    let quoted = |q: char| inner.strip_prefix(q).and_then(|rest| rest.strip_suffix(q));
    quoted('\'')
        .or_else(|| quoted('"'))
        .unwrap_or(inner)
        .to_string()
}

/// A warning's first line, at most a few lines long once wrapped.
fn warning(text: &str, retry: bool) -> Activity {
    let line = text.lines().next().unwrap_or_default();
    Activity::Warning {
        text: line.chars().take(300).collect(),
        retry,
    }
}

/// One line, short enough for the feed.
fn clip(text: &str) -> String {
    const MAX: usize = 72;
    let line = text.lines().next().unwrap_or_default();
    if line.chars().count() <= MAX && line.len() == text.len() {
        return line.to_string();
    }
    line.chars().take(MAX).chain(['…']).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tool(id: &str, kind: Kind, what: &str) -> Activity {
        Activity::Tool {
            id: id.into(),
            kind,
            what: what.into(),
        }
    }

    #[test]
    fn claude_text_streams_into_one_note_per_block() {
        let mut stream = Stream::new(Path::new("/world"));
        let start = json!({"type": "stream_event", "event": {"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}});
        let delta = |text: &str| json!({"type": "stream_event", "event": {"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": text}}});
        assert_eq!(stream.claude(&start), vec![]);
        assert_eq!(
            stream.claude(&delta("I'll read")),
            vec![Activity::Note("I'll read".into())]
        );
        assert_eq!(
            stream.claude(&delta(" the rules.")),
            vec![Activity::NoteMore(" the rules.".into())]
        );
        stream.claude(&start);
        assert_eq!(
            stream.claude(&delta("Now the wolves.")),
            vec![Activity::Note("Now the wolves.".into())]
        );
    }

    #[test]
    fn claude_tool_calls_name_what_they_touch() {
        let mut stream = Stream::new(Path::new("/world"));
        let message = json!({"type": "assistant", "message": {"content": [
            {"type": "text", "text": "already streamed"},
            {"type": "tool_use", "id": "a", "name": "Read", "input": {"file_path": "/world/packages/wolves.luau"}},
            {"type": "tool_use", "id": "b", "name": "Grep", "input": {"pattern": "howl"}},
            {"type": "tool_use", "id": "c", "name": "Write", "input": {"file_path": "/elsewhere/notes.md", "content": "x"}},
            {"type": "tool_use", "id": "d", "name": "mcp__genverse__test_game", "input": {}},
            {"type": "tool_use", "id": "e", "name": "StructuredOutput", "input": {"reply": "done"}},
        ]}});
        assert_eq!(
            stream.claude(&message),
            vec![
                tool("a", Kind::Look, "packages/wolves.luau"),
                tool("b", Kind::Look, "“howl”"),
                tool("c", Kind::Edit, "notes.md"),
                tool("d", Kind::World, "test_game"),
            ]
        );
        let results = json!({"type": "user", "message": {"content": [
            {"type": "tool_result", "tool_use_id": "a", "content": "…"},
            {"type": "tool_result", "tool_use_id": "c", "content": "denied", "is_error": true},
        ]}});
        assert_eq!(
            stream.claude(&results),
            vec![
                Activity::ToolDone {
                    id: "a".into(),
                    ok: true
                },
                Activity::ToolDone {
                    id: "c".into(),
                    ok: false
                }
            ]
        );
    }

    #[test]
    fn codex_items_become_steps_and_the_answer_stays_back() {
        let mut stream = Stream::new(Path::new("/world"));
        let events = [
            json!({"type": "turn.started"}),
            json!({"type": "item.completed", "item": {"id": "item_0", "type": "agent_message", "text": "{\"reply\":\"I'll read the note.\",\"build\":null}"}}),
            json!({"type": "item.started", "item": {"id": "item_1", "type": "command_execution", "command": "/bin/zsh -lc 'cat note.txt'", "exit_code": null, "status": "in_progress"}}),
            json!({"type": "item.completed", "item": {"id": "item_1", "type": "command_execution", "command": "/bin/zsh -lc 'cat note.txt'", "exit_code": 0, "status": "completed"}}),
            json!({"type": "item.completed", "item": {"id": "item_2", "type": "file_change", "changes": [{"path": "/world/note3.txt", "kind": "add"}], "status": "completed"}}),
            json!({"type": "item.started", "item": {"id": "item_3", "type": "mcp_tool_call", "tool": "test_game", "status": "in_progress"}}),
            json!({"type": "item.completed", "item": {"id": "item_3", "type": "mcp_tool_call", "tool": "test_game", "status": "failed"}}),
            json!({"type": "item.completed", "item": {"id": "item_4", "type": "agent_message", "text": "{\"reply\":\"Done.\",\"build\":null}"}}),
            json!({"type": "turn.completed"}),
        ];
        let seen: Vec<Activity> = events
            .iter()
            .flat_map(|event| stream.codex(event))
            .collect();
        assert_eq!(
            seen,
            vec![
                Activity::Note("I'll read the note.".into()),
                tool("item_1", Kind::Run, "cat note.txt"),
                Activity::ToolDone {
                    id: "item_1".into(),
                    ok: true
                },
                tool("item_2", Kind::Edit, "note3.txt"),
                Activity::ToolDone {
                    id: "item_2".into(),
                    ok: true
                },
                tool("item_3", Kind::World, "test_game"),
                Activity::ToolDone {
                    id: "item_3".into(),
                    ok: false
                },
            ]
        );
    }

    #[test]
    fn retries_and_errors_show_as_warnings() {
        let mut stream = Stream::new(Path::new("/w"));
        assert_eq!(
            stream.codex(&json!({"type": "error", "message": "Reconnecting... 1/5 (boom)"})),
            [Activity::Warning {
                text: "Reconnecting... 1/5 (boom)".into(),
                retry: true
            }]
        );
        assert_eq!(
            stream.codex(&json!({"type": "item.completed", "item": {"id": "item_0", "type": "error", "message": "no metadata"}})),
            [Activity::Warning { text: "no metadata".into(), retry: false }]
        );
        assert_eq!(
            stream.claude(&json!({"type": "system", "subtype": "api_retry", "attempt": 2, "max_retries": 10, "error_status": 401, "error": "authentication_failed"})),
            [Activity::Warning {
                text: "Retrying the model (2/10): HTTP 401, authentication_failed".into(),
                retry: true
            }]
        );
    }

    #[test]
    fn long_or_many_lined_steps_are_clipped() {
        assert_eq!(clip("short"), "short");
        assert_eq!(clip("first\nsecond"), "first…");
        assert_eq!(clip(&"x".repeat(100)).chars().count(), 73);
    }
}
