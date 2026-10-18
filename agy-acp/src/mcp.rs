//! ACP `mcpServers` support for agy.
//!
//! agy has no per-invocation MCP config: it reads only the global
//! `~/.gemini/config/mcp_config.json` (plus a workspace `.agents/mcp_config.json`), and it does
//! not expand `${VAR}` in http headers. A server handed to us in `session/new` /
//! `session/load` usually carries a per-session credential (openab's facade token), and
//! several sessions share one HOME and one workspace, so writing the server into agy's config
//! verbatim would let sessions overwrite each other's credentials.
//!
//! Instead the static config holds one stdio entry per server name that points back at this
//! binary (`agy-acp mcp-bridge <name>`), and the per-session part — url + headers — rides the
//! env of that session's `agy -p` child as `AGY_ACP_MCP_<NAME>`. agy spawns the bridge as its
//! own child, so the bridge inherits exactly its session's values and relays stdio JSON-RPC to
//! the Streamable HTTP server. A session that was not handed the server gets an empty server
//! (no tools) instead of someone else's.

use fs2::FileExt;
use serde_json::{json, Map, Value};
use std::fs;
use std::io::{self, BufRead, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Subcommand that runs the stdio bridge: `agy-acp mcp-bridge <name>`.
pub const BRIDGE_SUBCOMMAND: &str = "mcp-bridge";

/// An http MCP server from ACP `mcpServers`.
#[derive(Debug, Clone, PartialEq)]
pub struct HttpMcpServer {
    pub name: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
}

/// Pick the http servers out of an ACP `session/new` / `session/load` params object.
/// stdio and sse entries are skipped: we advertise only `mcpCapabilities.http`.
pub fn parse_http_servers(params: &Value) -> Vec<HttpMcpServer> {
    let Some(list) = params.get("mcpServers").and_then(|v| v.as_array()) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in list {
        let kind = entry
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap_or("stdio");
        let name = entry.get("name").and_then(|v| v.as_str()).unwrap_or("");
        let url = entry.get("url").and_then(|v| v.as_str()).unwrap_or("");
        if kind != "http" || name.is_empty() || url.is_empty() {
            eprintln!("[agy-acp] skipping unsupported mcpServers entry (type={kind}, name={name})");
            continue;
        }
        let headers = entry
            .get("headers")
            .and_then(|v| v.as_array())
            .map(|hs| {
                hs.iter()
                    .filter_map(|h| {
                        Some((
                            h.get("name")?.as_str()?.to_string(),
                            h.get("value")?.as_str()?.to_string(),
                        ))
                    })
                    .collect()
            })
            .unwrap_or_default();
        out.push(HttpMcpServer {
            name: name.to_string(),
            url: url.to_string(),
            headers,
        });
    }
    out
}

/// Env var carrying one server's url + headers to its bridge.
pub fn env_key(name: &str) -> String {
    let sanitized: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect();
    format!("AGY_ACP_MCP_{sanitized}")
}

/// The env a session's `agy -p` child needs so each bridge finds its server.
pub fn session_env(servers: &[HttpMcpServer]) -> Vec<(String, String)> {
    servers
        .iter()
        .map(|s| {
            let headers: Vec<Value> = s.headers.iter().map(|(k, v)| json!([k, v])).collect();
            (
                env_key(&s.name),
                json!({ "url": s.url, "headers": headers }).to_string(),
            )
        })
        .collect()
}

pub fn agy_mcp_config_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(home).join(".gemini/config/mcp_config.json")
}

fn is_bridge_entry(entry: &Value) -> bool {
    let command = entry.get("command").and_then(|v| v.as_str()).unwrap_or("");
    let first_arg = entry
        .get("args")
        .and_then(|v| v.get(0))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    command.ends_with("agy-acp") && first_arg == BRIDGE_SUBCOMMAND
}

