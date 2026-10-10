mod client;
mod proto;
mod pty;
mod server;
mod session;
mod term;

use std::io;
use std::os::fd::AsFd;
use std::os::unix::net::UnixStream;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};

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
        #[arg(short = 't')]
        target: Option<String>,
    },
    /// List sessions
    #[command(alias = "list-sessions")]
    Ls,
    /// Kill a session
    #[command(alias = "kill-session")]
    Kill {
        #[arg(short = 't')]
        target: Option<String>,
    },
}

fn main() -> Result<()> {
    match Cli::parse().cmd {
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
        Cmd::Kill { target } => {
            let name = session::resolve(target)?;
            let mut stream = UnixStream::connect(session::socket_path(&name)?)
                .with_context(|| format!("can't find session: {name}"))?;
            Msg::Kill.send(&mut stream)?;
        }
    }
    Ok(())
}
