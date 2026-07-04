//! Helpers shared by the verb submodules: the install-report printer, `name@range` splitting,
//! the project's default name, and the progress tickers that render library observer events
//! onto [`TaskStatus`]es. The manifest read/write helpers and the lock+install `sync` now live
//! in the public [`crate::project`] module (shared with library consumers); the first two are
//! re-exported here so the verb submodules keep their short `common::` paths.

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::Mutex;

use serde_json::Value;

use super::progress::{Progress, TaskStatus};
use super::source::{BareToken, Source};
use super::Res;
use crate::install::InstallEvent;
use crate::package_json::{manifest, spec};
use crate::registry::{PackumentDetail, Registry, ResolveEvent, Resolved};

// Manifest read/write moved to `crate::project` (public API); re-export for the verb submodules.
pub(super) use crate::project::{read_manifest, write_manifest};

/// Parse `tokens` as registry spec sources (`name`, `name@range`, `name=range` — a directory/file
/// source gets a clear error naming `verb`), resolve a missing range to `^latest`, record each in
/// `package.json` (scaffolding one if absent), and write the manifest back. Returns the updated
/// doc; `add` and `install <SOURCES>` share it.
pub(super) fn add_specs(tokens: &[String], dir: &Path, verb: &str) -> Res<Value> {
    let mut doc = if dir.join("package.json").exists() {
        read_manifest(dir)?
    } else {
        manifest::scaffold(&default_name(dir), "1.0.0")
    };

    let registry = Registry::npm();
    for token in tokens {
        let (name, range) = Source::parse(token, BareToken::Spec)?.into_spec(verb)?;
        let range = match range {
            Some(r) => r,
            None => format!("^{}", registry.resolve(&name, &spec::Range::any())?.version),
        };
        manifest::upsert_dependency(&mut doc, &name, &range);
        println!("+ {name}@{range}");
    }
    write_manifest(dir, &doc)?;
    Ok(doc)
}

/// Rewrite the lock from the manifest and install, then print the installed tree. The library
/// [`crate::project::sync`] does the work and returns the packages; this thin wrapper renders
/// its progress ([`SyncTasks`]) and reports them (the `add` / `install` / `upgrade` verbs share
/// it).
pub(super) fn sync(dir: &Path, doc: &Value, detail: PackumentDetail, progress: &Progress) -> Res {
    // `project::sync` always resolves against the public registry (Registry::npm).
    let tasks = SyncTasks::new(progress, "registry.npmjs.org");
    let installed = crate::project::sync_observed(
        dir,
        doc,
        detail,
        |event| tasks.on_resolve(event),
        |event| tasks.on_install(event),
    )?;
    tasks.done(installed.len());
    report_installed(&installed);
    Ok(())
}

/// Report an install's outcome: a count line plus each `name@version` (sorted by the installer).
pub(super) fn report_installed(installed: &[Resolved]) {
    println!("installed {} package(s)", installed.len());
    for r in installed {
        println!("  {}@{}", r.name, r.version);
    }
}

/// Renders [`ResolveEvent`]s onto a `[resolve]` task: the in-flight packument fetches on the
/// transient detail line, one count per resolved package. Fetch events arrive from the
/// resolver's worker threads — the observer bound is `Fn + Sync` — so the in-flight set lives
/// behind a Mutex and the task is ticked through its `&self` API.
pub(super) struct ResolveTicker<'t> {
    task: &'t TaskStatus,
    in_flight: Mutex<BTreeSet<String>>,
}

impl<'t> ResolveTicker<'t> {
    pub(super) fn new(task: &'t TaskStatus) -> ResolveTicker<'t> {
        ResolveTicker {
            task,
            in_flight: Mutex::new(BTreeSet::new()),
        }
    }

    pub(super) fn observe(&self, event: ResolveEvent<'_>) {
        tick_resolve(self.task, &self.in_flight, event);
    }
}

/// The shared `[resolve]`-task rendering: fetch begin/done maintain the in-flight set and the
/// detail line, a resolution counts the package ([`ResolveTicker`] and [`SyncTasks`] both route
/// here).
fn tick_resolve(task: &TaskStatus, in_flight: &Mutex<BTreeSet<String>>, event: ResolveEvent<'_>) {
    match event {
        ResolveEvent::FetchBegin { name } => {
            let mut set = in_flight.lock().expect("in-flight set poisoned");
            set.insert(name.to_string());
            task.detail(&render_in_flight(&set));
        }
        ResolveEvent::FetchDone { name } => {
            let mut set = in_flight.lock().expect("in-flight set poisoned");
            set.remove(name);
            task.detail(&render_in_flight(&set));
        }
        ResolveEvent::Resolved { package, .. } => {
            task.inc(&format!("{}@{}", package.name, package.version));
        }
    }
}

/// Drives the `[resolve]` + `[install]` task pair around a sync-shaped library call
/// ([`crate::project::sync_observed`] and friends). The resolve task is created eagerly — a
/// zero-dependency project still shows the line, matching audit — and finishes with its package
/// count on the first install event (resolution is over once installing starts) or in
/// [`SyncTasks::done`]. The install task appears with the first [`InstallEvent`], so a
/// skip-if-unchanged cache hit shows none.
pub(super) struct SyncTasks<'p> {
    progress: &'p Progress,
    resolve: Mutex<Option<TaskStatus>>,
    in_flight: Mutex<BTreeSet<String>>,
    install: Mutex<Option<TaskStatus>>,
}

