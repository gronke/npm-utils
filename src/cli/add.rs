//! `add` — resolve package(s), record them in `package.json`, write `package-lock.json`, install
//! (= `npm install <pkg>`).

use std::path::Path;

use super::common;
use super::Res;
use crate::registry::PackumentDetail;

/// Record each package (latest → `^x.y.z` when no range given) in `package.json` (scaffolding one
/// if absent), then `sync` the lock + `node_modules/`. The spec parsing and manifest work live in
/// [`common::add_specs`], shared with `install <SOURCES>`.
pub(super) fn run(packages: &[String], dir: &Path, detail: PackumentDetail) -> Res {
    let doc = common::add_specs(packages, dir, "add")?;
    common::sync(dir, &doc, detail)
}
