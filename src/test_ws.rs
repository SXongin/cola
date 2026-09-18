//! A tiny local WebSocket server for transport tests.
//!
//! The fake HTTP server's `/callback/ws/endpoint` route points cola at this
//! server's URL, so the real WS client dials a real socket on loopback and the
//! production read/write loop (pong, event acks, card-action responses, close)
//! runs in CI with no credentials and no external network. This is ADR-0031's
//! seam one layer up: the socket under test is the production one, nothing is
//! mocked. Compiled only for tests.

use std::net::SocketAddr;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;

/// A local WS listener that accepts cola's dial-in and hands the test a socket
/// for scripted frames.
pub struct TestWsServer {
    listener: TcpListener,
    addr: SocketAddr,
}

impl TestWsServer {
    pub async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind test ws server");
        let addr = listener.local_addr().expect("test ws server addr");
        Self { listener, addr }
    }

    /// The `ws://` URL to serve from the fake HTTP endpoint route.
    pub fn url(&self) -> String {
        format!("ws://{}", self.addr)
    }

    /// Accept the next client connection (cola dialing in).
    pub async fn accept(&self) -> TestWsSocket {
        let (stream, _) = self.listener.accept().await.expect("accept ws client");
        let ws = tokio_tungstenite::accept_async(stream)
            .await
            .expect("ws handshake");
        TestWsSocket { ws }
    }
}

/// One accepted connection, from the test's (server) side.
pub struct TestWsSocket {
    ws: tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
}

impl TestWsSocket {
    /// Send one binary frame (a pbbp2 frame, as Feishu would).
    pub async fn send_binary(&mut self, bytes: Vec<u8>) {
        self.ws
            .send(Message::Binary(bytes.into()))
            .await
            .expect("send ws binary frame");
    }

    /// The next binary frame within `timeout`; fails the test on timeout or
    /// close. Non-binary frames (library control traffic) are skipped.
    pub async fn next_binary(&mut self, timeout: Duration) -> Vec<u8> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let msg = tokio::time::timeout_at(deadline, self.ws.next())
                .await
                .expect("timed out waiting for a ws frame")
                .expect("ws stream ended")
                .expect("ws frame error");
            match msg {
                Message::Binary(data) => return data.to_vec(),
                Message::Close(_) => panic!("ws closed while waiting for a binary frame"),
                _ => continue,
            }
        }
    }

    /// Like [`next_binary`], but `None` on timeout or close instead of
    /// panicking — for asserting that NO frame follows.
    pub async fn try_next_binary(&mut self, timeout: Duration) -> Option<Vec<u8>> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let msg = tokio::time::timeout_at(deadline, self.ws.next()).await.ok()?;
            let msg = msg?.ok()?;
            match msg {
                Message::Binary(data) => return Some(data.to_vec()),
                Message::Close(_) => return None,
                _ => continue,
            }
        }
    }

    /// Close the connection cleanly (the close handshake), like a server
    /// shutting down.
    pub async fn close(&mut self) {
        self.ws.close(None).await.expect("close ws");
    }
}
