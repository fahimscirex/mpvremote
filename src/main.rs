mod mpv;

use std::path::{Path, PathBuf};

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, Request, State};
use axum::http::StatusCode;
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Json, Response};
use axum::routing::{get, post};
use axum::Router;
use serde_json::{json, Value};

const INDEX_HTML: &str = include_str!("index.html");

const MEDIA_EXTS: &[&str] = &[
    "mkv", "mp4", "webm", "avi", "mov", "ts", "m2ts", "wmv", "flv",
    "mp3", "flac", "opus", "m4a", "ogg", "wav", "aac", "wma",
];

#[derive(Clone)]
struct App {
    mpv: mpv::MpvHandle,
    root: PathBuf,
    port: u16,
}

/// Every non-loopback IPv4 the machine actually holds. Read straight from the
/// kernel: guessing via the default route picks a VPN tunnel over the LAN.
fn local_ipv4s() -> Vec<String> {
    let out = std::process::Command::new("ip")
        .args(["-4", "-o", "addr", "show", "scope", "global"])
        .output();
    let Ok(out) = out else { return Vec::new() };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| {
            let mut f = l.split_whitespace();
            let dev = f.nth(1)?;
            if dev.starts_with("tun") || dev.starts_with("docker") || dev.starts_with("br-") {
                return None;
            }
            f.find(|t| *t == "inet")?;
            Some(f.next()?.split('/').next()?.to_string())
        })
        .collect()
}

/// Where the running daemon records the port it bound, so a later CLI
/// invocation can find it without being told.
fn state_file() -> PathBuf {
    std::env::var("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/tmp"))
        .join("mpvremote.state")
}

fn is_running(pid: u32) -> bool {
    Path::new(&format!("/proc/{pid}")).exists()
}

/// `mpvremote status` — print where the running daemon is reachable, plus a QR
/// code to point a phone camera at.
fn print_status() -> std::process::ExitCode {
    let raw = std::fs::read_to_string(state_file()).ok();
    let parsed: Option<Value> = raw.as_deref().and_then(|s| serde_json::from_str(s).ok());
    let (Some(state), Some(_)) = (parsed.as_ref(), raw.as_ref()) else {
        eprintln!("mpvremote is not running (no state file at {})", state_file().display());
        return std::process::ExitCode::FAILURE;
    };
    let port = state.get("port").and_then(Value::as_u64).unwrap_or(0);
    let pid = state.get("pid").and_then(Value::as_u64).unwrap_or(0) as u32;
    if pid == 0 || !is_running(pid) {
        eprintln!("mpvremote is not running (stale state file, pid {pid})");
        return std::process::ExitCode::FAILURE;
    }

    let ips = local_ipv4s();
    let Some(primary) = ips.first() else {
        eprintln!("mpvremote is running on port {port}, but no LAN address was found");
        return std::process::ExitCode::FAILURE;
    };
    let url = format!("http://{primary}:{port}");

    let mut out = format!("mpvremote running (pid {pid})\n");
    for ip in &ips {
        out.push_str(&format!("  http://{ip}:{port}\n"));
    }
    out.push_str(&format!("\n{}\n  {url}\n", qr(&url)));
    // One write, errors ignored: `mpvremote status | head` closes the pipe
    // early and println! would panic on it.
    use std::io::Write;
    let _ = std::io::stdout().write_all(out.as_bytes());
    std::process::ExitCode::SUCCESS
}

/// Render a QR as terminal half-blocks: two rows of modules per text line, so
/// it comes out roughly square instead of stretched.
fn qr(text: &str) -> String {
    match qrcode::QrCode::new(text.as_bytes()) {
        Ok(code) => code
            .render::<qrcode::render::unicode::Dense1x2>()
            .quiet_zone(true)
            .build(),
        Err(e) => format!("(could not render QR: {e})"),
    }
}

