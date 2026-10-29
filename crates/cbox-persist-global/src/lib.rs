//! Carrying chosen Claude Code settings from one box to every later one:
//! cbox's `persist-global` feature, which the Nix package turns on when
//! `programs.cbox.persist.global` lists any keys. Without it, cbox doesn't
//! link this crate at all. cbox's side is `crates/cbox/src/persist_global.rs`,
//! which finds the session and hands over a way into the box; everything else
//! is here.
//!
//! Every box starts from a fresh home, so a `/model` or `/config` choice made
//! in one is gone in the next. This carries a list of top-level keys across:
//! read out of the box when its session ends, kept on the host in
//! `$XDG_STATE_HOME/cbox/persist-global.json`, and handed to the next box as
//! one more file in `cred/`, which the guest merges into its own.
//!
//! That makes it a channel between boxes on unrelated projects, and both ends
//! of it are files Claude Code takes hooks, helper commands and MCP servers
//! from. So the host filters what comes out, before storing it: listed keys
//! only, scalar values only, and what it hands over again has never been
//! anything else. Keys that hold a command even as a string (`apiKeyHelper`)
//! are refused at build time by package.nix.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};
use std::{env, fs, thread};

use anyhow::{Context, Result};
use serde_json::{Map, Value};

/// The files keys are kept from: their name in `persist-global.json` and in
/// the `cred/persist-global` hand-over (their path under the guest's home), and the
/// variable the Nix wrapper lists their keys in.
const FILES: [(&str, &str); 2] = [
    (".claude/settings.json", "CBOX_PERSIST_GLOBAL_SETTINGS"),
    (".claude.json", "CBOX_PERSIST_GLOBAL_CLAUDE_JSON"),
];

/// Where the guest keeps them; see guest.nix.
const GUEST_HOME: &str = "/home/agent";

/// ~/.claude.json carries caches and per-project state and grows to tens of
/// KiB; anything far past that is not a settings file.
const MAX_FILE: u64 = 4 << 20;
/// Model names, themes and the like. A longer string is not one of those.
const MAX_STRING: usize = 1024;

/// What this box was handed, per file, so that only what changed in it is
/// written back: a box that never touched `model` must not undo another box's
/// `/model` from in between just because it exits later.
pub struct Given(Map<String, Value>);

/// Read the store and put the kept keys into `persist-global` in the session's
/// `cred/` directory.
pub fn hand_over(store: &Path, cred: &Path) -> Result<Given> {
    let stored = read_store(store);
    let mut given = Map::new();
    for (file, var) in FILES {
        let kept = keep(stored.get(file), &keys(var));
        if !kept.is_empty() {
            given.insert(file.to_string(), Value::Object(kept));
        }
    }
    if !given.is_empty() {
        fs::write(
            cred.join("persist-global"),
            serde_json::to_vec(&given).expect("a Map serializes"),
        )?;
    }
    Ok(Given(given))
}

/// Read the listed keys back out of a box whose session has just ended, and
/// store whatever changed. `cat` reads a file in the box by its path there,
/// [`read_capped`] being the way to. Best effort: a box that can't be read, or
/// files that don't parse, leave the store as it was.
pub fn take_back(store: &Path, given: Given, mut cat: impl FnMut(&str) -> Option<Vec<u8>>) {
    let mut changes = Vec::new();
    for (file, var) in FILES {
        let Some(now) = cat(&format!("{GUEST_HOME}/{file}"))
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        else {
            continue;
        };
        changes.extend(changed(file, &keys(var), given.0.get(file), &now));
    }
    if changes.is_empty() {
        return;
    }

    // Re-read rather than reuse what we handed over: another box may have
    // stored something since.
    let mut stored = read_store(store);
    apply(&mut stored, changes);
    if let Err(e) = write_store(store, &stored) {
        eprintln!("cbox: could not keep settings for later boxes: {e:#}");
    }
}

