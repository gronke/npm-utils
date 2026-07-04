//! Helpers shared by the verb submodules: the install-report printer, `name@range` splitting,
//! the project's default name, and the progress tickers that render library observer events
//! onto [`TaskStatus`]es. The manifest read/write helpers and the lock+install `sync` now live
//! in the public [`crate::project`] module (shared with library consumers); the first two are
//! re-exported here so the verb submodules keep their short `common::` paths.

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::Mutex;

use serde_json::Value;

use super::progress::TaskStatus;
use super::source::{BareToken, Source};
use super::Res;
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
/// [`crate::project::sync`] does the work and returns the packages; this thin wrapper reports them
/// (the `add` / `install` / `upgrade` verbs share it).
pub(super) fn sync(dir: &Path, doc: &Value, detail: PackumentDetail) -> Res {
    report_installed(&crate::project::sync(dir, doc, detail)?);
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
        match event {
            ResolveEvent::FetchBegin { name } => {
                let mut set = self.in_flight.lock().expect("in-flight set poisoned");
                set.insert(name.to_string());
                self.task.detail(&render_in_flight(&set));
            }
            ResolveEvent::FetchDone { name } => {
                let mut set = self.in_flight.lock().expect("in-flight set poisoned");
                set.remove(name);
                self.task.detail(&render_in_flight(&set));
            }
            ResolveEvent::Resolved { package, .. } => {
                self.task
                    .inc(&format!("{}@{}", package.name, package.version));
            }
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
