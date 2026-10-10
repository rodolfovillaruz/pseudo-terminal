mod client;
mod proto;
mod pty;
mod server;
mod session;
mod term;
mod tty;
mod vt;

use std::io::{self, Read, Write};
use std::os::fd::AsFd;
use std::os::unix::net::UnixStream;

use anyhow::{Context, Result, bail};
use clap::{CommandFactory, FromArgMatches, Parser, Subcommand};
use clap_complete::engine::{ArgValueCandidates, CompletionCandidate};

use crate::proto::Msg;

#[derive(Parser)]
#[command(about = "A tiny tmux/screen clone built to learn how they work")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Start a new session and attach to it
    #[command(alias = "new-session")]
    New {
        /// Session name (default: next free number)
        #[arg(short = 's')]
        name: Option<String>,
        /// Don't attach
        #[arg(short = 'd')]
        detached: bool,
        /// Command to run instead of $SHELL (passed to sh -c)
        command: Vec<String>,
    },
    /// Attach to a running session
    #[command(alias = "a", alias = "attach-session")]
    Attach {
        #[arg(short = 't', add = ArgValueCandidates::new(sessions))]
        target: Option<String>,
    },
    /// List sessions
    #[command(alias = "list-sessions")]
    Ls,
    /// Print the session's screen as text (tmux: capture-pane -p)
    #[command(alias = "capture-pane")]
    Capture {
        #[arg(short = 't', add = ArgValueCandidates::new(sessions))]
        target: Option<String>,
    },
    /// Kill a session
    #[command(alias = "kill-session")]
    Kill {
        #[arg(short = 't', add = ArgValueCandidates::new(sessions))]
        target: Option<String>,
    },
}

fn main() -> Result<()> {
    // Only acts when the shell calls back with $COMPLETE set (see README).
    clap_complete::CompleteEnv::with_factory(command).complete();
    let cli = Cli::from_arg_matches(&command().get_matches()).unwrap_or_else(|e| e.exit());
    match cli.cmd {
        Cmd::New { name, detached, command } => {
            if !detached && std::env::var_os("PTMUX").is_some() {
                bail!("sessions should be nested with care, unset $PTMUX to force");
            }
            let name = match name {
                Some(n) => n,
                None => session::next_free_name()?,
            };
            let cmd = if command.is_empty() {
                server::default_command()?
            } else {
                server::shell_command(&command.join(" "))?
            };
            let ws = pty::get_winsize(io::stdin().as_fd()).unwrap_or(pty::winsize(24, 80));
            server::start(&name, cmd, ws)?;
            if !detached {
                client::attach(&name)?;
            }
        }
        Cmd::Attach { target } => client::attach(&session::resolve(target)?)?,
        Cmd::Ls => {
            for name in session::list()? {
                println!("{name}");
            }
        }
        Cmd::Capture { target } => {
            let mut stream = connect(&session::resolve(target)?)?;
            Msg::Capture.send(&mut stream)?;
            let mut buf = Vec::new();
            stream.read_to_end(&mut buf)?;
            while let Some(msg) = proto::decode(&mut buf)? {
                if let Msg::Output(text) = msg {
                    io::stdout().write_all(&text)?;
                }
            }
        }
        Cmd::Kill { target } => {
            let name = session::resolve(target)?;
            let mut stream = connect(&name)?;
            Msg::Kill.send(&mut stream)?;
        }
    }
    Ok(())
}

/// The clap command with each subcommand's aliases appended to its help line.
/// They stay plain `alias`es: clap_complete shows one name per command, and with
/// `visible_alias` it picks the alphabetically first (`ptmux a<TAB>` -> `a`).
fn command() -> clap::Command {
    let mut cmd = Cli::command();
    let names: Vec<String> = cmd.get_subcommands().map(|sc| sc.get_name().to_owned()).collect();
    for name in names {
        cmd = cmd.mut_subcommand(name, |sc| {
            let aliases: Vec<&str> = sc.get_all_aliases().collect();
            let label = match aliases.len() {
                0 => return sc,
                1 => "alias",
                _ => "aliases",
            };
            let about = format!("{} [{label}: {}]", sc.get_about().map(|a| a.to_string()).unwrap_or_default(), aliases.join(", "));
            sc.about(about)
        });
    }
    cmd
}

/// Session names for `-t <TAB>`. Completion must stay quiet, so errors give no candidates.
fn sessions() -> Vec<CompletionCandidate> {
    session::list().unwrap_or_default().into_iter().map(CompletionCandidate::new).collect()
}

fn connect(name: &str) -> Result<UnixStream> {
    UnixStream::connect(session::socket_path(name)?).with_context(|| format!("can't find session: {name}"))
}
