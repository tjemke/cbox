//! The libc corners std doesn't expose.
//!
//! All of it is small enough that pulling in `nix` or `rustix` would cost more
//! than it saves, and every call here is one the bash original made too.

use std::ffi::{CString, OsStr, OsString};
use std::fs::File;
use std::io::Read;

use anyhow::{Context, Result};

/// Session directories hold the private keys and the API token; nothing in
/// them is ever meant to be group- or world-readable.
pub fn set_private_umask() {
    unsafe { libc::umask(0o077) };
}

/// N random bytes. cbox needs very little randomness and needs it rarely, so
/// reading the device beats taking on a crate for it.
pub fn urandom<const N: usize>() -> Result<[u8; N]> {
    let mut buf = [0u8; N];
    File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut buf))
        .context("reading /dev/urandom")?;
    Ok(buf)
}

/// `kill -0`: does this pid name a process we could signal?
pub fn alive(pid: i32) -> bool {
    pid > 0 && unsafe { libc::kill(pid, 0) == 0 }
}

/// `access(2)`. std has no equivalent, and the obvious substitute — opening the
/// device to see whether it opens — is not a thing to do to /dev/kvm in passing.
pub fn accessible(path: &str, mode: libc::c_int) -> bool {
    let c = CString::new(path).expect("literal path has no NUL");
    unsafe { libc::access(c.as_ptr(), mode) == 0 }
}

/// Who the guest's virtiofsd should translate host-owned files to.
pub fn uid_gid() -> (u32, u32) {
    unsafe { (libc::getuid(), libc::getgid()) }
}

/// Only ever used in an error message, so a missing $USER degrading to the
/// numeric uid is fine.
pub fn whoami() -> String {
    std::env::var("USER")
        .or_else(|_| std::env::var("LOGNAME"))
        .unwrap_or_else(|_| format!("uid {}", unsafe { libc::getuid() }))
}

/// `--flag=VALUE` where VALUE is a path, which on Unix need not be UTF-8.
/// Going through `OsString` keeps such a path exact instead of lossily
/// stringifying it on the way to a child process.
pub fn opt(prefix: &str, value: impl AsRef<OsStr>) -> OsString {
    let mut o = OsString::from(prefix);
    o.push(value);
    o
}

/// `poll(2)` on one fd for input, for at most `timeout_ms`. A signal landing
/// mid-poll counts as "not yet", so the caller's loop gets to check for it.
pub fn readable(fd: libc::c_int, timeout_ms: libc::c_int) -> Result<bool> {
    let mut p = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    match unsafe { libc::poll(&mut p, 1, timeout_ms) } {
        n if n > 0 => Ok(true),
        0 => Ok(false),
        _ => {
            let e = std::io::Error::last_os_error();
            if e.kind() == std::io::ErrorKind::Interrupted {
                Ok(false)
            } else {
                Err(e).context("poll")
            }
        }
    }
}
