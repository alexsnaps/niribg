# niribg

Wallpaper daemon for the [niri](https://github.com/YaLTeR/niri) Wayland
compositor, with **blur on overview**: your wallpaper is sharp while you work
and crossfades to a blurred, dimmed backdrop whenever niri's overview is open.

- Per-output image or solid-colour wallpapers, `fill` / `fit` / `stretch` /
  `center`.
- Correct on fractional-scaled outputs; survives monitor hotplug.
- Blur + dim tied to niri's overview, with a configurable crossfade.
- One binary: a long-lived `daemon` plus quick `niribg set` / `get` /
  `reload` commands over a Unix socket.
- Runtime changes persist across restarts without ever rewriting your
  `config.toml`.
- Pure-Rust build — no `libwayland`, no `libxkbcommon`, no GPU.

## Why not `swaybg` / `swww` / `mpvpaper`?

| | `swaybg` | `swww` | `mpvpaper` | `niribg` |
|---|---|---|---|---|
| Per-output images | ✅ | ✅ | ✅ | ✅ |
| Fractional scale correct | ✅ | ✅ | ✅ | ✅ |
| Transitions | ❌ | ✅ | n/a | ✅ (crossfade) |
| **Blur/dim on niri overview** | ❌ | ❌ | ❌ | ✅ |
| Animated / video wallpaper | ❌ | ✅ (gif) | ✅ (video) | ❌ (v1) |

If you don't want the overview blur, `swaybg` or `swww` are perfectly good.
`niribg` exists for that one feature — it watches niri's IPC event stream and
swaps each output's wallpaper for a pre-rendered blurred version the moment
the overview opens.

## Install

```sh
# from crates.io
cargo install niribg

# or the prebuilt-binary installer (Linux x86_64, gnu or musl)
curl --proto '=https' --tlsv1.2 -LsSf \
  https://github.com/alexsnaps/niribg/releases/latest/download/niribg-installer.sh | sh

# or from source
git clone https://github.com/alexsnaps/niribg && cd niribg
cargo install --path .
```

The man page (`niribg.1`) and bash/zsh/fish completions are generated at
build time into `target/<profile>/build/niribg-*/out/`; the release tarballs
ship them alongside the binary. `cargo install` does not install them.

## Quickstart

Start the daemon from your niri config
(`~/.config/niri/config.kdl`):

```kdl
spawn-at-startup "niribg" "daemon"
```

…or as a systemd user unit (`contrib/niribg.service`):

```sh
install -Dm644 contrib/niribg.service ~/.config/systemd/user/niribg.service
systemctl --user enable --now niribg.service
```

Then set a wallpaper:

```sh
niribg set ~/Pictures/wall.jpg
niribg set ~/Pictures/ultrawide.jpg --output DP-1 --mode fit
niribg set --color '#1e1e2e'          # solid colour, no image
niribg get
```

`niribg` replaces any other wallpaper client — disable `swaybg` / `swww` /
your shell's built-in wallpaper, or you'll get flicker between the two.

## Configuration

`$XDG_CONFIG_HOME/niribg/config.toml` (→ `~/.config/niribg/config.toml`),
every key optional. Override the path with `--config`.

```toml
# Crossfade duration for blur toggles and wallpaper swaps, milliseconds.
# 0 disables all fades (instant swaps everywhere).
transition_ms = 250

[blur]
# When false, niribg is just a wallpaper daemon: it still connects to niri
# (so `niribg get` reports the connection) but never swaps to the blurred
# buffer.
enable = true
# Box-blur radius, applied at 1/4 resolution — 30 is a heavy, dreamy blur,
# which is what an overview backdrop wants.
radius = 30
# How much to darken the blurred backdrop, 0.0 .. 0.5.
dim = 0.15

# Applies to every output that has no [output."NAME"] section of its own.
[output.default]
path  = "~/Pictures/wallpaper.jpg"   # omit → solid `color`
mode  = "fill"                       # fill | fit | stretch | center
color = "#000000"                    # fill / letterbox colour; used alone if `path` is omitted

# Per-output override. Unset keys fall back to [output.default], then to the
# built-in defaults. Match by connector name (see `niri msg outputs`).
[output."DP-1"]
path = "~/Pictures/ultrawide.jpg"
mode = "fit"
```

| Key | Type | Default | Notes |
|---|---|---|---|
| `transition_ms` | integer | `250` | crossfade ms; `0` = instant |
| `blur.enable` | bool | `true` | |
| `blur.radius` | integer | `30` | px at ¼ resolution |
| `blur.dim` | float | `0.15` | `0.0`–`0.5`; out of range is a hard error |
| `output.<slot>.path` | string | — | `~` and `${VAR}` expanded; absent → colour |
| `output.<slot>.mode` | `fill`\|`fit`\|`stretch`\|`center` | `fill` | `center` is 1:1 pixels, like `swaybg` |
| `output.<slot>.color` | hex string | `#000000` | `#rgb`, `#rrggbb`, `#rrggbbaa` |

- **Fit modes:** `fill` covers and crops; `fit` letterboxes with `color`;
  `stretch` distorts to the exact aspect; `center` places the image at 1:1
  physical pixels, cropping or bordering with `color`.
- **Precedence:** built-in defaults ← `[output.default]` ←
  `[output."NAME"]`, merged key by key. A named `[output."NAME"]` table
  always wins over a `default`-slot change.
- Unknown keys are a **hard error** (catches typos). A missing config file is
  fine — every output shows its fill colour.
- `niribg reload` (or `SIGHUP`) re-reads the file. There is no automatic
  file-watching.

### Runtime changes & `state.json`

`niribg set` changes are written to
`$XDG_STATE_HOME/niribg/state.json` (→ `~/.local/state/niribg/state.json`)
and re-applied on the next daemon start — your hand-edited `config.toml` is
never touched. Effective config = `config.toml` ← `state.json`.

- `niribg set … --no-persist` applies the change for this session only (a
  later `reload` / `reset` drops it).
- `niribg reset` clears `state.json` and reverts to `config.toml`.

## Command reference

```
niribg daemon [--replace]      run the long-lived process (foreground)
niribg set [PATH]              set a wallpaper on one output or the default slot
    --output NAME              target one connector (default: every output
                               without its own [output."NAME"] table)
    --mode fill|fit|stretch|center
    --color HEX                fill colour; used alone if PATH is omitted
    --no-fade                  swap instantly, ignoring transition_ms
    --no-persist               apply for this session only
niribg get [--json]           daemon + per-output status
niribg reload                 re-read config.toml
niribg reset                  clear state.json, revert to config.toml
niribg quit                   ask the daemon to exit
```

Global: `--config PATH`, `--socket PATH`, `-v` / `-vv` (debug / trace),
`-q` (warnings only). Logging also honours `NIRIBG_LOG` / `RUST_LOG`.

`niribg daemon` runs in the foreground; niri's `spawn-at-startup` or a
systemd unit supervises it. It exits non-zero (with a message) if there is no
Wayland compositor, the config is broken, or `zwlr_layer_shell_v1` is
missing. Starting a second daemon on the same socket is refused unless
`--replace` is passed.

## Troubleshooting

- **No blur when I open the overview.** `niribg get` — if it says
  `niri: disconnected`, niri's IPC socket isn't reachable (`$NIRI_SOCKET`);
  niribg retries with backoff. If it says `blur: disabled`, set
  `blur.enable = true`.
- **Nothing on screen / flicker.** Another wallpaper client is running on the
  `background` layer. Disable it (`swaybg`, `swww`, your shell's wallpaper).
- **Wallpaper looks soft.** niribg renders at physical pixels via
  `wp_viewporter`; if your compositor lacks `wp_fractional_scale_manager_v1`
  it falls back to integer scale. On niri this shouldn't happen.
- **A `set` says `stored; DP-2 is not currently connected`.** Expected — it's
  saved and applies when that output appears.

## License

Apache-2.0.
