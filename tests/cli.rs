#![cfg(feature = "cli")]
//! End-to-end tests of the `npm-utils` CLI, driving the real binary the way a user does. The
//! library unit-tests the pure pieces (manifest/lock writers, arg parsing); the roundtrip tests
//! here exercise the whole `init → add → ci → upgrade` flow against the live registry, so they
//! are network-gated:
//!
//! ```text
//! cargo test --features cli --test cli -- --include-ignored
//! ```
//!
//! `ms` is a tiny, dependency-free, long-frozen package — a stable target whose tarball carries a
//! known sha512, so integrity is genuinely verified end to end. The progress checks at the bottom
//! run offline (empty lock / zero-dependency manifest — nothing to fetch).

use std::process::Command;

/// The CLI binary. Cargo sets `CARGO_BIN_EXE_npm-utils` because the bin's `required-features`
/// (`cli`) are active for this test build.
fn npm_utils() -> Command {
    Command::new(env!("CARGO_BIN_EXE_npm-utils"))
}

fn run(cmd: &mut Command, what: &str) -> String {
    let out = cmd.output().unwrap_or_else(|e| panic!("spawn {what}: {e}"));
    assert!(
        out.status.success(),
        "{what} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
#[ignore = "network: fetches ms from the npm registry"]
fn init_add_ci_upgrade_roundtrip() {
    let project = tempfile::tempdir().unwrap();
    let dir = project.path().to_str().unwrap();

    // init → a package.json is scaffolded.
    run(
        npm_utils().args(["init", "--dir", dir, "--name", "demo"]),
        "init",
    );
    assert!(project.path().join("package.json").is_file());

    // add ms@^2 → manifest records the range, a v3 lock pins the resolved version with a real
    // sha512, and node_modules/ is populated.
    let stdout = run(npm_utils().args(["add", "ms@^2", "--dir", dir]), "add");
    assert!(
        stdout.contains("installed"),
        "add reports an install: {stdout}"
    );

    let manifest = std::fs::read_to_string(project.path().join("package.json")).unwrap();
    assert!(
        manifest.contains("\"ms\""),
        "manifest records ms:\n{manifest}"
    );

    let lock = std::fs::read_to_string(project.path().join("package-lock.json")).unwrap();
    assert!(lock.contains("\"lockfileVersion\": 3"), "v3 lock:\n{lock}");
    assert!(lock.contains("node_modules/ms"), "lock pins ms");
    assert!(lock.contains("sha512-"), "lock carries integrity");
    assert!(
        project
            .path()
            .join("node_modules/ms/package.json")
            .is_file(),
        "ms downloaded, integrity-verified, extracted"
    );

    // ci in a FRESH dir from that exact lock reproduces the tree (the lock we wrote is consumable
    // by the npm-ci path).
    let fresh = tempfile::tempdir().unwrap();
    std::fs::copy(
        project.path().join("package-lock.json"),
        fresh.path().join("package-lock.json"),
    )
    .unwrap();
    run(
        npm_utils().args(["ci", fresh.path().to_str().unwrap()]),
        "ci",
    );
    assert!(
        fresh.path().join("node_modules/ms/package.json").is_file(),
        "ci reproduced ms from the generated lock"
    );

    // upgrade re-resolves within `^2` and refreshes the lock/tree without error (it may bump the
    // recorded floor to the latest 2.x — both outcomes are fine; we assert it stays valid).
    run(npm_utils().args(["upgrade", "--dir", dir]), "upgrade");
    let manifest = std::fs::read_to_string(project.path().join("package.json")).unwrap();
    assert!(
        manifest.contains("\"ms\""),
        "ms still present after upgrade"
    );
}

#[test]
#[ignore = "network: fetches ms from the npm registry"]
fn install_with_spec_sources_records_and_installs() {
    // `install <sources>` is npm-faithful `npm install <pkg>`: the spec is recorded in
    // package.json, the v3 lock pins it, node_modules/ is populated — and a bare re-`install` of
    // the same project stays clean.
    let project = tempfile::tempdir().unwrap();
    let dir = project.path().to_str().unwrap();

    run(
        npm_utils().args(["init", "--dir", dir, "--name", "demo"]),
        "init",
    );
    run(
        npm_utils().args(["install", "ms=^2", "--dir", dir]),
        "install ms=^2",
    );

    let manifest = std::fs::read_to_string(project.path().join("package.json")).unwrap();
    assert!(
        manifest.contains("\"ms\""),
        "manifest records ms:\n{manifest}"
    );
    let lock = std::fs::read_to_string(project.path().join("package-lock.json")).unwrap();
    assert!(lock.contains("\"lockfileVersion\": 3"), "v3 lock:\n{lock}");
    assert!(lock.contains("node_modules/ms"), "lock pins ms");
    assert!(lock.contains("sha512-"), "lock carries integrity");
    assert!(
        project
            .path()
            .join("node_modules/ms/package.json")
            .is_file(),
        "ms downloaded, integrity-verified, extracted"
    );

    // A bare install (no sources) keeps meaning "install this project".
    run(npm_utils().args(["install", "--dir", dir]), "bare install");
}

#[test]
#[ignore = "network: fetches debug + ms from the npm registry"]
fn remove_keeps_a_transitively_required_package() {
    // Regression for the release review: removing a direct dependency that is also required
    // transitively must not delete it from node_modules. `debug` depends on `ms`, so after
    // `remove ms` the package must remain (debug still needs it) with its lockfile entry.
    let project = tempfile::tempdir().unwrap();
    let dir = project.path().to_str().unwrap();

    run(
        npm_utils().args(["init", "--dir", dir, "--name", "demo"]),
        "init",
    );
    // Add both as *direct* dependencies; debug@4 also pulls ms transitively.
    run(
        npm_utils().args(["add", "debug@^4", "ms@^2", "--dir", dir]),
        "add",
    );
    assert!(
        project
            .path()
            .join("node_modules/ms/package.json")
            .is_file(),
        "ms installed after add"
    );

    // Drop the direct `ms` dependency. debug still requires ms, so it must stay installed.
    run(npm_utils().args(["remove", "ms", "--dir", dir]), "remove");

    let manifest = std::fs::read_to_string(project.path().join("package.json")).unwrap();
    assert!(
        !manifest.contains("\"ms\""),
        "ms dropped from direct dependencies:\n{manifest}"
    );
    assert!(manifest.contains("\"debug\""), "debug remains a direct dep");
    assert!(
        project
            .path()
            .join("node_modules/ms/package.json")
            .is_file(),
        "ms stays installed because debug requires it transitively"
    );
}

/// `ci` renders an `[install]` task even for an empty lock — begin line, then a finish with the
/// count and elapsed seconds. Offline: no installable entries means nothing is fetched.
#[test]
fn ci_of_an_empty_lock_shows_an_install_task() {
    let project = tempfile::tempdir().unwrap();
    std::fs::write(
        project.path().join("package-lock.json"),
        r#"{ "name": "demo", "version": "1.0.0", "lockfileVersion": 3, "packages": {
            "": { "name": "demo", "version": "1.0.0" }
        } }"#,
    )
    .unwrap();
    let out = npm_utils()
        .args(["ci", project.path().to_str().unwrap()])
        .output()
        .expect("spawn npm-utils ci");
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("[install] installing packages"), "{stderr}");
    assert!(stderr.contains("0 packages ("), "{stderr}");
    assert!(String::from_utf8_lossy(&out.stdout).contains("installed 0 package(s)"));
}

