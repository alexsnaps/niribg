// SPDX-License-Identifier: GPL-3.0-or-later
//! The long-lived `niribg daemon`.
//!
//! M1: a single-threaded `calloop` loop. Sources: the control socket
//! (non-blocking `accept`, one newline-framed request per connection), Unix
//! signals (`SIGHUP` → reload, `SIGTERM`/`SIGINT` → clean exit), and — added
//! in later M1 steps — the Wayland connection and the image-worker result
//! channel. Heavy image work runs on one worker thread; nothing else leaves
//! the loop.

pub mod anim;
pub mod ipc;
mod niri;
pub mod render;
mod wayland;
pub mod worker;

use std::collections::{HashMap, HashSet};
use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use calloop::generic::Generic;
use calloop::signals::{Signal, Signals};
use calloop::{EventLoop, Interest, LoopHandle, LoopSignal, Mode, PostAction};
use smithay_client_toolkit::reexports::calloop_wayland_source::WaylandSource;
use wayland_client::Connection;
use wayland_client::globals::registry_queue_init;

use crate::client;
use crate::config::Config;
use crate::proto::{Request, Status, WireReply};
use crate::state::State;

use wayland::Wayland;

use ipc::{Control, ReloadSummary, Reply, SetDispatch, SetRequest, Token};

/// Largest request line the daemon will buffer before dropping the connection.
const MAX_REQUEST_BYTES: usize = 64 * 1024;

/// Run the daemon until `quit` / `SIGTERM` / `SIGINT` (or a fatal error).
///
/// `config_path` is the `--config` override, if any; `None` uses the XDG
/// location. A present-but-broken config aborts startup.
pub fn run(socket: &Path, config_path: Option<PathBuf>, replace: bool) -> Result<()> {
    prepare_socket_path(socket, replace)?;

    let listener = UnixListener::bind(socket)
        .with_context(|| format!("binding control socket {}", socket.display()))?;
    listener
        .set_nonblocking(true)
        .context("setting control socket non-blocking")?;
    let _guard = SocketGuard(socket.to_path_buf());

    // Wayland: connect, enumerate globals, bind the ones we need.
    let conn = Connection::connect_to_env().context("connecting to the Wayland compositor")?;
    let (globals, event_queue) =
        registry_queue_init::<DaemonState>(&conn).context("initialising the Wayland registry")?;
    let qh = event_queue.handle();
    let wl = Wayland::bind(&globals, &qh)?;

    let mut event_loop: EventLoop<DaemonState> =
        EventLoop::try_new().context("creating the event loop")?;
    let handle = event_loop.handle();
    let signal = event_loop.get_signal();

    // Build the signal source *before* spawning any thread: `Signals::new`
    // blocks these signals via `pthread_sigmask`, which only masks the calling
    // thread. Threads spawned afterwards inherit the mask; one spawned earlier
    // (the render worker) would not, so a process-directed SIGHUP would land on
    // it and kill the daemon (SIGHUP's default disposition) instead of being
    // read from the signalfd here.
    let signals = Signals::new(&[Signal::SIGHUP, Signal::SIGTERM, Signal::SIGINT])
        .context("registering signal handler")?;

    // Image worker: jobs go out over an mpsc channel inside `Worker`, results
    // come back here over a calloop channel so the loop is woken to apply
    // them.
    let (result_tx, result_rx) = calloop::channel::channel::<worker::JobResult>();
    let worker = worker::Worker::spawn(result_tx);

    let mut state = DaemonState::load(config_path, wl, worker, handle.clone(), signal)?;

    WaylandSource::new(conn, event_queue)
        .insert(handle.clone())
        .map_err(|e| anyhow::anyhow!("inserting the Wayland event source: {e}"))?;

    handle
        .insert_source(result_rx, |event, _, state: &mut DaemonState| {
            if let calloop::channel::Event::Msg(result) = event {
                state.apply_rendered(result);
            }
        })
        .map_err(|e| anyhow::anyhow!("registering the render-result channel: {e}"))?;

    handle
        .insert_source(
            Generic::new(listener, Interest::READ, Mode::Level),
            |_readiness, listener, state: &mut DaemonState| {
                loop {
                    match listener.accept() {
                        Ok((stream, _addr)) => state.accept_client(stream),
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                        Err(e) => {
                            tracing::warn!(error = %e, "control socket accept failed");
                            break;
                        }
                    }
                }
                Ok(PostAction::Continue)
            },
        )
        .map_err(|e| anyhow::anyhow!("registering control socket: {e}"))?;

    handle
        .insert_source(
            signals,
            |event, _, state: &mut DaemonState| match event.signal() {
                Signal::SIGHUP => {
                    tracing::info!("SIGHUP: reloading config");
                    if let Err(e) = state.reload() {
                        tracing::warn!(error = %format!("{e:#}"), "reload failed");
                    }
                }
                other => {
                    tracing::info!(signal = ?other, "shutting down");
                    state.loop_signal.stop();
                }
            },
        )
        .map_err(|e| anyhow::anyhow!("registering signals: {e}"))?;

    // Kick off the niri IPC connection (self-reconnecting via a timer).
    state.niri_connect();

    tracing::info!(socket = %socket.display(), pid = std::process::id(), "niribg daemon listening");
    event_loop
        .run(None, &mut state, |_state| {})
        .context("event loop stopped with an error")?;
    Ok(())
}

