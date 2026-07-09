//! `from_lockfile()` — install the exact tree pinned by a `package-lock.json` (pure-Rust
//! `npm ci`), plus `node_modules/.bin/` shims.

use std::path::Path;

use crate::package_json::lock::{LockedPackage, Lockfile};
use semver::Version;

use crate::path_safety::safe_join;
use crate::registry::Resolved;

/// Install the exact dependency tree pinned by a `package-lock.json` into `<dest>/node_modules/`
/// — a pure-Rust, `npm ci`-faithful install.
///
/// The lockfile (v2/v3) is parsed by [`crate::package_json::lock`]; this installs every registry-tarball
/// entry whose `os`/`cpu` match the host (skipping off-platform optional deps like
/// darwin-only `fsevents` on Linux), verifies each `sha512` integrity, extracts it to the path the
/// lockfile names, and creates `node_modules/.bin/` symlinks — so installed CLIs (`tsc`,
/// `playwright`, …) run as under npm, with only the Node runtime, no `npm`. Skip-if-unchanged on
/// the lockfile's content hash. A *failed* on-platform optional dependency (a download or verify
/// error) is warned and skipped rather than aborting, as `npm ci` does. Returns the installed set
/// — any skipped optional omitted — sorted by install path.
///
/// **Workspaces.** A `link: true` entry (an npm workspace member, or a `file:`
/// dependency) is materialized as a relative symlink `node_modules/<name> →
/// <target>`, matching npm's hoisted-workspace layout — so a `ci` at the
/// workspace root wires every member into `node_modules/` without Node. Only a
/// link whose target stays **within** `dest` is created; one that escapes (a
/// `file:` dep pointing outside the project) is warned and skipped, matching
/// this crate's traversal posture. Links are not part of the returned set (they
/// carry no tarball/integrity), and, like the `.bin` shims, are Unix-only.
pub fn from_lockfile(
    package_lock: &Path,
    dest: &Path,
) -> Result<Vec<Resolved>, Box<dyn std::error::Error + Send + Sync>> {
    from_lockfile_observed(package_lock, dest, |_| {})
}

