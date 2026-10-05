//! The verbs that work on a box you already have.
//!
//! `cbox run`, which creates one, is in [`crate::boot`] — it is most of the
//! program and has nothing in common with these beyond the session directory.

use std::io::Write;
use std::process::{Command, ExitCode, Stdio};
use std::{fs, io};

use anyhow::{bail, Context, Result};

use crate::proc::{exit_code, wait_foreground};
use crate::session;
use crate::ssh;
use crate::sys::{alive, urandom};

pub fn attach(id: Option<String>, cmd: Vec<String>) -> Result<ExitCode> {
    let dir = session::pick_session(id.as_deref())?;
    let mut ssh = ssh::interactive(&dir, &cmd)?;
    Ok(exit_code(wait_foreground(
        ssh.spawn().context("starting ssh")?,
    )?))
}

pub fn view(id: Option<String>) -> Result<ExitCode> {
    let dir = session::pick_session(id.as_deref())?;
    let ctl = dir.join("vnc.ctl");

    // ssh -f backgrounds itself once the forward is up, so a port that was
    // already taken shows up as a non-zero exit and we just try another.
    let mut port = 0u16;
    let mut up = false;
    for _ in 0..5 {
        port = u16::from_ne_bytes(urandom::<2>()?) % 20000 + 40000;
        up = ssh::ssh_box(&dir)?
            .args(["-f", "-N", "-M", "-S"])
            .arg(&ctl)
            .args(["-o", "ExitOnForwardFailure=yes"])
            .arg("-L")
            .arg(format!("127.0.0.1:{port}:127.0.0.1:5900"))
            .arg("agent@cbox")
            .status()
            .is_ok_and(|s| s.success());
        if up {
            break;
        }
    }
    if !up {
        bail!("could not set up the VNC tunnel");
    }

    let viewer = Command::new("vncviewer")
        .arg(format!("127.0.0.1::{port}"))
        .spawn()
        .context("starting vncviewer")?;
    let status = wait_foreground(viewer);

    // Tear the tunnel down even if the viewer died badly.
    let _ = Command::new("ssh")
        .arg("-F")
        .arg("/dev/null")
        .arg("-S")
        .arg(&ctl)
        .args(["-O", "exit", "cbox"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    Ok(exit_code(status?))
}

pub fn ls() -> Result<ExitCode> {
    let mut out = io::stdout().lock();
    for dir in session::live_sessions(&session::state_root()?) {
        let id = dir.file_name().expect("session dir has a name");
        let project = fs::read(dir.join("project")).unwrap_or_default();
        let project = String::from_utf8_lossy(&project);
        writeln!(out, "{}\t{}", id.to_string_lossy(), project.trim_end())?;
    }
    Ok(ExitCode::SUCCESS)
}

pub fn stop(id: String) -> Result<ExitCode> {
    let dir = session::pick_session(Some(&id))?;
    let status = Command::new(session::runner()?.join("bin/microvm-shutdown"))
        .current_dir(&dir)
        .status()
        .context("running microvm-shutdown")?;
    Ok(exit_code(status))
}

/// Sessions whose cbox died without cleaning up — a kill -9, a crash, a reboot
/// that left the cache behind. Everything here is also what a clean exit does.
pub fn gc() -> Result<ExitCode> {
    let state_root = session::state_root()?;
    let disk_root = session::disk_root()?;
    for dir in session::session_dirs(&state_root) {
        let pidfile = dir.join("qemu.pid");
        if !pidfile.is_file() {
            continue; // never got as far as starting QEMU
        }
        if session::read_pid(&pidfile).is_some_and(alive) {
            continue; // still running; an unreadable pid counts as dead
        }
        let id = dir.file_name().expect("session dir has a name").to_owned();
        eprintln!("cbox: removing {}", id.to_string_lossy());
        let _ = fs::remove_dir_all(disk_root.join(&id));
        let _ = fs::remove_dir_all(&dir);
    }
    Ok(ExitCode::SUCCESS)
}
