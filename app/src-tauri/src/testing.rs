//! A scripted HTTP server on a real socket, so the tests exercise ureq's own
//! timeouts, connection handling and error kinds rather than a stand-in.

use serde_json::Value;
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// A fresh, empty folder under the system temp directory.
pub fn temp_dir(name: &str) -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "markey-{name}-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

/// Removes a test's folder when the test is over, however it ends.
pub struct Cleanup(pub PathBuf);

impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[derive(Clone)]
pub struct Request {
    pub method: String,
    pub path: String,
    pub headers: HashMap<String, String>,
    pub body: Vec<u8>,
}

impl Request {
    pub fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or(Value::Null)
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .get(&name.to_ascii_lowercase())
            .map(String::as_str)
    }

    /// `path` without its query.
    pub fn route(&self) -> &str {
        self.path.split('?').next().unwrap_or("")
    }
}

pub enum Reply {
    Json(u16, Value),
    /// Status, extra headers and body.
    Raw(u16, Vec<(String, String)>, Vec<u8>),
    /// Read the request and close the connection without answering: the
    /// server may have acted on it, and the caller will never know.
    Hangup,
    /// Answer after a pause.
    Slow(Duration, Box<Reply>),
}

pub struct Server {
    pub base: String,
    seen: Arc<Mutex<Vec<Request>>>,
}

fn read_request(stream: &mut TcpStream) -> Option<Request> {
    let mut raw = Vec::new();
    let mut byte = [0u8; 1];
    while !raw.ends_with(b"\r\n\r\n") {
        if stream.read(&mut byte).ok()? == 0 {
            return None;
        }
        raw.push(byte[0]);
    }
    let text = String::from_utf8_lossy(&raw).into_owned();
    let mut lines = text.split("\r\n");
    let mut first = lines.next()?.split_whitespace();
    let method = first.next()?.to_string();
    let path = first.next()?.to_string();
    let headers: HashMap<String, String> = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))
        .collect();
    let length = headers
        .get("content-length")
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(0);
    let mut body = vec![0u8; length];
    stream.read_exact(&mut body).ok()?;
    Some(Request {
        method,
        path,
        headers,
        body,
    })
}

fn answer(stream: &mut TcpStream, reply: Reply) {
    let (status, extra, body) = match reply {
        Reply::Hangup => return,
        Reply::Slow(pause, inner) => {
            std::thread::sleep(pause);
            return answer(stream, *inner);
        }
        Reply::Json(status, value) => (status, Vec::new(), value.to_string().into_bytes()),
        Reply::Raw(status, extra, body) => (status, extra, body),
    };
    let mut head = format!(
        "HTTP/1.1 {status} X\r\nContent-Length: {}\r\nConnection: close\r\n",
        body.len()
    );
    for (name, value) in extra {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("\r\n");
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(&body);
}

impl Server {
    /// Answer every request with `script`, one connection at a time unless
    /// the script itself is slow.
    pub fn start(script: impl FnMut(&Request) -> Reply + Send + 'static) -> Server {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let seen: Arc<Mutex<Vec<Request>>> = Arc::default();
        let log = seen.clone();
        let script = Arc::new(Mutex::new(script));
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let log = log.clone();
                let script = script.clone();
                std::thread::spawn(move || {
                    let Some(request) = read_request(&mut stream) else {
                        return;
                    };
                    log.lock().unwrap().push(request.clone());
                    let reply = (script.lock().unwrap())(&request);
                    answer(&mut stream, reply);
                });
            }
        });
        Server { base, seen }
    }

    pub fn requests(&self) -> Vec<Request> {
        self.seen.lock().unwrap().clone()
    }

    /// How many requests have reached a path ending in `suffix`.
    pub fn count(&self, suffix: &str) -> usize {
        self.requests()
            .iter()
            .filter(|r| r.route().ends_with(suffix))
            .count()
    }

    /// Wait briefly for background work to reach the server.
    pub fn wait_for(&self, suffix: &str) -> bool {
        for _ in 0..200 {
            if self.count(suffix) > 0 {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        false
    }
}

/// An address on this machine that refuses connections.
pub fn refused_base() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    format!("http://{addr}")
}
