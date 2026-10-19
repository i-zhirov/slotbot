//! A minimal HTTP server for unit tests.
//!
//! The provider clients are hard-wired to the real clinic sites. For tests this
//! server listens on `127.0.0.1` and answers each request with a canned
//! response, so the search functions can run without network access.

use std::sync::{Arc, Mutex};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// A canned response. A request is matched to a route by its path (the target
/// without the query string).
pub struct Route {
    path: String,
    content_type: &'static str,
    body: String,
}

impl Route {
    pub fn json(path: &str, body: &str) -> Self {
        Self {
            path: path.to_string(),
            content_type: "application/json",
            body: body.to_string(),
        }
    }

    pub fn html(path: &str, body: &str) -> Self {
        Self {
            path: path.to_string(),
            content_type: "text/html; charset=utf-8",
            body: body.to_string(),
        }
    }
}

/// A request the server received, for assertions.
#[derive(Debug, Clone)]
pub struct Request {
    pub method: String,
    pub target: String,
    pub body: String,
}

/// An HTTP/1.1 server on `127.0.0.1` that serves canned routes.
pub struct MockServer {
    base_url: String,
    requests: Arc<Mutex<Vec<Request>>>,
}

impl MockServer {
    /// Start the server. Each request is answered with the first route whose
    /// path matches; an unknown path gets an empty body.
    pub async fn start(routes: Vec<Route>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("failed to bind the test server");
        let addr = listener.local_addr().expect("no local address");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&requests);
        let routes = Arc::new(routes);

        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let routes = Arc::clone(&routes);
                let recorded = Arc::clone(&recorded);
                tokio::spawn(async move {
                    if let Some(req) = read_request(&mut stream).await {
                        let path = req.target.split('?').next().unwrap_or(&req.target);
                        let route = routes.iter().find(|r| r.path == path);
                        let (content_type, body) = match route {
                            Some(r) => (r.content_type, r.body.as_str()),
                            None => ("application/json", ""),
                        };
                        recorded.lock().unwrap().push(req);
                        write_response(&mut stream, content_type, body).await;
                    }
                });
            }
        });

        Self {
            base_url: format!("http://{addr}"),
            requests,
        }
    }

    /// Base URL for a client, e.g. `http://127.0.0.1:54321`.
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Every request the server received so far.
    pub fn requests(&self) -> Vec<Request> {
        self.requests.lock().unwrap().clone()
    }
}

/// Read one HTTP/1.1 request: request line, headers and the body.
async fn read_request(stream: &mut TcpStream) -> Option<Request> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        let n = stream.read(&mut chunk).await.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
        let Some(head_end) = find_subslice(&buf, b"\r\n\r\n") else {
            continue;
        };

        let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
        let mut lines = head.split("\r\n");
        let mut request_line = lines.next()?.split_whitespace();
        let method = request_line.next()?.to_string();
        let target = request_line.next()?.to_string();
        let content_length: usize = lines
            .filter_map(|l| l.split_once(':'))
            .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
            .and_then(|(_, v)| v.trim().parse().ok())
            .unwrap_or(0);

        let body_start = head_end + 4;
        while buf.len() < body_start + content_length {
            let n = stream.read(&mut chunk).await.ok()?;
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..n]);
        }
        let body = String::from_utf8_lossy(&buf[body_start..]).to_string();
        return Some(Request {
            method,
            target,
            body,
        });
    }
}

async fn write_response(stream: &mut TcpStream, content_type: &str, body: &str) {
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.shutdown().await;
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}