#[tokio::main]
async fn main() -> std::process::ExitCode {
    // --port wins over MPVREMOTE_PORT, which wins over the default.
    let args: Vec<String> = std::env::args().collect();

    match args.get(1).map(String::as_str) {
        Some("status" | "qr") => return print_status(),
        Some("--help" | "-h") => {
            println!("mpvremote               run the server (default)");
            println!("mpvremote status | qr   show the running daemon's address + QR code");
            println!("\noptions: --port <n>   (or MPVREMOTE_PORT, MPVREMOTE_SOCKET, MPVREMOTE_ROOT)");
            return std::process::ExitCode::SUCCESS;
        }
        _ => {}
    }
    let flag_port = args.iter().position(|a| a == "--port" || a == "-p")
        .and_then(|i| args.get(i + 1))
        .and_then(|v| v.parse::<u16>().ok());
    let port: u16 = flag_port
        .or_else(|| std::env::var("MPVREMOTE_PORT").ok().and_then(|v| v.parse().ok()))
        .unwrap_or(8000);
    let socket = std::env::var("MPVREMOTE_SOCKET").unwrap_or_else(|_| "/tmp/mpvremote.sock".into());
    let root = std::env::var("MPVREMOTE_ROOT")
        .or_else(|_| std::env::var("HOME"))
        .expect("set MPVREMOTE_ROOT or HOME");
    let root = std::fs::canonicalize(&root).expect("MPVREMOTE_ROOT must exist");

    let app = App { mpv: mpv::spawn(socket), root, port };

    let router = Router::new()
        .route("/", get(|| async { Html(INDEX_HTML) }))
        .route("/ws", get(ws_handler))
        .route("/api/command", post(api_command))
        .route("/api/open", post(api_open))
        .route("/api/browse", get(api_browse))
        .route("/api/info", get(api_info))
        .with_state(app)
        .layer(middleware::from_fn(guard_host));

    let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{port}"))
        .await
        .unwrap_or_else(|e| panic!("cannot bind port {port}: {e}"));

    // Record the bound port so `mpvremote status` can find us later.
    let pid = std::process::id();
    let _ = std::fs::write(state_file(), json!({"port": port, "pid": pid}).to_string());

    let ips = local_ipv4s();
    eprintln!("mpvremote listening on port {port} (all interfaces)");
    for ip in &ips {
        eprintln!("  http://{ip}:{port}");
    }
    // Only when someone is watching — under systemd this would just be noise.
    if std::io::IsTerminal::is_terminal(&std::io::stderr()) {
        if let Some(ip) = ips.first() {
            eprintln!("\n{}", qr(&format!("http://{ip}:{port}")));
        }
    }
    eprintln!("run `mpvremote status` for the address and a QR code");

    axum::serve(listener, router).await.unwrap();
    let _ = std::fs::remove_file(state_file());
    std::process::ExitCode::SUCCESS
}

/// Block DNS-rebinding: a malicious site pointing its own hostname at this
/// machine's LAN IP would arrive with that hostname in the Host header.
/// Legitimate access is always via an IP literal or `localhost`, so reject
/// anything that parses as neither. No Host header (raw HTTP/1.0) is allowed.
async fn guard_host(req: Request, next: Next) -> Response {
    if let Some(host) = req.headers().get("host").and_then(|h| h.to_str().ok()) {
        // strip the :port, and the [..] around an IPv6 literal
        let h = host.rsplit_once(':').map_or(host, |(a, _)| a);
        let h = h.strip_prefix('[').and_then(|s| s.strip_suffix(']')).unwrap_or(h);
        let ok = h == "localhost" || h.parse::<std::net::IpAddr>().is_ok();
        if !ok {
            return (StatusCode::FORBIDDEN, "bad host").into_response();
        }
    }
    next.run(req).await
}

async fn api_info(State(app): State<App>) -> impl IntoResponse {
    Json(json!({"port": app.port, "root": app.root}))
}

async fn ws_handler(ws: WebSocketUpgrade, State(app): State<App>) -> impl IntoResponse {
    ws.on_upgrade(move |socket| ws_client(socket, app))
}

