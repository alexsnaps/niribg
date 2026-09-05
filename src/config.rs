// SPDX-License-Identifier: GPL-3.0-or-later
//! `config.toml` — parsing, defaults, and per-output resolution.
//!
//! Precedence, merged key by key:
//! built-in defaults ← `[output.default]` ← `[output."NAME"]`.
//! Unknown keys are a hard error. `[blur]` and `transition_ms` are global
//! only. See `DESIGN.md` §6.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::color::Color;
use crate::paths;

/// The name of the fallback output slot in `[output.default]`.
pub const DEFAULT_SLOT: &str = "default";

/// How an image is placed when its aspect ratio differs from the output's.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    /// Cover the output, cropping the overflow. The default.
    #[default]
    Fill,
    /// Fit the whole image, letterboxing with `color`.
    Fit,
    /// Distort the image to the output's exact aspect ratio.
    Stretch,
    /// Original size, centred, `color` around it.
    Center,
}

impl std::fmt::Display for Mode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Mode::Fill => "fill",
            Mode::Fit => "fit",
            Mode::Stretch => "stretch",
            Mode::Center => "center",
        })
    }
}

/// Returned by `<Mode as FromStr>` for an unrecognised name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseModeError(String);

impl std::fmt::Display for ParseModeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "unknown mode {:?} (expected fill, fit, stretch, or center)",
            self.0
        )
    }
}

impl std::error::Error for ParseModeError {}

impl std::str::FromStr for Mode {
    type Err = ParseModeError;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s {
            "fill" => Ok(Mode::Fill),
            "fit" => Ok(Mode::Fit),
            "stretch" => Ok(Mode::Stretch),
            "center" => Ok(Mode::Center),
            other => Err(ParseModeError(other.to_string())),
        }
    }
}

/// The whole parsed `config.toml`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Crossfade duration in milliseconds; `0` means an instant swap.
    pub transition_ms: u32,
    pub blur: BlurConfig,
    /// Keyed by output connector name, plus the special [`DEFAULT_SLOT`].
    pub output: BTreeMap<String, OutputConfig>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            transition_ms: 250,
            blur: BlurConfig::default(),
            output: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct BlurConfig {
    /// When `false`, `niribg` is just a wallpaper daemon: overview events are
    /// still received (for `niri_connected` in status) but ignored.
    pub enable: bool,
    /// Stack-blur radius in physical pixels, applied at the downscaled
    /// resolution. See `DESIGN.md` §3.
    pub radius: u32,
    /// How much to darken the blurred buffer, `0.0..=0.5`.
    pub dim: f64,
}

impl Default for BlurConfig {
    fn default() -> Self {
        Self {
            enable: true,
            radius: 30,
            dim: 0.15,
        }
    }
}

/// A single `[output.*]` table. Every field is optional so it can layer over
/// `[output.default]` and the built-in defaults.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct OutputConfig {
    /// Image path, `~` and `${VAR}` not yet expanded. Absent → colour-only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mode: Option<Mode>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub color: Option<Color>,
}

impl OutputConfig {
    /// Layer `over` on top of `self`, `over`'s set fields winning.
    fn merged_with(&self, over: &OutputConfig) -> OutputConfig {
        OutputConfig {
            path: over.path.clone().or_else(|| self.path.clone()),
            mode: over.mode.or(self.mode),
            color: over.color.or(self.color),
        }
    }
}

/// Fold the fields a single `niribg set` provided into `into`, leaving unset
/// fields untouched so repeated `set`s on one slot accumulate.
pub(crate) fn merge_output_patch(into: &mut OutputConfig, patch: OutputConfig) {
    if patch.path.is_some() {
        into.path = patch.path;
    }
    if patch.mode.is_some() {
        into.mode = patch.mode;
    }
    if patch.color.is_some() {
        into.color = patch.color;
    }
}

/// A fully resolved wallpaper spec for one output: no more `Option`s, paths
/// expanded to absolute.
#[derive(Debug, Clone, PartialEq)]
pub struct Resolved {
    pub path: Option<PathBuf>,
    pub mode: Mode,
    pub color: Color,
}

impl Config {
    /// Parse from a TOML string. Rejects unknown keys and out-of-range values.
    pub fn parse(toml_src: &str) -> Result<Config> {
        let cfg: Config = toml::from_str(toml_src).context("parsing config.toml")?;
        cfg.validate()?;
        Ok(cfg)
    }

    /// Read and parse a file. A missing file is *not* an error here — callers
    /// that want a default-on-absent should check first via [`Self::load`].
    pub fn read(path: &Path) -> Result<Config> {
        let src =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        Config::parse(&src)
    }

