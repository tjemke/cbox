//! A session is one directory per box under `$XDG_RUNTIME_DIR/cbox`, plus a
//! matching disk directory under the cache that `disk` symlinks to.
//!
//! The layout is a contract with guest.nix: QEMU is started with the session
//! directory as its working directory and resolves `cid`, `cred/*`,
//! `disk/*.img` and the two sockets relative to it. Nothing may move without
//! moving there too.
//!
//! [`Session`] owns both directories. Once it exists, every exit path — an
//! error, a signal, or a clean logout — shuts the box down and removes them.

use std::ffi::OsStr;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;
use std::{env, fs};

use anyhow::{anyhow, bail, Context, Result};

use crate::proc::{terminate, wait_bounded};
use crate::sys::{alive, urandom};

// ------------------------------------------------------------------- the roots

pub fn state_root() -> Result<PathBuf> {
    let base = env::var_os("XDG_RUNTIME_DIR")
        .filter(|v| !v.is_empty())
        .context("XDG_RUNTIME_DIR is not set")?;
    Ok(PathBuf::from(base).join("cbox"))
}

/// Disks live in the cache, not the runtime dir: they are sparse but nominally
/// ~96 GiB, which is not what a tmpfs is for.
pub fn disk_root() -> Result<PathBuf> {
    if let Some(base) = env::var_os("XDG_CACHE_HOME").filter(|v| !v.is_empty()) {
        return Ok(PathBuf::from(base).join("cbox"));
    }
    Ok(home()?.join(".cache").join("cbox"))
}

pub fn home() -> Result<PathBuf> {
    let h = env::var_os("HOME")
        .filter(|v| !v.is_empty())
        .context("HOME is not set")?;
    Ok(PathBuf::from(h))
}

/// The microvm.nix runner, baked in by the Nix wrapper.
pub fn runner() -> Result<PathBuf> {
    let r = env::var_os("CBOX_RUNNER")
        .filter(|v| !v.is_empty())
        .context("CBOX_RUNNER is not set; cbox has to run from its Nix wrapper")?;
    Ok(PathBuf::from(r))
}

// ------------------------------------------------------------------ identities

/// A readable stem from the project directory plus enough randomness that two
/// boxes on the same project don't collide.
pub fn id_for(dir: &Path) -> Result<String> {
    let base: String = dir
        .file_name()
        .unwrap_or(OsStr::new("box"))
        .as_bytes()
        .iter()
        .take(24)
        .map(|&b| {
            if b.is_ascii_alphanumeric() || b"._-".contains(&b) {
                b as char
            } else {
                '_'
            }
        })
        .collect();
    let rnd: String = urandom::<3>()?.iter().map(|b| format!("{b:02x}")).collect();
    Ok(format!("{base}-{rnd}"))
}

/// Everything in the box can write the shared directory, and $HOME is full of
/// things the host executes, so neither $HOME nor any parent of it is sharable.
/// Both paths must already be canonical; comparing by component is what keeps
/// `/home/xy` from looking like a prefix of `/home/x`.
pub fn refuses_to_share(dir: &Path, home: &Path) -> bool {
    dir == Path::new("/") || home.starts_with(dir)
}

pub fn cid(dir: &Path) -> Result<String> {
    let path = dir.join("cid");
    let s = fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    Ok(s.trim().to_string())
}

pub fn read_pid(path: &Path) -> Option<i32> {
    fs::read_to_string(path).ok()?.trim().parse().ok()
}

// --------------------------------------------------------------------- lifetime

/// A live session and the processes behind it.
///
/// The three children are adopted as they start, so a failure partway through
/// a boot tears down exactly what had come up by then.
pub struct Session {
    pub run: PathBuf,
    pub fsd: Option<Child>,
    pub net: Option<Child>,
    pub qemu: Option<Child>,
    disk: PathBuf,
    runner: PathBuf,
    keep: bool,
}

impl Session {
    /// Lay out a new session directory and take ownership of it.
    pub fn create(id: &str, dir: &Path, runner: PathBuf, keep: bool) -> Result<Session> {
        let run = state_root()?.join(id);
        let disk = disk_root()?.join(id);
        fs::create_dir_all(run.parent().expect("has parent"))?;
        fs::create_dir_all(disk.parent().expect("has parent"))?;

        // create_dir, not create_dir_all: a collision must not adopt a live
        // session and then delete it on the way out.
        fs::create_dir(&run).with_context(|| format!("creating {}", run.display()))?;

        // Constructed here, before anything else can fail, so the rest of the
        // layout is covered by the same cleanup the caller gets.
        let session = Session {
            run,
            fsd: None,
            net: None,
            qemu: None,
            disk,
            runner,
            keep,
        };
        session.lay_out(dir)?;
        Ok(session)
    }

