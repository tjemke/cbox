//! cbox: a fresh NixOS microVM per session, with one host directory at /work.
//!
//! A box is three processes we own — virtiofsd for the shared directory, passt
//! for the network, QEMU for the VM — plus ssh over vsock to get in. This file
//! is only the command-line surface; the work is in:
//!
//! - [`session`]: the per-box directory, its lifetime, and finding existing ones
//! - [`cred`]: the keys and settings the guest reads out of fw_cfg
//! - [`ssh`]: getting into a box and handing it text to run
//! - [`proc`]: child processes and the signals that cut them short
//! - [`boot`]: `cbox run`, which creates a box
//! - [`commands`]: the verbs that work on a box you already have

mod boot;
mod commands;
mod cred;
mod proc;
mod session;
mod ssh;
mod sys;

use std::ffi::OsString;
use std::fmt;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Args, Parser, Subcommand, ValueEnum};

const SUBCOMMANDS: &[&str] = &["run", "attach", "view", "ls", "stop", "gc", "help"];

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
    sys::set_private_umask();
    if let Err(e) = proc::watch_signals() {
        eprintln!("cbox: {e:#}");
        return ExitCode::FAILURE;
    }

    let result = match Cli::parse_from(argv_with_default_subcommand()).cmd {
        Cmd::Run(a) => boot::run(a),
        Cmd::Attach { id, cmd } => commands::attach(id, cmd),
        Cmd::View { id } => commands::view(id),
        Cmd::Ls => commands::ls(),
        Cmd::Stop { id } => commands::stop(id),
        Cmd::Gc => commands::gc(),
    };

    // A signal preempts whatever else was going on, as the bash original's
    // `trap 'exit 130' INT TERM HUP` did. Cleanup has already run by here.
    if proc::interrupted() {
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

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;
    use std::path::Path;

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

    #[test]
    fn every_subcommand_is_listed_for_the_argv_fixup() {
        // A name clap knows but SUBCOMMANDS doesn't would be silently treated
        // as a project directory.
        let known: Vec<String> = Cli::command()
            .get_subcommands()
            .map(|s| s.get_name().to_string())
            .collect();
        for name in &known {
            assert!(
                SUBCOMMANDS.contains(&name.as_str()),
                "{name} is missing from SUBCOMMANDS"
            );
        }
        // clap generates `help` itself, so it never appears in get_subcommands()
        // — but `cbox help` still has to dispatch rather than be read as a path.
        assert!(SUBCOMMANDS.contains(&"help"));
        assert_eq!(known.len() + 1, SUBCOMMANDS.len());
    }
}
