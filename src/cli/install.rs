//! `install` — record any positional package sources in `package.json`, then resolve the
//! manifest's `dependencies`, write `package-lock.json`, and install `node_modules/`
//! (= `npm install` / `npm install <pkg>`).
//!
//! Two flags mirror the ecosystem's "lockfile vs install" knobs:
//! - `--lockfile-only` (npm `--package-lock-only`, pnpm `--lockfile-only`): resolve and write the
//!   lock, but don't touch `node_modules/`.
//! - `--no-lockfile` (yarn `--no-lockfile`, npm `--no-package-lock`): install `node_modules/`
//!   without writing a lock.

use std::path::Path;

use super::common::{self, read_manifest, report_installed, sync};
use super::Res;
use crate::install::node_modules;
use crate::package_json::lock::write_from_manifest;
use crate::registry::{PackumentDetail, Registry};

/// Record any `sources` in the manifest first — the same `name` / `name@range` / `name=range`
/// specs `add` takes, via the shared [`common::add_specs`] — then resolve the manifest's
/// transitive registry dependencies. By default (npm parity) write a fresh, licensed v3
/// `package-lock.json` and install the locked tree from it (the same `sync` `add` and `upgrade`
/// use). `lockfile_only` stops after writing the lock; `no_lockfile` installs straight from the
/// manifest and leaves any lock untouched (the pre-0.5 behavior). clap makes the two flags
/// mutually exclusive.
pub(super) fn run(
    sources: &[String],
    dir: &Path,
    lockfile_only: bool,
    no_lockfile: bool,
    detail: PackumentDetail,
) -> Res {
    if !sources.is_empty() {
        common::add_specs(sources, dir, "install")?;
    }

    if no_lockfile {
        report_installed(&node_modules(&dir.join("package.json"), dir)?);
        return Ok(());
    }

    if lockfile_only {
        let lockfile = dir.join("package-lock.json");
        write_from_manifest(
            &dir.join("package.json"),
            &lockfile,
            &Registry::npm().with_detail(detail),
        )?;
        println!("wrote {}", lockfile.display());
        return Ok(());
    }

    let doc = read_manifest(dir)?;
    sync(dir, &doc, detail)
}
