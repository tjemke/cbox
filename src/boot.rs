//! `cbox run`: bringing a new box up.
//!
//! The order is fixed by what QEMU needs at startup. virtiofsd and passt have
//! to be listening before the runner starts, because QEMU connects to both
//! sockets immediately; `cred/` has to be written before that, because QEMU
//! reads those files into fw_cfg as it boots.

use std::io::{self, BufRead, IsTerminal, Write};
use std::os::unix::fs::FileTypeExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitCode, Stdio};
use std::thread::sleep;
use std::time::{Duration, Instant};
use std::{env, fs};

use anyhow::{bail, Context, Result};
use jiff::civil::Date;
use jiff::Zoned;

use crate::proc::{exit_code, interrupted, spawn_logged, wait_foreground};
use crate::session::{self, Session};
use crate::ssh;
use crate::sys::{accessible, opt, readable, uid_gid, whoami};
use crate::{cred, GitMode, RunArgs};

pub fn run(a: RunArgs) -> Result<ExitCode> {
    let runner = session::runner()?;
    warn_if_stale();

    let mut pass_env = vec![
        "CLAUDE_CODE_OAUTH_TOKEN".to_string(),
        "ANTHROPIC_API_KEY".to_string(),
    ];
    pass_env.extend(a.env);
    for name in &pass_env {
        if !cred::is_var_name(name) {
            bail!("not a variable name: {name}");
        }
    }

    // Checked before anything is created, so an unusable host fails cheaply.
    if !accessible("/dev/kvm", libc::R_OK | libc::W_OK) {
        bail!("/dev/kvm is not usable by {}", whoami());
    }
    if !accessible("/dev/vhost-vsock", libc::W_OK) {
        bail!("/dev/vhost-vsock is not usable by {}", whoami());
    }

    let dir = share_target(a.dir)?;
    warn_about_git(&dir, a.git);
    if a.git == GitMode::Rw && !a.yes {
        confirm_git_rw()?;
    }

    let id = session::id_for(&dir)?;
    let mut sess = Session::create(&id, &dir, runner.clone(), a.keep)?;
    cred::write(&sess.run, a.git, a.lan, &pass_env)?;

    start_helpers(&mut sess, &dir)?;
    sess.qemu = Some(spawn_logged(
        Command::new(runner.join("bin/microvm-run")).current_dir(&sess.run),
        &sess.run.join("console.log"),
    )?);
    sess.record_qemu_pid()?;

    eprintln!("cbox: booting {id}");
    wait_until_ready(&sess.run, sess.qemu.as_mut().expect("just spawned"))?;
    cred::discard(&sess.run); // QEMU read the fw_cfg files at startup

    let mut ssh = ssh::interactive(&sess.run, &a.cmd)?;
    let status = wait_foreground(ssh.spawn().context("starting ssh")?)?;
    Ok(exit_code(status))
    // Returning drops `sess`, which shuts the box down and removes it.
}

/// After about a month a nixpkgs has usually missed a Nix daemon or kernel
/// security fix, and those are what stand between the agent and guest root.
const STALE_AFTER_DAYS: i32 = 30;

/// The Nix wrapper records the day the guest's nixpkgs was cut, when it knows.
/// Only a warning: an old box still works, it is just easier to break out of.
fn warn_if_stale() {
    let Ok(stamp) = env::var("CBOX_GUEST_NIXPKGS_DATE") else {
        return;
    };
    let Some((cut, age)) = age_in_days(&stamp, Zoned::now().date()) else {
        return;
    };
    if age > STALE_AFTER_DAYS {
        eprintln!("cbox: this box's nixpkgs is from {cut}, {age} days ago; its Nix daemon,");
        eprintln!("      kernel and claude-code lack every fix since. Update the nixpkgs input");
        eprintln!("      it is built from and rebuild (see programs.cbox.guestNixpkgs).");
    }
}

/// The day a YYYYMMDD stamp names, and how many days before `today` it was.
fn age_in_days(stamp: &str, today: Date) -> Option<(Date, i32)> {
    let cut = Date::strptime("%Y%m%d", stamp).ok()?;
    Some((cut, cut.until(today).ok()?.get_days()))
}

