//! Wire types for the `niribg` control socket.
//!
//! One newline-delimited JSON [`Request`] per line, one [`WireReply`] line
//! back. The client and daemon ship in the same binary, so the only
//! cross-version concern is a stale client meeting a newer daemon in a mixed
//! install — [`Request::Version`] exists to surface that loudly.

use serde::{Deserialize, Serialize};

use crate::color::Color;
use crate::config::Mode;

/// A single command from a client to the daemon. Internally tagged on `cmd`,
/// e.g. `{"cmd":"set","path":"/x.jpg","fade":true,"persist":true}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Request {
    /// Set a wallpaper (or a solid colour) on one output or the `default`
    /// slot. An absent `path` with a `color` means a colour-only wallpaper.
    Set {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        path: Option<String>,
        /// Output connector name, or `None` for the `default` slot (every
        /// output without its own override).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        output: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mode: Option<Mode>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        color: Option<Color>,
        /// Crossfade to the new wallpaper (`--no-fade` sends `false`).
        #[serde(default = "yes")]
        fade: bool,
        /// Persist to `state.json` so it survives a daemon restart
        /// (`--no-persist` sends `false`).
        #[serde(default = "yes")]
        persist: bool,
    },
    /// Report daemon + per-output status. Reply data is a [`Status`].
    Get,
    /// Re-read `config.toml` from disk and re-apply.
    Reload,
    /// Clear `state.json` and revert to `config.toml`.
    Reset,
    /// Ask the daemon to exit cleanly.
    Quit,
    /// Return the daemon's build version. Reply data is a [`VersionInfo`].
    Version,
}

fn yes() -> bool {
    true
}

/// `{"ok":true,"data":…}` or `{"ok":false,"error":"…"}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireReply {
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl WireReply {
    #[must_use]
    pub fn ok(data: serde_json::Value) -> Self {
        Self {
            ok: true,
            data: Some(data),
            error: None,
        }
    }

    #[must_use]
    pub fn ok_empty() -> Self {
        Self {
            ok: true,
            data: None,
            error: None,
        }
    }

    #[must_use]
    pub fn err(msg: impl Into<String>) -> Self {
        Self {
            ok: false,
            data: None,
            error: Some(msg.into()),
        }
    }
}

/// Reply payload for [`Request::Version`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VersionInfo {
    pub version: String,
}

/// Reply payload for [`Request::Get`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Status {
    pub pid: u32,
    pub niri_connected: bool,
    pub blur: BlurStatus,
    pub transition_ms: u32,
    pub outputs: Vec<OutputStatus>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BlurStatus {
    pub enable: bool,
    pub radius: u32,
    pub dim: f64,
    /// Live: is the overview open right now.
    pub active: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OutputStatus {
    pub name: String,
    pub logical: Option<LogicalOutput>,
    pub source: Source,
    pub mode: Mode,
    pub color: Color,
    /// Whether this output has its own `[output."NAME"]` override.
    pub overridden: bool,
    /// Whether the current source is decoded and on screen.
    pub loaded: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct LogicalOutput {
    pub width: u32,
    pub height: u32,
    pub scale: f64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Source {
    Image { path: String },
    Color,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(r: &Request) {
        let line = serde_json::to_string(r).unwrap();
        assert!(!line.contains('\n'), "wire form must be single-line");
        assert_eq!(&serde_json::from_str::<Request>(&line).unwrap(), r);
    }

    #[test]
    fn request_roundtrips() {
        roundtrip(&Request::Get);
        roundtrip(&Request::Reload);
        roundtrip(&Request::Reset);
        roundtrip(&Request::Quit);
        roundtrip(&Request::Version);
        roundtrip(&Request::Set {
            path: Some("/x.jpg".into()),
            output: Some("DP-1".into()),
            mode: Some(Mode::Fit),
            color: Some("#1e1e2e".parse().unwrap()),
            fade: false,
            persist: false,
        });
        roundtrip(&Request::Set {
            path: None,
            output: None,
            mode: None,
            color: Some(Color::BLACK),
            fade: true,
            persist: true,
        });
    }

    #[test]
    fn set_defaults_fade_and_persist_true() {
        let r: Request = serde_json::from_str(r#"{"cmd":"set","path":"/x.jpg"}"#).unwrap();
        assert_eq!(
            r,
            Request::Set {
                path: Some("/x.jpg".into()),
                output: None,
                mode: None,
                color: None,
                fade: true,
                persist: true,
            }
        );
    }

    #[test]
    fn tag_is_cmd_snake_case() {
        let line = serde_json::to_string(&Request::Version).unwrap();
        assert_eq!(line, r#"{"cmd":"version"}"#);
    }

    #[test]
    fn reply_shapes() {
        let ok = serde_json::to_string(&WireReply::ok(serde_json::json!({"a":1}))).unwrap();
        assert_eq!(ok, r#"{"ok":true,"data":{"a":1}}"#);
        let err = serde_json::to_string(&WireReply::err("boom")).unwrap();
        assert_eq!(err, r#"{"ok":false,"error":"boom"}"#);
        let empty = serde_json::to_string(&WireReply::ok_empty()).unwrap();
        assert_eq!(empty, r#"{"ok":true}"#);
    }

    #[test]
    fn status_roundtrips() {
        let s = Status {
            pid: 42,
            niri_connected: true,
            blur: BlurStatus {
                enable: true,
                radius: 30,
                dim: 0.15,
                active: false,
            },
            transition_ms: 250,
            outputs: vec![OutputStatus {
                name: "eDP-1".into(),
                logical: Some(LogicalOutput {
                    width: 1920,
                    height: 1200,
                    scale: 1.5,
                }),
                source: Source::Image {
                    path: "/abs/wall.jpg".into(),
                },
                mode: Mode::Fill,
                color: Color::BLACK,
                overridden: false,
                loaded: true,
                error: None,
            }],
        };
        let v = serde_json::to_value(&s).unwrap();
        assert_eq!(s, serde_json::from_value(v).unwrap());
    }
}
