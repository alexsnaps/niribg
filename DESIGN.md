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
- **M1 — Static wallpaper. ✅ done.** SCTK connect, output enumeration, one
  `background` surface per output, shm, fractional scale + viewporter. Decode
  + fit-mode math + Lanczos scale + colour source. Commit sharp buffer.
  Hotplug. `set` → worker decode → commit (no fade). `state.json` +
  `--no-persist` + `reset`. **→ usable `swaybg` replacement.** (See §13 for
  the implementation notes and carried-forward items.)
- **M2 — niri IPC + blur. ✅ done.** Raw `$NIRI_SOCKET` + niri-ipc *types* +
  `calloop` `Generic` source, `Timer` backoff reconnect (no resync query —
  niri replays state on connect). Downscale ×¼ → 3× box blur → upscale + dim,
  eager on the worker (both buffers per job). Overview event → instant
  sharp↔blurred swap (no fade yet). **→ blur follows overview.** (See §14.)
- **M3 — Transitions. ✅ done.** `anim.rs` (timed ease-out cubic),
  frame-callback crossfade `blend(from, to, t)` with snapshot-on-retarget,
  250 ms. Reused for `set` swap and `reload` changes (`--no-fade` /
  `transition_ms=0` snap). Per-output generation counter for rapid `set`.
  **→ feature-complete v1.** (See §15.)
- **M4 — Release.** Full `get` (human + JSON), `--replace`, `TESTING.md`
  pass, golden tests, `clap_mangen` man page, `contrib/niribg.service`,
  `examples/config.toml`, `CHANGELOG`, cargo-dist, tag `0.1.0`, publish to
  crates.io + GitHub Releases.

---

## 13. M1 implementation notes

Decisions from the M1 design pass. These refine §2–§10 for the "static
wallpaper" milestone; nothing here changes the v1 scope.

### Reconnaissance (niri 26.04, SCTK 0.21)

- niri advertises `wp_fractional_scale_manager_v1` v1, `wp_viewporter` v1,
  `zwlr_layer_shell_v1` v5, `wl_compositor` v6, `wl_output` v4, `wl_shm` v2.
- niri does **not** advertise `wp_single_pixel_buffer_v1` → a solid-colour
  wallpaper is an shm buffer filled with the colour (one code path with
  images).
- **SCTK 0.21 has no fractional-scale or viewporter support** → both
  protocols are wired by hand (`wayland-protocols` `staging` + `Dispatch`
  impls on the daemon state).
- SCTK 0.21 bundles `calloop` 0.14 + `calloop-wayland-source` 0.4 as default
  features and re-exports them → no version-skew risk.
- `ext_background_effect_manager_v1` is present on niri. Investigated for M2:
  it blurs what is *behind* a surface, and the wallpaper is the bottom-most
  surface, so it does not apply. M2 stays precompute-blur + crossfade.

### Event loop

Single-threaded `calloop`. The only thread is the image worker; it crosses
back via one `calloop::channel` carrying `Vec<u8>` payloads only.

- **Control socket:** `Generic` source on the listener fd
  (`set_nonblocking(true)`); on readable, `accept()`; each accepted stream
  becomes its own `Generic` source that accumulates bytes to `\n`, dispatches
  once, writes one reply line, then is dropped. No locking around daemon
  state.

### Reply timing

| Command | Reply |
| --- | --- |
| `version`, `get`, `quit` | immediate |
| `set` | **deferred** — the client's `UnixStream` is stashed by token; the worker result(s) for that token produce the `ok` / `err` (with per-output failures) |
| `reload`, `reset` | immediate `ok` once config is re-read and jobs are enqueued; decode errors surface via `niribg get`'s per-output `error` and logs |

If the client's 5 s read timeout fires first, the daemon still applies the
result. A client that disconnected before the deferred write just has its
write dropped.

### Image worker

