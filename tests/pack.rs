//! Adversarial `pack` tests at the library level: the never-ship veto, symlinks and special
//! files, the budgets and the mode normalization, against hostile package trees.

use std::path::Path;

use npm_utils::pack;

/// The never-ship set on disk, each file carrying a distinct `…_TOKEN` marker so a leak into the
/// tarball's bytes is caught, not just a leak into the listing.
const NEVER_SHIP: &[(&str, &str)] = &[
    (".npmrc", "//registry.npmjs.org/:_authToken=NPMRC_TOKEN"),
    (".git/config", "url = https://x:GITCONFIG_TOKEN@example.com"),
    (".git/HEAD", "ref: refs/heads/GITHEAD_TOKEN"),
    ("package-lock.json", "{\"lock\":\"PACKAGELOCK_TOKEN\"}"),
    ("npm-shrinkwrap.json", "{\"lock\":\"SHRINKWRAP_TOKEN\"}"),
    ("yarn.lock", "# YARNLOCK_TOKEN"),
    ("pnpm-lock.yaml", "lockfileVersion: PNPMLOCK_TOKEN"),
    ("bun.lockb", "BUNLOCKB_TOKEN"),
    ("bun.lock", "BUNLOCK_TOKEN"),
    ("node_modules/dep/index.js", "NODEMODULES_TOKEN"),
];

fn never_ship_paths() -> Vec<&'static str> {
    NEVER_SHIP.iter().map(|(path, _)| *path).collect()
}

/// A package directory with `files` on disk and `manifest` as its package.json.
fn package(files: &[(&str, &str)], manifest: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    for (path, content) in files {
        let full = dir.path().join(path);
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        std::fs::write(full, content).unwrap();
    }
    std::fs::write(dir.path().join("package.json"), manifest).unwrap();
    dir
}

/// The tarball's contents unpacked: `(path, content)` pairs, the `package/` prefix stripped.
fn extracted(dir: &Path) -> Vec<(String, String)> {
    let tarball = pack::tarball(dir).unwrap();
    let out = tempfile::tempdir().unwrap();
    let written = npm_utils::extract::tar_gz(
        &tarball.bytes,
        out.path(),
        Some("package/"),
        npm_utils::extract::Select::All,
    )
    .unwrap();
    assert!(written > 0, "the tarball always carries package.json");
    let mut files = Vec::new();
    collect(out.path(), "", &mut files);
    files
}

fn collect(dir: &Path, rel: &str, out: &mut Vec<(String, String)>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let entry = entry.unwrap();
        let name = entry.file_name().into_string().unwrap();
        let path = if rel.is_empty() {
            name
        } else {
            format!("{rel}/{name}")
        };
        if entry.file_type().unwrap().is_dir() {
            collect(&entry.path(), &path, out);
        } else {
            out.push((path, std::fs::read_to_string(entry.path()).unwrap()));
        }
    }
}

/// None of `never` is listed or packed, and no extracted file carries the `token` marker.
fn assert_never_ships(dir: &Path, never: &[&str], token: &str) {
    let listed = pack::list(dir).unwrap();
    for path in never {
        assert!(
            !listed.iter().any(|f| f == path),
            "{path} listed: {listed:?}"
        );
    }
    let files = extracted(dir);
    for path in never {
        assert!(
            !files.iter().any(|(p, _)| p == path),
            "{path} in the tarball"
        );
    }
    for (path, content) in &files {
        assert!(!content.contains(token), "{token} leaked into {path}");
    }
}

