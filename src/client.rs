//! The client half of the control protocol: connect, send one newline-JSON
//! [`Request`], read one [`WireReply`] line. Used both by the CLI
//! subcommands and by `niribg daemon` when it probes for an existing daemon.

use std::io::{BufRead, BufReader, ErrorKind, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail};

use crate::proto::{Request, VersionInfo, WireReply};

/// How long the client waits for the daemon to answer before giving up.
pub const TIMEOUT: Duration = Duration::from_secs(5);

/// Connect to the control socket. `Ok(None)` means nothing is listening (the
/// socket file is absent or stale); `Err` is a genuine failure.
fn connect(socket: &Path) -> Result<Option<UnixStream>> {
    match UnixStream::connect(socket) {
        Ok(stream) => Ok(Some(stream)),
        Err(e) if matches!(e.kind(), ErrorKind::NotFound | ErrorKind::ConnectionRefused) => {
            Ok(None)
        }
        Err(e) => Err(anyhow::Error::new(e).context(format!("connecting to {}", socket.display()))),
    }
}

/// Write one request line and read one reply line on an existing connection.
fn exchange(stream: &UnixStream, req: &Request) -> Result<WireReply> {
    stream.set_read_timeout(Some(TIMEOUT))?;
    stream.set_write_timeout(Some(TIMEOUT))?;

    let mut line = serde_json::to_string(req).context("encoding request")?;
    line.push('\n');
    let mut writer = stream; // &UnixStream implements Write
    writer
        .write_all(line.as_bytes())
        .context("sending request to daemon")?;

    let mut reader = BufReader::new(stream);
    let mut buf = String::new();
    let n = reader
        .read_line(&mut buf)
        .context("waiting for daemon reply")?;
    if n == 0 {
        bail!("daemon closed the connection without replying");
    }
    serde_json::from_str(buf.trim_end()).context("decoding daemon reply")
}

/// Send one request and return the daemon's reply. Errors if the daemon is
/// not reachable or does not answer within [`TIMEOUT`].
pub fn send(socket: &Path, req: &Request) -> Result<WireReply> {
    let stream = connect(socket)?.ok_or_else(|| {
        anyhow::anyhow!(
            "niribg daemon is not running (no socket at {}); start it with `niribg daemon`",
            socket.display()
        )
    })?;
    exchange(&stream, req)
}

/// Whether *something* currently accepts connections on `socket` (a bound,
/// live listener). Used by `--replace` to wait for the old daemon to let go.
/// Note a still-bound socket whose owner has stopped calling `accept` also
/// returns `true` until the owning process exits.
#[must_use]
pub fn socket_bound(socket: &Path) -> bool {
    UnixStream::connect(socket).is_ok()
}

/// Probe for a live daemon at `socket`.
///
/// * `Ok(Some(version))` — a daemon answered.
/// * `Ok(None)` — nothing is listening (stale or absent socket).
/// * `Err(_)` — a daemon is there but misbehaving.
pub fn probe(socket: &Path) -> Result<Option<String>> {
    let Some(stream) = connect(socket)? else {
        return Ok(None);
    };
    let reply = exchange(&stream, &Request::Version)?;
    let data = reply
        .data
        .filter(|_| reply.ok)
        .context("daemon rejected a version probe")?;
    let info: VersionInfo = serde_json::from_value(data).context("decoding version reply")?;
    Ok(Some(info.version))
}