```
Job      { token: u64, output: String, target: PixelSize, source: Resolved }
Result   { token: u64, output: String, outcome: Result<Rendered, String> }
Rendered { pixels: Vec<u8> /* BGRA, exact stride */, size: PixelSize }
```

One worker, FIFO. Worker: 100 MP dimension check → decode → Lanczos3 resize
per fit mode → compose over the fill colour → `Vec<u8>`. Never touches
Wayland. Colour-only sources are composed inline on the loop (no worker
round-trip). One job per (output × set). **Generation counter deferred to
M3** — no fade in M1, so a rapid `set A`→`set B` on one output is at worst a
one-frame flicker.

### Fractional scale

- Bind `wp_fractional_scale_manager_v1` + `wp_viewporter`; per surface
  `get_fractional_scale` + `get_viewport`. `preferred_scale` carries the
  scale in 1/120ths (u32).
- Physical buffer = `ceil(logical_w × scale / 120) × ceil(logical_h × scale
  / 120)`. Render there; `viewport.set_destination(logical_w, logical_h)`;
  `surface.set_buffer_scale(1)`.
- `preferred_scale` change → re-render that output.
- **Fallback** when `wp_fractional_scale_manager_v1` is absent (Sway etc.):
  integer scale via `wl_surface.preferred_buffer_scale` / output scale,
  `set_buffer_scale(n)`, no viewport. ~15 lines; correct on 1×/2×.

### `render.rs` (pure, unit-tested)

`place(src: Size, dst: Size, mode) -> Placement { dst_rect, src_crop }`, then
`compose(placement, src_pixels, fill, out, out_size, stride)`.

| Mode | Behaviour |
| --- | --- |
| `fill` | Lanczos3 cover, centred crop, no fill visible |
| `fit` | Lanczos3 contain, centred, `fill` in the bars |
| `stretch` | Lanczos3 to exactly `dst` |
| `center` | **1:1 physical pixels**, centred; crop if larger, `fill` border if smaller (matches `swaybg`) |

`fill`/`fit`/`stretch` do one Lanczos3 pass straight to the final rect size.
`center` does no scaling. EXIF orientation is ignored in M1.

### `ipc.rs` boundary

```rust
enum Reply { Now(WireReply), Deferred(Token), Shutdown(WireReply) }

trait Control {
    fn status(&self) -> Status;
    fn apply_set(&mut self, req: SetRequest) -> SetDispatch;   // Deferred(token) | Now(err)
    fn reload(&mut self) -> anyhow::Result<ReloadSummary>;
    fn reset(&mut self)  -> anyhow::Result<ReloadSummary>;
}

fn dispatch(req: Request, ctl: &mut impl Control) -> Reply
```

`DaemonState` implements `Control` in `daemon/mod.rs`. Tests drive `dispatch`
against a `FakeControl`. The deferred-reply plumbing (stash `UnixStream` by
token, match worker results) lives in `daemon/mod.rs`, not `ipc.rs`. M0's
`ipc::tests` port to `FakeControl`.

### Dependencies added

```toml
smithay-client-toolkit = "0.21"
wayland-client   = "0.31"
wayland-protocols = { version = "0.32", features = ["client", "staging"] }
image = { version = "0.25", default-features = false,
          features = ["jpeg", "png", "gif", "bmp", "tiff", "webp"] }
calloop = { version = "0.14", features = ["signals"] }
```

`cargo-deny`'s license allowlist gains `MPL-2.0` if a wayland-rs crate trips
it.

### Startup & signals

1. args → logging.
2. `Config::load` + `State::load` → effective config (`config ← state`).
3. `Connection::connect_to_env` → `registry_queue_init` → bind required
   globals (`wl_compositor`, `wl_shm`, `zwlr_layer_shell_v1`, `wl_output`).
   A missing required global, or a connect failure → exit non-zero with a
   message naming it. `wp_fractional_scale_manager_v1` / `wp_viewporter` are
   optional (fallback above).
