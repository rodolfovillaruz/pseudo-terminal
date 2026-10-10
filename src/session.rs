//! Where session sockets live and how sessions are found.
//!
//! A session *is* its socket: `<dir>/<name>`, with the server log next to
//! it as `<dir>/<name>.log`. tmux instead has one server per socket
//! (`/tmp/tmux-$UID/default`) holding many sessions. screen, like us, has
//! one socket per session (`<pid>.<tty>.<host>` in its socket dir).

use std::fs;
use std::io::ErrorKind;
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};

pub fn socket_dir() -> Result<PathBuf> {
    let dir = match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(run) => PathBuf::from(run).join("ptmux"),
        None => PathBuf::from(format!("/tmp/ptmux-{}", unsafe { libc::getuid() })),
    };
    fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    // Anyone who can connect to a socket can type into the session.
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))?;
    Ok(dir)
}

pub fn socket_path(name: &str) -> Result<PathBuf> {
    if name.is_empty() || name.contains(['/', '.']) {
        bail!("bad session name: {name:?}");
    }
    Ok(socket_dir()?.join(name))
}

/// Live session names, sorted. Sockets nobody listens on are removed.
pub fn list() -> Result<Vec<String>> {
    let mut names = Vec::new();
    for entry in fs::read_dir(socket_dir()?)? {
        let entry = entry?;
        if !entry.file_type()?.is_socket() {
            continue;
        }
        match UnixStream::connect(entry.path()) {
            Ok(_) => names.push(entry.file_name().to_string_lossy().into_owned()),
            Err(e) if e.kind() == ErrorKind::ConnectionRefused => {
                let _ = fs::remove_file(entry.path());
                let _ = fs::remove_file(entry.path().with_extension("log"));
            }
            Err(_) => {}
        }
    }
    names.sort_by_key(|n| (n.parse::<u64>().unwrap_or(u64::MAX), n.clone()));
    Ok(names)
}

/// Smallest unused number, like tmux's default session names.
pub fn next_free_name() -> Result<String> {
    let taken = list()?;
    Ok((0..).map(|n: u64| n.to_string()).find(|n| !taken.contains(n)).unwrap())
}

/// Resolves `-t`: an explicit name, or the only session if there is one.
pub fn resolve(target: Option<String>) -> Result<String> {
    if let Some(name) = target {
        return Ok(name);
    }
    match &list()?[..] {
        [] => bail!("no sessions"),
        [one] => Ok(one.clone()),
        many => bail!("several sessions ({}); pick one with -t", many.join(", ")),
    }
}