/// Make sure agy's config has a bridge entry for each name. Idempotent; never clobbers an
/// entry that is not one of our bridges (it is left alone with a warning).
pub fn ensure_bridge_entries(config_path: &Path, exe: &str, names: &[String]) -> io::Result<()> {
    if names.is_empty() {
        return Ok(());
    }
    if let Some(parent) = config_path.parent() {
        fs::create_dir_all(parent)?;
    }
    // Several agy-acp processes (one per broker session) may start at once.
    let lock = fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(config_path.with_extension("agy-acp.lock"))?;
    lock.lock_exclusive()?;

    let mut root: Value = match fs::read_to_string(config_path) {
        Ok(s) if !s.trim().is_empty() => serde_json::from_str(&s).map_err(|e| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{}: {e}", config_path.display()),
            )
        })?,
        Ok(_) => json!({}),
        Err(e) if e.kind() == io::ErrorKind::NotFound => json!({}),
        Err(e) => return Err(e),
    };
    let Some(obj) = root.as_object_mut() else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "mcp_config.json is not an object",
        ));
    };
    let servers = obj
        .entry("mcpServers")
        .or_insert_with(|| Value::Object(Map::new()));
    let Some(servers) = servers.as_object_mut() else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "mcpServers is not an object",
        ));
    };

    let mut changed = false;
    for name in names {
        let want = json!({ "command": exe, "args": [BRIDGE_SUBCOMMAND, name], "disabled": false });
        match servers.get(name) {
            Some(existing) if *existing == want => {}
            Some(existing) if !is_bridge_entry(existing) => {
                eprintln!("[agy-acp] WARN: agy MCP server {name:?} is user-defined; not replacing it with the session bridge");
            }
            _ => {
                servers.insert(name.clone(), want);
                changed = true;
            }
        }
    }
    if changed {
        let tmp = config_path.with_extension("json.agy-acp.tmp");
        fs::write(
            &tmp,
            serde_json::to_string_pretty(&root).unwrap_or_default(),
        )?;
        fs::rename(&tmp, config_path)?;
    }
    Ok(())
}

// --- Bridge ---------------------------------------------------------------------------------

struct Target {
    host: String,
    port: u16,
    path: String,
    headers: Vec<(String, String)>,
}

