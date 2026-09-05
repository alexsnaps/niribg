# Changelog

All notable changes to `niribg` are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); this project uses
[Semantic Versioning](https://semver.org/spec/v2.0.0.html) and makes no
stability promise before `1.0.0`.

## [0.1.0] – unreleased

First release. A wallpaper daemon for the niri Wayland compositor that
crossfades to a blurred, dimmed backdrop while the overview is open.

### Added

- **`niribg daemon`** — one `background` `wlr-layer-shell` surface per
  output, painted from a CPU `wl_shm` slot pool. Multi-output, monitor
  hotplug, and correct physical-pixel rendering on fractional-scaled outputs
  (`wp_fractional_scale_v1` + `wp_viewporter`, with an integer-scale
  fallback).
- **Wallpaper sources** — image (`png` / `jpeg` / `webp` / `gif` first frame
  / `bmp` / `tiff`) or solid colour. Fit modes `fill` / `fit` / `stretch` /
  `center`; Lanczos3 downscale; alpha-composited over the fill colour. Images
  larger than 100 MP are rejected. Decode / compose runs on a worker thread —
  the event loop never blocks.
- **Blur on overview** — each output's blurred backdrop (downscale ×¼ → 3×
  box blur → upscale → dim) is rendered eagerly alongside the sharp
  wallpaper. niri's `OverviewOpenedOrClosed` event drives a crossfade between
  the two (`transition_ms`, ease-out cubic, interruptible; `--no-fade` /
  `transition_ms = 0` snap). Reconnects to niri IPC with exponential backoff.
- **`niribg set`** — image / colour / `--mode` / `--output`, with
  named-override-wins semantics and a stored-and-deferred path for
  disconnected outputs. Persisted to `state.json` (config is never
  rewritten); `--no-persist` for one-shot changes.
- **`niribg get`** (human + `--json`), **`reload`** (`SIGHUP` too),
  **`reset`**, **`quit`**, **`daemon --replace`**.
- Config: `$XDG_CONFIG_HOME/niribg/config.toml`, unknown keys rejected, `~` /
  `${VAR}` expansion, per-output overrides.
- Generated man page and bash/zsh/fish completions (build time, into
  `OUT_DIR`).
- `contrib/niribg.service` (systemd user unit).

### Notes

- Not a goal for `0.1.0`: animated / video wallpapers, slideshows, a manual
  blur toggle, per-output blur settings, GPU rendering. See `DESIGN.md`.

[0.1.0]: https://github.com/alexsnaps/niribg/releases/tag/v0.1.0
