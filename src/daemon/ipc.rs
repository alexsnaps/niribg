//! Control-socket command handling, kept free of Wayland so it can be
//! exercised in tests. The transport (accept loop, newline framing) lives in
//! [`super`]; this module turns one [`Request`] into one [`WireReply`].

use std::ops::ControlFlow;

use crate::VERSION;
use crate::config::Config;
use crate::proto::{
    BlurStatus, LogicalOutput, OutputStatus, Request, Source, Status, VersionInfo, WireReply,
};

/// Everything the command handler needs. In M0 this is just the loaded
/// config and daemon identity; M1+ adds live output state and a handle to
/// apply wallpaper changes.
pub struct Handler {
    pub pid: u32,
    pub config: Config,
    /// Live overview state, flipped by the niri IPC source (M2).
    pub overview_open: bool,
    /// Whether the niri IPC connection is currently up (M2).
    pub niri_connected: bool,
}

impl Handler {
    #[must_use]
    pub fn new(config: Config) -> Self {
        Self {
            pid: std::process::id(),
            config,
            overview_open: false,
            niri_connected: false,
        }
    }

    /// Handle one request. The [`ControlFlow::Break`] result means the accept
    /// loop should reply and then shut the daemon down.
    pub fn handle(&mut self, req: Request) -> (WireReply, ControlFlow<()>) {
        match req {
            Request::Version => (
                WireReply::ok(
                    serde_json::to_value(VersionInfo {
                        version: VERSION.to_string(),
                    })
                    .expect("VersionInfo serializes"),
                ),
                ControlFlow::Continue(()),
            ),
            Request::Get => (
                WireReply::ok(serde_json::to_value(self.status()).expect("Status serializes")),
                ControlFlow::Continue(()),
            ),
            Request::Quit => (WireReply::ok_empty(), ControlFlow::Break(())),
            Request::Set { .. } | Request::Reload | Request::Reset => (
                WireReply::err("not implemented yet (arrives in M1)"),
                ControlFlow::Continue(()),
            ),
        }
    }

    /// Build the `niribg get` payload. In M0 there is no live output list, so
    /// this reports the configured `default` slot plus any named
    /// `[output."NAME"]` overrides, with `logical`/`loaded` unknown.
    fn status(&self) -> Status {
        let cfg = &self.config;
        let mut names: Vec<&str> = cfg
            .output
            .keys()
            .map(String::as_str)
            .filter(|n| *n != crate::config::DEFAULT_SLOT)
            .collect();
        if names.is_empty() {
            names.push(crate::config::DEFAULT_SLOT);
        }

        let outputs = names
            .into_iter()
            .filter_map(|name| {
                let (resolved, has_named_table) = cfg.resolve(name).ok()?;
                Some(OutputStatus {
                    name: name.to_string(),
                    logical: None::<LogicalOutput>,
                    source: match resolved.path {
                        Some(p) => Source::Image {
                            path: p.to_string_lossy().into_owned(),
                        },
                        None => Source::Color,
                    },
                    mode: resolved.mode,
                    color: resolved.color,
                    // The synthetic `default` row is never itself an override.
                    overridden: has_named_table && name != crate::config::DEFAULT_SLOT,
                    loaded: false,
                    error: None,
                })
            })
            .collect();

        Status {
            pid: self.pid,
            niri_connected: self.niri_connected,
            blur: BlurStatus {
                enable: cfg.blur.enable,
                radius: cfg.blur.radius,
                dim: cfg.blur.dim,
                active: self.overview_open && cfg.blur.enable,
            },
            transition_ms: cfg.transition_ms,
            outputs,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn handler() -> Handler {
        Handler::new(Config::default())
    }

    #[test]
    fn version_reports_build() {
        let (reply, flow) = handler().handle(Request::Version);
        assert!(reply.ok);
        assert_eq!(flow, ControlFlow::Continue(()));
        let info: VersionInfo = serde_json::from_value(reply.data.unwrap()).unwrap();
        assert_eq!(info.version, VERSION);
    }

    #[test]
    fn quit_breaks_the_loop() {
        let (reply, flow) = handler().handle(Request::Quit);
        assert!(reply.ok);
        assert_eq!(flow, ControlFlow::Break(()));
    }

    #[test]
    fn get_reports_config_derived_status() {
        let cfg = Config::parse(
            "transition_ms = 100\n[blur]\nradius = 12\n[output.default]\npath = \"/w.jpg\"\n",
        )
        .unwrap();
        let mut h = Handler::new(cfg);
        h.overview_open = true;

        let (reply, _) = h.handle(Request::Get);
        let status: Status = serde_json::from_value(reply.data.unwrap()).unwrap();

        assert_eq!(status.transition_ms, 100);
        assert_eq!(status.blur.radius, 12);
        assert!(status.blur.active); // overview open + blur enabled
        assert_eq!(status.outputs.len(), 1);
        assert_eq!(status.outputs[0].name, "default");
        assert!(!status.outputs[0].overridden); // the default row is not an override
        assert!(!status.outputs[0].loaded);
        assert_eq!(
            status.outputs[0].source,
            Source::Image {
                path: "/w.jpg".into()
            }
        );
    }

    #[test]
    fn get_lists_named_overrides_not_default_when_present() {
        let cfg = Config::parse(
            "[output.default]\npath = \"/d.jpg\"\n[output.\"DP-1\"]\nmode = \"fit\"\n",
        )
        .unwrap();
        let (reply, _) = Handler::new(cfg).handle(Request::Get);
        let status: Status = serde_json::from_value(reply.data.unwrap()).unwrap();
        let names: Vec<_> = status.outputs.iter().map(|o| o.name.as_str()).collect();
        assert_eq!(names, ["DP-1"]);
        assert!(status.outputs[0].overridden);
    }

    #[test]
    fn blur_inactive_when_disabled_even_with_overview_open() {
        let cfg = Config::parse("[blur]\nenable = false\n").unwrap();
        let mut h = Handler::new(cfg);
        h.overview_open = true;
        let (reply, _) = h.handle(Request::Get);
        let status: Status = serde_json::from_value(reply.data.unwrap()).unwrap();
        assert!(!status.blur.active);
    }

    #[test]
    fn unimplemented_commands_error_without_breaking() {
        let (reply, flow) = handler().handle(Request::Reload);
        assert!(!reply.ok);
        assert_eq!(flow, ControlFlow::Continue(()));
    }
}