/// The host directory to share, resolved and vetted.
fn share_target(requested: Option<PathBuf>) -> Result<PathBuf> {
    let requested = match requested {
        Some(d) => d,
        None => env::current_dir().context("no current directory")?,
    };
    let dir = fs::canonicalize(&requested)
        .with_context(|| format!("no such directory: {}", requested.display()))?;
    if !dir.is_dir() {
        bail!("not a directory: {}", dir.display());
    }
    let home = fs::canonicalize(session::home()?).context("$HOME does not resolve")?;
    if session::refuses_to_share(&dir, &home) {
        bail!(
            "refusing to share {}: it is $HOME or contains it",
            dir.display()
        );
    }
    Ok(dir)
}

/// Both of these are about what the *host* will execute out of the tree later.
/// Neither stops the session on its own: --git rw is the user's call, which
/// [`confirm_git_rw`] makes them take explicitly.
fn warn_about_git(dir: &Path, git: GitMode) {
    if git == GitMode::Rw {
        eprintln!(
            "cbox: --git rw lets the agent write {}/.git, and host git runs",
            dir.display()
        );
        eprintln!("      what it finds there: hooks, core.fsmonitor on every \"git status\",");
        eprintln!("      config pulled in through commondir. Review .git before you run git");
        eprintln!("      in this directory afterwards.");
    } else if dir.join(".gitmodules").is_file() {
        eprintln!(
            "cbox: {} has submodules; their working trees are agent-writable, and host git",
            dir.display()
        );
        eprintln!(
            "      reads their .git on \"git status\". \
             Afterwards use: git -c diff.ignoreSubmodules=all status"
        );
    }
}

/// Ask on the terminal; without one there is nobody to ask, so refuse rather
/// than assume. The prompt is the one place cbox reads stdin, so it polls
/// instead of blocking: a signal there has no foreground child to cut short.
fn confirm_git_rw() -> Result<()> {
    let stdin = io::stdin();
    if !stdin.is_terminal() {
        bail!("--git rw needs confirmation and stdin is not a terminal; pass --yes");
    }
    eprint!("cbox: start with a writable .git? [y/N] ");
    io::stderr().flush()?;
    while !readable(libc::STDIN_FILENO, 100)? {
        if interrupted() {
            eprintln!();
            bail!("interrupted");
        }
    }
    let mut answer = String::new();
    stdin.lock().read_line(&mut answer)?;
    if !is_yes(&answer) {
        bail!("not starting: --git rw was not confirmed");
    }
    Ok(())
}