/// `install --lockfile-only` renders a `[resolve]` task naming the registry host. Offline: a
/// zero-dependency manifest resolves to an empty tree without any fetch.
#[test]
fn lockfile_only_install_shows_a_resolve_task_offline() {
    let project = tempfile::tempdir().unwrap();
    std::fs::write(
        project.path().join("package.json"),
        r#"{ "name": "demo", "version": "1.0.0" }"#,
    )
    .unwrap();
    let out = npm_utils()
        .args([
            "install",
            "--lockfile-only",
            "--dir",
            project.path().to_str().unwrap(),
        ])
        .output()
        .expect("spawn npm-utils install");
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("[resolve] resolving dependency tree from registry.npmjs.org"),
        "{stderr}"
    );
    assert!(stderr.contains("0 packages ("), "{stderr}");
    assert!(String::from_utf8_lossy(&out.stdout).contains("wrote "));
    assert!(project.path().join("package-lock.json").is_file());
}

/// A package directory for the `pack` tests: an allowlist, a silenced `.npmignore`, the files
/// npm always ships and never ships, a `bin`, and a nested directory.
fn pack_fixture(root: &std::path::Path) {
    let write = |path: &str, content: &str| {
        let full = root.join(path);
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        std::fs::write(full, content).unwrap();
    };
    write(
        "package.json",
        r#"{"name":"@acme/demo","version":"1.2.3","files":["dist","LICENSE-MIT"],
            "main":"dist/index.js","bin":{"demo":"bin/demo.js"}}"#,
    );
    write("dist/index.js", "export const answer = 42;\n");
    write(
        "dist/nested/types.d.ts",
        "export declare const answer: number;\n",
    );
    write("dist/.DS_Store", "");
    write("src/index.ts", "export const answer = 42;\n");
    write("LICENSE-MIT", "MIT\n");
    write("README.md", "# demo\n");
    write("CHANGELOG.md", "nothing\n");
    write(".npmignore", "dist\n");
    write("node_modules/dep/index.js", "");
    write("package-lock.json", "{}");
    write(".git/HEAD", "ref: refs/heads/main\n");
    write("bin/demo.js", "#!/usr/bin/env node\n");
}

