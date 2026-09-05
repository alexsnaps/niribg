//! niri IPC — a `calloop` `Generic` source on `$NIRI_SOCKET` that turns
//! `OverviewOpenedOrClosed` events into blur-target changes.
//!
//! niri-ipc's `Socket` helper is blocking-only with no fd accessor, so we
//! drive the socket ourselves and use only its `Request` / `Reply` / `Event`
//! types for the wire format. Reconnect is an exponential-backoff
//! `calloop::timer::Timer` (250 ms → ×2 → cap 5 s), reset on a successful
//! handshake. niri replays full state on every connect, so the first
//! `OverviewOpenedOrClosed` after connecting resyncs us — no separate query.

use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

use calloop::generic::Generic;
use calloop::timer::{TimeoutAction, Timer};
use calloop::{Interest, Mode, PostAction};
use niri_ipc::socket::SOCKET_PATH_ENV;
use niri_ipc::{Event, Reply, Request, Response};

use super::DaemonState;

pub(super) const BACKOFF_MIN: Duration = Duration::from_millis(250);
const BACKOFF_MAX: Duration = Duration::from_secs(5);
/// Cap on a single event line before the connection is treated as broken.
const MAX_EVENT_BYTES: usize = 1024 * 1024;

impl DaemonState {
    /// Kick off the niri IPC connection (called once at startup, then again
    /// by the reconnect timer).
    pub(super) fn niri_connect(&mut self) {
        match self.try_niri_connect() {
            Ok(stream) => {
                self.niri_connected = true;
                self.niri_backoff = BACKOFF_MIN;
                tracing::info!("connected to niri IPC");
                self.register_niri_source(stream);
            }
            Err(e) => {
                // One info line the first time (backoff still at the floor),
                // then quiet.
                if self.niri_backoff == BACKOFF_MIN {
                    tracing::info!(error = %e, "niri IPC not available yet; will keep retrying");
                } else {
                    tracing::debug!(error = %e, "niri IPC connect failed");
                }
                self.schedule_niri_reconnect();
            }
        }
    }

    /// Connect, send `EventStream`, and read the `{"Ok":"Handled"}` line.
    /// Returns the (still blocking) stream positioned at the first event.
    fn try_niri_connect(&self) -> io::Result<UnixStream> {
        let path = std::env::var_os(SOCKET_PATH_ENV).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("{SOCKET_PATH_ENV} is not set"),
            )
        })?;
        let stream = UnixStream::connect(&path)?;

        let mut req = serde_json::to_string(&Request::EventStream).expect("Request serializes");
        req.push('\n');
        (&stream).write_all(req.as_bytes())?;

        // Read exactly one line by hand so no bytes of the first event are
        // swallowed by a BufReader.
        let line = read_line_blocking(&stream, MAX_EVENT_BYTES)?;
        check_handshake(&line)?;

        stream.set_nonblocking(true)?;
        Ok(stream)
    }

    /// Register the event stream as a `calloop` source. It removes itself on
    /// EOF / error and schedules a reconnect.
    fn register_niri_source(&mut self, stream: UnixStream) {
        let mut buf: Vec<u8> = Vec::new();
        let registered = self.loop_handle.insert_source(
            Generic::new(stream, Interest::READ, Mode::Level),
            move |_readiness, stream, state: &mut DaemonState| {
                let conn: &UnixStream = stream;
                let mut reader: &UnixStream = conn;
                let mut chunk = [0u8; 4096];
                loop {
                    match reader.read(&mut chunk) {
                        Ok(0) => {
                            state.niri_lost("event stream closed");
                            return Ok(PostAction::Remove);
                        }
                        Ok(n) => {
                            buf.extend_from_slice(&chunk[..n]);
                            while let Some(nl) = buf.iter().position(|&b| b == b'\n') {
                                let line: Vec<u8> = buf.drain(..=nl).collect();
                                state.handle_niri_line(&line[..line.len() - 1]);
                            }
                            if buf.len() > MAX_EVENT_BYTES {
                                state.niri_lost("event line too long");
                                return Ok(PostAction::Remove);
                            }
                        }
                        Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                            return Ok(PostAction::Continue);
                        }
                        Err(e) => {
                            state.niri_lost(&format!("read error: {e}"));
                            return Ok(PostAction::Remove);
                        }
                    }
                }
            },
        );
        if let Err(e) = registered {
            tracing::warn!(error = %e, "could not register the niri IPC source");
            self.niri_connected = false;
            self.schedule_niri_reconnect();
        }
    }

    fn handle_niri_line(&mut self, line: &[u8]) {
        if let Some(is_open) = overview_from_line(line) {
            self.set_overview_open(is_open);
        }
    }

    /// The niri connection went away: show sharp everywhere and reconnect.
    fn niri_lost(&mut self, why: &str) {
        if self.niri_connected {
            tracing::info!(reason = why, "niri IPC disconnected");
        }
        self.niri_connected = false;
        self.set_overview_open(false);
        self.schedule_niri_reconnect();
    }

    fn schedule_niri_reconnect(&mut self) {
        let delay = self.niri_backoff;
        self.niri_backoff = (self.niri_backoff * 2).min(BACKOFF_MAX);
        let registered = self.loop_handle.insert_source(
            Timer::from_duration(delay),
            |_instant, _, state: &mut DaemonState| {
                state.niri_connect();
                TimeoutAction::Drop
            },
        );
        if let Err(e) = registered {
            tracing::error!(error = %e, "could not schedule niri reconnect");
        }
    }
}

