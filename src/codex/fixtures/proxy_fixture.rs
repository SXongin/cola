//! The hermetic stand-in for `codex app-server proxy` (ticket #949).
//!
//! This file is **not** part of cola's module tree. `test_server::proxy_helper`
//! embeds it with `include_str!`, compiles it once with the crate's own
//! edition, and spawns it wherever the production transport would spawn the
//! real proxy. Keeping it a real file keeps it reviewable; because it sits
//! outside the crate's module tree, `cargo fmt` does not see it —
//! `cargo xtask check-fixture-format` is its rustfmt gate.
//!
//! It relays raw bytes between stdio and `--sock`:
//!
//! - a plain path is an AF_UNIX socket (every unix);
//! - `tcp://HOST:PORT` is a TCP target, used by the Windows suite because
//!   stable Rust cannot bind an AF_UNIX socket there;
//! - `exit` exits immediately, for the "child does not survive a failed
//!   handshake" path.
//!
//! Like the real proxy, it joins both copy directions: a daemon-side EOF ends
//! the socket→stdout pump but leaves stdout open until stdin EOF. Exiting on
//! the daemon's EOF instead would hide the transport's dead-daemon detection.

use std::io::{Read, Write};

fn main() {
    let arguments: Vec<String> = std::env::args().collect();
    let socket = arguments
        .iter()
        .position(|argument| argument == "--sock")
        .and_then(|index| arguments.get(index + 1))
        .cloned()
        .unwrap_or_else(|| {
            eprintln!("proxy fixture: no --sock");
            std::process::exit(64);
        });
    if socket == "exit" {
        std::process::exit(3);
    }
    if let Err(error) = run(&socket) {
        eprintln!("proxy fixture: {error}");
        std::process::exit(65);
    }
}

#[cfg(unix)]
fn run(socket: &str) -> std::io::Result<()> {
    if let Some(address) = socket.strip_prefix("tcp://") {
        return relay(std::net::TcpStream::connect(address)?);
    }
    relay(std::os::unix::net::UnixStream::connect(socket)?)
}

#[cfg(not(unix))]
fn run(socket: &str) -> std::io::Result<()> {
    let address = socket.strip_prefix("tcp://").unwrap_or(socket);
    relay(std::net::TcpStream::connect(address)?)
}

trait Relay: Read + Write + Send + 'static {
    fn duplicate(&self) -> std::io::Result<Self>
    where
        Self: Sized;
}

#[cfg(unix)]
impl Relay for std::os::unix::net::UnixStream {
    fn duplicate(&self) -> std::io::Result<Self> {
        self.try_clone()
    }
}

impl Relay for std::net::TcpStream {
    fn duplicate(&self) -> std::io::Result<Self> {
        self.try_clone()
    }
}

/// Copy one direction, flushing every chunk. `std::io::copy` into `stdout`
/// would buffer binary WebSocket frames in the line writer until a newline a
/// frame never carries.
fn pump<R: Read, W: Write>(mut reader: R, mut writer: W) -> std::io::Result<()> {
    let mut buffer = [0u8; 8192];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            return Ok(());
        }
        writer.write_all(&buffer[..read])?;
        writer.flush()?;
    }
}

fn relay<S: Relay>(stream: S) -> std::io::Result<()> {
    let mut writer = stream.duplicate()?;
    let stdin = std::thread::spawn(move || {
        let _ = pump(std::io::stdin(), &mut writer);
    });
    let _ = pump(stream, std::io::stdout());
    // Mimic the real proxy, which joins both copy directions: when the daemon
    // side reaches EOF, stdout stays open until stdin EOF too. Exiting here
    // instead would close cola's read half by itself and hide the transport's
    // dead-daemon detection.
    let _ = stdin.join();
    Ok(())
}
