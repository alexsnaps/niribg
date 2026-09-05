# Manual test checklist

Automated tests cover the pure logic (`cargo test` — render geometry, blur
pipeline, crossfade blend, easing, config/state, IPC dispatch, niri event
parsing, the image worker). This checklist is the pre-release pass for
everything that needs a real niri session.

Run it on a machine with niri, at least one output, and — for the
multi-output rows — a second display to plug in.

```sh
cargo build --release
BIN=target/release/niribg
SOCK=/tmp/niribg-test.sock            # so it doesn't fight your real daemon
```

Use `--socket "$SOCK"` on every command below, and a throwaway config via
`--config`. Watch the daemon's `-vv` log in another terminal.

## Wallpaper basics

- [ ] **Colour wallpaper.** `config` with only `[output.default] color = "#3050a0"`.
      `daemon` → the colour fills every output, edge to edge, under bars.
- [ ] **Image, `fill`.** Covers the output, no letterbox, centred crop.
- [ ] **Image, `fit`.** Whole image visible, letterbox bars are the `color`.
- [ ] **Image, `stretch`.** Fills exactly, aspect distorted.
- [ ] **Image, `center`.** 1:1 pixels, centred; a small image leaves a
      `color` border, a large one is cropped.
- [ ] **Fractional scale.** On a scaled output (e.g. 1.5), the wallpaper is
      crisp, not soft. Daemon log shows the physical buffer size
      (e.g. `2880x1800` for a 1920x1200 @1.5 output).
- [ ] **Bad path.** `set /nope.png` → command exits non-zero with a message;
      `niribg get` shows that output's `error`; the daemon keeps running and
      keeps the previous wallpaper.
- [ ] **`niribg get`** (human and `--json`) reports pid, `niri:` state,
      `blur:` state, `transition:`, and one row per output with size, scale,
      source, mode, and `[override]` only for real `[output."NAME"]` tables.

## Multi-output & hotplug

- [ ] **Per-output override.** `[output."<name>"]` with a different image →
      that output differs, others use `default`.
- [ ] **`set --output <name>`** changes just that output; `[override]`
      appears for it in `get`.
- [ ] **`set` with no `--output`** changes every output that has no named
      table.
- [ ] **Disconnected output.** `set --output DP-9 img.png` (with DP-9 absent)
      → `stored; DP-9 is not currently connected`, exit 0. `get` lists DP-9
      with `logical: null`.
- [ ] **Plug a monitor in** while the daemon runs → it gets a wallpaper
      (its override, or `default`) within a second. **Unplug** → no crash,
      no leak (log: `output disconnected`).

## Blur on overview

- [ ] **Open the overview** → each wallpaper crossfades to blurred + dimmed
      over ~`transition_ms`. **Close** → crossfades back to sharp.
- [ ] **`niribg get`** shows `blur: … currently ON` while the overview is
      open, `off` while closed.
- [ ] **Colour wallpaper** blurs too — the backdrop is the dimmed colour.
- [ ] **Fast toggle** — open, then close within ~100 ms. The second fade
      starts from the mid-fade image, no visible jump/flash.
- [ ] **`transition_ms = 0`** → overview toggle snaps instantly, no fade.
- [ ] **`blur.enable = false`** → overview toggle does nothing visually;
      `get` still shows `niri: connected` and `blur: disabled`.
- [ ] **`set` during the overview** → the new wallpaper appears already
      blurred; closing the overview crossfades it to sharp.

## Transitions

- [ ] **`set img.png`** (no flag) crossfades from the current wallpaper.
- [ ] **`set img.png --no-fade`** snaps.
- [ ] **`reload`** after editing `config.toml` → changed outputs crossfade,
      unchanged ones don't redraw.
- [ ] **Rapid `set A; set B`** (two commands back to back) → B wins; the
      daemon log shows `discarding stale render` for A.
- [ ] First paint on `daemon` start does **not** fade in from black — the
      fill colour shows immediately, then the image (fades if
      `transition_ms > 0`).

## niri IPC

- [ ] **Start the daemon before niri is ready** (or with `NIRI_SOCKET`
      pointing nowhere) → wallpaper still shows; log has one
      `niri IPC not available yet` line, then quiet; `get` shows
      `niri: disconnected`. It connects once niri is reachable.
- [ ] **Restart niri** (if you can spare the session) → the daemon's Wayland
      connection drops and it exits non-zero; your supervisor
      (`spawn-at-startup` / systemd) respawns it against the new instance.

## Lifecycle

- [ ] **`niribg quit`** → daemon exits 0, socket file removed.
- [ ] **`SIGTERM` / `SIGINT`** → same clean exit.
- [ ] **`SIGHUP`** → reload (same as `niribg reload`).
- [ ] **Second `daemon`** on the same socket → refused with
      `already listening … pass --replace`. With `--replace` → the old one
      exits, the new one takes over, `get` still answers.
- [ ] **No compositor** (`env -u WAYLAND_DISPLAY … daemon`) → exits non-zero
      with `connecting to the Wayland compositor`.
- [ ] **Broken config** (`bad = 1`) → exits non-zero, names the bad key, does
      not touch Wayland.
- [ ] **`state.json`** — `set` writes it; `set --no-persist` does not;
      `reset` removes it and reverts; a corrupt `state.json` is ignored with
      a warning, not fatal.

## Packaging

- [ ] `cargo build --release` emits `niribg.1` + `niribg.bash` / `_niribg` /
      `niribg.fish` under `target/release/build/niribg-*/out/`.
- [ ] `man target/release/build/niribg-*/out/niribg.1` renders.
- [ ] `objdump -p target/release/niribg | grep NEEDED` → only
      `libgcc_s`, `libm`, `libc`, `ld-linux` (no `libwayland`,
      `libxkbcommon`).
- [ ] `cargo publish --dry-run` succeeds.
