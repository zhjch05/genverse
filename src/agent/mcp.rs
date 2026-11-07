//! A minimal MCP server (streamable HTTP transport, JSON responses only) on
//! `127.0.0.1` with a random port and bearer token, so local agent CLIs can call the
//! game's read-only world tools.
//!
//! The HTTP thread never touches the ECS: each `tools/call` is forwarded to the game
//! as a [`ToolCall`] and answered by a Bevy system on the next frame.

use std::{
    collections::hash_map::RandomState,
    hash::{BuildHasher, Hasher},
    io::Read,
    sync::{
        Mutex,
        mpsc::{self, Receiver, Sender},
    },
    thread,
    time::Duration,
};

use bevy::prelude::*;
use serde_json::{Value, json};
use tiny_http::{Header, Response, Server};

pub const SERVER_NAME: &str = "cubelark";

/// One tool invocation waiting for the game to answer.
pub struct ToolCall {
    pub name: String,
    pub args: Value,
    pub reply: Sender<ToolResult>,
}

pub type ToolResult = Result<String, String>;

#[derive(Resource)]
pub struct McpServer {
    pub url: String,
    pub token: String,
    calls: Mutex<Receiver<ToolCall>>,
}

impl McpServer {
    /// Binds a random local port and starts serving `tools` (a `tools/list` array).
    pub fn start(tools: Value, instructions: &'static str) -> Result<Self, String> {
        let server = Server::http("127.0.0.1:0").map_err(|e| e.to_string())?;
        let port = server.server_addr().to_ip().map(|a| a.port()).ok_or("no TCP port")?;
        let token = random_token();
        let (tx, rx) = mpsc::channel();
        let expected = format!("Bearer {token}");
        thread::Builder::new()
            .name("mcp-server".into())
            .spawn(move || {
                for request in server.incoming_requests() {
                    serve(request, &expected, &tools, instructions, &tx);
                }
            })
            .map_err(|e| e.to_string())?;
        Ok(Self { url: format!("http://127.0.0.1:{port}/mcp"), token, calls: Mutex::new(rx) })
    }

    pub fn drain(&self) -> Vec<ToolCall> {
        self.calls.lock().map(|rx| rx.try_iter().collect()).unwrap_or_default()
    }
}

fn random_token() -> String {
    // RandomState is seeded from the OS per instance: good enough for a local token.
    (0..2)
        .map(|_| format!("{:016x}", RandomState::new().build_hasher().finish()))
        .collect()
}

fn serve(
    mut request: tiny_http::Request,
    expected_auth: &str,
    tools: &Value,
    instructions: &str,
    calls: &Sender<ToolCall>,
) {
    let respond = |request: tiny_http::Request, status: u16, body: Option<Value>| {
        let response = match body {
            Some(body) => Response::from_string(body.to_string())
                .with_header(Header::from_bytes("Content-Type", "application/json").expect("valid header")),
            None => Response::from_string(String::new()),
        };
        let _ = request.respond(response.with_status_code(status));
    };

    if request.url() != "/mcp" {
        return respond(request, 404, None);
    }
    let authorized = request
        .headers()
        .iter()
        .any(|h| h.field.equiv("Authorization") && h.value.as_str() == expected_auth);
    if !authorized {
        return respond(request, 401, None);
    }
    // No server-initiated streams or sessions: only POSTed JSON-RPC.
    if request.method() != &tiny_http::Method::Post {
        return respond(request, 405, None);
    }
    let mut body = String::new();
    if request.as_reader().take(1 << 20).read_to_string(&mut body).is_err() {
        return respond(request, 400, None);
    }
    let Ok(message) = serde_json::from_str::<Value>(&body) else {
        return respond(request, 400, Some(rpc_error(Value::Null, -32700, "parse error")));
    };
    let Some(id) = message.get("id").cloned() else {
        // Notifications (e.g. notifications/initialized) need no answer.
        return respond(request, 202, None);
    };
    let params = message.get("params").cloned().unwrap_or(Value::Null);
    let result = match message["method"].as_str().unwrap_or_default() {
        "initialize" => json!({
            "protocolVersion": params["protocolVersion"].as_str().unwrap_or("2025-06-18"),
            "capabilities": { "tools": { "listChanged": false } },
            "serverInfo": { "name": SERVER_NAME, "version": env!("CARGO_PKG_VERSION") },
            "instructions": instructions,
        }),
        "ping" => json!({}),
        "tools/list" => json!({ "tools": tools }),
        "tools/call" => {
            let (reply, answer) = mpsc::channel();
            let call = ToolCall {
                name: params["name"].as_str().unwrap_or_default().to_string(),
                args: params.get("arguments").cloned().unwrap_or(json!({})),
                reply,
            };
            let outcome = if calls.send(call).is_ok() {
                answer.recv_timeout(Duration::from_secs(20)).unwrap_or(Err("the game didn't answer in time".into()))
            } else {
                Err("the game is shutting down".into())
            };
            let (text, is_error) = match outcome {
                Ok(text) => (text, false),
                Err(text) => (text, true),
            };
            json!({ "content": [{ "type": "text", "text": text }], "isError": is_error })
        }
        other => return respond(request, 200, Some(rpc_error(id, -32601, &format!("unknown method {other}")))),
    };
    respond(request, 200, Some(json!({ "jsonrpc": "2.0", "id": id, "result": result })));
}

