//! Getting into a box, and handing it text to run.
//!
//! Everything here crosses into a shell we don't control the parsing of, so
//! [`quote`] is load-bearing: it is what keeps a project path or an API token
//! containing a quote from being read as anything but a literal. Both users of
//! it are far-end shells — the command ssh carries, and the `cred/env` file the
//! guest sources at login.

use std::path::Path;
use std::process::Command;

use anyhow::Result;

use crate::session;
use crate::sys::opt;

/// The guest's entry point: applies the session environment, cds to /work, and
/// execs either CMD or a login shell. See guest.nix.
const GUEST_ENTER: &str = "/run/current-system/sw/bin/cbox-enter";

/// POSIX single-quoting, for strings a shell on the far end will re-parse.
/// Single quotes make every byte literal, so the only case to handle is the
/// quote itself: close, emit an escaped one, reopen.
pub fn quote(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len() + 2);
    out.push(b'\'');
    for &b in bytes {
        if b == b'\'' {
            out.extend_from_slice(br"'\''");
        } else {
            out.push(b);
        }
    }
    out.push(b'\'');
    out
}

/// One shell-quoted string for the guest's login shell to parse. ssh joins its
/// trailing arguments with spaces and hands the result to a shell regardless,
/// so the quoting has to survive that round trip rather than fight it.
fn remote_cmd(cmd: &[String]) -> String {
    let mut out = quote(GUEST_ENTER.as_bytes());
    for arg in cmd {
        out.push(b' ');
        out.extend_from_slice(&quote(arg.as_bytes()));
    }
    String::from_utf8(out).expect("quoting preserves UTF-8")
}

/// The one way into a box: ssh over vsock, pinned host key, session key only,
/// and nothing from the user's ssh config, known_hosts or agent.
pub fn ssh_box(dir: &Path) -> Result<Command> {
    let cid = session::cid(dir)?;
    let mut c = Command::new("ssh");
    c.arg("-F")
        .arg("/dev/null")
        .arg("-i")
        .arg(dir.join("client"))
        .args(["-o", "IdentitiesOnly=yes", "-o", "IdentityAgent=none"])
        .arg("-o")
        .arg(opt("UserKnownHostsFile=", dir.join("known_hosts")))
        .args(["-o", "GlobalKnownHostsFile=/dev/null"])
        .args(["-o", "HostKeyAlias=cbox", "-o", "StrictHostKeyChecking=yes"])
        .arg("-o")
        .arg(format!("ProxyCommand=socat - VSOCK-CONNECT:{cid}:22"))
        .args([
            "-o",
            "BatchMode=yes",
            "-o",
            "ConnectTimeout=5",
            "-o",
            "ServerAliveInterval=15",
            "-o",
            "LogLevel=ERROR",
        ]);
    Ok(c)
}

/// An interactive session running CMD, or a login shell when CMD is empty.
///
/// The bare `--` is consumed by ssh itself: once it has taken the destination
/// it restarts option parsing, so `--` ends the options rather than becoming
/// the first word of the remote command.
pub fn interactive(dir: &Path, cmd: &[String]) -> Result<Command> {
    let mut ssh = ssh_box(dir)?;
    ssh.arg("-t")
        .arg("agent@cbox")
        .arg("--")
        .arg(remote_cmd(cmd));
    Ok(ssh)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn quoted(s: &str) -> String {
        String::from_utf8(quote(s.as_bytes())).unwrap()
    }

    #[test]
    fn quoting_survives_a_shell() {
        assert_eq!(quoted("claude"), "'claude'");
        assert_eq!(quoted("a b"), "'a b'");
        assert_eq!(quoted("it's"), r"'it'\''s'");
        assert_eq!(quoted("$(rm -rf /)"), "'$(rm -rf /)'");
        assert_eq!(quoted(""), "''");

        for case in ["claude", "a b", "it's", "$(id)", "x\ny", "`id`", "\\"] {
            let out = Command::new("sh")
                .arg("-c")
                .arg(format!("printf %s {}", quoted(case)))
                .output()
                .unwrap();
            assert_eq!(String::from_utf8_lossy(&out.stdout), case);
        }
    }

    #[test]
    fn remote_cmd_is_one_parsable_word_list() {
        assert_eq!(remote_cmd(&[]), format!("'{GUEST_ENTER}'"));
        assert_eq!(
            remote_cmd(&["claude".into(), "--model".into(), "opus".into()]),
            format!("'{GUEST_ENTER}' 'claude' '--model' 'opus'")
        );
    }
}