#[test]
fn entry_points_cannot_ship_the_never_ship_set() {
    // Each manifest names a never-ship file through an entry-point field; npm 9.2 and
    // npm-packlist 11.3.0 ship it, this crate must not.
    for manifest in [
        r#"{"name":"demo","version":"1.0.0","main":".npmrc"}"#,
        r#"{"name":"demo","version":"1.0.0","main":".git/config"}"#,
        r#"{"name":"demo","version":"1.0.0","main":"node_modules/dep/index.js"}"#,
        r#"{"name":"demo","version":"1.0.0","browser":"package-lock.json"}"#,
        r#"{"name":"demo","version":"1.0.0","bin":".npmrc"}"#,
        r#"{"name":"demo","version":"1.0.0","bin":{"x":".git/HEAD"}}"#,
        r#"{"name":"demo","version":"1.0.0","bin":["./package-lock.json"]}"#,
        r#"{"name":"demo","version":"1.0.0","directories":{"bin":"node_modules"}}"#,
    ] {
        let mut files = NEVER_SHIP.to_vec();
        files.push(("index.js", "console.log(1)\n"));
        let dir = package(&files, manifest);
        assert_never_ships(dir.path(), &never_ship_paths(), "TOKEN");
        let listed = pack::list(dir.path()).unwrap();
        assert!(
            listed.iter().any(|f| f == "index.js"),
            "{manifest}: the package still packs: {listed:?}"
        );
    }
}

#[test]
fn a_nested_npmignore_negation_cannot_reinclude_npmrc() {
    let dir = package(
        &[
            (
                "lib/.npmrc",
                "//registry.npmjs.org/:_authToken=NESTED_TOKEN",
            ),
            ("lib/.npmignore", "!.npmrc\n"),
            ("lib/ok.js", "export {}\n"),
            ("index.js", "console.log(1)\n"),
        ],
        r#"{"name":"demo","version":"1.0.0"}"#,
    );
    assert_never_ships(dir.path(), &["lib/.npmrc"], "NESTED_TOKEN");
    let listed = pack::list(dir.path()).unwrap();
    assert!(
        listed.iter().any(|f| f == "lib/ok.js"),
        "the veto is surgical, lib/ok.js ships: {listed:?}"
    );
}