/// [`from_lockfile`] with a progress observer: one [`InstallEvent::Fetch`] immediately before
/// each package's download/verify/extract — the CLI's `[install]` task ticks from it. A
/// skip-if-unchanged cache hit runs no populate step and emits no events.
pub(crate) fn from_lockfile_observed(
    package_lock: &Path,
    dest: &Path,
    mut on_event: impl FnMut(super::InstallEvent<'_>),
) -> Result<Vec<Resolved>, Box<dyn std::error::Error + Send + Sync>> {
    let lockfile = Lockfile::parse(&std::fs::read_to_string(package_lock)?)?;
    // What this host installs: platform-matching, non-link entries that are registry tarballs.
    let installable: Vec<&LockedPackage> = lockfile
        .installable(std::env::consts::OS, std::env::consts::ARCH)
        .into_iter()
        .filter(|p| p.is_registry_tarball())
        .collect();
    // Workspace members / `file:` deps: link entries symlinked into node_modules
    // (materialized after the tarball tree, since a link may sit under a scope
    // dir that a registry install just created — or need its own).
    let links: Vec<&LockedPackage> = lockfile
        .packages
        .iter()
        .filter(|p| p.link && p.key.starts_with("node_modules/"))
        .collect();
    // The lockfile fully determines the tree, so its content hash is the cache key.
    let want = crate::cache::file_hash(package_lock)?;

    let mut installed_idx: Vec<usize> = Vec::new();
    let mut populated = false;
    super::run_install(dest, &want, |node_modules| {
        populated = true;
        for (i, pkg) in installable.iter().enumerate() {
            on_event(super::InstallEvent::Fetch {
                index: i + 1,
                total: installable.len(),
                name: &pkg.name,
                version: &pkg.version,
            });
            // The key (`node_modules/…`) is validated into a contained path under `dest`.
            let dir = safe_join(dest, &pkg.key)?;
            let url = pkg.resolved.as_deref().unwrap_or_default();
            match super::fetch_verify_extract(&pkg.name, url, pkg.integrity.as_deref(), &dir) {
                Ok(()) => installed_idx.push(i),
                // npm ci treats a failed *optional* dependency as non-fatal: warn and skip it
                // rather than aborting the whole install.
                Err(e) if pkg.optional || pkg.dev_optional => {
                    crate::warn::warn(&format!(
                        "optional dependency `{}` failed to install ({e}); skipping",
                        pkg.name
                    ));
                }
                Err(e) => return Err(e),
            }
        }
        // Link bins only for packages that actually landed.
        let installed_pkgs: Vec<&LockedPackage> =
            installed_idx.iter().map(|&i| installable[i]).collect();
        link_bins(node_modules, &installed_pkgs)?;
        // Wire workspace members / file: links into the tree.
        link_locals(dest, &links)?;
        Ok(())
    })?;

    // On a populate run, report exactly what installed (a failed optional is omitted). On a
    // skip-if-unchanged cache hit, the prior run's full set is already present on disk.
    let result_set: Vec<&LockedPackage> = if populated {
        installed_idx.iter().map(|&i| installable[i]).collect()
    } else {
        installable.clone()
    };

    result_set
        .iter()
        .map(|pkg| {
            let version = Version::parse(&pkg.version).map_err(|e| {
                format!(
                    "package `{}`: invalid version {:?}: {e}",
                    pkg.name, pkg.version
                )
            })?;
            Ok(Resolved {
                name: pkg.name.clone(),
                version,
                tarball_url: pkg.resolved.clone().unwrap_or_default(),
                integrity: pkg.integrity.clone(),
                // Install-side report only; the lockfile model carries no license here.
                license: None,
            })
        })
        .collect()
}

/// Create `node_modules/.bin/<name>` symlinks for every package `bin`, so the installed CLIs run
/// as under npm. The shims are *relative* (the tree stays relocatable) and their targets are made
/// executable. On a name collision the first package (by sorted install path) wins. Unix only —
/// `.bin` shims elsewhere are out of scope.
///
/// Path-traversal-safe against a crafted lockfile: the link *name* must be a single filename (no
/// separator, `.` or `..`), and the link *target* is gated through [`safe_join`] — the same
/// validated relative path feeds both the chmod and the symlink, so neither can escape
/// `node_modules/`.
#[cfg(unix)]
fn link_bins(
    node_modules: &Path,
    plan: &[&LockedPackage],
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    use std::collections::BTreeSet;
    use std::os::unix::fs::{symlink, PermissionsExt};

    let bin_dir = node_modules.join(".bin");
    let mut linked: BTreeSet<String> = BTreeSet::new();
    for pkg in plan {
        let Some(install_rel) = pkg.key.strip_prefix("node_modules/") else {
            continue;
        };
        for (bin_name, bin_path) in &pkg.bin {
            // The link itself is a single filename directly under .bin/ — never a path, so it
            // can't escape .bin/. Reject '/', '.'/'..' and empty (on Unix '/' is the only
            // separator). NB: `safe_join` is wrong here — it permits a bare `.`, which would
            // resolve the link to `.bin` itself.
            if bin_name.is_empty() || bin_name.contains('/') || bin_name == "." || bin_name == ".."
            {
                continue;
            }
            if !linked.insert(bin_name.clone()) {
                continue; // collision: the first (sorted) package keeps the name
            }
            // The target relative to node_modules. `safe_join` is the traversal gate: it rejects
            // any `..`/absolute component in the (attacker-controlled) key or bin path, erroring
            // before any symlink is written. The *same* validated `rel` feeds both the chmod and
            // the symlink, so the two can never diverge.
            let rel = format!("{}/{}", install_rel, bin_path.trim_start_matches("./"));
            let target = safe_join(node_modules, &rel)?;
            std::fs::create_dir_all(&bin_dir)?;
            // chmod +x the real entry (npm does this on extract). metadata/set_permissions follow
            // symlinks, but extraction never creates symlinks inside node_modules, so `target` is
            // a regular file (or absent) — not an attacker-planted link out of the tree.
            if let Ok(meta) = std::fs::metadata(&target) {
                let mut perm = meta.permissions();
                perm.set_mode(perm.mode() | 0o111);
                let _ = std::fs::set_permissions(&target, perm);
            }
            // `../rel` from .bin/ resolves to node_modules/rel === the validated `target`.
            let link = bin_dir.join(bin_name);
            let _ = std::fs::remove_file(&link); // idempotent
            symlink(format!("../{rel}"), &link)?;
        }
    }
    Ok(())
}

#[cfg(not(unix))]
fn link_bins(
    _node_modules: &Path,
    _plan: &[&LockedPackage],
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    Ok(()) // `.bin` shims are Unix symlinks; skipped on other platforms
}

/// Materialize `link: true` entries — npm **workspace members** and `file:`
/// dependencies — as relative symlinks `node_modules/<name> → <target>`, so a
/// hoisted-workspace `ci` matches npm's layout. The target comes from the
/// entry's `resolved` (a repo-relative path, or a `file:`-prefixed one); the
/// symlink value is relative to the link's own parent (climbing back to `dest`
/// then descending into the target), so the tree stays relocatable — matching
/// the `.bin` shims.
///
/// Path-traversal-safe: the link *location* (`key`) and its *target* both pass
/// through [`safe_join`], which rejects `..`/absolute components. A link whose
/// target escapes `dest` (a `file:` dep pointing outside the project) is warned
/// and skipped rather than symlinked out of the tree — more conservative than
/// npm, matching this crate's posture. Unix only.
#[cfg(unix)]
fn link_locals(
    dest: &Path,
    links: &[&LockedPackage],
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    use std::os::unix::fs::symlink;

    for pkg in links {
        // The link target: `resolved` names it, `file:`-prefixed or bare. A
        // link with no target is malformed — skip it rather than guess.
        let Some(resolved) = pkg.resolved.as_deref() else {
            crate::warn::warn(&format!("link `{}` has no target; skipping", pkg.key));
            continue;
        };
        let target = resolved
            .strip_prefix("file:")
            .unwrap_or(resolved)
            .trim_start_matches("./");

        // Containment gates (both attacker-controlled from the lockfile): the
        // link location must be a contained `node_modules/…` path, and the
        // target must stay within `dest`. `safe_join` rejects `..`/absolute;
        // an escaping target (e.g. a `file:` dep outside the project) is
        // skipped, never symlinked out of the tree.
        let link_abs = safe_join(dest, &pkg.key)?;
        if safe_join(dest, target).is_err() {
            crate::warn::warn(&format!(
                "workspace link `{}` targets `{target}` outside the project; skipping",
                pkg.key
            ));
            continue;
        }

        // Relative, relocatable link value: climb from the link's parent back
        // to `dest` (one `..` per key segment above the leaf), then descend
        // into the target. e.g. `node_modules/@s/x → modules/x` ⇒ `../../modules/x`.
        let depth = pkg.key.split('/').filter(|s| !s.is_empty()).count() - 1;
        let link_value = format!("{}{target}", "../".repeat(depth));

        if let Some(parent) = link_abs.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let _ = std::fs::remove_file(&link_abs); // idempotent
        symlink(&link_value, &link_abs)?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn link_locals(
    _dest: &Path,
    _links: &[&LockedPackage],
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    Ok(()) // workspace/file: links are Unix symlinks; skipped on other platforms
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    /// Build a `LockedPackage` for the `.bin` test — only `key`, `name`, and `bin` matter here.
    fn locked(key: &str, bin: &[(&str, &str)]) -> LockedPackage {
        LockedPackage {
            name: key
                .rsplit("node_modules/")
                .next()
                .unwrap_or(key)
                .to_string(),
            key: key.to_string(),
            version: "1.0.0".into(),
            resolved: None,
            integrity: None,
            license: None,
            dev: false,
            optional: false,
            dev_optional: false,
            link: false,
            os: Vec::new(),
            cpu: Vec::new(),
            bin: bin
                .iter()
                .map(|(n, p)| (n.to_string(), p.to_string()))
                .collect(),
        }
    }

    /// A `link: true` entry with a `resolved` target — for the workspace tests.
    fn locked_link(key: &str, resolved: &str) -> LockedPackage {
        LockedPackage {
            name: key
                .rsplit("node_modules/")
                .next()
                .unwrap_or(key)
                .to_string(),
            key: key.to_string(),
            version: String::new(),
            resolved: Some(resolved.to_string()),
            integrity: None,
            license: None,
            dev: false,
            optional: false,
            dev_optional: false,
            link: true,
            os: Vec::new(),
            cpu: Vec::new(),
            bin: Vec::new(),
        }
    }

    #[test]
    #[cfg(unix)]
    fn link_locals_symlinks_members_relative_to_dest() {
        let tmp = tempdir().unwrap();
        let dest = tmp.path();
        // A scoped member (bare repo-relative target) and an unscoped one with a
        // `file:` prefix — both inside the project.
        let pkgs = [
            locked_link("node_modules/@schuhkarton/assets-web", "modules/assets/web"),
            locked_link("node_modules/plain", "file:packages/plain"),
        ];
        let links: Vec<&LockedPackage> = pkgs.iter().collect();
        link_locals(dest, &links).unwrap();

        // Scoped: two segments above dest (`node_modules/@schuhkarton/`) ⇒ `../../`.
        assert_eq!(
            std::fs::read_link(dest.join("node_modules/@schuhkarton/assets-web")).unwrap(),
            Path::new("../../modules/assets/web")
        );
        // Unscoped: one segment above dest ⇒ `../`, and the `file:` prefix is stripped.
        assert_eq!(
            std::fs::read_link(dest.join("node_modules/plain")).unwrap(),
            Path::new("../packages/plain")
        );
    }

    #[test]
    #[cfg(unix)]
    fn link_locals_skips_a_target_escaping_the_project() {
        // A `file:` dep pointing outside `dest` must not become a symlink out of
        // the tree — it is skipped, and nothing is created.
        let tmp = tempdir().unwrap();
        let dest = tmp.path();
        let pkgs = [locked_link("node_modules/outside", "file:../../../etc")];
        let links: Vec<&LockedPackage> = pkgs.iter().collect();
        link_locals(dest, &links).unwrap();
        assert!(
            !dest.join("node_modules/outside").exists()
                && std::fs::symlink_metadata(dest.join("node_modules/outside")).is_err(),
            "an escaping link target creates nothing"
        );
    }

    #[test]
    #[cfg(unix)]
    fn from_lockfile_materializes_a_workspace_link_offline() {
        // A lockfile with only a workspace member (no tarballs) installs offline:
        // the member is symlinked into node_modules, relative and relocatable.
        let tmp = tempdir().unwrap();
        let dest = tmp.path();
        std::fs::create_dir_all(dest.join("packages/member")).unwrap();
        let lock = dest.join("package-lock.json");
        std::fs::write(
            &lock,
            r#"{ "name": "root", "version": "1.0.0", "lockfileVersion": 3, "packages": {
                "": { "name": "root", "version": "1.0.0" },
                "packages/member": { "name": "@scope/member", "version": "1.0.0" },
                "node_modules/@scope/member": { "resolved": "packages/member", "link": true }
            } }"#,
        )
        .unwrap();

        let installed = from_lockfile(&lock, dest).unwrap();
        assert!(
            installed.is_empty(),
            "a link carries no tarball, so it is not in the installed set"
        );
        assert_eq!(
            std::fs::read_link(dest.join("node_modules/@scope/member")).unwrap(),
            Path::new("../../packages/member"),
            "the workspace member is symlinked relative to dest"
        );
    }

    #[test]
    fn from_lockfile_observed_emits_fetch_before_each_download() {
        // One required entry whose tarball URL points at a refused local port: the Fetch event
        // is recorded and THEN the download fails — emission precedes the work. An empty lock
        // emits nothing and succeeds. Both offline.
        let tmp = tempdir().unwrap();
        let lock_path = tmp.path().join("package-lock.json");
        std::fs::write(
            &lock_path,
            r#"{ "name": "demo", "version": "1.0.0", "lockfileVersion": 3, "packages": {
                "": { "name": "demo", "version": "1.0.0" },
                "node_modules/x": { "version": "1.2.3", "resolved": "https://localhost:1/x.tgz", "integrity": "sha512-AAA" }
            } }"#,
        )
        .unwrap();
        let mut events: Vec<String> = Vec::new();
        let result = from_lockfile_observed(&lock_path, tmp.path(), |event| {
            let super::super::InstallEvent::Fetch {
                index,
                total,
                name,
                version,
            } = event;
            events.push(format!("{index}/{total} {name}@{version}"));
        });
        assert!(result.is_err(), "the tarball fetch cannot succeed offline");
        assert_eq!(events, ["1/1 x@1.2.3"]);

        // The empty lock: no installable entries → no events, and the install succeeds.
        let empty = tempdir().unwrap();
        let empty_lock = empty.path().join("package-lock.json");
        std::fs::write(
            &empty_lock,
            r#"{ "name": "demo", "version": "1.0.0", "lockfileVersion": 3, "packages": {
                "": { "name": "demo", "version": "1.0.0" }
            } }"#,
        )
        .unwrap();
        let mut count = 0;
        let installed = from_lockfile_observed(&empty_lock, empty.path(), |_| count += 1).unwrap();
        assert_eq!(count, 0);
        assert!(installed.is_empty());
    }

    #[test]
    #[cfg(unix)]
    fn link_bins_creates_relative_exec_symlinks_first_wins() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempdir().unwrap();
        let nm = tmp.path().join("node_modules");
        for rel in [
            "@playwright/test/cli.js",
            "playwright/cli.js",
            "typescript/bin/tsc",
        ] {
            let p = nm.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, b"#!/usr/bin/env node\n").unwrap();
        }
        // Sorted by install path (as Lockfile::installable returns): @playwright/test < playwright.
        let pkgs = [
            locked("node_modules/@playwright/test", &[("playwright", "cli.js")]),
            locked("node_modules/playwright", &[("playwright", "cli.js")]),
            locked("node_modules/typescript", &[("tsc", "bin/tsc")]),
        ];
        let plan: Vec<&LockedPackage> = pkgs.iter().collect();
        link_bins(&nm, &plan).unwrap();

        // Relative, relocatable shims.
        assert_eq!(
            std::fs::read_link(nm.join(".bin/tsc")).unwrap(),
            Path::new("../typescript/bin/tsc")
        );
        // On the `playwright` collision the first (sorted) package keeps the name.
        assert_eq!(
            std::fs::read_link(nm.join(".bin/playwright")).unwrap(),
            Path::new("../@playwright/test/cli.js")
        );
        // The real entry file was made executable.
        let mode = std::fs::metadata(nm.join("typescript/bin/tsc"))
            .unwrap()
            .permissions()
            .mode();
        assert!(mode & 0o111 != 0, "bin target should be executable");
    }

    #[test]
    #[cfg(unix)]
    fn link_bins_rejects_a_traversing_bin_target() {
        // A crafted lockfile bin path that climbs out of node_modules must never become a symlink
        // pointing outside the tree: safe_join is the gate, so the install errors instead.
        let tmp = tempdir().unwrap();
        let nm = tmp.path().join("node_modules");
        let pkgs = [locked(
            "node_modules/evil",
            &[("evil", "../../../../../../tmp/pwned")],
        )];
        let plan: Vec<&LockedPackage> = pkgs.iter().collect();
        assert!(
            link_bins(&nm, &plan).is_err(),
            "a traversing bin target is rejected"
        );
        assert!(
            !nm.join(".bin/evil").exists(),
            "no symlink is created for a traversing target"
        );
    }

    #[test]
    #[cfg(unix)]
    fn link_bins_skips_bin_names_that_are_paths() {
        // A bin *name* is a single filename under .bin/; a name carrying a separator or `..` is
        // skipped (never a traversing link), while a valid sibling bin still links.
        let tmp = tempdir().unwrap();
        let nm = tmp.path().join("node_modules");
        std::fs::create_dir_all(nm.join("p")).unwrap();
        std::fs::write(nm.join("p/cli.js"), b"#!/usr/bin/env node\n").unwrap();
        let pkgs = [locked(
            "node_modules/p",
            &[("../escape", "cli.js"), ("ok", "cli.js")],
        )];
        let plan: Vec<&LockedPackage> = pkgs.iter().collect();
        link_bins(&nm, &plan).unwrap();
        assert!(nm.join(".bin/ok").exists(), "the valid bin is linked");
        assert!(
            !tmp.path().join("escape").exists() && !nm.join("escape").exists(),
            "a path-like bin name creates nothing outside .bin/"
        );
    }

    #[test]
    #[ignore = "network: hits the npm registry"]
    #[cfg(not(target_os = "macos"))]
    fn installs_a_locked_tree_and_skips_offplatform_optional() {
        // `ms@2.1.3` is a frozen package with a known sha512 (so integrity is really checked).
        // `darwin-only` carries a bogus URL that MUST NOT be fetched on a non-darwin host —
        // proving the platform skip end to end (a fetch would error on the invalid URL).
        let tmp = tempdir().unwrap();
        let lock = tmp.path().join("package-lock.json");
        std::fs::write(
            &lock,
            r#"{
              "name": "fixture",
              "lockfileVersion": 3,
              "packages": {
                "": { "name": "fixture", "dependencies": { "ms": "2.1.3" } },
                "node_modules/ms": {
                  "version": "2.1.3",
                  "resolved": "https://registry.npmjs.org/ms/-/ms-2.1.3.tgz",
                  "integrity": "sha512-6FlzubTLZG3J2a/NVCAleEhjzq5oxgHyaCU9yYXvcLsvoVaHJq/s5xXI6/XXP6tz7R9xAOtHnSO/tXtF3WRTlA=="
                },
                "node_modules/darwin-only": {
                  "version": "1.0.0",
                  "resolved": "https://example.invalid/never-fetched.tgz",
                  "integrity": "sha512-AAAA",
                  "optional": true,
                  "os": ["darwin"]
                }
              }
            }"#,
        )
        .unwrap();

        let installed = from_lockfile(&lock, tmp.path()).unwrap();
        let names: Vec<&str> = installed.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(
            names,
            ["ms"],
            "the darwin-only optional dep is skipped on this host"
        );

        let nm = tmp.path().join("node_modules");
        assert!(
            nm.join("ms/package.json").is_file(),
            "ms downloaded, integrity-verified and extracted"
        );
        assert!(
            !nm.join("darwin-only").exists(),
            "off-platform dep not installed"
        );

        // Idempotent: the lockfile-hash marker short-circuits the second call.
        let again = from_lockfile(&lock, tmp.path()).unwrap();
        assert_eq!(again.len(), 1);
    }

    #[test]
    #[ignore = "network: fetches ms from the npm registry"]
    fn tolerates_a_failing_onplatform_optional() {
        // `ms@2.1.3` is a frozen package with a known sha512. `flaky-optional` is `optional` with
        // NO os/cpu constraint (so it is on-platform here) and a bogus URL that can't be fetched —
        // `npm ci` tolerates a failed optional, so the install must succeed with just `ms`.
        let tmp = tempdir().unwrap();
        let lock = tmp.path().join("package-lock.json");
        std::fs::write(
            &lock,
            r#"{
              "name": "fixture",
              "lockfileVersion": 3,
              "packages": {
                "": { "name": "fixture", "dependencies": { "ms": "2.1.3" } },
                "node_modules/ms": {
                  "version": "2.1.3",
                  "resolved": "https://registry.npmjs.org/ms/-/ms-2.1.3.tgz",
                  "integrity": "sha512-6FlzubTLZG3J2a/NVCAleEhjzq5oxgHyaCU9yYXvcLsvoVaHJq/s5xXI6/XXP6tz7R9xAOtHnSO/tXtF3WRTlA=="
                },
                "node_modules/flaky-optional": {
                  "version": "1.0.0",
                  "resolved": "https://example.invalid/never-resolves.tgz",
                  "integrity": "sha512-AAAA",
                  "optional": true
                }
              }
            }"#,
        )
        .unwrap();

        let installed = from_lockfile(&lock, tmp.path()).unwrap();
        let names: Vec<&str> = installed.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(
            names,
            ["ms"],
            "a failing optional dep is skipped, not fatal"
        );

        let nm = tmp.path().join("node_modules");
        assert!(nm.join("ms/package.json").is_file());
        assert!(!nm.join("flaky-optional").exists());
    }
}
