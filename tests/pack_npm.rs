#![cfg(feature = "cli")]
//! The pinned differential specification of the listing: every case below is a small package
//! tree, and `tests/fixtures/pack/npm.json` records the files a pinned npm (`npm pack --dry-run
//! --json`) publishes for it. The ordinary test holds this crate's listing to that record on
//! every run, with no npm around; the ignored test rewrites the record from a host npm, so a
//! new npm-packlist release becomes a reviewable diff of the fixture rather than a drift the
//! matcher and walker acquire in silence.
//!
//! ```text
//! PATH=/path/to/npm-12/bin:$PATH cargo test --features cli --test pack_npm -- --ignored
//! ```
//!
//! The cases stay within what npm and this crate agree on: the never-ship veto (docs/pack.md)
//! deliberately diverges and is covered by tests/pack.rs.

use std::collections::BTreeMap;
use std::path::Path;

use npm_utils::pack;

const FIXTURE: &str = "tests/fixtures/pack/npm.json";

/// A package tree: the manifest and the files beside it (a directory exists when a file does).
struct Case {
    name: &'static str,
    manifest: &'static str,
    files: &'static [(&'static str, &'static str)],
}

const CASES: &[Case] = &[
    Case {
        name: "allowlist",
        manifest: r#"{"name":"@acme/demo","version":"1.2.3","files":["dist","LICENSE-MIT","lib/*.js","*.md","!lib/skip.js"],"main":"dist/index.js","bin":{"demo":"bin/demo.js"}}"#,
        files: &[
            ("dist/index.js", "export const answer = 42;\n"),
            (
                "dist/nested/types.d.ts",
                "export declare const answer: number;\n",
            ),
            ("dist/.DS_Store", ""),
            ("src/index.ts", ""),
            ("lib/a.js", ""),
            ("lib/skip.js", ""),
            ("lib/deep/b.js", ""),
            ("lib/c.ts", ""),
            ("LICENSE-MIT", "MIT\n"),
            ("README.md", "# demo\n"),
            ("CHANGELOG.md", "nothing\n"),
            ("notes.md", ""),
            (".npmignore", "dist\n"),
            ("node_modules/dep/index.js", ""),
            ("package-lock.json", "{}"),
            ("bin/demo.js", "#!/usr/bin/env node\n"),
        ],
    },
    Case {
        name: "files-later-negation-wins",
        manifest: r#"{"name":"demo","version":"1.0.0","files":["foo.js","!foo.js","index.js"]}"#,
        files: &[("foo.js", ""), ("index.js", "")],
    },
    Case {
        name: "files-later-inclusion-wins",
        manifest: r#"{"name":"demo","version":"1.0.0","files":["!foo.js","foo.js","index.js"]}"#,
        files: &[("foo.js", ""), ("index.js", "")],
    },
    Case {
        name: "files-star-is-root-level",
        manifest: r#"{"name":"demo","version":"1.0.0","files":["*.js"]}"#,
        files: &[("a.js", ""), ("lib/b.js", ""), ("deep/er/c.js", "")],
    },
    Case {
        name: "files-globstar-and-braces",
        manifest: r#"{"name":"demo","version":"1.0.0","files":["**/*.{js,d.ts}","!**/*.test.js"]}"#,
        files: &[
            ("a.js", ""),
            ("lib/b.js", ""),
            ("lib/b.test.js", ""),
            ("lib/types.d.ts", ""),
            ("lib/readme.txt", ""),
        ],
    },
    Case {
        name: "files-are-case-sensitive",
        manifest: r#"{"name":"demo","version":"1.0.0","files":["LIB","Index.js","src"]}"#,
        files: &[("lib/b.js", ""), ("index.js", ""), ("src/c.js", "")],
    },
    Case {
        name: "files-directory-forms",
        manifest: r#"{"name":"demo","version":"1.0.0","files":["lib/","./src","/vendor","dist/*","!dist/skip.js","missing","typo/*.js"]}"#,
        files: &[
            ("lib/a.js", ""),
            ("src/b.js", ""),
            ("vendor/v.js", ""),
            ("dist/c.js", ""),
            ("dist/skip.js", ""),
            ("dist/sub/d.js", ""),
            ("other/e.js", ""),
        ],
    },
    Case {
        name: "ignore-files-and-extglobs",
        manifest: r#"{"name":"demo","version":"0.0.1","main":"build/index.js"}"#,
        files: &[
            ("index.js", ""),
            ("build/index.js", ""),
            ("build/out.js", ""),
            ("docs/a.md", ""),
            ("docs/.npmignore", "*.md\n!keep.md\n"),
            ("docs/keep.md", ""),
            ("sub/x.js", ""),
            ("sub/y.js", ""),
            ("sub/.gitignore", "y.js\n"),
            ("scratch.orig", ""),
            (".gitignore", "build\n"),
            (
                ".npmignore",
                "docs/a.md\n*.@(pem|key)\ndebug.!(txt)\nlogs/{old,tmp}\n",
            ),
            ("secret.pem", ""),
            ("certs/server.key", ""),
            ("keep.log", ""),
            ("debug.log", ""),
            ("logs/old/x.log", ""),
            ("logs/keep/y.log", ""),
            ("LICENCE", ""),
            ("Readme", ""),
            ("readme.md~", ""),
            ("lib/.npmrc", ""),
            ("lib/x.js", ""),
        ],
    },
    Case {
        name: "directories-bin",
        manifest: r#"{"name":"demo","version":"0.0.1","files":[],"directories":{"bin":"bin"}}"#,
        files: &[("bin/cli.js", ""), ("bin/.hidden", ""), ("lib/x.js", "")],
    },
    Case {
        name: "extension-and-lockfiles",
        manifest: r#"{"name":"demo","version":"1.0.0"}"#,
        files: &[
            ("index.js", ""),
            (
                ".npm-extension.mjs",
                "export function transformManifest(m) { return m }\n",
            ),
            ("npm-shrinkwrap.json", "{}"),
            ("bun.lock", ""),
            ("pnpm-lock.yaml", ""),
            ("sub/yarn.lock", ""),
        ],
    },
    Case {
        name: "patched-dependencies",
        manifest: r#"{"name":"demo","version":"1.0.0","files":["patches","index.js"],"patchedDependencies":{"x@1":"patches/x.patch","y@2":"./patches/sub/y.patch"}}"#,
        files: &[
            ("patches/x.patch", ""),
            ("patches/y.patch", ""),
            ("patches/sub/y.patch", ""),
            ("patches/sub/z.patch", ""),
            ("index.js", ""),
        ],
    },
    Case {
        name: "entry-points-beat-ignores",
        manifest: r#"{"name":"demo","version":"1.0.0","main":"build/index.js","browser":"build/browser.js","bin":["build/cli.js"]}"#,
        files: &[
            ("build/index.js", ""),
            ("build/browser.js", ""),
            ("build/cli.js", ""),
            ("build/other.js", ""),
            ("index.js", ""),
            (".npmignore", "build\n"),
        ],
    },
];

