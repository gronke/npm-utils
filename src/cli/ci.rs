//! `ci` — install the exact tree a `package-lock.json` pins (= `npm ci`).

use std::path::Path;

use super::common::report_installed;
use super::progress::Progress;
use super::Res;
use crate::install::{from_lockfile_observed, InstallEvent};

/// Install the exact, integrity-checked tree the lockfile pins into `<dir>/node_modules/`,
/// counting each package on an `[install]` task (its total comes with the first event; a
/// skip-if-unchanged cache hit emits none and finishes straight to the summary).
pub(super) fn run(dir: &Path, progress: &Progress) -> Res {
    let task = progress.task("install", "installing packages");
    let mut totaled = false;
    let installed = from_lockfile_observed(&dir.join("package-lock.json"), dir, |event| {
        let InstallEvent::Fetch {
            total,
            name,
            version,
            ..
        } = event;
        if !totaled {
            task.set_total(total as u64);
            totaled = true;
        }
        task.inc(&format!("{name}@{version}"));
    })?;
    task.finish(&format!("{} packages", installed.len()));
    report_installed(&installed);
    Ok(())
}