#[test]
fn pack_dry_run_reports_the_files_npm_would_ship() {
    let project = tempfile::tempdir().unwrap();
    pack_fixture(project.path());
    let dir = project.path().to_str().unwrap();
    // The working directory is the package, so a dry run that wrote its tarball to `.` would
    // land where the assertion below looks.
    let stdout = run(
        npm_utils()
            .current_dir(project.path())
            .args(["pack", dir, "--dry-run", "--json"]),
        "pack --dry-run --json",
    );
    let report: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    // npm 12's shape: one object keyed by the package name.
    let report = &report["@acme/demo"];
    assert_eq!(report["id"], "@acme/demo@1.2.3");
    assert_eq!(report["filename"], "acme-demo-1.2.3.tgz");
    let paths: Vec<&str> = report["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["path"].as_str().unwrap())
        .collect();
    assert_eq!(
        paths,
        [
            "LICENSE-MIT",
            "README.md",
            "bin/demo.js",
            "dist/index.js",
            "dist/nested/types.d.ts",
            "package.json",
        ]
    );
    assert_eq!(report["entryCount"], 6);
    assert!(report["integrity"].as_str().unwrap().starts_with("sha512-"));
    assert_eq!(report["shasum"].as_str().unwrap().len(), 40);
    assert!(
        std::fs::read_dir(project.path()).unwrap().all(|e| !e
            .unwrap()
            .file_name()
            .to_string_lossy()
            .ends_with(".tgz")),
        "a dry run writes nothing"
    );
}

#[test]
fn pack_writes_the_tarball_where_asked() {
    let project = tempfile::tempdir().unwrap();
    pack_fixture(project.path());
    let out = tempfile::tempdir().unwrap();
    let stdout = run(
        npm_utils().args([
            "pack",
            project.path().to_str().unwrap(),
            "--pack-destination",
            out.path().to_str().unwrap(),
        ]),
        "pack --pack-destination",
    );
    assert_eq!(stdout.trim(), "acme-demo-1.2.3.tgz");
    let tarball = std::fs::read(out.path().join("acme-demo-1.2.3.tgz")).unwrap();
    let unpacked = tempfile::tempdir().unwrap();
    let written = npm_utils::extract::tar_gz(
        &tarball,
        unpacked.path(),
        Some("package/"),
        npm_utils::extract::Select::All,
    )
    .unwrap();
    assert_eq!(written, 6);
    assert!(unpacked.path().join("dist/nested/types.d.ts").is_file());
    assert!(!unpacked.path().join("src").exists());
}

#[test]
fn pack_dot_writes_beside_the_sources_without_packing_itself() {
    let project = tempfile::tempdir().unwrap();
    pack_fixture(project.path());
    let stdout = run(
        npm_utils().arg("pack").current_dir(project.path()),
        "pack . in the package directory",
    );
    assert_eq!(stdout.trim(), "acme-demo-1.2.3.tgz");
    let tarball = std::fs::read(project.path().join("acme-demo-1.2.3.tgz")).unwrap();
    let unpacked = tempfile::tempdir().unwrap();
    npm_utils::extract::tar_gz(
        &tarball,
        unpacked.path(),
        Some("package/"),
        npm_utils::extract::Select::All,
    )
    .unwrap();
    assert!(!unpacked.path().join("acme-demo-1.2.3.tgz").exists());
    assert!(unpacked.path().join("dist/index.js").is_file());
    assert!(
        std::fs::read_dir(project.path()).unwrap().all(|e| !e
            .unwrap()
            .file_name()
            .to_string_lossy()
            .ends_with(".part")),
        "no temporary file lingers"
    );
}

#[test]
fn pack_refuses_a_version_that_could_escape_the_destination() {
    let project = tempfile::tempdir().unwrap();
    std::fs::write(
        project.path().join("package.json"),
        r#"{"name":"demo","version":"1.0.0/../../escape"}"#,
    )
    .unwrap();
    std::fs::create_dir_all(project.path().join("demo-1.0.0")).unwrap();
    let out = npm_utils()
        .args(["pack", project.path().to_str().unwrap()])
        .current_dir(project.path())
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("'..'"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!project.path().parent().unwrap().join("escape.tgz").exists());
    assert!(std::fs::read_dir(project.path()).unwrap().all(|e| !e
        .unwrap()
        .file_name()
        .to_string_lossy()
        .ends_with(".tgz")));
}

#[test]
fn pack_refuses_a_hostile_ignore_file() {
    // A brace bomb in `.npmignore` fails the pack fast, naming the offending rule, and no
    // tarball is written. The negation bomb of the JavaScript evaluates here: the long file
    // ships and the tarball is written.
    let long = "a".repeat(200);
    let project = |ignore: &str| {
        let project = tempfile::tempdir().unwrap();
        std::fs::write(
            project.path().join("package.json"),
            r#"{"name":"demo","version":"1.0.0"}"#,
        )
        .unwrap();
        std::fs::write(project.path().join("index.js"), "x").unwrap();
        std::fs::write(project.path().join(&long), "x").unwrap();
        std::fs::write(project.path().join(".npmignore"), ignore).unwrap();
        project
    };
    let pack = |project: &tempfile::TempDir| {
        npm_utils()
            .args(["pack", project.path().to_str().unwrap()])
            .current_dir(project.path())
            .output()
            .unwrap()
    };
    let tarballs = |project: &tempfile::TempDir| {
        std::fs::read_dir(project.path())
            .unwrap()
            .filter(|e| {
                e.as_ref()
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .ends_with(".tgz")
            })
            .count()
    };

    let bomb = project("{1..100000000}\n");
    let out = pack(&bomb);
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("{1..100000000}"), "{stderr}");
    assert_eq!(tarballs(&bomb), 0, "no tarball is written");

    let negation = project("*(!(a))y\n");
    let out = pack(&negation);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(tarballs(&negation), 1);
}