fn rpc_error(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

#[cfg(test)]
mod tests {
    use std::{
        io::{Read, Write},
        net::TcpStream,
    };

    use super::*;

    /// Sends one HTTP POST and returns (status, body).
    fn post(url: &str, auth: Option<&str>, body: &Value) -> (u16, String) {
        let addr = url.trim_start_matches("http://").trim_end_matches("/mcp");
        let mut stream = TcpStream::connect(addr).unwrap();
        let body = body.to_string();
        let auth = auth.map(|t| format!("Authorization: Bearer {t}\r\n")).unwrap_or_default();
        write!(
            stream,
            "POST /mcp HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\n{auth}\
             Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        let status = response[9..12].parse().unwrap();
        let body = response.split("\r\n\r\n").nth(1).unwrap_or_default().to_string();
        (status, body)
    }

    #[test]
    fn handshake_tools_and_auth() {
        let server = McpServer::start(json!([{ "name": "echo" }]), "test").unwrap();
        let init = json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": { "protocolVersion": "2025-06-18" } });

        assert_eq!(post(&server.url, None, &init).0, 401);
        assert_eq!(post(&server.url, Some("wrong"), &init).0, 401);

        let (status, body) = post(&server.url, Some(&server.token), &init);
        assert_eq!(status, 200);
        let body: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(body["result"]["serverInfo"]["name"], SERVER_NAME);

        let note = json!({ "jsonrpc": "2.0", "method": "notifications/initialized" });
        assert_eq!(post(&server.url, Some(&server.token), &note).0, 202);

        let list = json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" });
        let (_, body) = post(&server.url, Some(&server.token), &list);
        assert_eq!(serde_json::from_str::<Value>(&body).unwrap()["result"]["tools"][0]["name"], "echo");

        // A tool call is answered by whoever drains the queue (the game, here a thread).
        let url = server.url.clone();
        let token = server.token.clone();
        let caller = thread::spawn(move || {
            let call = json!({ "jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": { "name": "echo", "arguments": { "x": 1 } } });
            post(&url, Some(&token), &call).1
        });
        let call = loop {
            if let Some(call) = server.drain().pop() {
                break call;
            }
            thread::sleep(Duration::from_millis(10));
        };
        assert_eq!(call.args["x"], 1);
        call.reply.send(Ok("pong".into())).unwrap();
        let body: Value = serde_json::from_str(&caller.join().unwrap()).unwrap();
        assert_eq!(body["result"]["content"][0]["text"], "pong");
        assert_eq!(body["result"]["isError"], false);
    }
}