/// One buffered, in-flight `set` whose render jobs have not all completed.
struct PendingSet {
    /// The client socket to answer once every job finishes (a duplicated fd;
    /// `None` only in the brief window between token creation and the
    /// transport attaching it).
    stream: Option<UnixStream>,
    /// Output names still awaiting a worker result.
    outstanding: HashSet<String>,
    /// `(output, reason)` for jobs that failed.
    failures: Vec<(String, String)>,
    /// Informational note (e.g. a stored set for a disconnected output).
    note: Option<String>,
}

/// The `Data` threaded through every event-loop callback.
struct DaemonState {
    loop_handle: LoopHandle<'static, DaemonState>,
    loop_signal: LoopSignal,

    /// Wayland globals and the per-output `background` surfaces.
    wl: Wayland,
    /// The image-decode/compose worker thread.
    worker: worker::Worker,

    /// Whether niri's overview is currently open (drives the blur swap).
    overview_open: bool,
    /// Whether the niri IPC event stream is currently connected.
    niri_connected: bool,
    /// Current niri IPC reconnect delay (grows on failure, resets on
    /// connect).
    niri_backoff: Duration,

    /// Effective config: `config.toml` overlaid with `state.json`.
    config: Config,
    /// Last-read `config.toml` without the state overlay; the base `reload`
    /// and `set` rebuild the effective config from.
    disk_config: Config,
    /// Runtime `niribg set` overrides, mirrored to `state.json`.
    state: State,
    /// The `--config` override as given on the command line, re-read on
    /// `reload`. `None` means the XDG location.
    explicit_config: Option<PathBuf>,
    state_path: PathBuf,

    /// Next `set` token.
    next_token: Token,
    pending_sets: HashMap<Token, PendingSet>,
}

impl DaemonState {
    /// Load config + state and build the initial effective config. A broken
    /// `config.toml` is fatal here (aborts `niribg daemon`).
    fn load(
        explicit_config: Option<PathBuf>,
        wl: Wayland,
        worker: worker::Worker,
        loop_handle: LoopHandle<'static, DaemonState>,
        loop_signal: LoopSignal,
    ) -> Result<Self> {
        let (disk_config, read_from) = Config::load(explicit_config.as_deref())?;
        match &read_from {
            Some(p) => tracing::info!(path = %p.display(), "loaded config"),
            None => tracing::info!("no config file; using defaults"),
        }
        let state_path = crate::paths::state_file()?;
        let runtime_state = State::load(&state_path);
        let mut effective = disk_config.clone();
        runtime_state.apply_to(&mut effective);

        Ok(Self {
            loop_handle,
            loop_signal,
            wl,
            worker,
            config: effective,
            disk_config,
            state: runtime_state,
            explicit_config,
            state_path,
            overview_open: false,
            niri_connected: false,
            niri_backoff: niri::BACKOFF_MIN,
            next_token: 1,
            pending_sets: HashMap::new(),
        })
    }