#[test]
fn pack_refuses_by_name_unless_npm_quirks_is_given() {
    let project = tempfile::tempdir().unwrap();
    std::fs::write(
        project.path().join("package.json"),
        r#"{"name":"demo","version":"1.0.0"}"#,
    )
    .unwrap();
    std::fs::write(project.path().join("index.js"), "x").unwrap();
    std::fs::write(project.path().join(".npmignore"), "!(dist)\n").unwrap();
    let out = npm_utils()
        .args(["pack", "--dry-run", "--json", "--npm-quirks"])
        .current_dir(project.path())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let out = npm_utils()
        .args(["pack", "--dry-run", "--json"])
        .current_dir(project.path())
        .output()
        .unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("!(dist)"), "{stderr}");
    assert!(stderr.contains("negation"), "{stderr}");
    assert!(
        std::fs::read_dir(project.path()).unwrap().all(|e| !e
            .unwrap()
            .file_name()
            .to_string_lossy()
            .ends_with(".tgz")),
        "no tarball is written"
    );
}

#[test]
fn a_secret_named_by_main_never_reaches_the_written_tarball() {
    // npm ships `.npmrc` when `main` names it (npm 9.2, npm-packlist 11.3.0); this crate never
    // does. Check the written tarball, not just the listing.
    let project = tempfile::tempdir().unwrap();
    std::fs::write(
        project.path().join("package.json"),
        r#"{"name":"demo","version":"1.0.0","main":".npmrc"}"#,
    )
    .unwrap();
    std::fs::write(
        project.path().join(".npmrc"),
        "//registry.npmjs.org/:_authToken=WRITTEN_TOKEN\n",
    )
    .unwrap();
    std::fs::write(project.path().join("index.js"), "console.log(1)\n").unwrap();
    let out = tempfile::tempdir().unwrap();
    run(
        npm_utils().args([
            "pack",
            project.path().to_str().unwrap(),
            "--pack-destination",
            out.path().to_str().unwrap(),
        ]),
        "pack with a hostile main",
    );
    let tarball = std::fs::read(out.path().join("demo-1.0.0.tgz")).unwrap();
    let unpacked = tempfile::tempdir().unwrap();
    npm_utils::extract::tar_gz(
        &tarball,
        unpacked.path(),
        Some("package/"),
        npm_utils::extract::Select::All,
    )
    .unwrap();
    assert!(!unpacked.path().join(".npmrc").exists());
    assert!(unpacked.path().join("index.js").is_file());
    for entry in std::fs::read_dir(unpacked.path()).unwrap() {
        let content = std::fs::read_to_string(entry.unwrap().path()).unwrap();
        assert!(!content.contains("WRITTEN_TOKEN"), "the token leaked");
    }
}

