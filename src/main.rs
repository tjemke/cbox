//! cbox: a fresh NixOS microVM per session, with one host directory at /work.
//!
//! The box is three processes we own — virtiofsd for the shared directory,
//! passt for the network, QEMU for the VM — plus ssh over vsock to get in.
//! Everything per-session lives in $XDG_RUNTIME_DIR/cbox/<id>, which QEMU
//! reads relative paths out of, so the runner is always started with that as
//! its working directory.
//!
//! CBOX_RUNNER (the microvm.nix runner) is set by the Nix wrapper.

use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::FileTypeExt;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitCode, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::thread::sleep;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use clap::{Args, Parser, Subcommand, ValueEnum};

const GUEST_ENTER: &str = "/run/current-system/sw/bin/cbox-enter";
const SUBCOMMANDS: &[&str] = &["run", "attach", "view", "ls", "stop", "gc", "help"];

// ---------------------------------------------------------------- command line

#[derive(Parser)]
#[command(
    name = "cbox",
    version,
    about = "A fresh NixOS microVM per session, with one host directory at /work",
    after_help = "`run` is the default: `cbox [options] [DIR] [-- CMD...]`.\n\
                  A project directory named like a subcommand needs `cbox run DIR`."
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Boot a fresh box with DIR (default: $PWD) at /work
    ///
    /// CMD defaults to a login shell. Exiting it destroys the box.
    Run(RunArgs),

    /// Another shell in a running box
    Attach {
        /// Session id; optional when only one box is running (see `cbox ls`)
        id: Option<String>,
        /// Command to run in the box (default: a login shell)
        #[arg(last = true, value_name = "CMD")]
        cmd: Vec<String>,
    },

    /// Watch/drive the box's display (:99) in a VNC viewer
    View {
        /// Session id; optional when only one box is running (see `cbox ls`)
        id: Option<String>,
    },

    /// List running boxes
    Ls,

    /// Shut a box down
    Stop {
        /// Session id (see `cbox ls`)
        id: String,
    },

    /// Remove leftovers of boxes that died uncleanly
    Gc,
}

#[derive(Args)]
struct RunArgs {
    /// Whether the agent can write /work/.git
    ///
    /// ro (default): /work/.git is read-only in the box, so no commits.
    /// rw: the agent can commit, and host git will then run whatever it wrote
    /// into .git (hooks, config, commondir). Trusted tasks only.
    #[arg(long, value_name = "MODE", default_value = "ro")]
    git: GitMode,

    /// Allow private/LAN destinations (default: blocked inside the guest)
    #[arg(long)]
    lan: bool,

    /// Pass host variable NAME into the box (repeatable)
    ///
    /// CLAUDE_CODE_OAUTH_TOKEN and ANTHROPIC_API_KEY are passed when set.
    #[arg(long, value_name = "NAME")]
    env: Vec<String>,

    /// Keep the session directory and disks after exit (debugging)
    #[arg(long)]
    keep: bool,

    /// Host directory to share at /work (default: $PWD)
    dir: Option<PathBuf>,

    /// Command to run in the box (default: a login shell)
    #[arg(last = true, value_name = "CMD")]
    cmd: Vec<String>,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
enum GitMode {
    Ro,
    Rw,
}

impl fmt::Display for GitMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            GitMode::Ro => "ro",
            GitMode::Rw => "rw",
        })
    }
}

/// `run` is the default subcommand, so `cbox ~/src/thing` works. Only the
/// first argument is inspected, which is what makes the "a project directory
/// named like a subcommand needs `cbox run DIR`" caveat true.
fn argv_with_default_subcommand() -> Vec<OsString> {
    let mut argv: Vec<OsString> = std::env::args_os().collect();
    let dispatches = argv.get(1).and_then(|a| a.to_str()).is_some_and(|a| {
        SUBCOMMANDS.contains(&a) || matches!(a, "-h" | "--help" | "-V" | "--version")
    });
    if !dispatches {
        argv.insert(1, "run".into());
    }
    argv
}

