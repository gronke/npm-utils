//! `upgrade` — re-resolve dependencies within their ranges, refresh the lock, install
//! (= `npm update`). Thin wrapper over [`crate::project::upgrade`].

use std::path::Path;

use super::common::{report_installed, SyncTasks};
use super::progress::Progress;
use super::Res;
use crate::project;
use crate::registry::PackumentDetail;

/// Upgrade the selected dependencies (empty = all) and refresh the lock + `node_modules/`, printing
/// each applied `name: from → to` change and the reinstalled tree. The plan's per-dependency
/// resolves count into the `[resolve]` task's elapsed time.
pub(super) fn run(
    packages: &[String],
    dir: &Path,
    detail: PackumentDetail,
    progress: &Progress,
) -> Res {
    let tasks = SyncTasks::new(progress, "registry.npmjs.org");
    let (changes, installed) = project::upgrade_observed(
        dir,
        packages,
        detail,
        |event| tasks.on_resolve(event),
        |event| tasks.on_install(event),
    )?;
    tasks.done(installed.len());
    for change in &changes {
        println!("{}: {} → {}", change.name, change.from, change.to);
    }
    report_installed(&installed);
    Ok(())
}