fn materialize(case: &Case, root: &Path) {
    std::fs::write(root.join("package.json"), case.manifest).unwrap();
    for (path, content) in case.files {
        let full = root.join(path);
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        std::fs::write(full, content).unwrap();
    }
}

/// The record: `{"npm": "<version>", "cases": {"<case>": [paths…]}}`.
struct Fixture {
    npm: String,
    cases: BTreeMap<String, Vec<String>>,
}

impl Fixture {
    fn read() -> Fixture {
        let text = std::fs::read_to_string(fixture_path())
            .unwrap_or_else(|e| panic!("{FIXTURE}: {e}; run the ignored recording test"));
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        let cases = value["cases"]
            .as_object()
            .expect("cases object")
            .iter()
            .map(|(name, files)| {
                let files = files
                    .as_array()
                    .expect("a list of paths")
                    .iter()
                    .map(|f| f.as_str().expect("a path").to_string())
                    .collect();
                (name.clone(), files)
            })
            .collect();
        Fixture {
            npm: value["npm"].as_str().expect("the npm version").to_string(),
            cases,
        }
    }

    fn write(&self) {
        let cases: serde_json::Map<String, serde_json::Value> = self
            .cases
            .iter()
            .map(|(name, files)| (name.clone(), serde_json::json!(files)))
            .collect();
        let value = serde_json::json!({ "npm": self.npm, "cases": cases });
        std::fs::write(
            fixture_path(),
            serde_json::to_string_pretty(&value).unwrap() + "\n",
        )
        .unwrap();
    }
}

fn fixture_path() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(FIXTURE)
}

/// This crate's listing of each case, as a sorted set of paths: npm's reading of the rules,
/// after checking that the strict default lists the same, since no case uses quirky syntax.
fn ours(case: &Case) -> Vec<String> {
    let dir = tempfile::tempdir().unwrap();
    materialize(case, dir.path());
    let mut files = pack::list_with(dir.path(), &pack::Settings { quirks: true }).unwrap();
    files.sort();
    let mut strict = pack::list(dir.path()).unwrap();
    strict.sort();
    assert_eq!(
        strict, files,
        "{}: the strict default reads these rules as npm does",
        case.name
    );
    files
}

#[test]
fn listings_match_the_pinned_npm() {
    let fixture = Fixture::read();
    let mut seen = 0;
    for case in CASES {
        let expected = fixture.cases.get(case.name).unwrap_or_else(|| {
            panic!(
                "{}: no record in {FIXTURE}; run the ignored regeneration test",
                case.name
            )
        });
        assert_eq!(
            &ours(case),
            expected,
            "{} against npm {}",
            case.name,
            fixture.npm
        );
        seen += 1;
    }
    assert_eq!(
        seen,
        fixture.cases.len(),
        "every recorded case is still a case"
    );
}

/// Rewrite the fixture from the `npm` on PATH.
#[test]
#[ignore = "needs npm on PATH: records `npm pack --dry-run --json` per case into the fixture"]
fn record_the_pinned_npm() {
    let version = String::from_utf8(
        std::process::Command::new("npm")
            .arg("--version")
            .output()
            .expect("npm on PATH")
            .stdout,
    )
    .unwrap()
    .trim()
    .to_string();
    let mut cases = BTreeMap::new();
    for case in CASES {
        let dir = tempfile::tempdir().unwrap();
        materialize(case, dir.path());
        let out = std::process::Command::new("npm")
            .args(["pack", "--dry-run", "--json"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}: {}",
            case.name,
            String::from_utf8_lossy(&out.stderr)
        );
        let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        // npm 12 keys the report by package name; npm 9–11 printed an array.
        let first = match &report {
            serde_json::Value::Array(items) => items[0].clone(),
            serde_json::Value::Object(map) => map.values().next().unwrap().clone(),
            other => panic!("{}: unexpected report {other}", case.name),
        };
        let mut files: Vec<String> = first["files"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| f["path"].as_str().unwrap().to_string())
            .collect();
        files.sort();
        cases.insert(case.name.to_string(), files);
    }
    Fixture {
        npm: version,
        cases,
    }
    .write();
}
