//! Control-socket command handling.
//!
//! [`dispatch`] turns one [`Request`] into a [`Reply`] against a [`Control`]
//! implementation, so it is exercised in tests with no Wayland. The transport
//! (non-blocking accept, newline framing, deferred-reply plumbing) lives in
//! [`super`].

use crate::VERSION;
use crate::color::Color;
use crate::config::{Config, DEFAULT_SLOT, Mode};
use crate::proto::{
    BlurStatus, LogicalOutput, OutputStatus, Request, Source, Status, VersionInfo, WireReply,
};

/// Identifies an in-flight `set` whose render jobs have not all finished.
pub type Token = u64;

/// The outcome of dispatching one request.
pub enum Reply {
    /// Write this line back to the client now.
    Now(WireReply),
    /// The reply is produced later, when the render jobs under this token
    /// finish. The transport stashes the client's socket keyed by the token.
    Deferred(Token),
    /// Write this line back, then shut the daemon down.
    Shutdown(WireReply),
}

/// What [`Control::apply_set`] decided to do with a validated `set`.
pub enum SetDispatch {
    /// Jobs were enqueued under this token; reply when they finish.
    Deferred(Token),
    /// A conclusive answer already (a stored set for a disconnected output,
    /// or an error). Reply now.
    Now(WireReply),
}

/// Result of `reload` / `reset`, reported back as `{"changed": N}`.
pub struct ReloadSummary {
    pub changed: usize,
}

/// A parsed, validated `set` command. `slot == None` means the `default`
/// slot (every output without its own `[output."NAME"]` table).
pub struct SetRequest {
    pub slot: Option<String>,
    pub path: Option<String>,
    pub mode: Option<Mode>,
    pub color: Option<Color>,
    pub fade: bool,
    pub persist: bool,
}

/// The daemon capabilities [`dispatch`] needs. `DaemonState` implements this
/// for real; tests use an in-memory fake.
pub trait Control {
    fn status(&self) -> Status;
    fn apply_set(&mut self, req: SetRequest) -> SetDispatch;
    fn reload(&mut self) -> anyhow::Result<ReloadSummary>;
    fn reset(&mut self) -> anyhow::Result<ReloadSummary>;
}

/// Turn one request into a [`Reply`].
pub fn dispatch(req: Request, ctl: &mut impl Control) -> Reply {
    match req {
        Request::Version => Reply::Now(WireReply::ok(
            serde_json::to_value(VersionInfo {
                version: VERSION.to_string(),
            })
            .expect("VersionInfo serializes"),
        )),
        Request::Get => Reply::Now(WireReply::ok(
            serde_json::to_value(ctl.status()).expect("Status serializes"),
        )),
        Request::Quit => Reply::Shutdown(WireReply::ok_empty()),
        Request::Set {
            path,
            output,
            mode,
            color,
            fade,
            persist,
        } => {
            if path.is_none() && color.is_none() {
                return Reply::Now(WireReply::err("provide an image path or --color"));
            }
            match ctl.apply_set(SetRequest {
                slot: output,
                path,
                mode,
                color,
                fade,
                persist,
            }) {
                SetDispatch::Deferred(token) => Reply::Deferred(token),
                SetDispatch::Now(reply) => Reply::Now(reply),
            }
        }
        Request::Reload => Reply::Now(summary_reply(ctl.reload())),
        Request::Reset => Reply::Now(summary_reply(ctl.reset())),
    }
}

fn summary_reply(result: anyhow::Result<ReloadSummary>) -> WireReply {
    match result {
        Ok(s) => WireReply::ok(serde_json::json!({ "changed": s.changed })),
        Err(e) => WireReply::err(format!("{e:#}")),
    }
}

/// Build a [`Status`] from config alone (no live output geometry). Used until
/// M1 step 5 wires the live output list, and as the shape `niribg get`
/// reports for configured-but-disconnected named slots.
#[must_use]
pub fn config_status(cfg: &Config, niri_connected: bool, overview_open: bool) -> Status {
    Status {
        pid: std::process::id(),
        niri_connected,
        blur: BlurStatus {
            enable: cfg.blur.enable,
            radius: cfg.blur.radius,
            dim: cfg.blur.dim,
            active: overview_open && cfg.blur.enable,
        },
        transition_ms: cfg.transition_ms,
        outputs: config_output_rows(cfg),
    }
}

