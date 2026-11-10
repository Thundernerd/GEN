//! A local HTTP server for tests, shared by every test module that needs one.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// A local HTTP server, running until the test ends.
pub struct TestServer {
    pub addr: SocketAddr,
    requests: Arc<Mutex<Vec<String>>>,
}

impl TestServer {
    /// The server's base URL, e.g. `http://127.0.0.1:1234`.
    pub fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// Every request received so far, oldest first.
    pub fn requests(&self) -> Vec<String> {
        self.requests.lock().unwrap().clone()
    }
}

/// Starts a local server that answers each request with the raw HTTP response `reply`
/// gives for it, and records the requests. `reply` gets the request's index (0 for the
/// first request) and the raw request.
///
/// A request is recorded before it is answered, so once a client has its response,
/// `requests()` includes the request. The response is written in full even when it is
/// larger than one write; write errors (a client that hung up early) are ignored.
pub async fn serve(reply: impl Fn(usize, &str) -> String + Send + 'static) -> TestServer {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let seen = requests.clone();
    tokio::spawn(async move {
        for index in 0.. {
            let Ok((mut conn, _)) = listener.accept().await else {
                return;
            };
            let mut buf = vec![0u8; 32 * 1024];
            let n = conn.read(&mut buf).await.unwrap_or(0);
            let request = String::from_utf8_lossy(&buf[..n]).into_owned();
            seen.lock().unwrap().push(request.clone());
            let response = reply(index, &request);
            let _ = conn.write_all(response.as_bytes()).await;
        }
    });
    TestServer { addr, requests }
}

/// A response with `status` (e.g. `"404 Not Found"`) and `headers` carrying `body`.
pub fn respond(status: &str, headers: &[(&str, &str)], body: &str) -> String {
    let headers: String = headers
        .iter()
        .map(|(name, value)| format!("{name}: {value}\r\n"))
        .collect();
    format!(
        "HTTP/1.1 {status}\r\n{headers}content-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    )
}

/// A `200 OK` response carrying `body`.
pub fn ok(body: &str) -> String {
    respond("200 OK", &[], body)
}

/// The answer to a request the test didn't expect, so the client fails on it.
pub fn unexpected() -> String {
    respond("500 Unexpected Request", &[], "")
}