/// The listing against real npm, on fixtures within what npm and this crate agree on; the
/// never-ship veto diverges on purpose and lives in tests/pack.rs.
#[test]
#[ignore = "needs npm on PATH: compares the listing with npm pack --dry-run --json"]
fn pack_listing_matches_npm() {
    let with_files = tempfile::tempdir().unwrap();
    pack_fixture(with_files.path());
    let with_ignores = tempfile::tempdir().unwrap();
    for (path, content) in [
        (
            "package.json",
            r#"{"name":"demo","version":"0.0.1","main":"index.js"}"#,
        ),
        ("index.js", ""),
        ("build/out.js", ""),
        ("docs/a.md", ""),
        ("docs/.npmignore", "*.md\n!keep.md\n"),
        ("docs/keep.md", ""),
        ("scratch.orig", ""),
        (".gitignore", "build\n"),
        (".npmignore", "docs/a.md\n*.@(pem|key)\ndebug.!(txt)\n"),
        ("LICENCE", ""),
        ("Readme", ""),
        ("readme.md~", ""),
        ("lib/.npmrc", ""),
        ("lib/x.js", ""),
        ("secret.pem", ""),
        ("certs/server.key", ""),
        ("keep.log", ""),
        ("debug.log", ""),
        ("bin/cli.js", ""),
    ] {
        let full = with_ignores.path().join(path);
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        std::fs::write(full, content).unwrap();
    }
    let with_bin_dir = tempfile::tempdir().unwrap();
    for (path, content) in [
        (
            "package.json",
            r#"{"name":"demo","version":"0.0.1","files":[],"directories":{"bin":"bin"}}"#,
        ),
        ("bin/cli.js", ""),
        ("bin/.hidden", ""),
        ("lib/x.js", ""),
    ] {
        let full = with_bin_dir.path().join(path);
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        std::fs::write(full, content).unwrap();
    }
    for dir in [with_files.path(), with_ignores.path(), with_bin_dir.path()] {
        let theirs = Command::new("npm")
            .args(["pack", "--dry-run", "--json"])
            .current_dir(dir)
            .output()
            .expect("npm on PATH");
        assert!(
            theirs.status.success(),
            "{}",
            String::from_utf8_lossy(&theirs.stderr)
        );
        let theirs: serde_json::Value =
            serde_json::from_slice(&theirs.stdout).expect("npm's report parses");
        let ours = run(
            npm_utils().args(["pack", dir.to_str().unwrap(), "--dry-run", "--json"]),
            "pack",
        );
        let ours: serde_json::Value = serde_json::from_str(&ours).unwrap();
        let paths = |report: &serde_json::Value| -> Vec<String> {
            // npm 12 keys the report by package name; npm 9 to 11 printed an array.
            let first = match report {
                serde_json::Value::Array(items) => &items[0],
                serde_json::Value::Object(map) => map.values().next().unwrap(),
                other => panic!("unexpected report {other}"),
            };
            let mut paths: Vec<String> = first["files"]
                .as_array()
                .unwrap()
                .iter()
                .map(|f| f["path"].as_str().unwrap().to_string())
                .collect();
            paths.sort();
            paths
        };
        assert_eq!(paths(&ours), paths(&theirs), "{}", dir.display());
    }
}

