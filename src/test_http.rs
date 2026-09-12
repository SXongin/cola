//! A tiny local HTTP/1.1 server for wire tests.
//!
//! Wire tests point a real client at a `TestHttpServer` and assert both sides
//! of the exchange: the recorded request log (method / path / query / headers /
//! body) and what the client parsed back. Nothing sits between the client and
//! the socket — no mock of the HTTP library — so the bytes under test are the
//! production ones (ADR-0031). Compiled only for tests.

use std::sync::{Arc, Mutex};

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};

/// A scripted response for requests whose method and path prefix match.
struct Route {
    method: String,
    path_prefix: String,
    status: u16,
    content_type: String,
    body: Vec<u8>,
}

/// One request received by the server, recorded for assertions.
#[derive(Debug, Clone)]
pub struct RecordedRequest {
    pub method: String,
    /// The request path, without the query string.
    pub path: String,
    /// Raw query string (everything after `?`), empty when absent.
    pub query: String,
    /// Headers with lowercase names, in receipt order.
    pub headers: Vec<(String, String)>,
    pub body: String,
}

impl RecordedRequest {
    /// Case-insensitive header lookup.
    pub fn header(&self, name: &str) -> Option<&str> {
        let name = name.to_ascii_lowercase();
        self.headers
            .iter()
            .find(|(key, _)| *key == name)
            .map(|(_, value)| value.as_str())
    }

    /// A percent-decoded query parameter value (`?a=1&b=2`).
    pub fn query_param(&self, name: &str) -> Option<String> {
        url::form_urlencoded::parse(self.query.as_bytes())
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.into_owned())
    }
}

#[derive(Default)]
struct ServerState {
    routes: Mutex<Vec<Route>>,
    requests: Mutex<Vec<RecordedRequest>>,
}

/// A local HTTP server that answers from a route table and records every
/// request. Binds `127.0.0.1` on an ephemeral port.
pub struct TestHttpServer {
    addr: std::net::SocketAddr,
    state: Arc<ServerState>,
    accept_task: tokio::task::JoinHandle<()>,
}

impl TestHttpServer {
    pub async fn start() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind test http server");
        let addr = listener.local_addr().expect("test http server addr");
        let state = Arc::new(ServerState::default());
        let accept_task = tokio::spawn({
            let state = Arc::clone(&state);
            async move {
                while let Ok((stream, _)) = listener.accept().await {
                    tokio::spawn(handle_connection(stream, Arc::clone(&state)));
                }
            }
        });
        Self {
            addr,
            state,
            accept_task,
        }
    }

    /// The base URL to hand to `Client::with_base_url` (no trailing slash).
    pub fn base_url(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// Answer every matching request with a JSON body.
    pub fn route(&self, method: &str, path_prefix: &str, status: u16, body: impl Into<String>) {
        self.route_raw(method, path_prefix, status, "application/json", body.into());
    }

    /// Answer every matching request with an explicit content type (HTML error
    /// pages, image bytes).
    pub fn route_raw(
        &self,
        method: &str,
        path_prefix: &str,
        status: u16,
        content_type: &str,
        body: impl Into<Vec<u8>>,
    ) {
        self.state.routes.lock().unwrap().push(Route {
            method: method.to_ascii_uppercase(),
            path_prefix: path_prefix.to_string(),
            status,
            content_type: content_type.to_string(),
            body: body.into(),
        });
    }

    /// Every request received so far, in arrival order.
    pub fn requests(&self) -> Vec<RecordedRequest> {
        self.state.requests.lock().unwrap().clone()
    }

    /// How many requests were received (a token cache hit means exactly one).
    pub fn request_count(&self) -> usize {
        self.state.requests.lock().unwrap().len()
    }
}

impl Drop for TestHttpServer {
    fn drop(&mut self) {
        self.accept_task.abort();
    }
}

/// A transport that never routes through the developer machine's env proxy.
/// Wire tests bind a loopback fake server and must behave the same in a
/// proxied shell as in CI (ticket 09); production clients keep honoring env
/// proxies. Auth headers stay the caller's business, built exactly as
/// production builds them (ADR-0031).
pub fn no_proxy_transport() -> reqwest::Client {
    reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("failed to build the no-proxy test transport")
}