fn main() -> ExitCode {
    // Session directories hold the private keys and the token; nothing in them
    // is ever meant to be group- or world-readable.
    unsafe { libc::umask(0o077) };
    if let Err(e) = watch_signals() {
        eprintln!("cbox: {e:#}");
        return ExitCode::FAILURE;
    }
    let result = match Cli::parse_from(argv_with_default_subcommand()).cmd {
        Cmd::Run(a) => cmd_run(a),
        Cmd::Attach { id, cmd } => cmd_attach(id, cmd),
        Cmd::View { id } => cmd_view(id),
        Cmd::Ls => cmd_ls(),
        Cmd::Stop { id } => cmd_stop(id),
        Cmd::Gc => cmd_gc(),
    };
    // A signal preempts whatever else was going on, as the bash original's
    // `trap 'exit 130' INT TERM HUP` did. Cleanup has already run by here.
    if interrupted() {
        return ExitCode::from(130);
    }
    match result {
        Ok(code) => code,
        Err(e) => {
            eprintln!("cbox: {e:#}");
            ExitCode::FAILURE
        }
    }
}

// -------------------------------------------------------------------- signals

static INTERRUPTED: AtomicBool = AtomicBool::new(false);
static FOREGROUND: AtomicI32 = AtomicI32::new(0);

/// INT/TERM/HUP have to unwind rather than kill us outright, or the VM, its
/// disks and the session's keys outlive the process. The handler thread kills
/// whatever child is in the foreground so the main thread's `wait` returns and
/// `Session::drop` gets to run.
fn watch_signals() -> Result<()> {
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

fn interrupted() -> bool {
    INTERRUPTED.load(Ordering::SeqCst)
}

/// Wait for a child that owns the terminal, letting [`watch_signals`] cut it
/// short. Returns the child's status even when it was the signal that ended it.
fn wait_foreground(mut child: Child) -> Result<ExitStatus> {
    FOREGROUND.store(child.id() as i32, Ordering::SeqCst);
    let status = child.wait();
    FOREGROUND.store(0, Ordering::SeqCst);
    status.context("waiting for child")
}

// ------------------------------------------------------------------- plumbing

fn state_root() -> Result<PathBuf> {
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .filter(|v| !v.is_empty())
        .context("XDG_RUNTIME_DIR is not set")?;
    Ok(PathBuf::from(base).join("cbox"))
}

fn disk_root() -> Result<PathBuf> {
    if let Some(base) = std::env::var_os("XDG_CACHE_HOME").filter(|v| !v.is_empty()) {
        return Ok(PathBuf::from(base).join("cbox"));
    }
    Ok(home()?.join(".cache").join("cbox"))
}

fn home() -> Result<PathBuf> {
    let h = std::env::var_os("HOME")
        .filter(|v| !v.is_empty())
        .context("HOME is not set")?;
    Ok(PathBuf::from(h))
}

fn runner() -> Result<PathBuf> {
    let r = std::env::var_os("CBOX_RUNNER")
        .filter(|v| !v.is_empty())
        .context("CBOX_RUNNER is not set; cbox has to run from its Nix wrapper")?;
    Ok(PathBuf::from(r))
}

fn urandom<const N: usize>() -> Result<[u8; N]> {
    let mut buf = [0u8; N];
    File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut buf))
        .context("reading /dev/urandom")?;
    Ok(buf)
}

fn alive(pid: i32) -> bool {
    pid > 0 && unsafe { libc::kill(pid, 0) } == 0
}

fn accessible(path: &str, mode: libc::c_int) -> bool {
    let c = std::ffi::CString::new(path).expect("literal path has no NUL");
    unsafe { libc::access(c.as_ptr(), mode) == 0 }
}

