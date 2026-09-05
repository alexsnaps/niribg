# niribg

Wallpaper daemon for the [niri](https://github.com/YaLTeR/niri) Wayland
compositor, with **blur on overview**: your wallpaper is sharp while you work
and fades to a blurred, dimmed backdrop whenever niri's overview is open.

> Status: early. M0 skeleton — the CLI, config, and control socket exist; the
> `daemon` does not draw anything yet. See [`DESIGN.md`](DESIGN.md) for the
> full plan and milestones.

## Why

`swaybg` draws wallpapers but doesn't know about niri. `swww` adds transitions
but still has no compositor awareness. `niribg` is niri-specific: it watches
niri's IPC event stream and blurs the wallpaper in sync with the overview,
with per-output images, hotplug handling, and correct fractional scaling.

## Install

```sh
cargo install niribg    # once published
```

## Quickstart

```sh
# in ~/.config/niri/config.kdl
spawn-at-startup "niribg" "daemon"
```

```sh
niribg set ~/Pictures/wall.jpg
niribg set ~/Pictures/ultrawide.jpg --output DP-1 --mode fit
niribg get
```

## Configuration

`~/.config/niribg/config.toml` (all optional):

```toml
transition_ms = 250

[blur]
enable = true
radius  = 30
dim     = 0.15

[output.default]
path  = "~/Pictures/wall.jpg"
mode  = "fill"          # fill | fit | stretch | center
color = "#000000"

[output."DP-1"]
path = "~/Pictures/ultrawide.jpg"
mode = "fit"
```

Runtime `niribg set` changes are remembered in
`~/.local/state/niribg/state.json` and survive a daemon restart; your
`config.toml` is never rewritten. `niribg reset` clears them.

## License

Apache-2.0.