async fn ws_client(mut socket: WebSocket, app: App) {
    let mut rx = app.mpv.events.subscribe();
    let full = json!({"status": Value::Object(app.mpv.status.lock().unwrap().clone())});
    if socket.send(Message::Text(full.to_string().into())).await.is_err() {
        return;
    }
    loop {
        tokio::select! {
            ev = rx.recv() => {
                match ev {
                    Ok(v) => {
                        if socket.send(Message::Text(v.to_string().into())).await.is_err() {
                            return;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(_) => return,
                }
            }
            msg = socket.recv() => {
                match msg {
                    Some(Ok(_)) => continue, // ignore pings/any client chatter
                    _ => return,
                }
            }
        }
    }
}

/// The only mpv commands the UI ever sends. This is a hard trust boundary, not
/// a convenience filter: a raw passthrough lets any LAN client (or a webpage
/// via DNS rebinding) send mpv's `run` / `load-script` commands, which execute
/// arbitrary programs — remote code execution. Allowlist the exact verbs the
/// remote needs and reject everything else.
fn command_allowed(cmd: &[Value]) -> bool {
    let Some(name) = cmd.first().and_then(Value::as_str) else { return false };
    match name {
        "stop" => cmd.len() == 1,
        "cycle" => matches!(cmd.get(1).and_then(Value::as_str), Some("pause" | "mute" | "fullscreen")),
        // seek <n> [absolute|relative]
        "seek" => {
            cmd.get(1).map(Value::is_number).unwrap_or(false)
                && matches!(cmd.get(2).and_then(Value::as_str), None | Some("absolute" | "relative"))
                && cmd.len() <= 3
        }
        "set_property" => {
            let prop = cmd.get(1).and_then(Value::as_str);
            let val_ok = match prop {
                Some("volume" | "speed") => cmd.get(2).map(Value::is_number).unwrap_or(false),
                // aid/sid accept a track number or the string "no"
                Some("aid" | "sid") => {
                    let v = cmd.get(2);
                    v.map(Value::is_number).unwrap_or(false) || v.and_then(Value::as_str) == Some("no")
                }
                Some("pause" | "fullscreen" | "mute") => cmd.get(2).map(Value::is_boolean).unwrap_or(false),
                _ => false,
            };
            val_ok && cmd.len() == 3
        }
        _ => false,
    }
}

async fn api_command(State(app): State<App>, Json(body): Json<Value>) -> impl IntoResponse {
    let Some(cmd) = body.get("command").and_then(Value::as_array).cloned() else {
        return (StatusCode::BAD_REQUEST, Json(json!({"error": "expected {\"command\": [...]}"})));
    };
    if !command_allowed(&cmd) {
        return (StatusCode::FORBIDDEN, Json(json!({"error": "command not allowed"})));
    }
    match app.mpv.command(Value::Array(cmd)).await {
        Ok(data) => (StatusCode::OK, Json(json!({"data": data}))),
        Err(e) => (StatusCode::BAD_GATEWAY, Json(json!({"error": e}))),
    }
}

async fn api_open(State(app): State<App>, Json(body): Json<Value>) -> impl IntoResponse {
    let Some(target) = body.get("target").and_then(Value::as_str) else {
        return (StatusCode::BAD_REQUEST, Json(json!({"error": "expected {\"target\": \"...\"}"})));
    };
    // Local paths must stay under root; URLs pass through to mpv/yt-dlp.
    if !target.contains("://") && check_path(&app.root, Path::new(target)).is_none() {
        return (StatusCode::FORBIDDEN, Json(json!({"error": "path outside root"})));
    }
    match app.mpv.command(json!(["loadfile", target, "replace"])).await {
        Ok(_) => {
            let _ = app.mpv.command(json!(["set_property", "pause", false])).await;
            (StatusCode::OK, Json(json!({"data": "ok"})))
        }
        Err(e) => (StatusCode::BAD_GATEWAY, Json(json!({"error": e}))),
    }
}

#[derive(serde::Deserialize)]
struct BrowseQuery {
    path: Option<String>,
}

async fn api_browse(State(app): State<App>, Query(q): Query<BrowseQuery>) -> impl IntoResponse {
    let requested = q.path.map(PathBuf::from).unwrap_or_else(|| app.root.clone());
    let Some(dir) = check_path(&app.root, &requested) else {
        return (StatusCode::FORBIDDEN, Json(json!({"error": "path outside root"})));
    };
    let mut entries: Vec<Value> = Vec::new();
    let Ok(rd) = std::fs::read_dir(&dir) else {
        return (StatusCode::NOT_FOUND, Json(json!({"error": "cannot read directory"})));
    };
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
        if !is_dir {
            let ext = name.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
            if !MEDIA_EXTS.contains(&ext.as_str()) {
                continue;
            }
        }
        entries.push(json!({"name": name, "is_dir": is_dir, "path": e.path()}));
    }
    entries.sort_by(|a, b| {
        let (ad, bd) = (a["is_dir"].as_bool().unwrap(), b["is_dir"].as_bool().unwrap());
        bd.cmp(&ad).then_with(|| a["name"].as_str().unwrap().cmp(b["name"].as_str().unwrap()))
    });
    let parent = if dir != app.root {
        dir.parent().map(|p| p.to_string_lossy().into_owned())
    } else {
        None
    };
    (StatusCode::OK, Json(json!({"path": dir, "parent": parent, "entries": entries})))
}

/// Canonicalize and require the path to stay under root. Trust boundary.
fn check_path(root: &Path, requested: &Path) -> Option<PathBuf> {
    let canon = std::fs::canonicalize(requested).ok()?;
    canon.starts_with(root).then_some(canon)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn allowed(cmd: Value) -> bool {
        command_allowed(cmd.as_array().unwrap())
    }

    /// The allowlist is what stands between a LAN client and arbitrary code
    /// execution through mpv's `run`. If this test ever fails, that hole is
    /// open again.
    #[test]
    fn rejects_code_execution_and_other_dangerous_commands() {
        for cmd in [
            json!(["run", "/bin/sh", "-c", "id"]),
            json!(["load-script", "/tmp/evil.lua"]),
            json!(["subprocess", "sh"]),
            json!(["set_property", "stream-open-filename", "/etc/passwd"]),
            json!(["set_property", "screenshot-directory", "/etc"]),
            json!(["screenshot-to-file", "/etc/x.png"]),
            json!(["quit"]),
            json!(["set", "script-opts", "x"]),
            json!(["loadfile", "/etc/passwd"]),
            json!([]),
            json!(["cycle", "sub-visibility"]), // not one of the three we allow
            json!(["seek", "10; run sh"]),      // non-numeric amount
            json!(["set_property", "volume", "loud"]),
            json!(["set_property", "pause", 1]), // must be a real boolean
        ] {
            assert!(!allowed(cmd.clone()), "should have been rejected: {cmd}");
        }
    }

    #[test]
    fn accepts_exactly_what_the_ui_sends() {
        for cmd in [
            json!(["cycle", "pause"]),
            json!(["cycle", "mute"]),
            json!(["cycle", "fullscreen"]),
            json!(["stop"]),
            json!(["seek", 10]),
            json!(["seek", -10]),
            json!(["seek", 30.5, "absolute"]),
            json!(["set_property", "volume", 90]),
            json!(["set_property", "speed", 1.25]),
            json!(["set_property", "aid", 1]),
            json!(["set_property", "sid", "no"]),
            json!(["set_property", "pause", false]),
            json!(["set_property", "fullscreen", true]),
        ] {
            assert!(allowed(cmd.clone()), "should have been allowed: {cmd}");
        }
    }

    #[test]
    fn browse_root_confinement() {
        let root = std::fs::canonicalize("/usr").unwrap();
        assert!(check_path(&root, Path::new("/usr/share")).is_some());
        assert!(check_path(&root, Path::new("/etc")).is_none());
        assert!(check_path(&root, Path::new("/usr/../etc")).is_none());
        assert!(check_path(&root, Path::new("/usr/nonexistent-xyz")).is_none());
    }
}