fn whoami() -> String {
    std::env::var("USER")
        .or_else(|_| std::env::var("LOGNAME"))
        .unwrap_or_else(|_| format!("uid {}", unsafe { libc::getuid() }))
}

/// `--flag=VALUE` where VALUE is a path that need not be valid UTF-8.
fn opt(prefix: &str, value: impl AsRef<OsStr>) -> OsString {
    let mut o = OsString::from(prefix);
    o.push(value);
    o
}

/// POSIX single-quoting, for strings a shell on the far end will re-parse.
fn shell_quote(bytes: &[u8]) -> Vec<u8> {
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

/// One shell-quoted string for the guest's login shell to parse.
fn remote_cmd(cmd: &[String]) -> String {
    let mut out = shell_quote(GUEST_ENTER.as_bytes());
    for arg in cmd {
        out.push(b' ');
        out.extend_from_slice(&shell_quote(arg.as_bytes()));
    }
    String::from_utf8(out).expect("quoting preserves UTF-8")
}

fn read_trimmed(path: &Path) -> Result<String> {
    let s = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    Ok(s.trim().to_string())
}

fn read_pid(path: &Path) -> Option<i32> {
    read_trimmed(path).ok()?.parse().ok()
}

/// The one way into a box: ssh over vsock, pinned host key, session key only,
/// nothing from the user's ssh config or agent.
fn ssh_box(dir: &Path) -> Result<Command> {
    let cid = read_trimmed(&dir.join("cid"))?;
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

fn live_sessions(state_root: &Path) -> Vec<PathBuf> {
    let mut found: Vec<PathBuf> = session_dirs(state_root)
        .into_iter()
        .filter(|d| read_pid(&d.join("qemu.pid")).is_some_and(alive))
        .collect();
    found.sort();
    found
}

fn session_dirs(state_root: &Path) -> Vec<PathBuf> {
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

fn pick_session(want: Option<&str>) -> Result<PathBuf> {
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

fn exit_code(status: ExitStatus) -> ExitCode {
    let code = status
        .code()
        .unwrap_or_else(|| 128 + status.signal().unwrap_or(0));
    ExitCode::from(code as u8)
}

// ------------------------------------------------------------- child lifetime

/// Spawn with stdout and stderr in one log file and stdin closed.
fn spawn_logged(cmd: &mut Command, log: &Path) -> Result<Child> {
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
fn wait_bounded(child: &mut Child, limit: Duration) {
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

/// SIGTERM, wait a little, then SIGKILL. Bounded so a wedged child can't hold
/// up removing the session directory it has open.
fn terminate(mut child: Child, grace: Duration) {
    unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };
    wait_bounded(&mut child, grace);
}

/// Show the tail of the session's logs, then the error. A failure this early is
/// almost always explained in one of them.
fn fail(run: &Path, msg: &str, logs: &[&str]) -> anyhow::Error {
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

/// Everything a `run` creates. Dropping it shuts the box down and, unless
/// --keep, deletes the session directory and its disks — the bash original's
/// `trap cleanup EXIT`.
struct Session {
    run: PathBuf,
    disk: PathBuf,
    runner: PathBuf,
    keep: bool,
    fsd: Option<Child>,
    net: Option<Child>,
    qemu: Option<Child>,
}

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

// ------------------------------------------------------------------------ run

fn cmd_run(a: RunArgs) -> Result<ExitCode> {
    let runner = runner()?;

    let mut pass_env = vec![
        "CLAUDE_CODE_OAUTH_TOKEN".to_string(),
        "ANTHROPIC_API_KEY".to_string(),
    ];
    pass_env.extend(a.env);
    for name in &pass_env {
        if !is_var_name(name) {
            bail!("not a variable name: {name}");
        }
    }

    if !accessible("/dev/kvm", libc::R_OK | libc::W_OK) {
        bail!("/dev/kvm is not usable by {}", whoami());
    }
    if !accessible("/dev/vhost-vsock", libc::W_OK) {
        bail!("/dev/vhost-vsock is not usable by {}", whoami());
    }

    let requested = match a.dir {
        Some(d) => d,
        None => std::env::current_dir().context("no current directory")?,
    };
    let dir = fs::canonicalize(&requested)
        .with_context(|| format!("no such directory: {}", requested.display()))?;
    if !dir.is_dir() {
        bail!("not a directory: {}", dir.display());
    }
    let home = fs::canonicalize(home()?).context("$HOME does not resolve")?;
    if refuses_to_share(&dir, &home) {
        bail!(
            "refusing to share {}: it is $HOME or contains it",
            dir.display()
        );
    }

    if a.git == GitMode::Rw {
        eprintln!(
            "cbox: --git rw: review .git before running host git in {} afterwards",
            dir.display()
        );
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

    let id = session_id(&dir)?;
    let run = state_root()?.join(&id);
    let disk = disk_root()?.join(&id);
    fs::create_dir_all(run.parent().expect("has parent"))?;
    fs::create_dir_all(disk.parent().expect("has parent"))?;
    // create_dir, not create_dir_all: a collision must not adopt a live session.
    fs::create_dir(&run).with_context(|| format!("creating {}", run.display()))?;

    // From here on the session owns both directories: every exit path cleans up.
    let mut sess = Session {
        run: run.clone(),
        disk: disk.clone(),
        runner: runner.clone(),
        keep: a.keep,
        fsd: None,
        net: None,
        qemu: None,
    };

    fs::create_dir(&disk).with_context(|| format!("creating {}", disk.display()))?;
    fs::create_dir(run.join("cred"))?;
    std::os::unix::fs::symlink(&disk, run.join("disk"))?;
    let mut project = dir.clone().into_os_string().into_vec();
    project.push(b'\n');
    fs::write(run.join("project"), project)?;
    let cid = u32::from_ne_bytes(urandom::<4>()?) % 4_000_000_000 + 1000;
    fs::write(run.join("cid"), format!("{cid}\n"))?;

    write_credentials(&run, a.git, a.lan, &pass_env)?;

    // Guest writes land as us; our files appear as agent (1000:100) inside.
    // Default --sandbox=namespace: virtiofsd pivot_roots into $dir, which is
    // what stops symlink escapes (--sandbox=none would not).
    let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
    sess.fsd = Some(spawn_logged(
        Command::new("virtiofsd")
            .arg(opt("--socket-path=", run.join("work.sock")))
            .arg(opt("--shared-dir=", &dir))
            .args(["--sandbox=namespace", "--cache=auto"])
            .arg(format!("--translate-uid=squash-guest:0:{uid}:4294967295"))
            .arg(format!("--translate-uid=host:{uid}:1000:1"))
            .arg(format!("--translate-gid=squash-guest:0:{gid}:4294967295"))
            .arg(format!("--translate-gid=host:{gid}:100:1")),
        &run.join("virtiofsd.log"),
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
            .arg(run.join("passt.sock"))
            .args([
                "--no-map-gw",
                "--dns-forward",
                "169.254.1.1",
                "--search",
                "none",
            ]),
        &run.join("passt.log"),
    )?);

    wait_for_socket(&run, "work.sock", sess.fsd.as_mut(), "virtiofsd")?;
    wait_for_socket(&run, "passt.sock", sess.net.as_mut(), "passt")?;

    sess.qemu = Some(spawn_logged(
        Command::new(runner.join("bin/microvm-run")).current_dir(&run),
        &run.join("console.log"),
    )?);
    let qemu_pid = sess.qemu.as_ref().expect("just set").id();
    fs::write(run.join("qemu.pid"), format!("{qemu_pid}\n"))?;

    eprintln!("cbox: booting {id}");
    wait_until_ready(&run, sess.qemu.as_mut().expect("just set"))?;
    let _ = fs::remove_dir_all(run.join("cred")); // QEMU read the fw_cfg files at startup

    let mut ssh = ssh_box(&run)?;
    ssh.arg("-t")
        .arg("agent@cbox")
        .arg("--")
        .arg(remote_cmd(&a.cmd));
    let status = wait_foreground(ssh.spawn().context("starting ssh")?)?;
    Ok(exit_code(status))
}

/// Everything in the box can write the shared directory, and $HOME is full of
/// things the host executes, so neither $HOME nor any parent of it is sharable.
/// Both paths must already be canonical.
fn refuses_to_share(dir: &Path, home: &Path) -> bool {
    dir == Path::new("/") || home.starts_with(dir)
}

fn is_var_name(s: &str) -> bool {
    let mut bytes = s.bytes();
    bytes
        .next()
        .is_some_and(|b| b.is_ascii_alphabetic() || b == b'_')
        && bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

fn session_id(dir: &Path) -> Result<String> {
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

/// Per-session keys and settings, one file per fw_cfg entry the guest reads.
/// The client key logs in; the host key is pinned in known_hosts.
fn write_credentials(run: &Path, git: GitMode, lan: bool, pass_env: &[String]) -> Result<()> {
    let cred = run.join("cred");
    keygen(&run.join("client"), "cbox")?;
    keygen(&cred.join("hostkey"), "cbox-host")?;

    let hostkey_pub = read_trimmed(&cred.join("hostkey.pub"))?;
    fs::write(run.join("known_hosts"), format!("cbox {hostkey_pub}\n"))?;
    fs::remove_file(cred.join("hostkey.pub"))?;
    fs::copy(run.join("client.pub"), cred.join("authorized_keys"))?;
    fs::write(cred.join("git"), format!("{git}\n"))?;
    fs::write(cred.join("lan"), if lan { "1\n" } else { "0\n" })?;

    let mut env = b"# cbox session environment\n".to_vec();
    for name in pass_env {
        let Some(value) = std::env::var_os(name).filter(|v| !v.is_empty()) else {
            continue;
        };
        env.extend_from_slice(format!("export {name}=").as_bytes());
        env.extend_from_slice(&shell_quote(&value.into_vec()));
        env.push(b'\n');
    }
    fs::write(cred.join("env"), env)?;
    Ok(())
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

fn wait_for_socket(run: &Path, sock: &str, child: Option<&mut Child>, name: &str) -> Result<()> {
    let log = format!("{name}.log");
    let path = run.join(sock);
    let child = child.expect("child was spawned");
    for _ in 0..200 {
        if fs::metadata(&path).is_ok_and(|m| m.file_type().is_socket()) {
            return Ok(());
        }
        if !matches!(child.try_wait(), Ok(None)) {
            return Err(fail(run, &format!("{name} exited during startup"), &[&log]));
        }
        if interrupted() {
            bail!("interrupted");
        }
        sleep(Duration::from_millis(50));
    }
    Err(fail(
        run,
        &format!("{name} did not create its socket"),
        &[&log],
    ))
}

fn wait_until_ready(run: &Path, qemu: &mut Child) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(180);
    loop {
        let ready = ssh_box(run)?
            .args(["agent@cbox", "--", "test", "-e", "/run/cbox/ready"])
            .stdin(Stdio::null())
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
            return Err(fail(
                run,
                "the VM exited during boot",
                &["console.log", "virtiofsd.log"],
            ));
        }
        if Instant::now() >= deadline {
            return Err(fail(
                run,
                "the VM was not ready after 180s",
                &["console.log"],
            ));
        }
        sleep(Duration::from_secs(1));
    }
}

// ------------------------------------------------------- the other subcommands

fn cmd_attach(id: Option<String>, cmd: Vec<String>) -> Result<ExitCode> {
    let dir = pick_session(id.as_deref())?;
    let mut ssh = ssh_box(&dir)?;
    ssh.arg("-t")
        .arg("agent@cbox")
        .arg("--")
        .arg(remote_cmd(&cmd));
    Ok(exit_code(wait_foreground(
        ssh.spawn().context("starting ssh")?,
    )?))
}

fn cmd_view(id: Option<String>) -> Result<ExitCode> {
    let dir = pick_session(id.as_deref())?;
    let ctl = dir.join("vnc.ctl");

    // ssh -f backgrounds itself once the forward is up, so a bound port shows
    // up as a non-zero exit and we just try another one.
    let mut port = 0u16;
    let mut up = false;
    for _ in 0..5 {
        port = u16::from_ne_bytes(urandom::<2>()?) % 20000 + 40000;
        up = ssh_box(&dir)?
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

fn cmd_ls() -> Result<ExitCode> {
    let mut out = std::io::stdout().lock();
    for dir in live_sessions(&state_root()?) {
        let id = dir.file_name().expect("session dir has a name");
        let project = fs::read(dir.join("project")).unwrap_or_default();
        let project = String::from_utf8_lossy(&project);
        writeln!(out, "{}\t{}", id.to_string_lossy(), project.trim_end())?;
    }
    Ok(ExitCode::SUCCESS)
}

fn cmd_stop(id: String) -> Result<ExitCode> {
    let dir = pick_session(Some(&id))?;
    let status = Command::new(runner()?.join("bin/microvm-shutdown"))
        .current_dir(&dir)
        .status()
        .context("running microvm-shutdown")?;
    Ok(exit_code(status))
}

fn cmd_gc() -> Result<ExitCode> {
    let state_root = state_root()?;
    let disk_root = disk_root()?;
    for dir in session_dirs(&state_root) {
        let pidfile = dir.join("qemu.pid");
        if !pidfile.is_file() {
            continue; // never got as far as starting QEMU
        }
        if read_pid(&pidfile).is_some_and(alive) {
            continue; // still running; an unreadable pid counts as dead
        }
        let id = dir.file_name().expect("session dir has a name").to_owned();
        eprintln!("cbox: removing {}", id.to_string_lossy());
        let _ = fs::remove_dir_all(disk_root.join(&id));
        let _ = fs::remove_dir_all(&dir);
    }
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn quoted(s: &str) -> String {
        String::from_utf8(shell_quote(s.as_bytes())).unwrap()
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

    #[test]
    fn variable_names_are_checked() {
        for ok in ["PATH", "_x", "A1", "ANTHROPIC_API_KEY"] {
            assert!(is_var_name(ok), "{ok}");
        }
        for bad in ["", "1A", "a-b", "a b", "FOO=bar", "a;id"] {
            assert!(!is_var_name(bad), "{bad}");
        }
    }

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
        let id = session_id(Path::new("/src/my project!")).unwrap();
        assert!(id.starts_with("my_project_-"), "{id}");
        let long = session_id(Path::new(&format!("/src/{}", "x".repeat(40)))).unwrap();
        assert_eq!(long.len(), 24 + 1 + 6, "{long}");
    }

    #[test]
    fn run_is_the_default_subcommand() {
        let parse = |args: &[&str]| Cli::try_parse_from(args).map(|c| c.cmd);
        assert!(matches!(parse(&["cbox", "run"]), Ok(Cmd::Run(_))));
        assert!(matches!(parse(&["cbox", "ls"]), Ok(Cmd::Ls)));

        // What argv_with_default_subcommand() produces for `cbox ~/src/thing`.
        let Ok(Cmd::Run(a)) = parse(&["cbox", "run", "/src/thing", "--", "claude", "-p"]) else {
            panic!("expected a run");
        };
        assert_eq!(a.dir.as_deref(), Some(Path::new("/src/thing")));
        assert_eq!(a.cmd, ["claude", "-p"]);
        assert_eq!(a.git, GitMode::Ro);
    }
}