#[test]
fn broad_negations_cannot_reinclude_the_never_ship_set() {
    for ignore in ["*\n!**\n", "*\n!*\n"] {
        let mut files = NEVER_SHIP.to_vec();
        files.push(("index.js", "console.log(1)\n"));
        files.push((".npmignore", ignore));
        let dir = package(&files, r#"{"name":"demo","version":"1.0.0"}"#);
        assert_never_ships(dir.path(), &never_ship_paths(), "TOKEN");
        let listed = pack::list(dir.path()).unwrap();
        assert!(
            listed.iter().any(|f| f == "index.js"),
            "{ignore:?}: the negation itself works: {listed:?}"
        );
    }
}

#[test]
fn the_files_allowlist_cannot_ship_the_never_ship_set() {
    for files_field in [
        r#""files":["**"]"#,
        r#""files":[".npmrc"]"#,
        r#""files":[".git/**"]"#,
        r#""files":["package-lock.json","index.js"]"#,
        r#""files":["node_modules"]"#,
    ] {
        let manifest = format!(r#"{{"name":"demo","version":"1.0.0",{files_field}}}"#);
        let mut files = NEVER_SHIP.to_vec();
        files.push(("index.js", "console.log(1)\n"));
        let dir = package(&files, &manifest);
        assert_never_ships(dir.path(), &never_ship_paths(), "TOKEN");
    }
}

#[test]
fn the_veto_folds_case_like_the_matcher() {
    // The matcher is case-insensitive, so the veto is too: a spelling the rules would catch
    // cannot slip past it.
    for manifest in [
        r#"{"name":"demo","version":"1.0.0","main":".NPMRC"}"#,
        r#"{"name":"demo","version":"1.0.0","main":"NODE_MODULES/dep/index.js"}"#,
        r#"{"name":"demo","version":"1.0.0","browser":"Package-Lock.JSON"}"#,
    ] {
        let mut files = NEVER_SHIP.to_vec();
        files.push(("index.js", "console.log(1)\n"));
        let dir = package(&files, manifest);
        assert_never_ships(dir.path(), &never_ship_paths(), "TOKEN");
    }
    // A file literally named `.NPMRC` is caught by the same fold.
    let dir = package(
        &[(".NPMRC", "UPPER_TOKEN"), ("index.js", "console.log(1)\n")],
        r#"{"name":"demo","version":"1.0.0"}"#,
    );
    assert_never_ships(dir.path(), &[".NPMRC"], "UPPER_TOKEN");
}

#[test]
fn files_naming_paths_outside_the_package_ship_nothing() {
    // A `files` entry may probe outside the package root (npm does the same), but nothing
    // outside can be listed or packed.
    let outer = tempfile::tempdir().unwrap();
    let pkg = outer.path().join("pkg");
    std::fs::create_dir(&pkg).unwrap();
    std::fs::write(
        pkg.join("package.json"),
        r#"{"name":"demo","version":"1.0.0","files":["../outside.txt","/etc/passwd","index.js"]}"#,
    )
    .unwrap();
    std::fs::write(pkg.join("index.js"), "console.log(1)\n").unwrap();
    std::fs::write(outer.path().join("outside.txt"), "OUTSIDE_TOKEN").unwrap();
    let listed = pack::list(&pkg).unwrap();
    assert!(
        listed.iter().all(|f| !f.contains("..")),
        "no traversal in the listing: {listed:?}"
    );
    let files = extracted(&pkg);
    assert!(
        !files.iter().any(|(_, c)| c.contains("OUTSIDE_TOKEN")),
        "the sibling file leaked"
    );
    assert!(files.iter().any(|(p, _)| p == "index.js"));
}

#[test]
fn nested_lockfiles_ship_but_root_lockfiles_never_do() {
    // npm anchors the lockfile exclusion at the root; a nested yarn.lock is ordinary content.
    let dir = package(
        &[
            ("index.js", "console.log(1)\n"),
            ("sub/yarn.lock", "# NESTED_YARN"),
            ("yarn.lock", "# ROOT_YARN_TOKEN"),
            ("npm-shrinkwrap.json", "{\"lock\":\"SHRINKWRAP_TOKEN\"}"),
            ("bun.lock", "BUNLOCK_TOKEN"),
        ],
        r#"{"name":"demo","version":"1.0.0","files":["sub","index.js"]}"#,
    );
    let listed = pack::list(dir.path()).unwrap();
    assert!(
        listed.iter().any(|f| f == "sub/yarn.lock"),
        "a nested yarn.lock ships: {listed:?}"
    );
    assert_never_ships(
        dir.path(),
        &["yarn.lock", "npm-shrinkwrap.json", "bun.lock"],
        "TOKEN",
    );
}

#[test]
fn the_root_npm_extension_entry_points_stay_out() {
    // npm-packlist 11.3.0 excludes the root `.npm-extension` entry points; nested ones ship.
    let dir = package(
        &[
            ("index.js", "console.log(1)\n"),
            (".npm-extension.mjs", "export {}\n"),
            (".npm-extension.cjs", "module.exports = {}\n"),
            ("sub/.npm-extension.mjs", "export {}\n"),
        ],
        r#"{"name":"demo","version":"1.0.0"}"#,
    );
    let listed = pack::list(dir.path()).unwrap();
    assert!(listed.iter().any(|f| f == "index.js"), "{listed:?}");
    assert!(
        listed.iter().any(|f| f == "sub/.npm-extension.mjs"),
        "a nested entry point is ordinary content: {listed:?}"
    );
    assert!(
        listed.iter().all(|f| !f.starts_with(".npm-extension")),
        "{listed:?}"
    );
}

#[test]
fn an_over_budget_brace_sequence_fails_naming_the_line() {
    // `{1..100000000}` would expand to a hundred million patterns; the pack fails fast instead.
    let dir = package(
        &[
            ("index.js", "console.log(1)\n"),
            (".npmignore", "{1..100000000}\n"),
        ],
        r#"{"name":"demo","version":"1.0.0"}"#,
    );
    let error = pack::list(dir.path()).unwrap_err().to_string();
    assert!(error.contains("{1..100000000}"), "{error}");
    assert!(error.contains("brace expansion"), "{error}");
    assert!(pack::tarball(dir.path()).is_err(), "nothing is packed");
}

#[test]
fn multiplicative_braces_fail_fast() {
    // Sixteen `{a,b,c}` groups name 43 million alternatives through a `files` entry.
    let files_entry = "{a,b,c}".repeat(16);
    let manifest = format!(r#"{{"name":"demo","version":"1.0.0","files":["{files_entry}"]}}"#);
    let dir = package(&[("index.js", "console.log(1)\n")], &manifest);
    let error = pack::list(dir.path()).unwrap_err().to_string();
    assert!(error.contains("brace expansion"), "{error}");
}

#[test]
fn a_pathological_ignore_pattern_errors_instead_of_hanging() {
    // A 200-char name and a negated group under a repeat, which keeps the backtracking engine
    // busy: the pack fails naming the rule, in milliseconds, instead of hanging.
    let long = "a".repeat(200);
    for line in ["*(!(a))y", "+(!(a)|b)c"] {
        let dir = package(
            &[
                ("index.js", "console.log(1)\n"),
                (&long, "x"),
                (".npmignore", line),
            ],
            r#"{"name":"demo","version":"1.0.0"}"#,
        );
        let error = pack::list(dir.path()).unwrap_err().to_string();
        assert!(error.contains(line), "{error}");
        assert!(error.contains("step limit"), "{error}");
        assert!(pack::tarball(dir.path()).is_err(), "nothing is packed");
    }
}

#[test]
fn patterns_without_lookaround_run_in_linear_time() {
    // These needed ~1e13 steps in a backtracking matcher; on the engine they run on
    // regex-automata, so the pack lists the file at once (neither rule matches it).
    let long = "b".repeat(200);
    let dir = package(
        &[
            ("index.js", "console.log(1)\n"),
            (&long, "x"),
            (
                ".npmignore",
                "*b*b*b*b*b*b*b*c\n+(b|bb)+(b|bb)+(b|bb)+(b|bb)c\n",
            ),
        ],
        r#"{"name":"demo","version":"1.0.0"}"#,
    );
    let started = std::time::Instant::now();
    let listed = pack::list(dir.path()).unwrap();
    assert!(listed.iter().any(|f| f == &long), "{listed:?}");
    assert!(started.elapsed() < std::time::Duration::from_secs(2));
}

#[test]
fn the_default_refuses_what_npm_guesses_at() {
    let manifest = r#"{"name":"demo","version":"1.0.0"}"#;
    let npm = pack::Settings { quirks: true };
    // npm reads `!(dist)` as the negation of a literal `(dist)`: a rule that matches nothing.
    let dir = package(
        &[
            ("index.js", ""),
            ("dist/x.js", ""),
            (".npmignore", "!(dist)\n"),
        ],
        manifest,
    );
    let listed = pack::list_with(dir.path(), &npm).unwrap();
    assert!(listed.iter().any(|f| f == "dist/x.js"), "{listed:?}");
    let error = pack::list(dir.path()).unwrap_err().to_string();
    // The ignore file's path leads, then the rule and the reason.
    assert!(
        error.ends_with(
            ".npmignore: ignore rule \"!(dist)\": a leading `!(` is negation in npm and a group \
             in Bash; write `!@(…)` to negate a group match or `@(!(…))` for the group"
        ),
        "{error}"
    );
    // `*\.js` compares the raw extension in npm; the strict default honours the escape.
    let dir = package(
        &[("a.js", ""), ("b.txt", ""), (".npmignore", "*\\.js\n")],
        manifest,
    );
    let listed = pack::list_with(dir.path(), &npm).unwrap();
    assert!(listed.iter().any(|f| f == "a.js"), "{listed:?}");
    let listed = pack::list(dir.path()).unwrap();
    assert!(listed.iter().all(|f| f != "a.js"), "{listed:?}");
    assert!(listed.iter().any(|f| f == "b.txt"), "{listed:?}");
    // The plan carries the setting the same way.
    let plan = pack::Plan::with(dir.path(), &npm).unwrap();
    assert!(plan.files().iter().any(|f| f == "a.js"));
    let plan = pack::Plan::new(dir.path()).unwrap();
    assert!(plan.files().iter().all(|f| f != "a.js"));
}

#[test]
fn a_just_under_budget_pattern_still_matches_correctly() {
    // The budget must not be so tight that a nasty-but-legal rule misbehaves: `*x*x*x*y` does
    // not match 40 `x`s (the file ships), `*x*x*x*` does (it is excluded).
    let xs = "x".repeat(40);
    let included = package(
        &[
            ("index.js", "console.log(1)\n"),
            (&xs, "x"),
            (".npmignore", "*x*x*x*y\n"),
        ],
        r#"{"name":"demo","version":"1.0.0"}"#,
    );
    let listed = pack::list(included.path()).unwrap();
    assert!(listed.iter().any(|f| f == &xs), "{listed:?}");
    let excluded = package(
        &[
            ("index.js", "console.log(1)\n"),
            (&xs, "x"),
            (".npmignore", "*x*x*x*\n"),
        ],
        r#"{"name":"demo","version":"1.0.0"}"#,
    );
    let listed = pack::list(excluded.path()).unwrap();
    assert!(!listed.iter().any(|f| f == &xs), "{listed:?}");
}

#[cfg(unix)]
#[test]
fn a_symlink_to_an_outside_secret_never_ships() {
    use std::os::unix::fs::symlink;
    // However the link is named, plainly, in `files`, as `main` or as a `bin` target, its
    // target never ships.
    for manifest in [
        r#"{"name":"demo","version":"1.0.0"}"#,
        r#"{"name":"demo","version":"1.0.0","files":["abs-link","rel-link"]}"#,
        r#"{"name":"demo","version":"1.0.0","main":"abs-link"}"#,
        r#"{"name":"demo","version":"1.0.0","bin":{"x":"rel-link"}}"#,
    ] {
        let outer = tempfile::tempdir().unwrap();
        let pkg = outer.path().join("pkg");
        std::fs::create_dir(&pkg).unwrap();
        std::fs::write(pkg.join("package.json"), manifest).unwrap();
        std::fs::write(pkg.join("index.js"), "console.log(1)\n").unwrap();
        std::fs::write(outer.path().join("secret.txt"), "OUTSIDE_SECRET_TOKEN").unwrap();
        // An absolute link out, and a relative one through a parent component.
        symlink(outer.path().join("secret.txt"), pkg.join("abs-link")).unwrap();
        symlink("../secret.txt", pkg.join("rel-link")).unwrap();
        let listed = pack::list(&pkg).unwrap();
        assert!(
            !listed.iter().any(|f| f.contains("link")),
            "{manifest}: {listed:?}"
        );
        let files = extracted(&pkg);
        assert!(
            !files
                .iter()
                .any(|(_, c)| c.contains("OUTSIDE_SECRET_TOKEN")),
            "{manifest}: the link target's content leaked"
        );
        assert!(files.iter().any(|(p, _)| p == "package.json"), "{manifest}");
    }
}

#[cfg(unix)]
#[test]
fn a_symlinked_directory_is_never_descended() {
    use std::os::unix::fs::symlink;
    for manifest in [
        r#"{"name":"demo","version":"1.0.0"}"#,
        r#"{"name":"demo","version":"1.0.0","files":["link","link/**"]}"#,
    ] {
        let outer = tempfile::tempdir().unwrap();
        let pkg = outer.path().join("pkg");
        std::fs::create_dir(&pkg).unwrap();
        std::fs::write(pkg.join("package.json"), manifest).unwrap();
        std::fs::write(pkg.join("index.js"), "console.log(1)\n").unwrap();
        let target = outer.path().join("target");
        std::fs::create_dir(&target).unwrap();
        std::fs::write(target.join("secret.js"), "DIR_SECRET_TOKEN").unwrap();
        symlink(&target, pkg.join("link")).unwrap();
        let listed = pack::list(&pkg).unwrap();
        assert!(
            !listed.iter().any(|f| f.starts_with("link")),
            "{manifest}: {listed:?}"
        );
        let files = extracted(&pkg);
        assert!(
            !files.iter().any(|(_, c)| c.contains("DIR_SECRET_TOKEN")),
            "{manifest}: the linked directory's content leaked"
        );
    }
}

#[cfg(unix)]
#[test]
fn a_dangling_symlink_is_skipped() {
    use std::os::unix::fs::symlink;
    let dir = package(
        &[("index.js", "console.log(1)\n")],
        r#"{"name":"demo","version":"1.0.0"}"#,
    );
    symlink("no/such/target", dir.path().join("dangling.js")).unwrap();
    let listed = pack::list(dir.path()).unwrap();
    assert!(!listed.iter().any(|f| f == "dangling.js"), "{listed:?}");
    assert!(pack::tarball(dir.path()).is_ok());
}

#[cfg(unix)]
#[test]
fn a_unix_socket_in_the_package_is_skipped() {
    // A special file must not ship and must not block the pack (a FIFO would wait on a writer
    // if it were ever opened; the walk only ever opens regular files).
    let dir = package(
        &[("index.js", "console.log(1)\n")],
        r#"{"name":"demo","version":"1.0.0"}"#,
    );
    let _listener = std::os::unix::net::UnixListener::bind(dir.path().join("sock")).unwrap();
    let listed = pack::list(dir.path()).unwrap();
    assert!(!listed.iter().any(|f| f == "sock"), "{listed:?}");
    assert!(pack::tarball(dir.path()).is_ok());
}

#[test]
fn deeply_nested_packages_pack() {
    // Build a one-char-directory chain of `levels` with a file at the bottom; returns the
    // package dir and the leaf's relative path.
    fn nested(levels: usize) -> (tempfile::TempDir, String) {
        let dir = package(
            &[("index.js", "console.log(1)\n")],
            r#"{"name":"demo","version":"1.0.0"}"#,
        );
        let mut rel = String::new();
        let mut full = dir.path().to_path_buf();
        for _ in 0..levels {
            rel.push_str("a/");
            full.push("a");
        }
        std::fs::create_dir_all(&full).unwrap();
        std::fs::write(full.join("f.js"), "x").unwrap();
        (dir, format!("{rel}f.js"))
    }
    // Forty levels pack fine; real trees are a few dozen at most.
    let (dir, leaf) = nested(40);
    let listed = pack::list(dir.path()).unwrap();
    assert!(listed.iter().any(|f| f == &leaf), "the leaf ships");
    assert!(pack::tarball(dir.path()).is_ok());
    // Past the depth cap the pack fails closed with a clear error instead of overflowing the
    // stack (the walk and the rule filter recurse per level).
    let (dir, _) = nested(100);
    let error = pack::list(dir.path()).unwrap_err().to_string();
    assert!(error.contains("-level limit"), "{error}");
}

#[cfg(unix)]
#[test]
fn a_setuid_file_is_normalized_to_plain_permissions() {
    use std::os::unix::fs::PermissionsExt;
    let dir = package(
        &[("tool", "binary")],
        r#"{"name":"demo","version":"1.0.0"}"#,
    );
    std::fs::set_permissions(
        dir.path().join("tool"),
        std::fs::Permissions::from_mode(0o4755),
    )
    .unwrap();
    let tarball = pack::tarball(dir.path()).unwrap();
    let entry = tarball.files.iter().find(|f| f.path == "tool").unwrap();
    assert_eq!(entry.mode, 0o755, "no setuid bit reaches the tarball");
}

#[test]
fn a_path_longer_than_ustar_round_trips() {
    // `package/` plus ten 30-char directories exceeds the 255 bytes a ustar header splits
    // into; the tar crate falls back to a GNU longname entry, and the crate's own extractor
    // reads it back byte-identically.
    let leaf = format!(
        "{}/leaf.js",
        (0..10)
            .map(|_| "d".repeat(30))
            .collect::<Vec<_>>()
            .join("/")
    );
    assert!(leaf.len() > 255, "the fixture is past the ustar limit");
    let dir = package(
        &[(leaf.as_str(), "LONGPATH_TOKEN")],
        r#"{"name":"demo","version":"1.0.0"}"#,
    );
    let tarball = pack::tarball(dir.path()).unwrap();
    assert!(tarball.files.iter().any(|f| f.path == leaf));
    let files = extracted(dir.path());
    assert!(
        files
            .iter()
            .any(|(p, c)| p == &leaf && c == "LONGPATH_TOKEN"),
        "the long path round-trips"
    );
}
