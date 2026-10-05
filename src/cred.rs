//! Per-session credentials and settings.
//!
//! Every file in `cred/` becomes one `-fw_cfg name=opt/cbox/<basename>` entry
//! on QEMU's command line (see guest.nix's `extraArgsScript`), so this
//! directory's contents are exactly what the guest gets. They are passed as
//! *files* rather than strings so nothing secret lands in argv, where any other
//! user on the host could read it out of /proc.
//!
//! QEMU reads them all at startup, after which [`discard`] takes them away
//! again — the running box keeps its copies under /run/cbox.

use std::os::unix::ffi::OsStringExt;
use std::path::Path;
use std::process::Command;
use std::{env, fs};

use anyhow::{bail, Context, Result};

use crate::ssh::quote;
use crate::GitMode;

/// Write the session's keys and settings. The client key logs in; the host key
/// is pinned in `known_hosts`, so neither end trusts anything but this session.
pub fn write(run: &Path, git: GitMode, lan: bool, pass_env: &[String]) -> Result<()> {
    let cred = run.join("cred");
    keygen(&run.join("client"), "cbox")?;
    keygen(&cred.join("hostkey"), "cbox-host")?;

    // ssh-keygen always writes the public half too; it must not stay in cred/
    // or it would become a pointless extra fw_cfg entry.
    let hostkey_pub = fs::read_to_string(cred.join("hostkey.pub"))?;
    fs::write(
        run.join("known_hosts"),
        format!("cbox {}\n", hostkey_pub.trim_end()),
    )?;
    fs::remove_file(cred.join("hostkey.pub"))?;

    fs::copy(run.join("client.pub"), cred.join("authorized_keys"))?;
    fs::write(cred.join("git"), format!("{git}\n"))?;
    fs::write(cred.join("lan"), if lan { "1\n" } else { "0\n" })?;

    let set: Vec<(&str, Vec<u8>)> = pass_env
        .iter()
        .filter_map(|name| {
            let value = env::var_os(name).filter(|v| !v.is_empty())?;
            Some((name.as_str(), value.into_vec()))
        })
        .collect();
    fs::write(cred.join("env"), env_file(&set))?;
    Ok(())
}

/// A shell fragment the guest sources at login. Values are quoted rather than
/// validated: a token is opaque bytes and may legitimately contain anything.
fn env_file(vars: &[(&str, Vec<u8>)]) -> Vec<u8> {
    let mut out = b"# cbox session environment\n".to_vec();
    for (name, value) in vars {
        out.extend_from_slice(format!("export {name}=").as_bytes());
        out.extend_from_slice(&quote(value));
        out.push(b'\n');
    }
    out
}

/// Drop the fw_cfg files once QEMU has read them, so a long-lived session isn't
/// sitting on a second copy of its own host key.
pub fn discard(run: &Path) {
    let _ = fs::remove_dir_all(run.join("cred"));
}

fn keygen(path: &Path, comment: &str) -> Result<()> {
    let status = Command::new("ssh-keygen")
        .args(["-q", "-t", "ed25519", "-N", "", "-C", comment, "-f"])
        .arg(path)
        .status()
        .context("running ssh-keygen")?;
    if !status.success() {
        bail!("ssh-keygen failed for {}", path.display());
    }
    Ok(())
}

/// `--env` takes a name to copy from our own environment, never `NAME=VALUE`.
/// Checking the shape up front means [`env_file`] can only ever emit one
/// assignment per line.
pub fn is_var_name(s: &str) -> bool {
    let mut bytes = s.bytes();
    bytes
        .next()
        .is_some_and(|b| b.is_ascii_alphabetic() || b == b'_')
        && bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn variable_names_are_checked() {
        for ok in ["PATH", "_x", "A1", "ANTHROPIC_API_KEY"] {
            assert!(is_var_name(ok), "{ok}");
        }
        for bad in ["", "1A", "a-b", "a b", "FOO=bar", "a;id", "a\nb"] {
            assert!(!is_var_name(bad), "{bad}");
        }
    }

    #[test]
    fn env_file_quotes_values() {
        let vars = [("ANTHROPIC_API_KEY", b"sk-tok'en with spaces".to_vec())];
        assert_eq!(
            String::from_utf8(env_file(&vars)).unwrap(),
            "# cbox session environment\n\
             export ANTHROPIC_API_KEY='sk-tok'\\''en with spaces'\n"
        );
        assert_eq!(
            String::from_utf8(env_file(&[])).unwrap(),
            "# cbox session environment\n"
        );
    }
}
