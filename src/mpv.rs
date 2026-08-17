use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::{broadcast, mpsc, oneshot};

const OBSERVED: &[&str] = &[
    "pause", "time-pos", "duration", "volume", "mute", "speed",
    "media-title", "path", "track-list", "idle-active", "fullscreen",
];

pub type Reply = oneshot::Sender<Result<Value, String>>;

#[derive(Clone)]
pub struct MpvHandle {
    pub cmd_tx: mpsc::Sender<(Value, Reply)>,
    pub events: broadcast::Sender<Value>,
    pub status: Arc<Mutex<serde_json::Map<String, Value>>>,
}

impl MpvHandle {
    /// Send a command array to mpv and await its response.
    pub async fn command(&self, cmd: Value) -> Result<Value, String> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send((cmd, tx))
            .await
            .map_err(|_| "mpv actor gone".to_string())?;
        rx.await.map_err(|_| "mpv disconnected".to_string())?
    }
}

pub fn spawn(socket_path: String) -> MpvHandle {
    let (cmd_tx, cmd_rx) = mpsc::channel::<(Value, Reply)>(64);
    let (events, _) = broadcast::channel(256);
    let status = Arc::new(Mutex::new(serde_json::Map::new()));
    let handle = MpvHandle { cmd_tx, events: events.clone(), status: status.clone() };

    tokio::spawn(actor(socket_path, cmd_rx, events, status));
    handle
}

async fn actor(
    socket_path: String,
    mut cmd_rx: mpsc::Receiver<(Value, Reply)>,
    events: broadcast::Sender<Value>,
    status: Arc<Mutex<serde_json::Map<String, Value>>>,
) {
    loop {
        let stream = connect_or_launch(&socket_path).await;
        eprintln!("mpv: connected to {socket_path}");
        let _ = events.send(json!({"connected": true}));
        run_connection(stream, &mut cmd_rx, &events, &status).await;
        eprintln!("mpv: connection lost, reconnecting...");
        status.lock().unwrap().clear();
        let _ = events.send(json!({"connected": false}));
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

async fn connect_or_launch(socket_path: &str) -> UnixStream {
    let mut launched = false;
    loop {
        if let Ok(s) = UnixStream::connect(socket_path).await {
            return s;
        }
        if !launched {
            eprintln!("mpv: no socket at {socket_path}, launching mpv --idle");
            let res = tokio::process::Command::new("mpv")
                .arg("--idle=yes")
                .arg(format!("--input-ipc-server={socket_path}"))
                .spawn();
            match res {
                Ok(_) => launched = true,
                Err(e) => eprintln!("mpv: failed to launch: {e}"),
            }
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// Drive one live connection until it drops.
async fn run_connection(
    stream: UnixStream,
    cmd_rx: &mut mpsc::Receiver<(Value, Reply)>,
    events: &broadcast::Sender<Value>,
    status: &Arc<Mutex<serde_json::Map<String, Value>>>,
) {
    let (read_half, mut write_half) = stream.into_split();
    let mut lines = BufReader::new(read_half).lines();
    let mut pending: HashMap<u64, Reply> = HashMap::new();
    let mut next_id: u64 = 1;
    let mut last_timepos = tokio::time::Instant::now() - Duration::from_secs(1);

    for prop in OBSERVED {
        let msg = json!({"command": ["observe_property", 0, prop]});
        if write_line(&mut write_half, &msg).await.is_err() {
            return;
        }
    }

    loop {
        tokio::select! {
            line = lines.next_line() => {
                let line = match line {
                    Ok(Some(l)) => l,
                    _ => break, // EOF or read error: mpv died
                };
                let Ok(msg): Result<Value, _> = serde_json::from_str(&line) else { continue };

                if let Some(id) = msg.get("request_id").and_then(Value::as_u64) {
                    if let Some(reply) = pending.remove(&id) {
                        let ok = msg.get("error").and_then(Value::as_str) == Some("success");
                        let _ = reply.send(if ok {
                            Ok(msg.get("data").cloned().unwrap_or(Value::Null))
                        } else {
                            Err(msg.get("error").and_then(Value::as_str).unwrap_or("unknown").to_string())
                        });
                    }
                    continue;
                }

                if msg.get("event").and_then(Value::as_str) == Some("property-change") {
                    let name = msg.get("name").and_then(Value::as_str).unwrap_or("").to_string();
                    let data = msg.get("data").cloned().unwrap_or(Value::Null);
                    status.lock().unwrap().insert(name.clone(), data.clone());
                    // ponytail: time-pos fires every frame; throttle to 2/s
                    if name == "time-pos" {
                        if last_timepos.elapsed() < Duration::from_millis(500) {
                            continue;
                        }
                        last_timepos = tokio::time::Instant::now();
                    }
                    let _ = events.send(json!({name: data}));
                }
            }
            cmd = cmd_rx.recv() => {
                let Some((cmd, reply)) = cmd else { return };
                let id = next_id;
                next_id += 1;
                let msg = json!({"command": cmd, "request_id": id});
                if write_line(&mut write_half, &msg).await.is_err() {
                    let _ = reply.send(Err("mpv write failed".into()));
                    break;
                }
                pending.insert(id, reply);
            }
        }
    }
}

async fn write_line(
    w: &mut tokio::net::unix::OwnedWriteHalf,
    msg: &Value,
) -> std::io::Result<()> {
    let mut buf = serde_json::to_vec(msg).unwrap();
    buf.push(b'\n');
    w.write_all(&buf).await
}