impl<'p> SyncTasks<'p> {
    pub(super) fn new(progress: &'p Progress, host: &str) -> SyncTasks<'p> {
        let resolve = progress.task("resolve", format!("resolving dependency tree from {host}"));
        SyncTasks {
            progress,
            resolve: Mutex::new(Some(resolve)),
            in_flight: Mutex::new(BTreeSet::new()),
            install: Mutex::new(None),
        }
    }

    pub(super) fn on_resolve(&self, event: ResolveEvent<'_>) {
        if let Some(task) = self.resolve.lock().expect("resolve task poisoned").as_ref() {
            tick_resolve(task, &self.in_flight, event);
        }
    }

    pub(super) fn on_install(&self, event: InstallEvent<'_>) {
        let InstallEvent::Fetch {
            total,
            name,
            version,
            ..
        } = event;
        let mut install = self.install.lock().expect("install task poisoned");
        if install.is_none() {
            self.finish_resolve();
            let task = self.progress.task("install", "installing packages");
            task.set_total(total as u64);
            *install = Some(task);
        }
        if let Some(task) = install.as_ref() {
            task.inc(&format!("{name}@{version}"));
        }
    }

    /// Finish whichever tasks are still open: the resolve task with its own count (no install
    /// event ever arrived — lockfile-only or a cache hit), the install task with the final
    /// installed count.
    pub(super) fn done(self, installed: usize) {
        self.finish_resolve();
        if let Some(task) = self.install.lock().expect("install task poisoned").take() {
            task.finish(&format!("{installed} packages"));
        }
    }

    fn finish_resolve(&self) {
        if let Some(task) = self.resolve.lock().expect("resolve task poisoned").take() {
            let count = task.count();
            task.finish(&format!("{count} packages"));
        }
    }
}

/// `fetching a, b, c (+2 more)` — the first three in-flight names in set order; an empty set
/// clears the detail line.
fn render_in_flight(set: &BTreeSet<String>) -> String {
    if set.is_empty() {
        return String::new();
    }
    let shown: Vec<&str> = set.iter().take(3).map(String::as_str).collect();
    let rest = set.len() - shown.len();
    if rest == 0 {
        format!("fetching {}", shown.join(", "))
    } else {
        format!("fetching {} (+{rest} more)", shown.join(", "))
    }
}

/// The display host of a registry base URL — scheme and path stripped
/// (`https://r.example/npm/` → `r.example`); purely cosmetic, for the `[resolve]` task title.
pub(super) fn host_of(url: &str) -> &str {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .unwrap_or(url);
    rest.split('/').next().unwrap_or(rest)
}

/// Split `name@range` honoring scoped names: the version separator is the *last* `@` (a leading
/// `@` is the scope). `lit@^3` → `("lit", "^3")`; `@lit/context@^1` → `("@lit/context", "^1")`;
/// `lit` → `("lit", None)`.
pub(super) fn split_name_range(pkg: &str) -> (&str, Option<&str>) {
    match pkg.rfind('@') {
        Some(i) if i > 0 => (&pkg[..i], Some(&pkg[i + 1..])),
        _ => (pkg, None),
    }
}

/// The project's default package name: the (canonicalized) directory's file name, else `app`.
pub(super) fn default_name(dir: &Path) -> String {
    std::fs::canonicalize(dir)
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| "app".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_in_flight_caps_at_three_names() {
        let set: BTreeSet<String> = BTreeSet::new();
        assert_eq!(render_in_flight(&set), "");
        let set: BTreeSet<String> = ["b", "a"].iter().map(|s| s.to_string()).collect();
        assert_eq!(render_in_flight(&set), "fetching a, b");
        let set: BTreeSet<String> = ["e", "d", "c", "b", "a"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(render_in_flight(&set), "fetching a, b, c (+2 more)");
    }

    #[test]
    fn split_name_range_handles_scopes_and_bare_names() {
        assert_eq!(split_name_range("lit"), ("lit", None));
        assert_eq!(split_name_range("lit@^3"), ("lit", Some("^3")));
        assert_eq!(
            split_name_range("@lit/context@^1"),
            ("@lit/context", Some("^1"))
        );
        // A bare scoped name keeps its leading `@` (the scope is not a version marker).
        assert_eq!(split_name_range("@scope/pkg"), ("@scope/pkg", None));
    }
}
