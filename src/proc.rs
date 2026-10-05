//! Child processes, and the signals that cut them short.
//!
//! cbox is mostly an orchestrator: three long-lived helpers per box plus a
//! stream of short ssh invocations. The whole job of this module is that none
//! of them outlive us. [`terminate`] is bounded so a wedged child can't hold a
//! session directory open forever, and [`watch_signals`] keeps a stray SIGTERM
//! from bypassing `Session`'s cleanup the way a default signal disposition
//! would.

use std::fs::File;
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::process::{Child, Command, ExitCode, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::thread::sleep;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

static INTERRUPTED: AtomicBool = AtomicBool::new(false);
static FOREGROUND: AtomicI32 = AtomicI32::new(0);

/// INT/TERM/HUP have to unwind rather than kill us outright, or the VM, its
/// disks and the session's keys outlive the process. The handler thread kills
/// whatever child is in the foreground so the main thread's `wait` returns and
/// `Session::drop` gets to run — the bash original's `trap 'exit 130'` reaching
/// its `trap cleanup EXIT`.
pub fn watch_signals() -> Result<()> {
    use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM};
    let mut signals = signal_hook::iterator::Signals::new([SIGINT, SIGTERM, SIGHUP])
        .context("installing signal handlers")?;
    std::thread::spawn(move || {
        for _ in signals.forever() {
            INTERRUPTED.store(true, Ordering::SeqCst);
            let pid = FOREGROUND.load(Ordering::SeqCst);
            if pid > 0 {
                unsafe { libc::kill(pid, SIGTERM) };
            }
        }
    });
    Ok(())
}

/// Whether a signal has arrived. Long waits poll this so they give up promptly
/// instead of running out their own deadline first.
pub fn interrupted() -> bool {
    INTERRUPTED.load(Ordering::SeqCst)
}

/// Wait for a child that owns the terminal, letting [`watch_signals`] cut it
/// short. Returns the child's status even when it was the signal that ended it.
pub fn wait_foreground(mut child: Child) -> Result<ExitStatus> {
    FOREGROUND.store(child.id() as i32, Ordering::SeqCst);
    let status = child.wait();
    FOREGROUND.store(0, Ordering::SeqCst);
    status.context("waiting for child")
}

/// Spawn with stdout and stderr in one log file and stdin closed. These logs
/// are the only record of what a box's filesystem and network looked like, and
/// the first thing to read when a boot goes wrong.
pub fn spawn_logged(cmd: &mut Command, log: &Path) -> Result<Child> {
    let out = File::create(log).with_context(|| format!("creating {}", log.display()))?;
    let err = out.try_clone().context("duplicating log handle")?;
    let name = cmd.get_program().to_string_lossy().into_owned();
    cmd.stdin(Stdio::null())
        .stdout(out)
        .stderr(err)
        .spawn()
        .with_context(|| format!("starting {name}"))
}

/// Wait up to `limit`, then kill. The bash original's `timeout 20 ...`.
pub fn wait_bounded(child: &mut Child, limit: Duration) {
    let deadline = Instant::now() + limit;
    while !matches!(child.try_wait(), Ok(Some(_)) | Err(_)) {
        if Instant::now() >= deadline {
            let _ = child.kill();
            break;
        }
        sleep(Duration::from_millis(50));
    }
    let _ = child.wait();
}

/// SIGTERM, wait a little, then SIGKILL. Taking the child by value is the point:
/// its pid stays reserved until we reap it, so the signal can't land on whatever
/// process the kernel hands the number to next.
pub fn terminate(mut child: Child, grace: Duration) {
    unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };
    wait_bounded(&mut child, grace);
}

/// A child's status as our own exit status, shells' 128+N convention included.
pub fn exit_code(status: ExitStatus) -> ExitCode {
    let code = status
        .code()
        .unwrap_or_else(|| 128 + status.signal().unwrap_or(0));
    ExitCode::from(code as u8)
}
