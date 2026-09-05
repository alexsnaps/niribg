//! The long-lived `niribg daemon`.
//!
//! M0: a single-threaded blocking `UnixListener` accept loop that answers
//! `version` / `get` / `quit`. No Wayland, no niri IPC, nothing drawn yet.
//! M1 replaces the accept loop with a `calloop` loop (so its version is
//! co-selected with `smithay-client-toolkit`) and adds the surface, worker
//! thread, and niri event source.

pub mod ipc;

use std::io::{BufRead, BufReader, Write};
use std::ops::ControlFlow;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use crate::client;
use crate::config::Config;
use crate::proto::{Request, WireReply};

use ipc::Handler;

/// Run the daemon until a `quit` command (or a fatal error).
pub fn run(socket: &Path, config: Config, replace: bool) -> Result<()> {
    prepare_socket_path(socket, replace)?;

    let listener = UnixListener::bind(socket)
        .with_context(|| format!("binding control socket {}", socket.display()))?;
    let _guard = SocketGuard(socket.to_path_buf());
    tracing::info!(socket = %socket.display(), pid = std::process::id(), "niribg daemon listening");

    let mut handler = Handler::new(config);

    for conn in listener.incoming() {
        let stream = match conn {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(error = %e, "accept failed");
                continue;
            }
        };
        if serve_one(&mut handler, stream).is_break() {
            tracing::info!("quit requested; shutting down");
            break;
        }
    }
    Ok(())
}

/// Read one request line, dispatch it, write one reply line.
fn serve_one(handler: &mut Handler, stream: UnixStream) -> ControlFlow<()> {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let mut reader = BufReader::new(&stream);
    let mut line = String::new();
    match reader.read_line(&mut line) {
        Ok(0) => return ControlFlow::Continue(()), // client hung up
        Ok(_) => {}
        Err(e) => {
            tracing::warn!(error = %e, "reading request");
            return ControlFlow::Continue(());
        }
    }

    let (reply, flow) = match serde_json::from_str::<Request>(line.trim_end()) {
        Ok(req) => handler.handle(req),
        Err(e) => (
            WireReply::err(format!("malformed request: {e}")),
            ControlFlow::Continue(()),
        ),
    };

    if let Err(e) = write_reply(&stream, &reply) {
        tracing::warn!(error = %e, "writing reply");
    }
    flow
}

fn write_reply(mut stream: &UnixStream, reply: &WireReply) -> Result<()> {
    let mut line = serde_json::to_string(reply).context("encoding reply")?;
    line.push('\n');
    stream.write_all(line.as_bytes()).context("writing reply")?;
    stream.flush().context("flushing reply")
}

/// Resolve what to do about anything already at the socket path: a live
/// daemon (refuse, or replace), or a stale file (unlink).
fn prepare_socket_path(socket: &Path, replace: bool) -> Result<()> {
    match client::probe(socket)? {
        Some(version) if !replace => {
            bail!(
                "a niribg daemon (version {version}) is already listening on {}; \
                 pass --replace to take over",
                socket.display()
            );
        }
        Some(version) => {
            tracing::info!(%version, "replacing the running daemon");
            let _ = client::send(socket, &Request::Quit);
            wait_for_socket_free(socket)?;
        }
        None => {
            if socket.exists() {
                std::fs::remove_file(socket)
                    .with_context(|| format!("removing stale socket {}", socket.display()))?;
            }
        }
    }

    if let Some(dir) = socket.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    Ok(())
}

/// Poll until the old daemon has released `socket`, then clear any file it
/// left behind. Uses a connect-only check so a daemon that has stopped
/// accepting but not yet exited still counts as busy.
fn wait_for_socket_free(socket: &Path) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(3);
    while client::socket_bound(socket) {
        if Instant::now() >= deadline {
            bail!(
                "timed out waiting for the previous daemon to release {}",
                socket.display()
            );
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    if socket.exists() {
        let _ = std::fs::remove_file(socket);
    }
    Ok(())
}

/// Unlinks the socket file when the daemon returns normally. A signal-killed
/// daemon skips this; the next start's stale-socket handling covers that.
struct SocketGuard(PathBuf);

impl Drop for SocketGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Load config for the daemon, mapping "no file" to defaults. Kept here so
/// `main` doesn't need to know the rule.
pub fn load_config(explicit: Option<&Path>) -> Result<(Config, Option<PathBuf>)> {
    Config::load(explicit)
}