fn parse_target(raw: &str) -> Result<Target, String> {
    let cfg: Value = serde_json::from_str(raw).map_err(|e| format!("bad bridge config: {e}"))?;
    let url = cfg
        .get("url")
        .and_then(|v| v.as_str())
        .ok_or("bridge config has no url")?;
    let rest = url
        .strip_prefix("http://")
        .ok_or_else(|| format!("unsupported MCP url (only http:// is bridged): {url}"))?;
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let bad_port = || format!("bad port in {url}");
    let (host, port) = if let Some(v6) = authority.strip_prefix('[') {
        // `[::1]:8848` — the brackets are URL syntax, not part of the address.
        let (addr, rest) = v6
            .split_once(']')
            .ok_or_else(|| format!("bad IPv6 host in {url}"))?;
        let port = match rest.strip_prefix(':') {
            Some(p) => p.parse::<u16>().map_err(|_| bad_port())?,
            None => 80,
        };
        (addr.to_string(), port)
    } else {
        match authority.rsplit_once(':') {
            Some((h, p)) => (h.to_string(), p.parse::<u16>().map_err(|_| bad_port())?),
            None => (authority.to_string(), 80),
        }
    };
    let headers = cfg
        .get("headers")
        .and_then(|v| v.as_array())
        .map(|hs| {
            hs.iter()
                .filter_map(|h| {
                    Some((
                        h.get(0)?.as_str()?.to_string(),
                        h.get(1)?.as_str()?.to_string(),
                    ))
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(Target {
        host,
        port,
        path: path.to_string(),
        headers,
    })
}

struct HttpResponse {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl HttpResponse {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

fn decode_chunked(mut data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    while let Some(eol) = data.windows(2).position(|w| w == b"\r\n") {
        let size_str = String::from_utf8_lossy(&data[..eol]);
        let size =
            usize::from_str_radix(size_str.split(';').next().unwrap_or("").trim(), 16).unwrap_or(0);
        data = &data[eol + 2..];
        if size == 0 || data.len() < size {
            out.extend_from_slice(&data[..size.min(data.len())]);
            break;
        }
        out.extend_from_slice(&data[..size]);
        data = data.get(size + 2..).unwrap_or(&[]);
    }
    out
}

/// Per-request read timeout. A bit above openab's default `[mcp] tunnel_timeout_seconds` (170s),
/// so a hung server fails the call instead of wedging the bridge. `AGY_ACP_MCP_TIMEOUT_SECS`
/// overrides it.
fn request_timeout() -> Duration {
    let secs = std::env::var("AGY_ACP_MCP_TIMEOUT_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|&v| v > 0)
        .unwrap_or(240);
    Duration::from_secs(secs)
}

/// One POST, `Connection: close`, read to EOF. The facade ends each response stream once the
/// request is answered, so reading to EOF is enough.
fn post(target: &Target, session: Option<&str>, body: &str) -> io::Result<HttpResponse> {
    let mut stream = TcpStream::connect((target.host.as_str(), target.port))?;
    stream.set_read_timeout(Some(request_timeout()))?;
    let mut req = format!(
        "POST {} HTTP/1.1\r\nHost: {}:{}\r\nContent-Type: application/json\r\nAccept: application/json, text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n",
        target.path,
        target.host,
        target.port,
        body.len()
    );
    for (k, v) in &target.headers {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    if let Some(sid) = session {
        req.push_str(&format!("Mcp-Session-Id: {sid}\r\n"));
    }
    req.push_str("\r\n");
    stream.write_all(req.as_bytes())?;
    stream.write_all(body.as_bytes())?;

    let mut raw = Vec::new();
    stream.read_to_end(&mut raw)?;
    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "no HTTP header terminator"))?;
    let head = String::from_utf8_lossy(&raw[..split]).to_string();
    let mut lines = head.split("\r\n");
    let status = lines
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let headers: Vec<(String, String)> = lines
        .filter_map(|l| {
            l.split_once(':')
                .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
        })
        .collect();
    let mut resp = HttpResponse {
        status,
        headers,
        body: raw[split + 4..].to_vec(),
    };
    if resp
        .header("transfer-encoding")
        .is_some_and(|v| v.eq_ignore_ascii_case("chunked"))
    {
        resp.body = decode_chunked(&resp.body);
    }
    Ok(resp)
}

/// JSON-RPC messages in a response body: SSE `data:` events, or a plain JSON body.
fn messages_from(resp: &HttpResponse) -> Vec<Value> {
    let text = String::from_utf8_lossy(&resp.body);
    let sse = resp
        .header("content-type")
        .is_some_and(|v| v.contains("text/event-stream"));
    if !sse {
        return serde_json::from_str::<Value>(text.trim())
            .into_iter()
            .collect();
    }
    text.replace("\r\n", "\n")
        .split("\n\n")
        .filter_map(|event| {
            let data: Vec<&str> = event
                .lines()
                .filter_map(|l| l.strip_prefix("data:"))
                .map(|d| d.strip_prefix(' ').unwrap_or(d))
                .collect();
            let data = data.join("\n");
            if data.trim().is_empty() {
                None
            } else {
                serde_json::from_str(&data).ok()
            }
        })
        .collect()
}

fn is_reply_to(msg: &Value, id: &Value) -> bool {
    msg.get("id") == Some(id) && (msg.get("result").is_some() || msg.get("error").is_some())
}

/// Reply for a session that was not handed this server: an empty server, so agy starts
/// cleanly with no tools instead of hanging or seeing another session's server.
fn empty_server_reply(msg: &Value) -> Option<Value> {
    let id = msg.get("id")?.clone();
    let result = match msg.get("method").and_then(|v| v.as_str()).unwrap_or("") {
        "initialize" => json!({
            "protocolVersion": msg.pointer("/params/protocolVersion").cloned().unwrap_or(json!("2025-06-18")),
            "capabilities": { "tools": {} },
            "serverInfo": { "name": "agy-acp-empty-bridge", "version": env!("CARGO_PKG_VERSION") },
        }),
        "tools/list" => json!({ "tools": [] }),
        "ping" => json!({}),
        _ => {
            return Some(
                json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32601, "message": "Method not found" } }),
            )
        }
    };
    Some(json!({ "jsonrpc": "2.0", "id": id, "result": result }))
}

/// Bridge state shared by the request threads.
struct Bridge {
    name: String,
    target: Target,
    /// `Mcp-Session-Id` handed out by the server on `initialize`.
    session: Mutex<Option<String>>,
    /// The client's `initialize`, replayed if the server expires our session (rmcp's local
    /// session manager reaps idle sessions; a long shell step between two tool calls is enough).
    initialize: Mutex<Option<String>>,
}

impl Bridge {
    fn send(&self, body: &str) -> io::Result<HttpResponse> {
        let sid = self.session.lock().unwrap().clone();
        let resp = post(&self.target, sid.as_deref(), body)?;
        if let Some(new_sid) = resp.header("mcp-session-id") {
            *self.session.lock().unwrap() = Some(new_sid.to_string());
        }
        Ok(resp)
    }

    /// The server dropped our session (HTTP 404 with a session id): open a fresh one by
    /// replaying `initialize` + `notifications/initialized`. Returns whether it worked.
    fn reinitialize(&self) -> bool {
        let Some(init) = self.initialize.lock().unwrap().clone() else {
            return false;
        };
        *self.session.lock().unwrap() = None;
        eprintln!(
            "[agy-acp mcp-bridge] {}: server session expired; re-initializing",
            self.name
        );
        let ok = self.send(&init).is_ok_and(|r| r.status < 400);
        if ok {
            let _ = self.send(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#);
        }
        ok
    }

    /// Relay one message; returns whatever JSON-RPC messages came back, guaranteeing a reply for
    /// a request.
    fn relay(&self, line: &str, msg: &Value) -> Vec<Value> {
        let method = msg.get("method").and_then(|v| v.as_str()).unwrap_or("?");
        let had_session = self.session.lock().unwrap().is_some();
        let mut result = self.send(line);
        if had_session
            && method != "initialize"
            && matches!(&result, Ok(r) if r.status == 404)
            && self.reinitialize()
        {
            result = self.send(line);
        }
        let (mut replies, failure) = match result {
            Ok(resp) => {
                let replies = messages_from(&resp);
                let failure =
                    (resp.status >= 400).then(|| (resp.status, format!("HTTP {}", resp.status)));
                (replies, failure)
            }
            Err(e) => (Vec::new(), Some((0, e.to_string()))),
        };
        if let Some((_, why)) = &failure {
            if replies.is_empty() {
                eprintln!("[agy-acp mcp-bridge] {}: {why} for {method}", self.name);
            }
        }
        if let Some(id) = msg.get("id") {
            if !replies.iter().any(|r| is_reply_to(r, id)) {
                replies.push(synthesized_error(id, method, failure));
            }
        }
        replies
    }
}

/// Error for a request the server did not answer. A method the server rejects outright
/// (`server/discover` → HTTP 422 from servers that predate it) is "method not found" so the
/// client falls back; anything else is an internal error naming the cause, so a failed
/// `tools/call` does not masquerade as a missing tool.
fn synthesized_error(id: &Value, method: &str, failure: Option<(u16, String)>) -> Value {
    let (code, message) = match failure {
        Some((422, _)) => (-32601, "Method not found".to_string()),
        None if method == "server/discover" => (-32601, "Method not found".to_string()),
        Some((_, why)) => (-32603, format!("MCP server unavailable: {why}")),
        None => (-32603, "MCP server returned no response".to_string()),
    };
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

fn write_lines(out: &Mutex<io::Stdout>, replies: Vec<Value>) {
    let out = out.lock().unwrap();
    let mut out = out.lock();
    for r in replies {
        let _ = writeln!(out, "{r}");
    }
    let _ = out.flush();
}

/// Relay agy's stdio JSON-RPC to the session's http MCP server. `initialize` and notifications
/// are handled in order (the session id must exist before anything else); every other request
/// runs on its own thread, so a long tool call does not block `ping`, cancellation, or other
/// calls. Every request gets a reply: agy opens with `server/discover` (newer MCP), which
/// Streamable HTTP servers that predate it answer with a plain HTTP error; a synthesized JSON-RPC
/// error makes agy fall back to `initialize` instead of waiting forever.
pub fn run_bridge(name: &str) -> i32 {
    let target = match std::env::var(env_key(name)) {
        Ok(raw) => match parse_target(&raw) {
            Ok(t) => Some(t),
            Err(e) => {
                eprintln!("[agy-acp mcp-bridge] {name}: {e}");
                None
            }
        },
        Err(_) => None,
    };
    let stdout = Arc::new(Mutex::new(io::stdout()));
    let bridge = target.map(|target| {
        Arc::new(Bridge {
            name: name.to_string(),
            target,
            session: Mutex::new(None),
            initialize: Mutex::new(None),
        })
    });
    for line in io::stdin().lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let Ok(msg) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let Some(bridge) = &bridge else {
            write_lines(&stdout, empty_server_reply(&msg).into_iter().collect());
            continue;
        };
        let method = msg.get("method").and_then(|v| v.as_str()).unwrap_or("");
        if method == "initialize" {
            *bridge.initialize.lock().unwrap() = Some(line.clone());
        }
        if method == "initialize" || msg.get("id").is_none() {
            write_lines(&stdout, bridge.relay(&line, &msg));
        } else {
            let (bridge, stdout) = (Arc::clone(bridge), Arc::clone(&stdout));
            std::thread::spawn(move || write_lines(&stdout, bridge.relay(&line, &msg)));
        }
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    #[test]
    fn parses_only_http_servers_with_headers() {
        let params = json!({ "mcpServers": [
            { "type": "http", "name": "oab", "url": "http://127.0.0.1:8848/mcp",
              "headers": [{ "name": "Authorization", "value": "Bearer t1" }] },
            { "name": "fs", "command": "npx", "args": [], "env": [] },
            { "type": "sse", "name": "s", "url": "http://x/sse", "headers": [] },
        ]});
        assert_eq!(
            parse_http_servers(&params),
            vec![HttpMcpServer {
                name: "oab".into(),
                url: "http://127.0.0.1:8848/mcp".into(),
                headers: vec![("Authorization".into(), "Bearer t1".into())],
            }]
        );
        assert!(parse_http_servers(&json!({ "mcpServers": [] })).is_empty());
        assert!(parse_http_servers(&json!({})).is_empty());
    }

    #[test]
    fn env_key_is_shell_safe() {
        assert_eq!(env_key("oab"), "AGY_ACP_MCP_OAB");
        assert_eq!(env_key("my-server.v2"), "AGY_ACP_MCP_MY_SERVER_V2");
    }

    #[test]
    fn session_env_round_trips_through_parse_target() {
        let env = session_env(&[HttpMcpServer {
            name: "oab".into(),
            url: "http://127.0.0.1:8848/mcp".into(),
            headers: vec![("Authorization".into(), "Bearer t1".into())],
        }]);
        assert_eq!(env[0].0, "AGY_ACP_MCP_OAB");
        let t = parse_target(&env[0].1).unwrap();
        assert_eq!(
            (t.host.as_str(), t.port, t.path.as_str()),
            ("127.0.0.1", 8848, "/mcp")
        );
        assert_eq!(
            t.headers,
            vec![("Authorization".to_string(), "Bearer t1".to_string())]
        );
        assert!(parse_target(r#"{"url":"https://x/mcp"}"#).is_err());
    }

    fn temp_config() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("agy-acp-mcp-{}", uuid::Uuid::new_v4()));
        dir.join("config/mcp_config.json")
    }

    #[test]
    fn ensure_bridge_entries_is_idempotent_and_keeps_other_servers() {
        let path = temp_config();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            r#"{"mcpServers":{"fs":{"command":"npx","args":["fs"]}}}"#,
        )
        .unwrap();
        ensure_bridge_entries(&path, "/usr/local/bin/agy-acp", &["oab".into()]).unwrap();
        ensure_bridge_entries(&path, "/usr/local/bin/agy-acp", &["oab".into()]).unwrap();
        let v: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(v["mcpServers"]["fs"]["command"], "npx");
        assert_eq!(v["mcpServers"]["oab"]["args"], json!(["mcp-bridge", "oab"]));
        let _ = fs::remove_dir_all(path.parent().unwrap().parent().unwrap());
    }

    #[test]
    fn ensure_bridge_entries_does_not_clobber_user_entry() {
        let path = temp_config();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            r#"{"mcpServers":{"oab":{"serverUrl":"http://elsewhere/mcp"}}}"#,
        )
        .unwrap();
        ensure_bridge_entries(&path, "/usr/local/bin/agy-acp", &["oab".into()]).unwrap();
        let v: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(v["mcpServers"]["oab"]["serverUrl"], "http://elsewhere/mcp");
        let _ = fs::remove_dir_all(path.parent().unwrap().parent().unwrap());
    }

    #[test]
    fn ensure_bridge_entries_creates_missing_file() {
        let path = temp_config();
        ensure_bridge_entries(&path, "/x/agy-acp", &["oab".into()]).unwrap();
        let v: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(v["mcpServers"]["oab"]["command"], "/x/agy-acp");
        let _ = fs::remove_dir_all(path.parent().unwrap().parent().unwrap());
    }

    #[test]
    fn empty_server_answers_handshake_without_tools() {
        let init = empty_server_reply(&json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25"}})).unwrap();
        assert_eq!(init["result"]["protocolVersion"], "2025-11-25");
        let list =
            empty_server_reply(&json!({"jsonrpc":"2.0","id":2,"method":"tools/list"})).unwrap();
        assert_eq!(list["result"]["tools"], json!([]));
        let other = empty_server_reply(&json!({"jsonrpc":"2.0","id":3,"method":"server/discover"}))
            .unwrap();
        assert_eq!(other["error"]["code"], -32601);
        assert!(
            empty_server_reply(&json!({"jsonrpc":"2.0","method":"notifications/initialized"}))
                .is_none()
        );
    }

    #[test]
    fn chunked_sse_response_is_decoded() {
        let body = "data: \nid: 0\n\ndata: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}\n\n";
        let chunked = format!("{:x}\r\n{}\r\n0\r\n\r\n", body.len(), body);
        let resp = HttpResponse {
            status: 200,
            headers: vec![("Content-Type".into(), "text/event-stream".into())],
            body: decode_chunked(chunked.as_bytes()),
        };
        assert_eq!(
            messages_from(&resp),
            vec![json!({"jsonrpc":"2.0","id":1,"result":{}})]
        );
    }

    #[test]
    fn parse_target_handles_ipv6_hosts() {
        let t = parse_target(r#"{"url":"http://[::1]:8848/mcp"}"#).unwrap();
        assert_eq!(
            (t.host.as_str(), t.port, t.path.as_str()),
            ("::1", 8848, "/mcp")
        );
        let t = parse_target(r#"{"url":"http://[::1]/mcp"}"#).unwrap();
        assert_eq!((t.host.as_str(), t.port), ("::1", 80));
        assert!(parse_target(r#"{"url":"http://[::1:8848/mcp"}"#).is_err());
    }

    #[test]
    fn synthesized_errors_keep_method_not_found_for_rejected_methods_only() {
        let id = json!(7);
        let discover = synthesized_error(&id, "server/discover", Some((422, "HTTP 422".into())));
        assert_eq!(discover["error"]["code"], -32601);
        let call = synthesized_error(&id, "tools/call", Some((500, "HTTP 500".into())));
        assert_eq!(call["error"]["code"], -32603);
        assert!(call["error"]["message"]
            .as_str()
            .unwrap()
            .contains("HTTP 500"));
        let down = synthesized_error(&id, "tools/call", Some((0, "connection refused".into())));
        assert_eq!(down["error"]["code"], -32603);
        assert_eq!(
            synthesized_error(&id, "tools/call", None)["error"]["code"],
            -32603
        );
    }

    /// Minimal HTTP server: answers each connection with `respond(request)`, `n` times.
    fn fake_server(
        n: usize,
        respond: impl Fn(&str) -> String + Send + 'static,
    ) -> (u16, std::thread::JoinHandle<Vec<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = std::thread::spawn(move || {
            let mut seen = Vec::new();
            for _ in 0..n {
                let (mut conn, _) = listener.accept().unwrap();
                let mut buf = vec![0u8; 8192];
                let mut len = 0;
                // Read headers + Content-Length body.
                loop {
                    let k = conn.read(&mut buf[len..]).unwrap();
                    len += k;
                    let text = String::from_utf8_lossy(&buf[..len]).to_string();
                    if let Some(h) = text.find("\r\n\r\n") {
                        let cl = text
                            .lines()
                            .find_map(|l| l.strip_prefix("Content-Length: "))
                            .and_then(|v| v.trim().parse::<usize>().ok())
                            .unwrap_or(0);
                        if len >= h + 4 + cl || k == 0 {
                            break;
                        }
                    }
                }
                let req = String::from_utf8_lossy(&buf[..len]).to_string();
                conn.write_all(respond(&req).as_bytes()).unwrap();
                seen.push(req);
            }
            seen
        });
        (port, handle)
    }

    fn http(status: &str, extra: &str, body: &str) -> String {
        format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\n{extra}Content-Length: {}\r\n\r\n{body}",
            body.len()
        )
    }

    #[test]
    fn expired_server_session_is_reinitialized_and_the_call_retried() {
        let (port, server) = fake_server(5, |req| {
            if req.contains("\"initialize\"") {
                let sid = if req.contains("Mcp-Session-Id") {
                    "s-x"
                } else if req.contains("\"replay\":true") {
                    "s-2"
                } else {
                    "s-1"
                };
                http(
                    "200 OK",
                    &format!("Mcp-Session-Id: {sid}\r\n"),
                    r#"{"jsonrpc":"2.0","id":1,"result":{}}"#,
                )
            } else if req.contains("notifications/initialized") {
                http("202 Accepted", "", "")
            } else if req.contains("Mcp-Session-Id: s-1") {
                http("404 Not Found", "", "")
            } else {
                http(
                    "200 OK",
                    "",
                    r#"{"jsonrpc":"2.0","id":2,"result":{"tools":[]}}"#,
                )
            }
        });
        let bridge = Bridge {
            name: "t".into(),
            target: Target {
                host: "127.0.0.1".into(),
                port,
                path: "/mcp".into(),
                headers: vec![],
            },
            session: Mutex::new(None),
            initialize: Mutex::new(None),
        };
        // Recorded initialize carries a marker so the fake server can tell the replay apart.
        let init = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"replay":false}}"#;
        let first = bridge.relay(init, &serde_json::from_str(init).unwrap());
        assert_eq!(first[0]["id"], 1);
        assert_eq!(bridge.session.lock().unwrap().as_deref(), Some("s-1"));
        *bridge.initialize.lock().unwrap() = Some(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"replay":true}}"#.into(),
        );

        let list = r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#;
        let replies = bridge.relay(list, &serde_json::from_str(list).unwrap());
        assert_eq!(
            replies,
            vec![json!({"jsonrpc":"2.0","id":2,"result":{"tools":[]}})]
        );
        assert_eq!(bridge.session.lock().unwrap().as_deref(), Some("s-2"));
        let seen = server.join().unwrap();
        assert!(
            seen[4].contains("Mcp-Session-Id: s-2"),
            "retry must use the new session"
        );
    }

    #[test]
    fn post_sends_headers_and_reads_session_id() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let (mut conn, _) = listener.accept().unwrap();
            let mut buf = [0u8; 4096];
            let n = conn.read(&mut buf).unwrap();
            let req = String::from_utf8_lossy(&buf[..n]).to_string();
            let body = r#"{"jsonrpc":"2.0","id":1,"result":{}}"#;
            write!(conn, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nMcp-Session-Id: s-1\r\nContent-Length: {}\r\n\r\n{}", body.len(), body).unwrap();
            req
        });
        let target = Target {
            host: "127.0.0.1".into(),
            port,
            path: "/mcp".into(),
            headers: vec![("Authorization".into(), "Bearer t1".into())],
        };
        let resp = post(
            &target,
            None,
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize"}"#,
        )
        .unwrap();
        let req = server.join().unwrap();
        assert!(req.contains("Authorization: Bearer t1\r\n"));
        assert_eq!(resp.header("mcp-session-id"), Some("s-1"));
        assert_eq!(
            messages_from(&resp),
            vec![json!({"jsonrpc":"2.0","id":1,"result":{}})]
        );
    }
}