/// One [`OutputStatus`] row per named `[output."NAME"]` table, or a single
/// `default` row when there are none.
#[must_use]
pub fn config_output_rows(cfg: &Config) -> Vec<OutputStatus> {
    let mut names: Vec<&str> = cfg
        .output
        .keys()
        .map(String::as_str)
        .filter(|n| *n != DEFAULT_SLOT)
        .collect();
    if names.is_empty() {
        names.push(DEFAULT_SLOT);
    }

    names
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
                overridden: has_named_table && name != DEFAULT_SLOT,
                loaded: false,
                error: None,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::WireReply;

    /// In-memory [`Control`] for exercising [`dispatch`].
    struct FakeControl {
        config: Config,
        next_token: Token,
        set_calls: Vec<(Option<String>, Option<String>)>,
        set_result: SetOutcome,
        reload_changed: usize,
    }

    #[derive(Clone, Copy)]
    enum SetOutcome {
        Defer,
        NowOk,
    }

    impl FakeControl {
        fn new(config: Config) -> Self {
            Self {
                config,
                next_token: 1,
                set_calls: Vec::new(),
                set_result: SetOutcome::Defer,
                reload_changed: 0,
            }
        }
    }

    impl Control for FakeControl {
        fn status(&self) -> Status {
            config_status(&self.config, false, false)
        }

        fn apply_set(&mut self, req: SetRequest) -> SetDispatch {
            self.set_calls.push((req.slot.clone(), req.path.clone()));
            match self.set_result {
                SetOutcome::Defer => {
                    let t = self.next_token;
                    self.next_token += 1;
                    SetDispatch::Deferred(t)
                }
                SetOutcome::NowOk => SetDispatch::Now(WireReply::ok_empty()),
            }
        }

        fn reload(&mut self) -> anyhow::Result<ReloadSummary> {
            Ok(ReloadSummary {
                changed: self.reload_changed,
            })
        }

        fn reset(&mut self) -> anyhow::Result<ReloadSummary> {
            Ok(ReloadSummary { changed: 0 })
        }
    }

    fn fake() -> FakeControl {
        FakeControl::new(Config::default())
    }

    fn now(reply: Reply) -> WireReply {
        match reply {
            Reply::Now(w) => w,
            _ => panic!("expected Reply::Now"),
        }
    }

    #[test]
    fn version_reports_build() {
        let info: VersionInfo =
            serde_json::from_value(now(dispatch(Request::Version, &mut fake())).data.unwrap())
                .unwrap();
        assert_eq!(info.version, VERSION);
    }

    #[test]
    fn quit_is_shutdown() {
        assert!(matches!(
            dispatch(Request::Quit, &mut fake()),
            Reply::Shutdown(w) if w.ok
        ));
    }

    #[test]
    fn get_reports_config_status() {
        let cfg = Config::parse(
            "transition_ms = 100\n[blur]\nradius = 12\n[output.default]\npath = \"/w.jpg\"\n",
        )
        .unwrap();
        let status: Status = serde_json::from_value(
            now(dispatch(Request::Get, &mut FakeControl::new(cfg)))
                .data
                .unwrap(),
        )
        .unwrap();
        assert_eq!(status.transition_ms, 100);
        assert_eq!(status.blur.radius, 12);
        assert_eq!(status.outputs.len(), 1);
        assert_eq!(status.outputs[0].name, "default");
        assert!(!status.outputs[0].overridden);
    }

    #[test]
    fn set_without_path_or_color_is_rejected_before_apply() {
        let mut ctl = fake();
        let reply = now(dispatch(
            Request::Set {
                path: None,
                output: None,
                mode: None,
                color: None,
                fade: true,
                persist: true,
            },
            &mut ctl,
        ));
        assert!(!reply.ok);
        assert!(ctl.set_calls.is_empty(), "apply_set must not be called");
    }

    #[test]
    fn set_with_path_defers() {
        let mut ctl = fake();
        let reply = dispatch(
            Request::Set {
                path: Some("/x.jpg".into()),
                output: Some("DP-1".into()),
                mode: None,
                color: None,
                fade: true,
                persist: true,
            },
            &mut ctl,
        );
        assert!(matches!(reply, Reply::Deferred(1)));
        assert_eq!(
            ctl.set_calls,
            [(Some("DP-1".into()), Some("/x.jpg".into()))]
        );
    }

    #[test]
    fn set_with_only_color_reaches_apply() {
        let mut ctl = fake();
        ctl.set_result = SetOutcome::NowOk;
        let reply = now(dispatch(
            Request::Set {
                path: None,
                output: None,
                mode: None,
                color: Some(Color::BLACK),
                fade: true,
                persist: false,
            },
            &mut ctl,
        ));
        assert!(reply.ok);
        assert_eq!(ctl.set_calls, [(None, None)]);
    }

    #[test]
    fn reload_reports_changed_count() {
        let mut ctl = fake();
        ctl.reload_changed = 3;
        let reply = now(dispatch(Request::Reload, &mut ctl));
        assert!(reply.ok);
        assert_eq!(reply.data.unwrap()["changed"], 3);
    }
}