/// One key of one file to set in the store, or with `None`, to forget.
type Change = (&'static str, String, Option<Value>);

/// The listed keys whose value in the box at its end differs from what it was
/// handed. A key the box no longer has was taken out in it, and is forgotten.
fn changed(
    file: &'static str,
    keys: &[String],
    before: Option<&Value>,
    now: &Value,
) -> Vec<Change> {
    let before = keep(before, keys);
    let now = keep(Some(now), keys);
    keys.iter()
        .filter(|k| now.get(*k) != before.get(*k))
        .map(|k| (file, k.clone(), now.get(k).cloned()))
        .collect()
}

fn apply(stored: &mut Map<String, Value>, changes: Vec<Change>) {
    for (file, key, value) in changes {
        let entry = stored
            .entry(file)
            .or_insert_with(|| Value::Object(Map::new()));
        if !entry.is_object() {
            *entry = Value::Object(Map::new());
        }
        let map = entry.as_object_mut().expect("just made an object");
        match value {
            Some(v) => map.insert(key, v),
            None => map.remove(&key),
        };
    }
}

/// The keys the Nix wrapper lists, comma-separated. package.nix already only
/// lets through names made of these characters; checking again keeps a stray
/// environment from asking for anything odd.
fn keys(var: &str) -> Vec<String> {
    env::var(var)
        .unwrap_or_default()
        .split(',')
        .filter(|k| {
            !k.is_empty()
                && k.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"_-.".contains(&b))
        })
        .map(str::to_string)
        .collect()
}

/// The listed keys of an object, those with scalar values only. Objects and
/// arrays are where hooks, MCP servers, plugin lists and environment
/// variables live, so none of them come across even under a listed key.
fn keep(obj: Option<&Value>, keys: &[String]) -> Map<String, Value> {
    let Some(obj) = obj.and_then(Value::as_object) else {
        return Map::new();
    };
    keys.iter()
        .filter_map(|k| {
            let v = obj.get(k)?;
            let scalar = match v {
                Value::String(s) => s.len() <= MAX_STRING,
                Value::Bool(_) | Value::Number(_) => true,
                Value::Null | Value::Array(_) | Value::Object(_) => false,
            };
            scalar.then(|| (k.clone(), v.clone()))
        })
        .collect()
}

/// `$XDG_STATE_HOME/cbox/persist-global.json`, by default under `home`.
pub fn store_path(home: &Path) -> PathBuf {
    let base = match env::var_os("XDG_STATE_HOME").filter(|v| !v.is_empty()) {
        Some(b) => PathBuf::from(b),
        None => home.join(".local").join("state"),
    };
    base.join("cbox").join("persist-global.json")
}

/// A missing store is an empty one, and so is one that doesn't parse, with a
/// warning: it is only ever a convenience.
fn read_store(path: &Path) -> Map<String, Value> {
    let Ok(bytes) = fs::read(path) else {
        return Map::new();
    };
    match serde_json::from_slice::<Value>(&bytes) {
        Ok(Value::Object(m)) => m,
        _ => {
            eprintln!("cbox: ignoring {}: not a JSON object", path.display());
            Map::new()
        }
    }
}

/// Write-and-rename, so two boxes exiting at once leave one whole file.
fn write_store(path: &Path, stored: &Map<String, Value>) -> Result<()> {
    let dir = path.parent().expect("has parent");
    fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let tmp = dir.join(format!(".persist-global.{}", std::process::id()));
    let mut json = serde_json::to_vec_pretty(stored).expect("a Map serializes");
    json.push(b'\n');
    fs::write(&tmp, json).with_context(|| format!("writing {}", tmp.display()))?;
    fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))
}

/// What `cmd` prints, if it exits successfully. It reads one of the box's
/// files, and everything about that file is up to the agent, down to whether
/// `cat` ever finishes (a FIFO in its place), so the read is capped in both
/// size and time.
pub fn read_capped(mut cmd: Command, limit: Duration) -> Option<Vec<u8>> {
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let out = child.stdout.take()?;
    let reader = thread::spawn(move || {
        let mut buf = Vec::new();
        out.take(MAX_FILE + 1).read_to_end(&mut buf).map(|_| buf)
    });
    let ok = wait_bounded(&mut child, limit).is_some_and(|s| s.success());
    let buf = reader.join().ok()?.ok()?;
    (ok && buf.len() as u64 <= MAX_FILE).then_some(buf)
}

