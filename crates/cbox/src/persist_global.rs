//! The `persist-global` feature's side of a session: where the store is,
//! whether the box is still there to read, and how to read it. The rest is in the
//! cbox-persist-global crate, which a build without the feature doesn't link.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Result;
use cbox_persist_global::Given;

use crate::proc::interrupted;
use crate::session::{self, Session};
use crate::ssh;

pub fn hand_over(run: &Path) -> Result<Given> {
    cbox_persist_global::hand_over(&store()?, &run.join("cred"))
}

/// Once the session has ended, while the box is still up to be asked.
pub fn take_back(sess: &mut Session, given: Given) {
    let running = sess
        .qemu
        .as_mut()
        .is_some_and(|q| matches!(q.try_wait(), Ok(None)));
    if !running || interrupted() {
        return;
    }
    let Ok(store) = store() else { return };
    cbox_persist_global::take_back(&store, given, |path| {
        let mut cat = ssh::ssh_box(&sess.run).ok()?;
        cat.args(["agent@cbox", "--", "cat", path]);
        cbox_persist_global::read_capped(cat, Duration::from_secs(10))
    });
}

fn store() -> Result<PathBuf> {
    Ok(cbox_persist_global::store_path(&session::home()?))
}
