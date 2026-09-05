//! `niribg` command-line entry point: parse args, set up logging, then either
//! run the daemon or send one command to a running one.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result};
use clap::{ArgAction, Parser, Subcommand};
use niribg::client;
use niribg::color::Color;
use niribg::config::Mode;
use niribg::daemon;
use niribg::paths;
use niribg::proto::{Request, Status, WireReply};

#[derive(Parser)]
#[command(name = "niribg", version, about, arg_required_else_help = true)]
struct Cli {
    /// Path to config.toml (default: $XDG_CONFIG_HOME/niribg/config.toml).
    #[arg(long, global = true, value_name = "PATH")]
    config: Option<PathBuf>,

    /// Control socket path (default: $XDG_RUNTIME_DIR/niribg-$WAYLAND_DISPLAY.sock).
    #[arg(long, global = true, value_name = "PATH")]
    socket: Option<PathBuf>,

    /// Increase log verbosity: -v = debug, -vv = trace.
    #[arg(short, long, global = true, action = ArgAction::Count)]
    verbose: u8,

    /// Only log warnings and errors.
    #[arg(short, long, global = true, conflicts_with = "verbose")]
    quiet: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the long-lived wallpaper daemon (foreground).
    Daemon {
        /// Take over from an already-running daemon.
        #[arg(long)]
        replace: bool,
    },
    /// Set a wallpaper image, or a solid colour, on one output or the default slot.
    Set {
        /// Image file. Omit to set a solid colour with --color.
        path: Option<PathBuf>,
        /// Output connector name (default: the shared `default` slot).
        #[arg(long, value_name = "NAME")]
        output: Option<String>,
        /// Fit mode.
        #[arg(long, value_name = "MODE")]
        mode: Option<Mode>,
        /// Fill / letterbox colour, e.g. #1e1e2e. Used alone if no path is given.
        #[arg(long, value_name = "HEX")]
        color: Option<Color>,
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

fn main() -> ExitCode {
    let cli = Cli::parse();
    init_logging(cli.verbose, cli.quiet);

    let result = match &cli.command {
        Command::Daemon { replace } => run_daemon(&cli, *replace),
        Command::Get { json } => run_get(&cli, *json),
        cmd => run_simple(&cli, cmd),
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            // `{:#}` renders the anyhow context chain on one line.
            eprintln!("niribg: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn init_logging(verbose: u8, quiet: bool) {
    use tracing_subscriber::EnvFilter;

    let filter = if let Ok(env) = std::env::var("NIRIBG_LOG").or_else(|_| std::env::var("RUST_LOG"))
    {
        EnvFilter::new(env)
    } else {
        let level = if quiet {
            "warn"
        } else {
            match verbose {
                0 => "info",
                1 => "debug",
                _ => "trace",
            }
        };
        EnvFilter::new(format!("niribg={level}"))
    };

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_writer(std::io::stderr)
        .init();
}

fn socket_path(cli: &Cli) -> Result<PathBuf> {
    match &cli.socket {
        Some(p) => Ok(p.clone()),
        None => paths::socket_path(),
    }
}

fn run_daemon(cli: &Cli, replace: bool) -> Result<()> {
    daemon::run(&socket_path(cli)?, cli.config.clone(), replace)
}

fn run_simple(cli: &Cli, cmd: &Command) -> Result<()> {
    let req = match cmd {
        Command::Set {
            path,
            output,
            mode,
            color,
            no_fade,
            no_persist,
        } => Request::Set {
            path: path.as_deref().map(to_abs_string),
            output: output.clone(),
            mode: *mode,
            color: *color,
            fade: !no_fade,
            persist: !no_persist,
        },
        Command::Reload => Request::Reload,
        Command::Reset => Request::Reset,
        Command::Quit => Request::Quit,
        Command::Daemon { .. } | Command::Get { .. } => unreachable!("handled earlier"),
    };

    let reply = client::send(&socket_path(cli)?, &req)?;
    finish(reply, |_data| Ok(()))
}

fn run_get(cli: &Cli, json: bool) -> Result<()> {
    let reply = client::send(&socket_path(cli)?, &Request::Get)?;
    finish(reply, |data| {
        let status: Status = serde_json::from_value(data).context("decoding status from daemon")?;
        if json {
            println!("{}", serde_json::to_string_pretty(&status)?);
        } else {
            print_status(&status);
        }
        Ok(())
    })
}

/// Turn a [`WireReply`] into a `Result`, running `on_ok` with the payload
/// (possibly `Null`) when the daemon reported success.
fn finish(reply: WireReply, on_ok: impl FnOnce(serde_json::Value) -> Result<()>) -> Result<()> {
    if reply.ok {
        on_ok(reply.data.unwrap_or(serde_json::Value::Null))
    } else {
        anyhow::bail!(
            reply
                .error
                .unwrap_or_else(|| "daemon reported failure".into())
        )
    }
}

fn to_abs_string(p: &Path) -> String {
    std::path::absolute(p)
        .unwrap_or_else(|_| p.to_path_buf())
        .to_string_lossy()
        .into_owned()
}

fn print_status(s: &Status) {
    let niri = if s.niri_connected {
        "connected"
    } else {
        "disconnected"
    };
    println!("daemon:     running (pid {})", s.pid);
    println!("niri:       {niri}");
    if s.blur.enable {
        let state = if s.blur.active { "ON" } else { "off" };
        println!(
            "blur:       enabled (radius {}, dim {:.2}) \u{2014} currently {state}",
            s.blur.radius, s.blur.dim
        );
    } else {
        println!("blur:       disabled");
    }
    println!("transition: {}ms", s.transition_ms);
    println!();

    for o in &s.outputs {
        let geom = o.logical.map_or_else(
            || "-".to_string(),
            |l| format!("{}x{} @{}", l.width, l.height, l.scale),
        );
        let source = match &o.source {
            niribg::proto::Source::Image { path } => path.clone(),
            niribg::proto::Source::Color => format!("(colour {})", o.color),
        };
        let tag = if o.overridden { "  [override]" } else { "" };
        let load = match &o.error {
            Some(e) => format!("  ERROR: {e}"),
            None if o.loaded => String::new(),
            None => "  (pending)".to_string(),
        };
        println!(
            "{:<10} {:<18} {:<8} {}{}{}",
            o.name, geom, o.mode, source, tag, load
        );
    }
}