async fn handle_connection(stream: tokio::net::TcpStream, state: Arc<ServerState>) {
    let mut reader = tokio::io::BufReader::new(stream);
    let Some(request) = read_request(&mut reader).await else {
        return;
    };
    state.requests.lock().unwrap().push(request.clone());
    let (status, content_type, body) = response_for(&state, &request);
    let mut stream = reader.into_inner();
    write_response(&mut stream, status, &content_type, &body).await;
}

async fn read_request(reader: &mut tokio::io::BufReader<tokio::net::TcpStream>) -> Option<RecordedRequest> {
    let mut request_line = String::new();
    if reader.read_line(&mut request_line).await.ok()? == 0 {
        return None;
    }
    let mut parts = request_line.split_whitespace();
    let method = parts.next()?.to_ascii_uppercase();
    let target = parts.next()?;
    let (path, query) = match target.split_once('?') {
        Some((path, query)) => (path.to_string(), query.to_string()),
        None => (target.to_string(), String::new()),
    };

    let mut headers = Vec::new();
    let mut content_length = 0usize;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).await.ok()? == 0 || line == "\r\n" || line == "\n" {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            let name = name.trim().to_ascii_lowercase();
            let value = value.trim().to_string();
            if name == "content-length" {
                content_length = value.parse().unwrap_or(0);
            }
            headers.push((name, value));
        }
    }

    let mut body = vec![0u8; content_length];
    if content_length > 0 {
        reader.read_exact(&mut body).await.ok()?;
    }
    Some(RecordedRequest {
        method,
        path,
        query,
        headers,
        body: String::from_utf8_lossy(&body).into_owned(),
    })
}

fn response_for(state: &ServerState, request: &RecordedRequest) -> (u16, String, Vec<u8>) {
    let routes = state.routes.lock().unwrap();
    let matched = routes
        .iter()
        .filter(|route| route.method == request.method && request.path.starts_with(&route.path_prefix))
        .max_by_key(|route| route.path_prefix.len());
    match matched {
        Some(route) => (route.status, route.content_type.clone(), route.body.clone()),
        None => (
            404,
            "application/json".to_string(),
            serde_json::json!({
                "code": -1,
                "msg": format!("no route for {} {}", request.method, request.path),
            })
            .to_string()
            .into_bytes(),
        ),
    }
}

async fn write_response(stream: &mut tokio::net::TcpStream, status: u16, content_type: &str, body: &[u8]) {
    let head = format!(
        "HTTP/1.1 {status} {}\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
        reason_phrase(status),
        body.len(),
    );
    let _ = stream.write_all(head.as_bytes()).await;
    let _ = stream.write_all(body).await;
    let _ = stream.flush().await;
}

fn reason_phrase(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        204 => "No Content",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        _ => "Unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn records_method_path_query_headers_and_body() {
        let server = TestHttpServer::start().await;
        server.route("POST", "/echo", 200, r#"{"code":0}"#);

        let response = no_proxy_transport()
            .post(format!("{}/echo?a=1&b=two", server.base_url()))
            .header("x-test", "yes")
            .body("hello")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);

        let requests = server.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].method, "POST");
        assert_eq!(requests[0].path, "/echo");
        assert_eq!(requests[0].query_param("a").as_deref(), Some("1"));
        assert_eq!(requests[0].query_param("b").as_deref(), Some("two"));
        assert_eq!(requests[0].query_param("missing"), None);
        assert_eq!(requests[0].header("X-Test"), Some("yes"));
        assert_eq!(requests[0].body, "hello");
    }

    #[tokio::test]
    async fn unrouted_request_gets_a_diagnostic_404() {
        let server = TestHttpServer::start().await;

        let response = no_proxy_transport()
            .get(format!("{}/nope", server.base_url()))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 404);
        let body: serde_json::Value = response.json().await.unwrap();
        assert_eq!(body["code"], -1);
        assert!(body["msg"].as_str().unwrap().contains("/nope"));
    }
}
