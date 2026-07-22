# Changelog

All notable changes to this project are documented in this file.
The format follows [Keep a Changelog](https://keepachangelog.com); releases are cut from the `[Unreleased]` section by `gronke/rust-ci`'s `changelog` action.

## [Unreleased]

### Security

- download: the shared agent sets `https_only`, so the https scheme guard now covers every request in a redirect chain, not just the initial URL — a hostile or compromised endpoint can no longer steer a fetch to plain http by redirecting.
- cache: `clear_directory` unlinks a symlink at its target instead of following it — a dangling link previously survived the wipe and `create_dir_all` then created the link's target directory outside the tree, anchoring the subsequent extraction there.
- package_json: `validate_package_name` rejects a leading `/`, empty `/`-separated segments, and the exact name `.` — an absolute name made `Path::join` replace its base, so `package_dir(from, "/etc")` resolved outside any `node_modules`.
- install: `from_lockfile` warns once per distinct tarball host that is not `registry.npmjs.org` — a lockfile names each tarball's URL and the sha512 it is verified against, so on an untrusted lockfile the integrity check authenticates nothing and off-registry fetches must be visible.

### Added

- resolve: `package_dir_within` / `package_file_within` — the Node-style `node_modules` ascent bounded at a project directory, so a caller resolving on behalf of an untrusted tree can keep it from naming packages installed only above the project. The unbounded `package_dir` / `package_file` keep Node's semantics.
- install: `from_lockfile` materializes workspace-member and `file:` links as relative symlinks under `node_modules/` — an `npm ci` for workspaces, still no Node. A link target escaping the project is warned and skipped; Unix only, like the `.bin` shims.
- package_json: `set_field` and `remove_field` — the write-side of `npm pkg set` and `npm pkg delete` for plain top-level keys, so scaffold, `set_field` and `to_pretty` compose into assembling a publishable `package.json`.
- resolve: `package_dir_within` and `package_file_within` bound the `node_modules` ascent at a caller-given boundary, compared canonically at each level.

### Security

- download: redirects follow on https only — the scheme guard covered just the initial URL, and a later hop could downgrade to plain http.
- cache: `clear_directory` unlinks a symlink at its target first — a dangling link survived the wipe and anchored extraction outside the tree.
- package_json: absolute and dot package names are rejected — a leading `/` made `Path::join` replace its base and resolve outside any `node_modules`.
- install: one warning per distinct off-registry tarball host before any download — on an untrusted lockfile the sha512 check authenticates nothing; private registries keep working.

## [0.6.1] - 2026-07-06

### Added

- resolve: locate files inside an installed dependency under `node_modules/` — `package_dir` walks up to `node_modules/<name>`, and `package_file` maps `<name>/<subpath>` to a real file, honoring the package's exports.

### Security

- `package_file` resolves through the package's canonical directory and refuses a result outside it, so an in-package symlink cannot redirect a read past the module.

## [0.6.0] - 2026-07-05

### Changed

- **Breaking:** audit and install take package sources — a directory, a manifest or lockfile path, or `name=range` specs; the project directory moved to `--dir`.
- **Breaking:** audit walks `optionalDependencies` and reports what it cannot cover as omissions, failing closed (exit 2) unless `--allow-incomplete`.

### Added

- registry: search the npm registry.
- package_json: `remove_dependency`.
- project: new module — sync, upgrade (with dry-run), remove.
- audit: multi-source vulnerability checks against npm and OSV, reporting incomplete runs and keeping confirmed and unrated OSV findings.

### Fixed

- spec: accept whitespace between a comparator operator and its version.
- Lock entries count under their real package name (npm: aliases) and under workspace paths — in audit and SBOM alike.
- OSV querybatch requests page at the 1000-query cap.

## [0.5.3] - 2026-06-28

### Added

- CLI: `sbom --license-source` (auto | lockfile | package) recovers each package's license, e.g. from its `package.json`.
- CLI: global `--timeout` / `--no-timeout` set the per-request HTTP timeout.
- registry: abbreviated vs. full packument detail, plus a `License` trait.

### Changed

- Per-package license is skipped by default on install / add / upgrade (faster, abbreviated packument); `--no-skip-license` records it.
- The error type is now `Send + Sync`, with `Result` / `Error` aliases; the crate forbids unsafe.

### Fixed

- node-semver: a bare partial version (e.g. "1" or "1.2") is now read as an x-range.

### Security

- TLS is verified against the platform / native certificate store.
- Integrity checks compare decoded SHA-512 digest bytes (not base64 text), reject short digests, and require an exact match.
- Archive extraction hardened: safe directory creation and rejection of non-UTF-8 entry names.

## [0.5.2] - 2026-06-22

### Added

- sbom: license summary plus CycloneDX and SPDX output from a lockfile.
- lockfile: record per-package license, and a public install-free writer.
- CLI: install writes `package-lock.json` (`--lockfile-only` / `--no-lockfile`).

## [0.5.1] - 2026-06-22

### Fixed

- cache: reclaim a lock by file age, not the waiter's wait.
- spec: report unsupported dist-tags clearly instead of a semver error.
- install: tolerate a failing optional dependency (npm-ci-faithful).

### Security

- extract: refuse writing through a pre-existing leaf symlink.
- download: require https and set connect/global timeouts.

## [0.5.0] - 2026-06-09

### Added

- Pure-Rust npm CLI (`npm-utils` / `cargo npm-utils`): install, ci, add, init, upgrade, behind the opt-in `cli` feature.
- add/upgrade write a lockfileVersion-3 `package-lock.json` and edit `package.json`.
- Resolution handles npm OR-ranges (`||`) and space-separated comparators.

## [0.4.0] - 2026-06-08

### Added

- install: `from_lockfile` — an `npm ci` in Rust: install the exact tree a `package-lock.json` (v2/v3) pins, devDependencies and `node_modules/.bin` shims included, off-platform optional deps skipped.
- integrity: every downloaded tarball is sha512-verified — the registry's `dist.integrity` (node_modules path) and the lockfile-pinned hash (ci path).
- package_json: rolled-own npm schemas — adds `package-lock.json` parsing and the package-spec dependency grammar alongside the `package.json` reader.

### Security

- extract: hardened archive extraction and shared path-safety (path traversal, `.bin` symlinks).

## [0.3.0] - 2026-06-07

### Added

- install: resolve a `package.json`'s transitive dependency graph and extract the flat tree into `node_modules/` (CommonJS packages and all).
- registry: `resolve_tree` (graph walk, dedup, cycle-safe) and `version_req` (npm-faithful bare-version pinning).

## [0.2.0] - 2026-06-02

### Added

- `package.json` browser resolver for import maps.

## [0.1.0] - 2026-06-01

### Added

- First release — pure-Rust utilities for the npm registry and web assets, for vendoring browser/JS dependencies at build time without Node or npm.
- registry: resolve a version against a semver range; tarball URLs (incl. `@scope/pkg`); fetch packuments.
- download: HTTP fetch with one retry and a 100 MB cap; GitHub archive URLs.
- extract: tar.gz / zip with All / explicit-map / predicate selection and path-traversal protection (unsafe paths error, not skip).
- cache: content-hash markers, a cross-process build lock, and skip-if-unchanged directory helpers.
- package_json: read pinned dependency versions from `package.json`.

[Unreleased]: https://github.com/gronke/npm-utils/compare/v0.6.1...HEAD
[0.6.1]: https://github.com/gronke/npm-utils/compare/v0.6.0...v0.6.1
[0.6.0]: https://github.com/gronke/npm-utils/compare/v0.5.3...v0.6.0
[0.5.3]: https://github.com/gronke/npm-utils/compare/v0.5.2...v0.5.3
[0.5.2]: https://github.com/gronke/npm-utils/compare/v0.5.1...v0.5.2
[0.5.1]: https://github.com/gronke/npm-utils/compare/v0.5.0...v0.5.1
[0.5.0]: https://github.com/gronke/npm-utils/compare/v0.4.0...v0.5.0
[0.4.0]: https://github.com/gronke/npm-utils/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/gronke/npm-utils/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/gronke/npm-utils/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/gronke/npm-utils/releases/tag/v0.1.0