    fn lay_out(&self, dir: &Path) -> Result<()> {
        fs::create_dir(&self.disk).with_context(|| format!("creating {}", self.disk.display()))?;
        fs::create_dir(self.run.join("cred"))?;
        std::os::unix::fs::symlink(&self.disk, self.run.join("disk"))?;

        let mut project = dir.to_path_buf().into_os_string().into_vec();
        project.push(b'\n');
        fs::write(self.run.join("project"), project)?;

        let cid = u32::from_ne_bytes(urandom::<4>()?) % 4_000_000_000 + 1000;
        fs::write(self.run.join("cid"), format!("{cid}\n"))?;
        Ok(())
    }

    /// Record QEMU's pid so `ls`, `stop` and `gc` can find this box from
    /// another process.
    pub fn record_qemu_pid(&self) -> Result<()> {
        let pid = self.qemu.as_ref().expect("qemu was spawned").id();
        fs::write(self.run.join("qemu.pid"), format!("{pid}\n"))?;
        Ok(())
    }
}

/// Show the tail of the session's logs, then the error. A failure during
/// startup is almost always explained in one of them.
///
/// Free rather than a method on [`Session`]: its callers are already holding a
/// mutable borrow of the child they are waiting on.
pub fn fail(run: &Path, msg: &str, logs: &[&str]) -> anyhow::Error {
    for log in logs {
        let Ok(bytes) = fs::read(run.join(log)) else {
            continue;
        };
        if bytes.is_empty() {
            continue;
        }
        let text = String::from_utf8_lossy(&bytes);
        let lines: Vec<&str> = text.lines().collect();
        eprintln!("\n--- {log} (last 30 lines) ---");
        for line in &lines[lines.len().saturating_sub(30)..] {
            eprintln!("{line}");
        }
    }
    anyhow!("{msg}")
}

/// The bash original's `trap cleanup EXIT`.
impl Drop for Session {
    fn drop(&mut self) {
        if let Some(mut qemu) = self.qemu.take() {
            if matches!(qemu.try_wait(), Ok(None)) {
                // Ask the guest to shut down; killing QEMU is the backstop.
                if let Ok(mut shutdown) = Command::new(self.runner.join("bin/microvm-shutdown"))
                    .current_dir(&self.run)
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn()
                {
                    wait_bounded(&mut shutdown, Duration::from_secs(20));
                }
                terminate(qemu, Duration::from_secs(5));
            } else {
                let _ = qemu.wait();
            }
        }
        // After QEMU: these two are what it was talking to.
        for child in [self.net.take(), self.fsd.take()].into_iter().flatten() {
            terminate(child, Duration::from_secs(5));
        }
        if self.keep {
            eprintln!(
                "cbox: kept {} and {}",
                self.run.display(),
                self.disk.display()
            );
        } else {
            let _ = fs::remove_dir_all(&self.run);
            let _ = fs::remove_dir_all(&self.disk);
        }
    }
}

// -------------------------------------------------------------------- discovery

/// Every session directory, live or not, in a stable order.
pub fn session_dirs(state_root: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(state_root) else {
        return Vec::new();
    };
    let mut dirs: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    dirs.sort();
    dirs
}

/// Those whose QEMU is still running. An unreadable pid counts as not running.
pub fn live_sessions(state_root: &Path) -> Vec<PathBuf> {
    session_dirs(state_root)
        .into_iter()
        .filter(|d| read_pid(&d.join("qemu.pid")).is_some_and(alive))
        .collect()
}

/// The one live box the user meant. With no id that means "the only one", so
/// an ambiguous request is an error rather than a guess.
pub fn pick_session(want: Option<&str>) -> Result<PathBuf> {
    let mut found = live_sessions(&state_root()?);
    if let Some(want) = want {
        found.retain(|d| d.file_name().and_then(OsStr::to_str) == Some(want));
    }
    match found.len() {
        1 => Ok(found.pop().expect("len == 1")),
        0 => match want {
            Some(want) => bail!("no running box named {want}"),
            None => bail!("no running box"),
        },
        _ => bail!("several boxes are running; pass an ID (see: cbox ls)"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn home_and_its_parents_are_refused() {
        let home = Path::new("/home/x");
        for refused in ["/", "/home", "/home/x"] {
            assert!(refuses_to_share(Path::new(refused), home), "{refused}");
        }
        for allowed in ["/home/x/src/p", "/srv/p", "/home/xy"] {
            assert!(!refuses_to_share(Path::new(allowed), home), "{allowed}");
        }
    }

    #[test]
    fn session_ids_are_filename_safe() {
        let id = id_for(Path::new("/src/my project!")).unwrap();
        assert!(id.starts_with("my_project_-"), "{id}");

        let long = id_for(Path::new(&format!("/src/{}", "x".repeat(40)))).unwrap();
        assert_eq!(long.len(), 24 + 1 + 6, "{long}");

        // A path separator would escape the state directory entirely.
        let slashy = id_for(Path::new("/src/a/../b")).unwrap();
        assert!(!slashy.contains('/'), "{slashy}");
    }
}