#[test]
fn pack_creates_a_missing_destination_directory() {
    let project = tempfile::tempdir().unwrap();
    pack_fixture(project.path());
    let dest = project.path().join("out").join("nested");
    let stdout = run(
        npm_utils().args([
            "pack",
            project.path().to_str().unwrap(),
            "--pack-destination",
            dest.to_str().unwrap(),
        ]),
        "pack --pack-destination",
    );
    assert_eq!(stdout.trim(), "acme-demo-1.2.3.tgz");
    assert!(dest.join("acme-demo-1.2.3.tgz").is_file());
}

#[cfg(unix)]
#[test]
fn pack_refuses_to_write_through_a_planted_symlink() {
    let project = tempfile::tempdir().unwrap();
    pack_fixture(project.path());
    let outside = tempfile::tempdir().unwrap();
    let victim = outside.path().join("victim");
    std::fs::write(&victim, "untouched").unwrap();
    let dest = project.path().join("out");
    std::fs::create_dir_all(&dest).unwrap();
    let planted = dest.join("acme-demo-1.2.3.tgz");
    std::os::unix::fs::symlink(&victim, &planted).unwrap();
    let out = npm_utils()
        .args([
            "pack",
            project.path().to_str().unwrap(),
            "--pack-destination",
            dest.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        !out.status.success(),
        "a symlink planted at the destination is refused"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("symlink"), "{stderr}");
    assert_eq!(std::fs::read_to_string(&victim).unwrap(), "untouched");
    assert!(
        std::fs::symlink_metadata(&planted)
            .unwrap()
            .file_type()
            .is_symlink(),
        "the link itself is left alone"
    );
}

#[test]
fn pack_help_names_the_npm_12_report_shape() {
    let stdout = run(npm_utils().args(["pack", "--help"]), "pack --help");
    assert!(stdout.contains("npm 12"), "{stdout}");
}