4. calloop loop: `WaylandSource`, control-socket `Generic`, worker-result
   `channel`, signal source.
5. Per output as it arrives: `background` layer surface (namespace `niribg`,
   anchor all, exclusive `-1`, no keyboard, empty input region, size `0×0`)
   + fractional-scale + viewport. First `configure` → paint (colour inline;
   image → worker job).

| Signal | Action |
| --- | --- |
| `SIGHUP` | reload |
| `SIGTERM` / `SIGINT` | clean shutdown: drop surfaces, unlink socket, exit `0` |

### `set` semantics

| `path` | `--color` | Result |
| --- | --- | --- |
| set | — | image, letterbox = current/default colour |
| set | set | image, letterbox = given colour |
| — | set | solid colour |
| — | — | error: "provide an image path or --color" |

- `--output NAME` → that connector's slot; no `--output` → the `default`
  slot, applied to every connected output with **no `[output."NAME"]`**
  override (a named override always wins over a `default`-slot `set`).
- `--output` naming a disconnected connector → accept, persist, reply `ok`
  with an informational note; applies on hotplug.
- Order: build field-wise `patch` → (unless `--no-persist`)
  `state.apply_set` + `state.save` → rebuild effective config → re-resolve
  affected outputs → enqueue one job per affected connected output sharing a
  `token` → last job for the token produces the reply.

### shm & commit

One shared `SlotPool`, `Xrgb8888`, opaque region = full surface. Commit:
attach at (0,0) → `damage_buffer(0,0,w,h)` → `viewport.set_destination` →
`commit`. SCTK acks the layer-surface configure. No frame callbacks in M1
(those arrive with transitions in M3).

### `DaemonState` shape

Keyed by `wl_output` `ObjectId`. Per entry: connector name, surface, layer
surface, optional frac/viewport objects, `scale_120`, last logical size,
current `Resolved` spec, `status` (Pending | Loaded | Failed(String)),
`last_token`. Plus: effective `config`, last-read `disk_config`, runtime
`state`, `config_path`, `state_path`, `worker_tx`, `next_token`,
`pending_sets: HashMap<Token, {stream, outstanding, failures}>`,
`loop_signal`. `get` lists live outputs **and** configured-but-disconnected
named slots.

### Tests added in M1

- `render::place` / `render::compose` across all four modes and
  src/dst-size relationships; stride with awkward widths; Lanczos sanity.
- `scale_120` physical-size math (1.0, 1.25, 1.5, 2.0).
- config diff for `reload` (which output names changed).
- `set` arg → `patch` + the source-resolution table (incl. both-absent
  error).