    /// Load the effective config: an explicit `--config` path if given, else
    /// the XDG location. A missing file yields [`Config::default`]; a present
    /// but broken file is an error. Returns the path it read, if any.
    pub fn load(explicit: Option<&Path>) -> Result<(Config, Option<PathBuf>)> {
        let path = match explicit {
            Some(p) => p.to_path_buf(),
            None => paths::config_file()?,
        };
        if path.exists() {
            Ok((Config::read(&path)?, Some(path)))
        } else if explicit.is_some() {
            bail!("config file not found: {}", path.display());
        } else {
            Ok((Config::default(), None))
        }
    }

    fn validate(&self) -> Result<()> {
        if !(0.0..=0.5).contains(&self.blur.dim) {
            bail!(
                "blur.dim must be between 0.0 and 0.5 (got {})",
                self.blur.dim
            );
        }
        Ok(())
    }

    /// Resolve the wallpaper spec for `output_name`, layering
    /// defaults ← `[output.default]` ← `[output."NAME"]` and expanding the
    /// path. `Ok(bool)` in the tuple is whether a `[output."NAME"]` override
    /// exists.
    pub fn resolve(&self, output_name: &str) -> Result<(Resolved, bool)> {
        let base = OutputConfig::default();
        let with_default = match self.output.get(DEFAULT_SLOT) {
            Some(d) => base.merged_with(d),
            None => base,
        };
        let overridden = self.output.contains_key(output_name);
        let merged = match self.output.get(output_name) {
            Some(o) => with_default.merged_with(o),
            None => with_default,
        };
        let path = match merged.path {
            Some(p) => Some(paths::expand(&p).with_context(|| format!("expanding path {p:?}"))?),
            None => None,
        };
        Ok((
            Resolved {
                path,
                mode: merged.mode.unwrap_or_default(),
                color: merged.color.unwrap_or_default(),
            },
            overridden,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_config_is_defaults() {
        let c = Config::parse("").unwrap();
        assert_eq!(c, Config::default());
        assert_eq!(c.transition_ms, 250);
        assert!(c.blur.enable);
        assert_eq!(c.blur.radius, 30);
        assert_eq!(c.blur.dim, 0.15);
    }

    #[test]
    fn partial_blur_table_keeps_other_defaults() {
        let c = Config::parse("[blur]\nradius = 8\n").unwrap();
        assert_eq!(c.blur.radius, 8);
        assert!(c.blur.enable);
        assert_eq!(c.blur.dim, 0.15);
    }

    #[test]
    fn unknown_key_is_hard_error() {
        assert!(Config::parse("transtion_ms = 100").is_err());
        assert!(Config::parse("[blur]\nradius = 10\nwiggle = true").is_err());
        assert!(Config::parse("[output.default]\npath = \"x\"\nfit = \"fill\"").is_err());
    }

    #[test]
    fn dim_out_of_range_rejected() {
        assert!(Config::parse("[blur]\ndim = 0.9").is_err());
        assert!(Config::parse("[blur]\ndim = -0.1").is_err());
        assert!(Config::parse("[blur]\ndim = 0.5").is_ok());
    }

    #[test]
    fn resolve_layers_default_then_override() {
        let c = Config::parse(
            r##"
            [output.default]
            path = "/base.jpg"
            mode = "fill"
            color = "#000000"

            [output."DP-1"]
            mode = "fit"
        "##,
        )
        .unwrap();

        let (dp1, overridden) = c.resolve("DP-1").unwrap();
        assert!(overridden);
        assert_eq!(dp1.path.as_deref(), Some(Path::new("/base.jpg"))); // inherited
        assert_eq!(dp1.mode, Mode::Fit); // overridden
        assert_eq!(dp1.color, Color::BLACK); // inherited

        let (other, overridden) = c.resolve("HDMI-A-1").unwrap();
        assert!(!overridden);
        assert_eq!(other.path.as_deref(), Some(Path::new("/base.jpg")));
        assert_eq!(other.mode, Mode::Fill);
    }

    #[test]
    fn resolve_with_no_tables_is_black_colour_only() {
        let c = Config::default();
        let (r, overridden) = c.resolve("eDP-1").unwrap();
        assert!(!overridden);
        assert_eq!(r.path, None);
        assert_eq!(r.mode, Mode::Fill);
        assert_eq!(r.color, Color::BLACK);
    }

    #[test]
    fn full_example_config_parses() {
        let src = r##"
            transition_ms = 200

            [blur]
            enable = true
            radius = 24
            dim = 0.2

            [output.default]
            path = "/home/x/wall.jpg"
            mode = "fill"
            color = "#1e1e2e"

            [output."DP-1"]
            path = "/home/x/uw.jpg"
            mode = "fit"
        "##;
        let c = Config::parse(src).unwrap();
        assert_eq!(c.transition_ms, 200);
        assert_eq!(c.blur.radius, 24);
        assert_eq!(c.output.len(), 2);
    }
}