/// Validate the `EventStream` handshake reply line.
fn check_handshake(line: &str) -> io::Result<()> {
    let invalid = |m: String| io::Error::new(io::ErrorKind::InvalidData, m);
    match serde_json::from_str::<Reply>(line.trim()) {
        Ok(Ok(Response::Handled)) => Ok(()),
        Ok(Ok(other)) => Err(invalid(format!(
            "unexpected EventStream response: {other:?}"
        ))),
        Ok(Err(msg)) => Err(invalid(format!("niri rejected EventStream: {msg}"))),
        Err(e) => Err(invalid(format!("bad EventStream reply {line:?}: {e}"))),
    }
}

/// `Some(is_open)` when `line` is an `OverviewOpenedOrClosed` event; `None`
/// for any other valid event or a parse error (logged at trace).
fn overview_from_line(line: &[u8]) -> Option<bool> {
    match serde_json::from_slice::<Event>(line) {
        Ok(Event::OverviewOpenedOrClosed { is_open }) => Some(is_open),
        Ok(_) => None,
        Err(e) => {
            tracing::trace!(error = %e, "unparsed niri event");
            None
        }
    }
}

/// Read one `\n`-terminated line from a blocking stream, one byte at a time
/// (used only for the handshake reply, ~20 bytes).
fn read_line_blocking(stream: &UnixStream, max: usize) -> io::Result<String> {
    let mut reader: &UnixStream = stream;
    let mut out = Vec::new();
    let mut b = [0u8; 1];
    loop {
        if reader.read(&mut b)? == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        if b[0] == b'\n' {
            return Ok(String::from_utf8_lossy(&out).into_owned());
        }
        out.push(b[0]);
        if out.len() > max {
            return Err(io::ErrorKind::InvalidData.into());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handshake_accepts_handled() {
        assert!(check_handshake(r#"{"Ok":"Handled"}"#).is_ok());
        assert!(check_handshake("  {\"Ok\":\"Handled\"}  \n").is_ok());
    }

    #[test]
    fn handshake_rejects_error_and_garbage() {
        assert!(check_handshake(r#"{"Err":"no can do"}"#).is_err());
        assert!(check_handshake("not json at all").is_err());
        assert!(check_handshake("").is_err());
    }

    #[test]
    fn overview_event_parsed_both_ways() {
        assert_eq!(
            overview_from_line(br#"{"OverviewOpenedOrClosed":{"is_open":true}}"#),
            Some(true)
        );
        assert_eq!(
            overview_from_line(br#"{"OverviewOpenedOrClosed":{"is_open":false}}"#),
            Some(false)
        );
    }

    #[test]
    fn other_events_and_junk_are_ignored() {
        assert_eq!(
            overview_from_line(br#"{"WorkspacesChanged":{"workspaces":[]}}"#),
            None
        );
        assert_eq!(
            overview_from_line(br#"{"WindowsChanged":{"windows":[]}}"#),
            None
        );
        assert_eq!(overview_from_line(b"{ garbage"), None);
        assert_eq!(overview_from_line(b""), None);
    }
}
