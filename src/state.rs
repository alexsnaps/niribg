// SPDX-License-Identifier: GPL-3.0-or-later
//! `state.json` — runtime `niribg set` overrides that outlive a daemon
//! restart without ever rewriting the user's hand-edited `config.toml`.
//!
//! Effective config = `config.toml` ← `state.json`, merged key by key, state
//! winning. A corrupt state file is ignored (with a warning), never fatal.
//! See `DESIGN.md` §6.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::config::{Config, OutputConfig};

/// Persisted runtime overrides, keyed by output slot name (a connector name
/// or [`crate::config::DEFAULT_SLOT`]).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct State {
    pub output: BTreeMap<String, OutputConfig>,
}

impl State {
    /// Load from `path`. A missing file yields an empty [`State`]. A file that
    /// fails to parse also yields an empty [`State`], with a `warn!` — a bad
    /// state file must not stop the daemon starting.
    #[must_use]
    pub fn load(path: &Path) -> State {
        match std::fs::read_to_string(path) {
            Ok(src) => match serde_json::from_str(&src) {
                Ok(state) => state,
                Err(e) => {
                    tracing::warn!(
                        path = %path.display(),
                        error = %e,
                        "ignoring unreadable state.json; falling back to config.toml"
                    );
                    State::default()
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => State::default(),
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "could not read state.json");
                State::default()
            }
        }
    }

    /// Write to `path` atomically (temp file in the same directory, then
    /// rename), creating parent directories as needed.
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        let mut json = serde_json::to_string_pretty(self).context("serializing state")?;
        json.push('\n');

        let tmp = path.with_extension("json.tmp");
        {
            let mut f = std::fs::File::create(&tmp)
                .with_context(|| format!("creating {}", tmp.display()))?;
            f.write_all(json.as_bytes())
                .and_then(|()| f.sync_all())
                .with_context(|| format!("writing {}", tmp.display()))?;
        }
        std::fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))?;
        Ok(())
    }

    /// Remove the state file if present (`niribg reset`).
    pub fn clear(path: &Path) -> Result<()> {
        match std::fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e).with_context(|| format!("removing {}", path.display())),
        }
    }

    /// Merge the fields set by one `niribg set` into `slot`'s entry. Fields
    /// left `None` in `patch` keep their existing value, so repeated `set`s
    /// accumulate (`set a.jpg` then `set --mode fit` → both stick).
    pub fn apply_set(&mut self, slot: &str, patch: OutputConfig) {
        crate::config::merge_output_patch(self.output.entry(slot.to_string()).or_default(), patch);
    }

    /// Overlay these overrides onto `cfg`, state winning key by key.
    pub fn apply_to(&self, cfg: &mut Config) {
        for (slot, over) in &self.output {
            let entry = cfg.output.entry(slot.clone()).or_default();
            if over.path.is_some() {
                entry.path = over.path.clone();
            }
            if over.mode.is_some() {
                entry.mode = over.mode;
            }
            if over.color.is_some() {
                entry.color = over.color;
            }
        }
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.output.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::color::Color;
    use crate::config::{DEFAULT_SLOT, Mode};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn scratch() -> PathBuf {
        static N: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "niribg-test-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn missing_file_is_empty_state() {
        let s = State::load(&scratch().join("nope/state.json"));
        assert!(s.is_empty());
    }

    #[test]
    fn corrupt_file_is_ignored() {
        let dir = scratch();
        let p = dir.join("state.json");
        std::fs::write(&p, b"{not json").unwrap();
        assert!(State::load(&p).is_empty());
    }

    #[test]
    fn save_load_roundtrip_and_reset() {
        let dir = scratch();
        let p = dir.join("sub/state.json");

        let mut s = State::default();
        s.apply_set(
            DEFAULT_SLOT,
            OutputConfig {
                path: Some("/w.jpg".into()),
                mode: Some(Mode::Fit),
                color: None,
            },
        );
        s.save(&p).unwrap();
        assert!(p.exists());
        assert_eq!(State::load(&p), s);

        State::clear(&p).unwrap();
        assert!(!p.exists());
        State::clear(&p).unwrap(); // idempotent
    }

    #[test]
    fn repeated_set_accumulates_fields() {
        let mut s = State::default();
        s.apply_set(
            "DP-1",
            OutputConfig {
                path: Some("/a.jpg".into()),
                mode: None,
                color: None,
            },
        );
        s.apply_set(
            "DP-1",
            OutputConfig {
                path: None,
                mode: Some(Mode::Center),
                color: Some(Color::BLACK),
            },
        );
        let e = &s.output["DP-1"];
        assert_eq!(e.path.as_deref(), Some("/a.jpg"));
        assert_eq!(e.mode, Some(Mode::Center));
        assert_eq!(e.color, Some(Color::BLACK));
    }

    #[test]
    fn state_overrides_config_key_by_key() {
        let mut cfg =
            Config::parse("[output.default]\npath = \"/cfg.jpg\"\nmode = \"fill\"\n").unwrap();

        let mut st = State::default();
        st.apply_set(
            DEFAULT_SLOT,
            OutputConfig {
                path: Some("/runtime.jpg".into()),
                mode: None,
                color: None,
            },
        );
        st.apply_to(&mut cfg);

        let (r, _) = cfg.resolve("eDP-1").unwrap();
        assert_eq!(
            r.path.as_deref(),
            Some(std::path::Path::new("/runtime.jpg"))
        ); // state won
        assert_eq!(r.mode, Mode::Fill); // config kept
    }

    #[test]
    fn deny_unknown_fields_in_state() {
        assert!(serde_json::from_str::<State>(r#"{"outputs":{}}"#).is_err());
    }
}
