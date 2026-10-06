# Changelog

All notable changes to this project are documented in this file.
The format follows [Keep a Changelog](https://keepachangelog.com); releases are cut from the `[Unreleased]` section by `gronke/rust-ci`'s `changelog` action.

## [Unreleased]

### Added

- pack: a pure-Rust local package packer as `npm_utils::pack` and the `pack` verb: npm-packlist 11.3.0 file selection pinned by a recorded npm 12.1.0 fixture, pacote's tarball layout streamed through a hashing writer, npm 12's `--json` shape.
- pack: `.npmrc`, VCS metadata, `node_modules`, root lockfiles, a root `.npm-extension.*` and the patch files of `patchedDependencies` never ship, whatever the manifest says; hostile ignore rules and deep trees fail with an error naming the input.
- pack: lifecycle scripts, workspaces and package specs are out of scope; a manifest declaring bundled dependencies is refused.
- pack: an end-to-end job packs real packages, twelve from git at their release commit and twenty registry tarballs unpacked and packed again, against npm 12.1.0 in a sealed node container, and holds the pinned records to the live tools (`tests/pack_e2e.rs`, `ci/sealed-node`).
- minimatch: npm's glob matcher (minimatch 10.2.5) as `npm_utils::minimatch`, negation-free runs on the regex crate and `!()` groups evaluated as zero-width checks, so the backtracking classes of the JavaScript do not compile at all; pinned by a fixture recorded from the JavaScript; brace, step, globstar and extglob-nesting budgets error instead of truncating, hanging or overflowing the parser stack. `pack` matches its rules with it.
- minimatch: `Options::max_extglob_nodes` bounds the tree that sequential `!()` groups double, as `Error::Nodes`, where npm's algorithm grows without limit; the end check of a nested group asks its parent once, where the JavaScript doubles the work per level.
- pack: the rules are read strictly by default: minimatch's ten quirks, named in `npm_utils::minimatch::Quirk`, are off, so ambiguous and unclosed rule syntax fails the pack naming the rule, escapes hold everywhere, `[[:print:]]` and `[[:punct:]]` are corrected; `--npm-quirks` (`pack::Settings { quirks: true }`) reads them as npm does.

### Security

- extract: the inflated stream of an archive is capped at the write cap plus 256 MiB, extension-header bodies and skipped entries included, where tar read a GNU long-name or PAX body whole with no cap at all.
- extract: an entry name is validated as the archive wrote it, before any selection maps it, so `../x` and `/etc/passwd` are errors in every mode where the install path relativized them; zip names are read as written instead of cleaned, and a NUL or an interior `.` segment is refused in every contained path.
- path_safety: a write's parent is checked for containment before any directory is created beneath it, where a planted symlink to the outside had its target directories created on the way to the refusal.
- extract: a file is created exclusively; a regular file already at the destination is unlinked instead of written through (a hardlink's twin keeps its bytes), and a directory, FIFO, device or socket there is refused by name, where a FIFO blocked the extraction.
- install: a workspace link's climb back to the project is counted on the components the key is written with, where a key spelled `./node_modules/x` climbed one level too many and the link pointed outside the project.
- pack: the header size of a file comes from the open handle and the bytes are counted against it, so a file that shrinks or grows while it is packed fails the pack instead of misaligning the archive under digests that still verify.
- pack: a symlinked `.npmignore` or `.gitignore` is refused by name instead of read, in both modes, since a strict refusal quotes the rule at fault and a planted link would print a line of whatever it points at.

## [0.6.2] - 2026-07-22

### Security

- download: the shared agent sets `https_only`, so the https scheme guard now covers every request in a redirect chain, not just the initial URL — a hostile or compromised endpoint can no longer steer a fetch to plain http by redirecting.
- cache: `clear_directory` unlinks a symlink at its target instead of following it — a dangling link previously survived the wipe and `create_dir_all` then created the link's target directory outside the tree, anchoring the subsequent extraction there.
- package_json: `validate_package_name` rejects a leading `/`, empty `/`-separated segments, and the exact name `.` — an absolute name made `Path::join` replace its base, so `package_dir(from, "/etc")` resolved outside any `node_modules`.
- install: `from_lockfile` warns once per distinct tarball host that is not `registry.npmjs.org` — a lockfile names each tarball's URL and the sha512 it is verified against, so on an untrusted lockfile the integrity check authenticates nothing and off-registry fetches must be visible.

### Added

- resolve: `package_dir_within` / `package_file_within` — the Node-style `node_modules` ascent bounded at a project directory, so a caller resolving on behalf of an untrusted tree can keep it from naming packages installed only above the project. The unbounded `package_dir` / `package_file` keep Node's semantics.
- install: `from_lockfile` materializes workspace-member and `file:` links as relative symlinks under `node_modules/` — an `npm ci` for workspaces, still no Node. A link target escaping the project is warned and skipped; Unix only, like the `.bin` shims.
- package_json: `set_field` and `remove_field` — the write-side of `npm pkg set` and `npm pkg delete` for plain top-level keys, so scaffold, `set_field` and `to_pretty` compose into assembling a publishable `package.json`.

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

[Unreleased]: https://github.com/gronke/npm-utils/compare/v0.6.2...HEAD
[0.6.2]: https://github.com/gronke/npm-utils/compare/v0.6.1...v0.6.2
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
