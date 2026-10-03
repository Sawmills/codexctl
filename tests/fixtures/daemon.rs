//! Synthetic daemon protocol for CLI restart tests; no live Codex sessions.
use serde_json::{Value, json};
use std::{
    os::unix::net::UnixListener,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::JoinHandle,
    time::Duration,
};
use tungstenite::Message;

pub struct Daemon {
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<Vec<Value>>>,
}

impl Daemon {
    pub fn start(home: &Path, fail_resume: bool) -> Self {
        let directory = home.join("app-server-control");
        std::fs::create_dir_all(&directory).unwrap();
        let listener = UnixListener::bind(directory.join("app-server-control.sock")).unwrap();
        listener.set_nonblocking(true).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = stop.clone();
        let restarted = home.join("restart-config.toml");
        let worker = std::thread::spawn(move || {
            let mut seen = Vec::new();
            while !stopping.load(Ordering::Relaxed) {
                let stream = match listener.accept() {
                    Ok((stream, _)) => stream,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(10));
                        continue;
                    }
                    Err(e) => panic!("daemon accept failed: {e}"),
                };
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                stream
                    .set_write_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut socket = tungstenite::accept(stream).unwrap();
                while let Ok(Message::Text(text)) = socket.read() {
                    let request: Value = serde_json::from_str(&text).unwrap();
                    let Some(id) = request.get("id") else {
                        continue;
                    };
                    let method = request["method"].as_str().unwrap();
                    let thread = &request["params"]["threadId"];
                    seen.push(request.clone());
                    if fail_resume && method == "thread/resume" && thread == "running" {
                        socket.send(Message::text(json!({"id":id,"error":{"code":-32000,"message":"synthetic resume failure"}}).to_string())).unwrap();
                        continue;
                    }
                    let result = match method {
                        "initialize" => json!({}),
                        "account/read" => json!({
                            "account":{"type":"chatgpt","email":"local@test"},
                            "workspaceRouting":{"chatgptAccountId":"unrelated-local-seat"}
                        }),
                        "thread/loaded/list" => {
                            json!({"data":["running","limited","done"],"nextCursor":null})
                        }
                        "thread/read" => json!({"thread":{"id":thread,"name":thread}}),
                        "thread/turns/list" if thread == "done" => {
                            json!({"data":[{"id":"old","status":"completed"}]})
                        }
                        "thread/turns/list" if thread == "running" && !restarted.exists() => {
                            json!({"data":[{"id":"old","status":"inProgress"}]})
                        }
                        "thread/turns/list" => {
                            json!({"data":[{"id":"old","status":"failed","error":{"codexErrorInfo":"usageLimitExceeded"}}]})
                        }
                        "thread/resume" => {
                            json!({"thread":{"id":thread},"approvalPolicy":"never","sandbox":{"type":"dangerFullAccess"}})
                        }
                        "thread/settings/update" => json!({}),
                        "turn/start" => json!({"turn":{"id":"new","status":"inProgress"}}),
                        other => panic!("unexpected daemon method {other}"),
                    };
                    socket
                        .send(Message::text(json!({"id":id,"result":result}).to_string()))
                        .unwrap();
                    if method == "turn/start" {
                        socket.send(Message::text(json!({"method":"turn/started","params":{"threadId":thread,"turn":{"id":"new"}}}).to_string())).unwrap();
                    }
                }
            }
            seen
        });
        Self {
            stop,
            worker: Some(worker),
        }
    }

    pub fn finish(mut self) -> Vec<Value> {
        self.stop.store(true, Ordering::Relaxed);
        self.worker.take().unwrap().join().unwrap()
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}
