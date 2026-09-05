//! The `niribg` command-line interface.
//!
//! Kept free of the `niribg` library crate so `build.rs` can pull it in with
//! `#[path = "src/cli.rs"] mod cli;` to generate the man page and shell
//! completions. `main.rs` parses `--mode` / `--color` strings into the real
//! `Mode` / `Color` types.

use std::path::PathBuf;

use clap::{ArgAction, Parser, Subcommand};

/// Values accepted by `--mode`.
pub const MODES: [&str; 4] = ["fill", "fit", "stretch", "center"];

#[derive(Parser)]
#[command(name = "niribg", version, about, arg_required_else_help = true)]
pub struct Cli {
    /// Path to config.toml (default: $XDG_CONFIG_HOME/niribg/config.toml).
    #[arg(long, global = true, value_name = "PATH")]
    pub config: Option<PathBuf>,

    /// Control socket path
    /// (default: $XDG_RUNTIME_DIR/niribg-$WAYLAND_DISPLAY.sock).
    #[arg(long, global = true, value_name = "PATH")]
    pub socket: Option<PathBuf>,

    /// Increase log verbosity: -v = debug, -vv = trace.
    #[arg(short, long, global = true, action = ArgAction::Count)]
    pub verbose: u8,

    /// Only log warnings and errors.
    #[arg(short, long, global = true, conflicts_with = "verbose")]
    pub quiet: bool,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand)]
pub enum Command {
    /// Run the long-lived wallpaper daemon (foreground).
    Daemon {
        /// Take over from an already-running daemon.
        #[arg(long)]
        replace: bool,
    },
    /// Set a wallpaper image, or a solid colour, on one output or the
    /// default slot.
    Set {
        /// Image file. Omit to set a solid colour with --color.
        path: Option<PathBuf>,
        /// Output connector name (default: the shared `default` slot).
        #[arg(long, value_name = "NAME")]
        output: Option<String>,
        /// Fit mode.
        #[arg(long, value_name = "MODE", value_parser = MODES)]
        mode: Option<String>,
        /// Fill / letterbox colour, e.g. #1e1e2e. Used alone if no path is
        /// given.
        #[arg(long, value_name = "HEX")]
        color: Option<String>,
        /// Swap instantly instead of crossfading.
        #[arg(long = "no-fade")]
        no_fade: bool,
        /// Do not persist this change to state.json.
        #[arg(long = "no-persist")]
        no_persist: bool,
    },
    /// Print daemon and per-output status.
    Get {
        /// Emit JSON instead of a human table.
        #[arg(long)]
        json: bool,
    },
    /// Re-read config.toml from disk and re-apply.
    Reload,
    /// Clear state.json and revert to config.toml.
    Reset,
    /// Ask the daemon to exit cleanly.
    Quit,
}

/// The built `clap::Command` — used by `build.rs` for `clap_mangen` /
/// `clap_complete`. (Unused by the binary itself, which calls
/// `Cli::parse()`.)
#[must_use]
#[allow(dead_code)]
pub fn command() -> clap::Command {
    <Cli as clap::CommandFactory>::command()
}