- `ipc::dispatch` against `FakeControl` (ports M0's `ipc::tests`).
- `pending_sets`: multi-output token, partial failure lists failures,
  client-gone drops cleanly.
- worker integration: PNG fixture in `tests/fixtures/` → `Rendered`; missing
  path / oversize → `Err`.
- full socket round-trip against a stub-Wayland `DaemonState`.

Golden-image tests stay M3.

### Build order

1. ✅ Deps + calloop skeleton (control socket + signals in calloop; `Control`
   trait; port `ipc::tests`; `set` still stubbed).
2. ✅ `render.rs` pure + tests.
3. ✅ Worker thread + `calloop::channel` + fixture tests.
4. ✅ Layer surfaces + fractional scale (+ integer fallback); paint fill
   colour. Multi-output + hotplug.
5. ✅ Wire image sources through the worker; `get` reports live
   `logical`/`loaded`/`error`.
6. ✅ `set` / `reload` / `reset` for real; deferred replies;
   disconnected-output stored sets.

### M1 status — done

Verified on niri 26.04: colour + image wallpapers on the `background` layer,
fractional scale 1.5 → physical 2880×1800 buffers, `set` (persist /
`--no-persist` / `--output` / `--mode` / `--color`, named-override-wins,
disconnected-output stored + noted), `reload` (re-resolve + repaint changed),
`reset` (clear `state.json`), live `niribg get`, hotplug, and the failure
paths (no compositor / broken config / missing required global → exit 1 with
a message). 62 unit tests; clippy + fmt clean.

Carried forward:

- **Generation counter** for rapid `set` stays M3 (M1 uses a
  `physical() == rendered.size` staleness check).
- **`--no-persist` is dropped by `reload`/`reset`** (it never enters
  `state.json`) — documented behaviour, not a bug.
- **First image paint** shows the fill colour for the ~decode+Lanczos
  duration (a 4× upscale is ~300 ms), then swaps. M3's crossfade will make
  the swap a fade; a faster resize (`fast_image_resize`) is a later option.
- **`DaemonState` integration-test harness** (stub Wayland + socket
  round-trip from DESIGN §10) not built — the command-dispatch layer is
  covered by `ipc::dispatch` + `FakeControl`, and step 6 behaviour was
  verified manually against niri. Revisit when `DaemonState` stabilises.
- **`update_output`** is a no-op; `configure` + `preferred_scale` cover
  runtime resolution/scale changes.

---

## 14. M2 implementation notes

Decisions from the M2 design pass. Refines §3–§4.

### niri IPC — mechanism

`niri-ipc` v26.4.0. Its `Socket` helper is **blocking-only with no fd
accessor**, so we use only its `Event` / `Request` / `Reply` *types* and
drive the socket ourselves:

- Raw `UnixStream::connect($NIRI_SOCKET)` → write `"EventStream"\n` → read the
  `{"Ok":"Handled"}` line → set non-blocking → register as a `calloop`
  `Generic` source, newline-framed like the control socket, each line
  `serde_json::from_str::<niri_ipc::Event>`.
- Single-threaded. Clean shutdown = drop the source.
- **No resync query.** niri replays full current state on every connect, so
  the first `OverviewOpenedOrClosed` after `EventStream` sets the blur
  target.
- **Reconnect:** `calloop::timer::Timer`, exponential backoff 250 ms → ×2 →
  cap 5 s, reset on a successful handshake. `$NIRI_SOCKET` missing → same
  loop, one `info` log then quiet.
- **On EOF / read error:** remove the source, `niri_connected = false`, blur
  target → `false` (show sharp), schedule reconnect.

### Blur pipeline

`render::blur_dim(sharp_bgra, size, radius, dim) -> Vec<u8>` — pure:

1. Downscale sharp **×¼**, box average.
2. **Stack blur** the small buffer (hand-rolled, no dep). `blur.radius` is
   applied at the **downscaled** resolution (default 30 ⇒ a heavy overview
   backdrop).
3. Bilinear upscale to full size.
4. Multiply RGB by `(1 − dim)`; leave the X byte.

The worker computes both buffers in one job:

```rust
Job     { …, blur: BlurParams { radius: u32, dim: f64 } }
Rendered { size, sharp: Vec<u8>, blurred: Option<Vec<u8>> }
```

`blurred` is computed **whenever the source is an image** (≈1/16 the pixels
+ a down/upscale; tens of ms), so toggling `blur.enable` via `reload` is
instant. A colour source's `blurred` is `render::solid(color × (1 − dim))`,
built inline.

### Applying the toggle (M2 = snap, no fade)

- `DaemonState` gains `overview_open: bool`, `niri_connected: bool`.
  `OutputEntry` gains `sharp: Option<Vec<u8>>`, `blurred: Option<Vec<u8>>`
  (the last render's buffers, ≈41 MB/output at 2880×1800 — §9's budget; M3's
  crossfade blends them).
- `commit_pixels` splits: the attach/damage/commit half stays; `present(id)`
  picks `overview_open && blur.enable && blurred.is_some() ? blurred : sharp`
  and commits it.
- `OverviewOpenedOrClosed` → set `overview_open`, `present` every connected
  output.
- Worker result → store both buffers, then `present(id)` (respects current
  overview state).
- `blur.enable = false` → IPC still connects and events still arrive (so
  `niri_connected` / `blur.active` report truthfully); `present` always picks
  sharp.
- `niribg get`: `blur.active = overview_open && blur.enable`;
  `niri_connected` real. Human line: `… — currently ON` / `off`.

### M2 build order

1. ✅ `render::blur_dim` + `downscale_avg` / `upscale_bilinear` /
   `box_blur_3x` (f32 internally — integer truncation across 6 passes bled
   ~84 % of the mass), `dim_in_place`. Pure + 9 tests.
2. ✅ Worker returns `Rendered { sharp, blurred }`; `Job` carries
   `blur_radius`/`blur_dim`; `OutputEntry` retains both buffers +
   `buffers_size`; `present()` split from `commit_pixels`; colour source's
   `blurred` = `solid(color.dimmed(dim))` inline.
3. ✅ `daemon/niri.rs`: raw `$NIRI_SOCKET` connect, `"EventStream"` +
   hand-read `{"Ok":"Handled"}` line (byte-at-a-time so no over-read),
   non-blocking `Generic` source, `Timer` backoff reconnect,
   `OverviewOpenedOrClosed` → `set_overview_open` → `present` every output.
4. ✅ Polish: `set_overview_open` early-returns when `blur.enable=false` (no
   wasteful re-commit); one `info` line on first IPC failure then quiet;
   `check_handshake` / `overview_from_line` extracted + 4 parse tests.

### M2 status — done

Verified on niri 26.04: `niri: connected` on startup, initial
`OverviewOpenedOrClosed` consumed without a spurious blur, overview
open/close → instant sharp↔blurred swap for both image and colour
wallpapers, `blur.enable=false` receives events but never swaps,
`$NIRI_SOCKET` absent/bad → one info line + exponential-backoff retry while
the wallpaper still works, `set`/`reload`/`reset` unaffected. 75 unit tests;
clippy + fmt clean.

Carried forward:

- **Reconnect after niri actually restarts** (compositor exit) not manually
  verified — would mean restarting the session. The EOF → `niri_lost` →
  reconnect path is the same one the bad-socket retry test exercises.
- **Worker time** for an image is now ~320 ms in release (Lanczos 4× upscale
  + blur pipeline). Still fully off the loop; the placeholder colour shows
  first. M3's crossfade covers the swap; `fast_image_resize` remains an
  option.
- **`box_blur_3x` allocates a scratch `Vec<f32>` per output per render** —
  fine at M2's cadence; pool it if M3's per-frame path ever needs it (it
  won't — M3 blends the two finished `u8` buffers, no re-blur).

---

## 15. M3 implementation notes

Decisions from the M3 design pass. Refines §3 (the transition primitive).

niri has **no compositor-side alpha** (`wp_alpha_modifier_v1` absent), so the
crossfade is a CPU per-byte blend into a fresh shm buffer.

### The primitive

One `Transition` per output, `Option`, `None` when idle:

```rust
struct Transition { from: Vec<u8>, to: Vec<u8>, size: Size, anim: Anim }
```

Displayed pixels = `render::blend(from, to, anim.eased())`, blended each
frame. **Every retarget snapshots**: `from ← <current displayed pixels,
cloned or blended once>`, `to ← <new target buffer, cloned>`, timer
restarts. Blur open / close / `set` / `reload`-change are all the same code
path — only `to` differs. On `anim.done()`: commit `to` exactly, drop the
`Transition`, go idle (zero-redraw steady state, as M1/M2).

`from`/`to` are owned `Vec<u8>` (≈2×20 MB cloned per transition start;
transitions happen ~1/sec at most).

### `anim.rs`

Minimal, timing only — interruption is the caller replacing the whole
`Transition`.

```rust
pub struct Anim { start: Instant, duration: Duration }
Anim::new(duration) · progress() -> f32 (0..=1) · eased() -> f32
  (ease-out cubic, 1-(1-p)^3) · done() -> bool
```

`duration` from `config.transition_ms`; `0` never builds an `Anim` (instant
path).

### Driving it

- **Retarget:** build `Transition`, blend+commit frame 0 synchronously,
  `surface.frame(qh, FrameCallbackData(surface.clone()))`. Per-output
  `frame_pending: bool` so callbacks never stack.
- **`CompositorHandler::frame`:** match surface → entry, clear
  `frame_pending`. If it has a `Transition`: `t = anim.eased()` from a
  wall-clock `Instant` (the compositor `time` arg is ignored — the eased
  timer self-corrects if frames drop). Blend → commit. `t < 1` → request
  another frame; `t ≥ 1` → commit `to` exactly, drop the `Transition`.
- **Idle** → no frame requested.
- `set_overview_open` / `apply_rendered` / `repaint_tagged` call
  `present_target(id, instant: bool)` instead of `present()`. `instant`
  commits directly (M2 behaviour); otherwise start/retarget the
  `Transition`.

### What fades

Overview open/close · `set` (unless `--no-fade`) · `reload`/`reset` changed
outputs. **Not**: first paint (nothing to fade from), scale/resolution
change (snap), `transition_ms = 0` (global instant).

### Generation counter

`OutputEntry.generation: u64`, bumped in `repaint_tagged` before each job
submit. `worker::Job` / `worker::JobResult` carry it. `apply_rendered`
discards buffers whose `generation != entry.generation` (still calls
`note_result` so the deferred `set` reply resolves). Replaces the
`output_wants` size check. `token` routes the reply; `generation` decides
whose pixels win — separate concerns.

### M3 build order

1. `anim.rs` + `render::blend()`. Pure + tests.
2. `Transition` + `present_target(id, instant)`; `OutputEntry` gains
   `transition` / `frame_pending` / `generation`; frame-callback loop in
   `CompositorHandler::frame`.
3. Generation counter through `Job`/`JobResult`; drop `output_wants`.
4. `--no-fade` threaded through `set_wallpaper`; first-paint / scale-change
   pass `instant=true`; `TESTING.md` manual pass.

### Tests

`render::blend` (t=0→a, t=1→b, t=0.5→midpoint ±1, length, clamp) · `anim`
(eased/progress/done at 0, mid, past-end via `Instant` arithmetic). No golden
PNGs — the blur pipeline has statistical tests; the crossfade's testable
core is `blend` + `anim`; the visual is a `TESTING.md` check.

### M3 status — done

Verified on niri 26.04: overview open/close crossfades ~7 frames over 250 ms
with a visible ease-out curve; `set` fades placeholder→image; `set
--no-fade` and `transition_ms=0` snap; `reload` changes fade; fast overview
toggle (open then close ~90 ms in) retargets from the mid-fade blend with no
jump; rapid `set A; set B` → B wins, A's late render logged "discarding stale
render". `blur.enable=false` → zero crossfades on toggle. Failure paths and
M1/M2 behaviour unchanged. 84 unit tests; clippy + fmt clean.

Carried forward:

- **`commit_pixels` only re-sends surface geometry (opaque region, buffer
  scale, viewport dest) when the size changed** — a 15-frame fade no longer
  churns 15 throwaway `wl_region` objects.
- **`render::blend` `vec![0u8; n]` then overwrites** (~2–3 ms of the
  ~6–8 ms/frame budget at 2880×1800). `wide`/SIMD or `spare_capacity_mut` is
  the win if 4K users report choppiness; the eased wall-clock timer already
  makes dropped frames a non-issue for *timing*.
- **Frame callbacks arrive at ~30 Hz here**, not 60 — niri's pacing for a
  background-layer surface. 7 frames / 250 ms still reads as a smooth fade;
  not worth chasing.