fn is_yes(answer: &str) -> bool {
    matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

/// virtiofsd and passt, and the wait for each to be listening.
fn start_helpers(sess: &mut Session, dir: &Path) -> Result<()> {
    // Guest writes land as us; our files appear as agent (1000:100) inside.
    // Default --sandbox=namespace: virtiofsd pivot_roots into $dir, which is
    // what stops symlink escapes (--sandbox=none would not).
    let (uid, gid) = uid_gid();
    sess.fsd = Some(spawn_logged(
        Command::new("virtiofsd")
            .arg(opt("--socket-path=", sess.run.join("work.sock")))
            .arg(opt("--shared-dir=", dir))
            .args(["--sandbox=namespace", "--cache=auto"])
            .arg(format!("--translate-uid=squash-guest:0:{uid}:4294967295"))
            .arg(format!("--translate-uid=host:{uid}:1000:1"))
            .arg(format!("--translate-gid=squash-guest:0:{gid}:4294967295"))
            .arg(format!("--translate-gid=host:{gid}:100:1")),
        &sess.run.join("virtiofsd.log"),
    )?);

    // No inbound forwards (passt's default), no host-loopback mapping, IPv4 only.
    // --dns-forward makes 169.254.1.1 (the guest's only nameserver) the address
    // passt matches DNS on; the resolver it forwards *to* is --dns-host, which
    // defaults to the first nameserver in the host's /etc/resolv.conf. Do not add
    // `--dns ADDR` here: it sets ip4.dns[0], which makes passt's get_dns() skip
    // every `nameserver` line, leaving --dns-host unset (0.0.0.0) and silently
    // NATing all guest DNS to the host's own 127.0.0.1:53. See NOTES.md.
    // --search none keeps the host's internal search domains out of the box.
    // Not --quiet: passt's startup config goes to passt.log, which is cheap and
    // the only record of what the box's network actually looked like.
    sess.net = Some(spawn_logged(
        Command::new("passt")
            .args(["--foreground", "--one-off", "--ipv4-only", "--socket"])
            .arg(sess.run.join("passt.sock"))
            .args([
                "--no-map-gw",
                "--dns-forward",
                "169.254.1.1",
                "--search",
                "none",
            ]),
        &sess.run.join("passt.log"),
    )?);

    wait_for_socket(
        &sess.run,
        "work.sock",
        sess.fsd.as_mut().expect("just spawned"),
        "virtiofsd",
    )?;
    wait_for_socket(
        &sess.run,
        "passt.sock",
        sess.net.as_mut().expect("just spawned"),
        "passt",
    )
}

/// Poll for a helper's socket, giving up early if the helper itself dies.
fn wait_for_socket(run: &Path, sock: &str, child: &mut Child, name: &str) -> Result<()> {
    let log = format!("{name}.log");
    let path = run.join(sock);
    for _ in 0..200 {
        if fs::metadata(&path).is_ok_and(|m| m.file_type().is_socket()) {
            return Ok(());
        }
        if !matches!(child.try_wait(), Ok(None)) {
            return Err(session::fail(
                run,
                &format!("{name} exited during startup"),
                &[&log],
            ));
        }
        if interrupted() {
            bail!("interrupted");
        }
        sleep(Duration::from_millis(50));
    }
    Err(session::fail(
        run,
        &format!("{name} did not create its socket"),
        &[&log],
    ))
}

/// Poll until the guest's `cbox-setup` has touched /run/cbox/ready. That unit
/// fails closed, so waiting for the file is also how we learn the session's
/// settings were applied — not merely that something booted.
fn wait_until_ready(run: &Path, qemu: &mut Child) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(180);
    loop {
        let ready = ssh::ssh_box(run)?
            .args(["agent@cbox", "--", "test", "-e", "/run/cbox/ready"])
            .stdin(Stdio::null()) // else it eats keystrokes meant for the shell to come
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
        if ready {
            return Ok(());
        }
        if interrupted() {
            bail!("interrupted");
        }
        if !matches!(qemu.try_wait(), Ok(None)) {
            return Err(session::fail(
                run,
                "the VM exited during boot",
                &["console.log", "virtiofsd.log"],
            ));
        }
        if Instant::now() >= deadline {
            return Err(session::fail(
                run,
                "the VM was not ready after 180s",
                &["console.log"],
            ));
        }
        sleep(Duration::from_secs(1));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn age_is_counted_in_calendar_days() {
        let today = jiff::civil::date(2026, 10, 10);
        let age = |stamp| age_in_days(stamp, today).map(|(_, days)| days);
        assert_eq!(age("20261010"), Some(0));
        assert_eq!(age("20260910"), Some(30));
        assert_eq!(age("20260801"), Some(70));
        assert_eq!(age("20240229"), Some(954));
        assert_eq!(age("20261011"), Some(-1)); // a host clock that's behind
        for bad in [
            "",
            "garbage",
            "2026-10-03",
            "20261303",
            "20260230",
            "2026100x",
        ] {
            assert_eq!(age(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn only_an_explicit_yes_confirms() {
        for yes in ["y\n", "Y\n", "yes\n", " YES \n"] {
            assert!(is_yes(yes), "{yes:?}");
        }
        // EOF (Ctrl-D) reads as "", and Enter alone takes the default.
        for no in ["", "\n", "n\n", "no\n", "yess\n", "y es\n"] {
            assert!(!is_yes(no), "{no:?}");
        }
    }
}