    /// Register a freshly accepted client connection as its own read source.
    /// One newline-terminated request is read, dispatched, answered, and the
    /// source removed.
    fn accept_client(&mut self, stream: UnixStream) {
        let mut buf: Vec<u8> = Vec::new();
        let source = Generic::new(stream, Interest::READ, Mode::Level);
        let registered = self.loop_handle.insert_source(
            source,
            move |_readiness, stream, state: &mut DaemonState| {
                let conn: &UnixStream = stream;
                let mut reader: &UnixStream = conn;
                let mut chunk = [0u8; 1024];
                loop {
                    match reader.read(&mut chunk) {
                        Ok(0) => return Ok(PostAction::Remove), // client hung up
                        Ok(n) => {
                            buf.extend_from_slice(&chunk[..n]);
                            if let Some(nl) = buf.iter().position(|&b| b == b'\n') {
                                let line: Vec<u8> = buf.drain(..=nl).collect();
                                state.handle_line(&line[..line.len() - 1], conn);
                                return Ok(PostAction::Remove);
                            }
                            if buf.len() > MAX_REQUEST_BYTES {
                                tracing::warn!(
                                    "control request exceeded {MAX_REQUEST_BYTES} bytes"
                                );
                                return Ok(PostAction::Remove);
                            }
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            return Ok(PostAction::Continue);
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, "reading control request");
                            return Ok(PostAction::Remove);
                        }
                    }
                }
            },
        );
        if let Err(e) = registered {
            tracing::warn!(error = %e, "could not register client connection");
        }
    }

    /// Parse and act on one request line, writing the reply (unless deferred).
    fn handle_line(&mut self, line: &[u8], stream: &UnixStream) {
        let req: Request = match serde_json::from_slice(line) {
            Ok(req) => req,
            Err(e) => {
                write_reply(stream, &WireReply::err(format!("malformed request: {e}")));
                return;
            }
        };

        match ipc::dispatch(req, self) {
            Reply::Now(reply) => write_reply(stream, &reply),
            Reply::Shutdown(reply) => {
                write_reply(stream, &reply);
                self.loop_signal.stop();
            }
            Reply::Deferred(token) => match stream.try_clone() {
                Ok(dup) => {
                    if let Some(pending) = self.pending_sets.get_mut(&token) {
                        pending.stream = Some(dup);
                    }
                    self.try_finish_pending(token);
                }
                Err(e) => {
                    tracing::warn!(error = %e, "could not retain client socket for deferred reply");
                    self.pending_sets.remove(&token);
                }
            },
        }
    }

    /// If every job under `token` has reported, answer the stashed client and
    /// drop the entry.
    fn try_finish_pending(&mut self, token: Token) {
        let ready = self
            .pending_sets
            .get(&token)
            .is_some_and(|p| p.stream.is_some() && p.outstanding.is_empty());
        if !ready {
            return;
        }
        let Some(mut pending) = self.pending_sets.remove(&token) else {
            return;
        };
        let Some(stream) = pending.stream.take() else {
            return;
        };
        let reply = if pending.failures.is_empty() {
            match pending.note {
                Some(note) => WireReply::ok(serde_json::json!({ "note": note })),
                None => WireReply::ok_empty(),
            }
        } else {
            let detail = pending
                .failures
                .iter()
                .map(|(o, r)| format!("{o}: {r}"))
                .collect::<Vec<_>>()
                .join("; ");
            WireReply::err(detail)
        };
        write_reply(&stream, &reply);
    }
}

impl Control for DaemonState {
    fn status(&self) -> Status {
        self.live_status()
    }

    fn apply_set(&mut self, req: SetRequest) -> SetDispatch {
        self.set_wallpaper(req)
    }

    fn reload(&mut self) -> Result<ReloadSummary> {
        Ok(ReloadSummary {
            changed: self.reload_config(false)?,
        })
    }

    fn reset(&mut self) -> Result<ReloadSummary> {
        Ok(ReloadSummary {
            changed: self.reload_config(true)?,
        })
    }
}

fn write_reply(stream: &UnixStream, reply: &WireReply) {
    let mut line = match serde_json::to_string(reply) {
        Ok(line) => line,
        Err(e) => {
            tracing::warn!(error = %e, "encoding reply");
            return;
        }
    };
    line.push('\n');
    let mut w: &UnixStream = stream;
    if let Err(e) = w.write_all(line.as_bytes()).and_then(|()| w.flush()) {
        tracing::warn!(error = %e, "writing reply");
    }
}

// --- pre-loop socket lifecycle (unchanged from M0) --------------------------

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
/// left behind.
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