/// Wait up to `limit`, then kill; the exit status if it got to exit. cbox's
/// own `proc::wait_bounded`, which this crate can't reach.
fn wait_bounded(child: &mut Child, limit: Duration) -> Option<ExitStatus> {
    let deadline = Instant::now() + limit;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status),
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(50)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn names(ks: &[&str]) -> Vec<String> {
        ks.iter().map(|k| k.to_string()).collect()
    }

    #[test]
    fn only_listed_scalars_are_kept() {
        let file = json!({
            "model": "opus",
            "verbose": true,
            "cleanupPeriodDays": 30,
            "hooks": {"PreToolUse": []},
            "env": {"FOO": "bar"},
            "theme": null,
            "outputStyle": "x".repeat(MAX_STRING + 1),
            "unlisted": "kept out",
        });
        let keys = names(&[
            "model",
            "verbose",
            "cleanupPeriodDays",
            "hooks",
            "env",
            "theme",
            "outputStyle",
            "absent",
        ]);
        assert_eq!(
            Value::Object(keep(Some(&file), &keys)),
            json!({"model": "opus", "verbose": true, "cleanupPeriodDays": 30})
        );
        assert!(keep(Some(&json!(["model"])), &keys).is_empty());
        assert!(keep(None, &keys).is_empty());
    }

    #[test]
    fn only_what_a_box_changed_is_stored() {
        let keys = names(&["model", "theme", "verbose"]);
        let given = json!({"model": "opus", "theme": "dark"});
        // model changed, theme taken out, verbose new, hooks never listed.
        let now = json!({"model": "sonnet", "verbose": true, "hooks": {}});
        let changes = changed("f", &keys, Some(&given), &now);
        assert_eq!(
            changes,
            vec![
                ("f", "model".into(), Some(json!("sonnet"))),
                ("f", "theme".into(), None),
                ("f", "verbose".into(), Some(json!(true))),
            ]
        );
        // A box that changed nothing leaves alone what another stored since.
        assert!(changed("f", &keys, Some(&given), &given).is_empty());
        // A value that would not be kept is not a change either.
        assert!(changed(
            "f",
            &keys,
            Some(&given),
            &json!({"model": "opus", "theme": "dark", "verbose": [1]})
        )
        .is_empty());

        let mut stored = Map::new();
        stored.insert(
            "f".into(),
            json!({"model": "haiku", "theme": "dark", "other": 1}),
        );
        stored.insert("g".into(), json!("not an object"));
        apply(&mut stored, changes);
        apply(
            &mut stored,
            vec![("g", "theme".into(), Some(json!("light")))],
        );
        assert_eq!(
            Value::Object(stored),
            json!({
                "f": {"model": "sonnet", "verbose": true, "other": 1},
                "g": {"theme": "light"},
            })
        );
    }

    #[test]
    fn reads_out_of_a_box_are_capped() {
        let sh = |script: &str| {
            let mut c = Command::new("sh");
            c.args(["-c", script]);
            read_capped(c, Duration::from_secs(2))
        };
        assert_eq!(sh("printf '{}'"), Some(b"{}".to_vec()));
        assert_eq!(sh("printf '{}'; exit 1"), None);
        assert_eq!(sh(&format!("head -c {} /dev/zero", MAX_FILE + 1)), None);
        assert_eq!(
            sh(&format!("head -c {} /dev/zero", MAX_FILE)).map(|b| b.len() as u64),
            Some(MAX_FILE)
        );
        let start = Instant::now();
        assert_eq!(sh("sleep 30"), None);
        assert!(start.elapsed() < Duration::from_secs(10));
    }

    #[test]
    fn key_lists_drop_odd_names() {
        env::set_var(
            "CBOX_PERSIST_GLOBAL_TEST",
            "model,,theme,a b,x=y,editor-Mode.v2",
        );
        assert_eq!(
            keys("CBOX_PERSIST_GLOBAL_TEST"),
            names(&["model", "theme", "editor-Mode.v2"])
        );
        assert!(keys("CBOX_PERSIST_GLOBAL_UNSET").is_empty());
    }

    #[test]
    fn a_broken_store_reads_as_empty() {
        let dir = env::temp_dir().join(format!("cbox-persist-global-test-{}", std::process::id()));
        let path = dir.join("cbox/persist-global.json");
        assert!(read_store(&path).is_empty());

        let mut stored = Map::new();
        stored.insert(".claude/settings.json".into(), json!({"model": "opus"}));
        write_store(&path, &stored).unwrap();
        assert_eq!(read_store(&path), stored);

        fs::write(&path, b"[1, 2").unwrap();
        assert!(read_store(&path).is_empty());
        fs::remove_dir_all(&dir).unwrap();
    }
}
