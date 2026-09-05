# niribg — design

`niribg` is a Rust CLI + daemon that draws desktop wallpapers on the
[niri](https://github.com/YaLTeR/niri) Wayland compositor and blurs them while
niri's **overview** is open.

Status: pre-implementation. This document is the ratified plan; it is the
contract the milestones below build against.

---

## 1. Scope

### In scope for v1

- Per-output static image or solid-colour wallpaper via `wlr-layer-shell`.
- Blur (with dim) that fades in when niri's overview opens and out when it
  closes. This is the **only** blur trigger.
- One binary, subcommand-dispatched: a long-lived `daemon` plus short-lived
  client subcommands that talk to it over a Unix socket.
- Runtime wallpaper changes that survive a daemon restart via a separate state
  file (user's `config.toml` is never rewritten).
- Multi-output, hotplug-aware, fractional-scale-correct.
- Crossfade transitions (blur toggle and wallpaper swap share one primitive).

### Explicit non-goals for v1

`blur` subcommand / manual blur toggle · per-output blur knobs · animated
wallpapers (gif playback / apng / video / mpvpaper) · slideshow, directory or
random-from-folder or timed rotation · SVG · AVIF by default (behind an
off-by-default feature flag) · `tile` fit mode · GPU / shader effects ·
blur triggers other than overview (workspace occupancy, window class, idle,
manual) · inotify config auto-reload · in-process Wayland reconnect on
compositor restart · make/model/serial output matching (connector name only) ·
multiple `niribg` instances / multi-seat · frame-perfect matching of niri's
overview spring curve · `sd_notify` readiness · distro packaging beyond
crates.io + cargo-dist (no AUR/COPR/Nix) · metrics / any IPC surface beyond
`niribg get`.

---

## 2. Architecture

One binary, two roles:

- `niribg daemon` — long-lived. Owns one `background` layer-shell surface per
  output, subscribes to niri's IPC event stream, listens on a Unix socket.
- `niribg <subcommand>` — short-lived. Connects to the socket, sends one
  request line, prints the reply, exits.

Bare `niribg` prints help. The daemon runs in the foreground; niri's
`spawn-at-startup` (or a systemd user unit) supervises it. No fork/daemonize.

### Event loop

`calloop` (the loop SCTK already uses), single-threaded. Sources:

- Wayland display (SCTK).
- niri IPC socket fd (`niri-ipc`).
- Unix control socket (accept + per-connection read).
- `calloop` signal source for `SIGHUP` (→ reload).
- `calloop::ping` / channel from the image worker thread.

Heavy image work (decode, Lanczos scale, downscale → stack blur → upscale,
dim) runs on a **worker thread**. Finished buffers are handed back to the loop
via the ping/channel. The loop itself never decodes or blurs, so an overview
blur transition can't be stalled by a large `set`.

### On failure

- **niri IPC socket absent / niri restarts:** daemon keeps running, wallpaper
  keeps showing, IPC reconnects with backoff; on reconnect it re-queries
  `overview-state` to resync blur state. Blur simply doesn't work while
  disconnected.
- **Wayland connection breaks (compositor restart):** daemon exits non-zero;
  the supervisor respawns it against the new instance. No in-process
  reconnect.
- **Invalid `config.toml` at startup:** print error, exit non-zero.
- **Missing `config.toml`:** run with defaults.
- **Corrupt `state.json`:** warn, ignore, fall back to config.
- **Panic:** `panic = "abort"` in release → non-zero exit → respawn.

---

## 3. Rendering

CPU only, `wl_shm` buffers. No GPU, no shaders, no `wgpu`.

Per output, at physical-pixel resolution (logical size × fractional scale;
the daemon speaks `wp_fractional_scale_v1` + `wp_viewporter`):

1. Decode the source image (`image` crate). Hard cap ~100 MP; clear error
   above it. On decode failure: keep the current wallpaper (or fill colour),
   return an error over the socket.
2. Compose the **sharp** buffer: place the image per fit mode
   (`fill` = cover+crop, default; `fit` = contain+letterbox; `stretch`;
   `center`), Lanczos3 scale, fill remaining area with `color`. A colour-only
   source is just the fill.
3. Compose the **blurred** buffer: downscale the sharp buffer ×¼ → stack blur
   (O(1) in radius, ~40 lines hand-rolled) → bilinear upscale to full size →
   multiply by `(1 - dim)`. The upscale smooths; cost is ~1/16 the pixels.
4. Drop the decoded source pixels.

Both buffers are built eagerly at load time (on the worker), so the first
overview open is instant. Rebuilt per output on mode / resolution / scale
change and on hotplug.

Idle steady state: commit the sharp buffer once, then no redraws. Frame
callbacks are requested only while a transition animates.

### Transition primitive (`anim.rs` + crossfade compositor)

- One animation value per surface: `current` → `target`, lerped toward target
  each frame. Interruptible — a fast overview open/close retargets from the
  current position, never jumps.
- Default `transition_ms = 250`, ease-out cubic. `0` = instant swap.
- Blur toggle: `target` = 1.0 (overview open) or 0.0 (closed); each frame
  alpha-blend sharp↔blurred into a double-buffered shm buffer, commit.
- Wallpaper swap (`niribg set`): same crossfade, old composite → new
  composite, over `transition_ms`; `--no-fade` skips it.
- If overview toggles while a `set` decode is in flight: keep animating the
  old image's sharp/blurred buffers; swap to the new ones (crossfading at
  whatever blur alpha is current) when they land.
- Rapid `set` A→B: per-output monotonic generation counter on worker jobs;
  results with a stale generation are discarded even if they finish first.

niri fires `{"OverviewOpenedOrClosed":{"is_open":bool}}` at the *start* of the
toggle with no progress info, so the crossfade runs on its own timer and only
needs to feel concurrent, not frame-sync.

---

## 4. niri integration

Via the **`niri-ipc`** crate (pinned, bumped deliberately): typed
`Event::OverviewOpenedOrClosed { is_open }` plus the connect helper. The
socket fd is registered as a `calloop` source — no worker thread, no channel.
Honours `$NIRI_SOCKET`.

Overview is global in niri (single boolean), so blur is a single global
target applied to every output's surface. There is no per-output blur state.

niri renders the `background` layer un-zoomed behind the zoomed overview
cards, so an ordinary full-output surface is exactly what "blur on overview"
needs — no special handling.

---

## 5. Layer-shell surface

Same shape as `swaybg`:

| Property                | Value                                            |
|-------------------------|-------------------------------------------------|
| Layer                   | `background`                                     |
| Namespace               | `niribg`                                         |
| Anchor                  | all four edges                                   |
| Exclusive zone          | `-1` (ignore bars, span full output)             |
| Keyboard interactivity  | `none`                                           |
| Input region            | empty (clicks pass through)                      |
| Surface ↔ output        | one per `wl_output`, pinned via the output arg   |
| Size                    | request `0×0`, use the `configure` size × scale  |
| Frame callbacks         | only while a transition animates                 |

`niribg` replaces any other wallpaper client (e.g. the quickshell /
DankMaterialShell background). Running two wallpaper clients on `background`
at once gives compositor-defined z-order — documented as unsupported.

---

## 6. Configuration

### `config.toml`

`$XDG_CONFIG_HOME/niribg/config.toml` (→ `~/.config/niribg/config.toml`),
overridable with `--config PATH`. Optional — absent means defaults, and every
output shows its fill `color` with a log line pointing at `niribg set`.

```toml
transition_ms = 250              # crossfade; 0 = instant

[blur]
enable = true
radius  = 30
dim     = 0.15                   # 0.0..=0.5

[output.default]                 # applies to any output without an override
path  = "~/Pictures/wall.jpg"    # omit → solid `color`
mode  = "fill"                   # fill | fit | stretch | center
color = "#000000"

[output."DP-1"]                  # optional, repeatable
path = "~/Pictures/ultrawide.jpg"
mode = "fit"
```

- `[blur]` and `transition_ms` are **global only** — never per-output.
- Precedence, merged key-by-key: built-in defaults ← `[output.default]` ←
  `[output."NAME"]`. So `[output."DP-1"] { mode = "fit" }` keeps the
  default's `path` and `color`.
- Unknown keys are a **hard error** (`serde(deny_unknown_fields)`).
- `~` and `${VAR}` in paths are expanded.
- TOML format (`serde` + `toml`). Not KDL.

### `state.json`

`$XDG_STATE_HOME/niribg/state.json` (→ `~/.local/state/niribg/state.json`).
Written by the daemon when a `niribg set` runs without `--no-persist`.
Effective config = `config.toml` defaults ← `state.json` overrides, so runtime
changes survive a restart without touching the hand-edited config. `niribg
reset` clears it. Corrupt → ignored with a warning.

### Reload

`niribg reload` (socket) and `SIGHUP` re-read `config.toml` from disk and
re-apply, preserving the current blur alpha. No inotify watching.

---

## 7. CLI

```
niribg daemon              run the long-lived process (foreground)
    --replace              take over from an existing daemon
niribg set <PATH>          --output NAME   --mode fill|fit|stretch|center
                           --color HEX     (standalone if no PATH)
                           --no-fade       --no-persist
niribg get                 status; --json for machine form
niribg reload              re-read config.toml from disk
niribg reset               clear state.json, revert to config.toml
niribg quit                tell the daemon to exit cleanly
```

Global flags: `--config PATH`, `--socket PATH`, `-v` / `-vv` (debug/trace),
`-q` (warn), `--version`. Parser: `clap` derive.

- No `blur` subcommand — blur is overview-driven only. `niribg get` still
  reports blur state.
- `niribg set` does **not** autospawn the daemon; it errors with a hint to run
  `niribg daemon`.
- `set` for a not-currently-connected output (`DP-9`) is accepted, stored, and
  warned about; it applies if that output later connects.

### `niribg get --json`

```json
{
  "pid": 12345,
  "niri_connected": true,
  "blur": { "enable": true, "radius": 30, "dim": 0.15, "active": false },
  "transition_ms": 250,
  "outputs": [
    { "name": "eDP-1",
      "logical": { "width": 1920, "height": 1200, "scale": 1.5 },
      "source": { "type": "image", "path": "/abs/wall.jpg" },
      "mode": "fill", "color": "#000000",
      "overridden": false, "loaded": true, "error": null }
  ]
}
```

`source.type` is `image` or `color`. Paths are resolved absolute.
`blur.active` reflects live overview state. Per-output `loaded` / `error` make
a failed `set` visible in status, not only on the failing command's stderr.

---

## 8. Control protocol

| Aspect        | Choice                                                                       |
|---------------|-----------------------------------------------------------------------------|
| Transport     | Unix stream socket, `$XDG_RUNTIME_DIR/niribg-$WAYLAND_DISPLAY.sock`, `0600` |
| Override      | `--socket PATH`                                                             |
| Framing       | newline-delimited JSON, one request line → one response line               |
| Request       | internally-tagged serde enum: `{"cmd":"set", ...}`                          |
| Response      | `{"ok":true,"data":{...}}` / `{"ok":false,"error":"..."}`                   |
| Version skew  | `{"cmd":"version"}` returns build version; mixed installs fail loudly       |
| Client        | send one line, read one line, 5 s timeout, non-zero exit on error          |
| Stale socket  | on start: connect-test → if dead, unlink + rebind; if alive, "already      |
|               | running" unless `--replace` (send `quit`, wait, bind)                      |

Command handling operates on a `State` behind a trait so it can run in tests
without a live Wayland connection.

---

## 9. Project layout

Single binary crate, `lib.rs` + `main.rs` split. No workspace. Edition 2024,
tracks latest stable, no pre-1.0 MSRV promise.

```
src/
  main.rs        clap dispatch → daemon | client
  lib.rs
  proto.rs       shared request/response types
  client.rs      newline-JSON socket client
  config.rs      TOML load, defaults, deny_unknown_fields
  state.rs       state.json read/write, config ← state merge
  paths.rs       XDG resolution from env (hand-rolled)
  color.rs       hex parse
  daemon/
    mod.rs       calloop wiring, worker-thread handoff
    wayland.rs   SCTK: outputs, layer surfaces, shm, fractional scale, hotplug
    niri.rs      niri-ipc event source → blur target
    ipc.rs       UDS listener + command handling over `State`
    render.rs    fit/scale, stack blur, downscale/upscale, dim, crossfade
    anim.rs      animation value (current → target lerp, interruptible)
```

### Dependencies

`smithay-client-toolkit` (+ `calloop`), `niri-ipc`, `image` (png / jpeg /
webp / gif / bmp / tiff; avif behind an off feature), `clap` (derive),
`serde` + `serde_json` (socket + niri) + `toml` (config), `tracing` +
`tracing-subscriber`, `anyhow`. Stack blur and XDG path resolution are
hand-rolled (no `stackblur` / `dirs` deps). Signals via `calloop`'s built-in
source.

---

## 10. Testing

**Unit (no Wayland) — the bulk:**

- `render`: fit-mode rect math (fill / fit / stretch / center × src / dst /
  scale), stack blur (small known input → expected), downscale → blur →
  upscale pipeline, crossfade alpha lerp, dim multiply.
- `anim`: stepping, interruptible retarget, `0 ms` = instant, clamping.
- `config` / `state`: TOML parse + defaults, config ← state merge precedence,
  invalid config errors, corrupt state ignored.
- `proto`: serde round-trips. Hex-colour parse. XDG path resolution from env.

**Integration:**

- Socket IPC: `ipc.rs` + command handling against a mock Wayland handle —
  `set` / `get` / `reload` / `quit`, version skew, stale socket, `--replace`.
- niri parsing: replay recorded event lines → assert the blur target flips.
- Golden images: 2–3 committed PNGs of the blur + dim + crossfade pipeline at
  a fixed small size, tolerance compare.

**Wayland / layer-shell:** manual for v1 — `TESTING.md` checklist (all
outputs, hotplug survival, fractional-scale sharpness, overview fade in/out,
fast-toggle no-jump, `set` during overview, big image doesn't stall the
transition). Nested-compositor automation is a later investment.

**CI:** `cargo test` + `clippy -D warnings` + `fmt --check` + `cargo-deny`, on
stable.

---

## 11. Distribution

- **License:** Apache-2.0.
- `cargo install niribg` (crates.io) — baseline.
- `cargo-dist` → GitHub Releases with prebuilt `x86_64` gnu + musl tarballs.
- No AUR / COPR / Nix in v1.
- SemVer from `0.1.0`, no pre-1.0 stability promise.
- Docs: `README` (what / why, install, quickstart, full config reference,
  "vs `swaybg` + `swww`" positioning), `TESTING.md`, `CHANGELOG.md`
  (keep-a-changelog), `examples/config.toml`, generated `niribg.1` via a
  `clap_mangen` xtask.
- Service: `spawn-at-startup "niribg" "daemon"` is the documented default;
  `contrib/niribg.service` (user unit, `Restart=on-failure`,
  `WantedBy=graphical-session.target`) also shipped. Plain `Type=simple`.

---

## 12. Milestones

Each is independently testable.

- **M0 — Skeleton.** Cargo (ed. 2024), clap dispatch, `proto.rs`,
  `config` / `state` + tests, XDG paths, colour parse, `tracing`, CI,
  Apache-2.0, README stub. `niribg daemon` runs a plain single-threaded
  `UnixListener` accept loop (replaced by the `calloop` loop in M1 so its
  version is co-selected with SCTK); `niribg get` / `version` / `quit` work
  over the socket, `get` returning a stub status.
- **M1 — Static wallpaper.** SCTK connect, output enumeration, one
  `background` surface per output, shm, fractional scale + viewporter. Decode
  + fit-mode math + Lanczos scale + colour source. Commit sharp buffer.
  Hotplug. `set` → worker decode → commit (no fade). `state.json` +
  `--no-persist` + `reset`. **→ usable `swaybg` replacement.**
- **M2 — niri IPC + blur.** `niri-ipc` `calloop` source, backoff reconnect,
  resync via `overview-state`. Downscale → stack blur → upscale + dim, eager
  on the worker. Overview event → blur target 0/1 (snap, no fade yet).
  **→ blur follows overview.**
- **M3 — Transitions.** `anim.rs` (interruptible lerp), frame-callback
  crossfade compositor sharp↔blurred, 250 ms. Reused for `set` swap
  (`--no-fade`). Generation counter for rapid `set`. **→ feature-complete
  v1.**
- **M4 — Release.** Full `get` (human + JSON), `--replace`, `TESTING.md`
  pass, golden tests, `clap_mangen` man page, `contrib/niribg.service`,
  `examples/config.toml`, `CHANGELOG`, cargo-dist, tag `0.1.0`, publish to
  crates.io + GitHub Releases.
