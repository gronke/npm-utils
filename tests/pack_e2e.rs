//! End to end, against the world: real packages packed by this crate, held to the registry's
//! tarballs and to a pinned npm. Every test needs the network, so all are ignored by default:
//!
//! ```text
//! cargo test --test pack_e2e -- --ignored                                  # the two corpora
//! PATH=ci/sealed-node/bin:$PATH cargo test --test pack_e2e -- --ignored    # plus npm 12.1.0, sealed
//! ```
//!
//! `FROM_GIT` are packages published straight from their git tree (no build or lifecycle
//! script), fetched at the commit their release tag names (the packument's `gitHead` for every
//! package but picocolors, whose `gitHead` is the commit before its version bump) and packed from
//! source: the listing must equal npm 12.1.0's and the tarball the registry's, file by file. `FROM_REGISTRY`
//! are registry tarballs, build steps and scopes included, unpacked and packed again: the files,
//! sizes and modes must come back as they were and, where npm 8 or newer wrote the tarball, in
//! the same order under the same fixed headers. The integrity strings pin every ground-truth
//! tarball, so a run is deterministic as long as the registry serves the same bytes.
//!
//! `ci/sealed-node` puts npm 12.1.0 on PATH from a digest-pinned node container without
//! network; the `pack-e2e` CI job runs all of this on every push.

use std::collections::BTreeSet;
use std::io::{Read, Write};
use std::path::Path;
use std::process::Command;

use npm_utils::{download, integrity, pack};
use sha2::{Digest, Sha256};

/// A published version; the registry integrity pins the ground-truth tarball.
struct Published {
    name: &'static str,
    version: &'static str,
    integrity: &'static str,
    /// The npm that published it (`_npmVersion`); `None` where another tool did.
    #[allow(dead_code)]
    npm: Option<&'static str>,
    /// npm 8 or newer wrote the tarball, so its order and fixed headers are npm's.
    npm_layout: bool,
}

/// A package published from its git tree, at the commit the packument names.
struct FromGit {
    published: Published,
    repo: &'static str,
    git_head: &'static str,
    /// Files whose bytes legitimately differ from the git tree, each with the reason.
    known: &'static [(&'static str, &'static str)],
}

const FROM_GIT: &[FromGit] = &[
    FromGit {
        published:
        Published {
            name: "ms",
            version: "2.1.3",
            integrity: "sha512-6FlzubTLZG3J2a/NVCAleEhjzq5oxgHyaCU9yYXvcLsvoVaHJq/s5xXI6/XXP6tz7R9xAOtHnSO/tXtF3WRTlA==",
            npm: Some("6.14.6"),
            npm_layout: true,
        },
        repo: "https://github.com/vercel/ms.git",
        git_head: "1c6264b795492e8fdecbc82cb8802fcfbfc08d26",
        known: &[],
    },
    FromGit {
        published:
        Published {
            name: "debug",
            version: "4.4.3",
            integrity: "sha512-RGwwWnwQvkVfavKVt22FGLw+xYSdzARwm0ru6DhTVA3umU5hZc28V3kO4stgYryrTlLpuvgI9GiijltAjNbcqA==",
            npm: Some("10.9.2"),
            npm_layout: true,
        },
        repo: "https://github.com/debug-js/debug.git",
        git_head: "6b2c5fbdb7d414483d9e306ef234acb4cd7ea67c",
        known: &[],
    },
    FromGit {
        published:
        Published {
            name: "semver",
            version: "7.8.5",
            integrity: "sha512-Y7/KDsb8LjooZpwaqGyulO6DQlksgCncchHGk+sZIY4SBvUocMBEFH5Ur1fI4dV+Jvl0w6cjvucaIi40puRioA==",
            npm: Some("11.17.0"),
            npm_layout: true,
        },
        repo: "https://github.com/npm/node-semver.git",
        git_head: "6e05b7637396ac66522cff8731f07cfe0ef49a29",
        known: &[],
    },
    FromGit {
        published:
        Published {
            name: "express",
            version: "5.2.1",
            integrity: "sha512-hIS4idWWai69NezIdRt2xFVofaF4j+6INOpJlVOLDO8zXGpUVEVzIYk12UUi2JzjEzWL3IOAxcTubgz9Po0yXw==",
            npm: Some("10.8.2"),
            npm_layout: true,
        },
        repo: "https://github.com/expressjs/express.git",
        git_head: "dbac741a49a5a64336b70c06e85c2e2706e36336",
        known: &[],
    },
    FromGit {
        published:
        Published {
            name: "commander",
            version: "15.0.0",
            integrity: "sha512-z67u4ZhzCL/Tydu1lJARtEZYWbWaN7oYLHbsuzocr6y4N6WZAagG3RQ4FW61V1/0+jImpj293XfrcYnd1qxtPg==",
            npm: Some("9.2.0"),
            npm_layout: true,
        },
        repo: "https://github.com/tj/commander.js.git",
        git_head: "ba6d13ddb4243e5913367734f8c159089ffe7834",
        known: &[],
    },
    FromGit {
        published:
        Published {
            name: "chalk",
            version: "6.0.1",
            integrity: "sha512-/Ce6KNm3vIbWdMlNna6RVIZ/ICQxnJxCicet5LBKK9ZffBkqzDw0xh9EiKSljdRtiIQ1S1z4YgcscUUGzNCWrA==",
            npm: Some("12.0.1"),
            npm_layout: true,
        },
        repo: "https://github.com/chalk/chalk.git",
        git_head: "47fc05abd46171b235e24174cd2dba83d25bf037",
        known: &[],
    },
    FromGit {
        published:
        Published {
            name: "strip-ansi",
            version: "7.2.0",
            integrity: "sha512-yDPMNjp4WyfYBkHnjIRLfca1i6KMyGCtsVgoKe/z1+6vukgaENdgGBZt+ZmKPc4gavvEZ5OgHfHdrazhgNyG7w==",
            npm: Some("11.8.0"),
            npm_layout: true,
        },
        repo: "https://github.com/chalk/strip-ansi.git",
        git_head: "38ff9f2282540422031ed523f0060c7bb575e20f",
        known: &[],
    },
    FromGit {
        published:
        Published {
            name: "picocolors",
            version: "1.1.1",
            integrity: "sha512-xceH2snhtb5M9liqDsmEw56le376mTZkEX/jEb/RxNFyegNul7eNslCXP9FDj/Lcu0X8KEyMceP2ntpaHrDEVA==",
            npm: Some("10.8.3"),
            npm_layout: true,
        },
        repo: "https://github.com/alexeyraspopov/picocolors.git",
        git_head: "7249f8c5d4825550f70bc1ea98652639933d3bbd",
        known: &[
            (
                "package.json",
                "the maintainers publish through clean-publish, which removes scripts, devDependencies, prettier and its own config from package.json; the repository's file keeps them",
            ),
            (
                "README.md",
                "clean-publish's cleanDocs option publishes the README's opening part and appends a Docs section linking to GitHub; the repository's README continues with Motivation, Prior Art and Benchmarks",
            ),
        ],
    },
    FromGit {
        published:
        Published {
            name: "mime-types",
            version: "3.0.2",
            integrity: "sha512-Lbgzdk0h4juoQ9fCKXW4by0UJqj+nOOrI9MJ1sSj4nI8aI2eo1qmvQEie4VD1glsS250n15LsWsYtCugiStS5A==",
            npm: Some("10.9.0"),
            npm_layout: true,
        },
        repo: "https://github.com/jshttp/mime-types.git",
        git_head: "29a0302d799933a45384892df0722f3c5bb1b033",
        known: &[],
    },
    FromGit {
        published:
        Published {
            name: "body-parser",
            version: "2.3.0",
            integrity: "sha512-2cGmJupaNgg+QUwVLAucDuWuoMZ6EX9iHDRswZ5lsNYEmwPaRknMPCLZz07yTzVq/83p4o/wzbDZbBrTvGGTIw==",
            npm: Some("10.9.0"),
            npm_layout: true,
        },
        repo: "https://github.com/expressjs/body-parser.git",
        git_head: "d0f2ace6c74769da7d19b8661b9a01c01bdb0bf7",
        known: &[],
    },
    FromGit {
        published:
        Published {
            name: "ignore-walk",
            version: "9.0.0",
            integrity: "sha512-tCBEZV2z2FNpIDl2vrhiWzIHzs4qOAuIDEO85eS02vZ3L1U3P56qpPL8GuGGAijDktAEaq2swMkO/Fmbo7YmfQ==",
            npm: Some("11.14.1"),
            npm_layout: true,
        },
        repo: "https://github.com/npm/ignore-walk.git",
        git_head: "2977e5e20d6f1727c532b9e0e99e69c14c72e0df",
        known: &[],
    },
    FromGit {
        published:
        Published {
            name: "npm-packlist",
            version: "11.3.0",
            integrity: "sha512-cS1yVkyriZgQAbiK8PtwhZHEtsFOsKHsCg5Ww2ONckAvXIspgqd6o4WirOzvkupU24iMRZ4xtO4kb2iK2rbnag==",
            npm: Some("11.16.0"),
            npm_layout: true,
        },
        repo: "https://github.com/npm/npm-packlist.git",
        git_head: "d1eed617b1ff1eedf5909efec7867aee385d0350",
        known: &[],
    },
];

const FROM_REGISTRY: &[Published] = &[
    Published {
        name: "minimatch",
        version: "10.2.5",
        integrity: "sha512-MULkVLfKGYDFYejP07QOurDLLQpcjk7Fw+7jXS2R2czRQzR56yHRveU5NDJEOviH+hETZKSkIk5c+T23GjFUMg==",
        npm: Some("11.11.1"),
        npm_layout: true,
    },
    Published {
        name: "glob",
        version: "13.0.6",
        integrity: "sha512-Wjlyrolmm8uDpm/ogGyXZXb1Z+Ca2B8NbJwqBVg0axK9GbBeoS7yGV6vjXnYdGm6X53iehEuxxbyiKp8QmN4Vw==",
        npm: Some("11.10.0"),
        npm_layout: true,
    },
    Published {
        name: "lru-cache",
        version: "11.5.3",
        integrity: "sha512-U4N8FgzmWxc8k1VH8Kr6lQg18U7Fjvby6wXHVRX/ZZ7IwWbRMgrRbP0Wrb5q5NVinryp4SQampHKdvtecItxUg==",
        npm: Some("11.17.0"),
        npm_layout: true,
    },
    Published {
        name: "rimraf",
        version: "6.1.3",
        integrity: "sha512-LKg+Cr2ZF61fkcaK1UdkH2yEBBKnYjTyWzTJT6KNPcSPaiT7HSdhtMXQuN5wkTX0Xu72KQ1l8S42rlmexS2hSA==",
        npm: Some("11.10.0"),
        npm_layout: true,
    },
    Published {
        name: "yallist",
        version: "5.0.0",
        integrity: "sha512-YgvUTfwqyc7UXVMrB+SImsVYSmTS8X/tSrtdNZMImM+n7+QTriRXyXim0mBrTXNeqzVF0KWGgHPeiyViFFrNDw==",
        npm: Some("10.5.0"),
        npm_layout: true,
    },
    Published {
        name: "dotenv",
        version: "18.0.5",
        integrity: "sha512-aBrGvt6KhjxbEnatqMWOMidftwsrCiCvqh1yNIGB9QyF0J+VcWkKDMsvYOEm7ZS82oiV+BT1sTOtIlAmUlTpwQ==",
        npm: Some("11.17.0"),
        npm_layout: true,
    },
    Published {
        name: "qs",
        version: "6.16.0",
        integrity: "sha512-h6fhOIaRrID2CbEY2fqs+7t+UXZo+MLAnU5gRIq85uFtdiUPCdsApMlHhXogKVM4HM2DVbIjGNTTYH2OcmP1vA==",
        npm: Some("11.19.0"),
        npm_layout: true,
    },
    Published {
        name: "cookie",
        version: "2.0.1",
        integrity: "sha512-yuToqVvRrj6pfDXREyQAAv8SkAEk/8GS3jQRTiUMm66TVtBYmqQeoEjL2Lmq8Rpo6271vH76InTChTitEAm65w==",
        npm: Some("11.17.0"),
        npm_layout: true,
    },
    Published {
        name: "minimist",
        version: "1.2.8",
        integrity: "sha512-2yyAR8qBkN3YuheJanUpWC5U3bb5osDywNB8RzDVlDwDHbocAJveqqj1u8+SVD7jkWT4yvsHCpWqqWqAxb0zCA==",
        npm: Some("9.4.0"),
        npm_layout: false,
    },
    Published {
        name: "once",
        version: "1.4.0",
        integrity: "sha512-lNaJgI+2Q5URQBkccEKHTQOPaXdUxnZZElQTZY0MFUAuaEqe1E+Nyvgdz/aIyNi6Z9MzO5dv1H8n58/GELp3+w==",
        npm: Some("3.10.7"),
        npm_layout: false,
    },
    Published {
        name: "inherits",
        version: "2.0.4",
        integrity: "sha512-k/vGaX4/Yla3WzyMCvTQOXYeIHvqOKtnqBduzTHpzpQZzAskKMhZ2K+EnBiSM9zGSoIFeMpXKxa4dYeZIQqewQ==",
        npm: Some("6.9.0"),
        npm_layout: false,
    },
    Published {
        name: "safe-buffer",
        version: "5.2.1",
        integrity: "sha512-rp3So07KcdmmKbGvgaNxQSJr7bGVSVk5S9Eq1F+ppbRo70+YeaDxkw5Dd8NPN+GD6bjnYm2VuPuCXmpuYvmCXQ==",
        npm: Some("6.14.5"),
        npm_layout: true,
    },
    Published {
        name: "is-odd",
        version: "3.0.1",
        integrity: "sha512-CQpnWPrDwmP1+SMHXZhtLtJv90yiyVfluGsX5iNCVkrhQtU3TQHsUWPG9wkdk9Lgd5yNpAg9jQEo90CBaXgWMA==",
        npm: Some("6.0.1"),
        npm_layout: false,
    },
    Published {
        name: "lodash",
        version: "4.17.21",
        integrity: "sha512-v2kDEe57lecTulaDIuNTPy3Ry4gLGJ6Z1O3vE1krgXZNrsQ+LFTGHVxVjcXPs17LhbZVGedAJv8XZ1tvj5FvSg==",
        npm: Some("6.14.11"),
        npm_layout: true,
    },
    Published {
        name: "moment",
        version: "2.30.1",
        integrity: "sha512-uEmtNhbDOrWPFS+hdjFCBfy9f2YoyzRpwcl+DqpC6taX21FzsTLQVbMV/W7PzNSX6x/bhC1zA3c2UQ5NzH6how==",
        npm: Some("8.19.2"),
        npm_layout: true,
    },
    Published {
        name: "tslib",
        version: "2.8.1",
        integrity: "sha512-oJFu94HQb+KVduSUQL7wnpmqnfmLsOA/nAh6b6EH0wCEoK0/mPeXU6c3wKDV83MkOuHPRHtSXKKU99IBazS/2w==",
        npm: Some("10.9.0"),
        npm_layout: true,
    },
    Published {
        name: "uuid",
        version: "14.0.2",
        integrity: "sha512-xZe/16rV4aa+HGSOCiY2YeLT1OybRLrrkL/Rqaq7p7GMVXjFh+6wN4oMYgjFmnSnhY8t6Xpdl2l9qmnHYuMHwQ==",
        npm: Some("11.17.0"),
        npm_layout: true,
    },
    Published {
        name: "zod",
        version: "4.6.5",
        integrity: "sha512-v5l/aFXZQeai4awLbOpSoHecE9UiMrnfx75tEXLjNonXVARxQ5mOeipTjROUchszUNCqnE+hqAMujRsRHsut2Q==",
        npm: None,
        npm_layout: false,
    },
    Published {
        name: "@types/node",
        version: "26.6.4",
        integrity: "sha512-ldVPDCzj7fsaGZrLB0NuHuTvJcsNasysBAqMolr/cgxrLd1xbqxIr3XJiPnHHJUCxj5sNF1vnRj9aWnrVh5Jcg==",
        npm: None,
        npm_layout: false,
    },
    Published {
        name: "@babel/core",
        version: "8.0.6",
        integrity: "sha512-5zwYt1V4ji3mXKKRIAkohSJ29WepxNwxQi+7y7P5AcVPh1dcxnZBU2cfxHnPwS+u9+lZnXPsv30TQaF2xg+Tfg==",
        npm: None,
        npm_layout: false,
    },
];

/// npm's fixed entry mtime, 1985-10-26T08:15:00Z.
const FIXED_MTIME: u64 = 499_162_500;

#[derive(Debug, Clone, PartialEq, Eq)]
struct Entry {
    path: String,
    size: u64,
    mode: u32,
    uid: u64,
    gid: u64,
    mtime: u64,
    sha256: String,
}

fn tarball_url(name: &str, version: &str) -> String {
    let base = name.rsplit('/').next().unwrap();
    format!("https://registry.npmjs.org/{name}/-/{base}-{version}.tgz")
}

/// The registry's tarball, its integrity checked against the pinned string.
fn fetch(p: &Published) -> Vec<u8> {
    let url = tarball_url(p.name, p.version);
    let bytes = download::fetch(&url).unwrap_or_else(|e| panic!("{url}: {e}"));
    integrity::verify(p.name, &bytes, p.integrity)
        .unwrap_or_else(|e| panic!("{}@{}: {e}", p.name, p.version));
    bytes
}

/// The regular files of a gzipped tar in archive order, the first path component (`package/`,
/// or `node/` as DefinitelyTyped writes it) stripped; with `unpack_to`, written there with
/// their modes.
fn entries(bytes: &[u8], unpack_to: Option<&Path>) -> Vec<Entry> {
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(bytes));
    let mut out = Vec::new();
    for entry in archive.entries().unwrap() {
        let mut entry = entry.unwrap();
        let header = entry.header().clone();
        if header.entry_type() != tar::EntryType::Regular {
            continue;
        }
        let raw = entry.path().unwrap().to_string_lossy().into_owned();
        let path = strip_top(&raw).unwrap_or(raw);
        let mut data = Vec::new();
        (&mut entry)
            .take(256 * 1024 * 1024)
            .read_to_end(&mut data)
            .unwrap();
        out.push(Entry {
            path,
            size: header.size().unwrap(),
            mode: header.mode().unwrap() & 0o7777,
            // node-tar's portable mode leaves the owner fields blank, which the tar crate
            // refuses to parse as a number; blank means unset, which is zero.
            uid: header.uid().unwrap_or(0),
            gid: header.gid().unwrap_or(0),
            mtime: header.mtime().unwrap(),
            sha256: format!("{:x}", Sha256::digest(&data)),
        });
    }
    if let Some(root) = unpack_to {
        // The crate's own hardened extractor writes the files; the modes it never applies come
        // from the headers afterwards, since the repack comparison reads them from disk.
        let strip = |rel: &str| strip_top(rel);
        npm_utils::extract::tar_gz(
            bytes,
            root,
            None,
            npm_utils::extract::Select::Matching(&strip),
        )
        .unwrap();
        #[cfg(unix)]
        for entry in &out {
            use std::os::unix::fs::PermissionsExt;
            let full = npm_utils::path_safety::safe_join(root, &entry.path).unwrap();
            std::fs::set_permissions(&full, std::fs::Permissions::from_mode(entry.mode & 0o777))
                .unwrap();
        }
    }
    out
}

/// A tarball entry's path below its top directory, whatever that directory is named.
fn strip_top(rel: &str) -> Option<String> {
    rel.split_once('/')
        .map(|(_, rest)| rest.to_string())
        .filter(|rest| !rest.is_empty())
}

/// This crate's tarball of `dir` in quirks mode (npm's reading), with its report.
fn ours(dir: &Path) -> (pack::Packed, Vec<u8>) {
    let plan = pack::Plan::with(dir, &pack::Settings { quirks: true })
        .unwrap_or_else(|e| panic!("{}: {e}", dir.display()));
    let mut buf = Vec::new();
    let packed = plan.write(&mut buf).unwrap();
    (packed, buf)
}

/// The repository at the published commit, fetched shallowly by hash.
fn clone_at(case: &FromGit) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    for args in [
        vec!["init", "-q"],
        vec!["fetch", "-q", "--depth", "1", case.repo, case.git_head],
        vec!["checkout", "-q", "FETCH_HEAD"],
    ] {
        let out = Command::new("git")
            .args(&args)
            .current_dir(dir.path())
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?} for {}: {}",
            case.repo,
            String::from_utf8_lossy(&out.stderr)
        );
    }
    dir
}

/// node-tar's portable mode, the normalization both npm and this crate apply.
fn portable(mode: u32) -> u32 {
    ((mode & 0o777) | 0o600) & !0o022
}

/// Where `mine` and `theirs` disagree: the file sets, the order when asked, and per file the
/// size, the portable mode and (when asked) the content; `known` paths skip the per-file checks.
fn divergences(
    mine: &[Entry],
    theirs: &[Entry],
    order: bool,
    content: bool,
    known: &[(&str, &str)],
) -> Vec<String> {
    let mut out = Vec::new();
    let paths_m: Vec<&str> = mine.iter().map(|e| e.path.as_str()).collect();
    let paths_t: Vec<&str> = theirs.iter().map(|e| e.path.as_str()).collect();
    let set_m: BTreeSet<&str> = paths_m.iter().copied().collect();
    let set_t: BTreeSet<&str> = paths_t.iter().copied().collect();
    let only_m: Vec<&&str> = set_m.difference(&set_t).collect();
    let only_t: Vec<&&str> = set_t.difference(&set_m).collect();
    if !only_m.is_empty() {
        out.push(format!("only ours: {only_m:?}"));
    }
    if !only_t.is_empty() {
        out.push(format!("only theirs: {only_t:?}"));
    }
    if order && only_m.is_empty() && only_t.is_empty() && paths_m != paths_t {
        let at = paths_m
            .iter()
            .zip(&paths_t)
            .position(|(a, b)| a != b)
            .unwrap();
        out.push(format!(
            "order differs at #{at}: ours {:?}, theirs {:?}",
            paths_m[at], paths_t[at]
        ));
    }
    for e in mine {
        let Some(o) = theirs.iter().find(|t| t.path == e.path) else {
            continue;
        };
        if known.iter().any(|(path, _)| *path == e.path) {
            continue;
        }
        if e.size != o.size {
            out.push(format!("{}: size {} vs {}", e.path, e.size, o.size));
        }
        if e.mode != portable(o.mode) {
            out.push(format!("{}: mode {:o} vs {:o}", e.path, e.mode, o.mode));
        }
        if content && e.sha256 != o.sha256 {
            out.push(format!("{}: content differs", e.path));
        }
    }
    out
}

/// Our headers are always npm's fixed ones.
fn fixed_headers(mine: &[Entry]) -> Vec<String> {
    mine.iter()
        .filter(|e| e.uid != 0 || e.gid != 0 || e.mtime != FIXED_MTIME)
        .map(|e| format!("{}: uid/gid/mtime {}/{}/{}", e.path, e.uid, e.gid, e.mtime))
        .collect()
}

/// One row of the job's step summary, when GitHub provides one.
fn summary(line: &str) {
    if let Ok(path) = std::env::var("GITHUB_STEP_SUMMARY") {
        if let Ok(mut f) = std::fs::OpenOptions::new().append(true).open(path) {
            let _ = writeln!(f, "{line}");
        }
    }
}

fn summary_table(title: &str, columns: &[&str]) {
    summary(&format!(
        "\n### {title}\n\n| {} |\n|{}|",
        columns.join(" | "),
        columns.iter().map(|_| "---").collect::<Vec<_>>().join("|")
    ));
}

/// One package's row: what was compared, which files were exempt and why, and the outcome;
/// the row also goes to stdout, so `--nocapture` shows it. Returns the failure text, if any.
fn verdict(cells: &[&str], compared: &str, known: &[(&str, &str)], d: &[String]) -> String {
    let exempt = known
        .iter()
        .map(|(path, why)| format!("{path} ({why})"))
        .collect::<Vec<_>>()
        .join("; ");
    let result = match (d.is_empty(), known.is_empty()) {
        (true, true) => "identical".to_string(),
        (true, false) => format!("identical except the known files: {exempt}"),
        (false, _) => format!("diverges: {}", d.join("; ")),
    };
    let row = format!("| {} | {compared} | {result} |", cells.join(" | "));
    println!("{row}");
    summary(&row);
    if d.is_empty() {
        String::new()
    } else {
        format!("{}:\n  {}", cells.join("@"), d.join("\n  "))
    }
}

/// The closing line of a table: how many rows went which way.
fn totals(rows: usize, failures: usize) {
    summary(&format!(
        "\n{rows} packages, {} as expected, {failures} diverging.",
        rows - failures
    ));
}

#[test]
#[ignore = "network: fetches twenty registry tarballs"]
fn registry_tarballs_repack_to_themselves() {
    summary_table(
        "Registry tarballs, unpacked and packed again",
        &["package", "version", "publisher", "compared", "result"],
    );
    let mut failures = Vec::new();
    for p in FROM_REGISTRY {
        let bytes = fetch(p);
        let tmp = tempfile::tempdir().unwrap();
        let pkg = tmp.path().join("pkg");
        let theirs = entries(&bytes, Some(&pkg));
        let (packed, ours_bytes) = ours(&pkg);
        let mine = entries(&ours_bytes, None);
        let mut d = divergences(&mine, &theirs, p.npm_layout, true, &[]);
        d.extend(fixed_headers(&mine));
        if p.npm_layout {
            d.extend(
                fixed_headers(&theirs)
                    .into_iter()
                    .map(|s| format!("registry {s}")),
            );
        }
        if packed.files.len() != theirs.len() {
            d.push(format!(
                "{} files reported, {} in the tarball",
                packed.files.len(),
                theirs.len()
            ));
        }
        let unpacked: u64 = theirs.iter().map(|e| e.size).sum();
        if packed.unpacked_size != unpacked {
            d.push(format!(
                "unpacked size {} reported, {unpacked} in the tarball",
                packed.unpacked_size
            ));
        }
        integrity::verify(p.name, &ours_bytes, &packed.integrity).unwrap();
        if pack::list(&pkg).unwrap()
            != pack::list_with(&pkg, &pack::Settings { quirks: true }).unwrap()
        {
            d.push("the strict and the quirks listing differ".into());
        }
        let compared = if p.npm_layout {
            "files, order, sizes, modes, content, fixed headers"
        } else {
            "files, sizes, modes, content (order and headers are the publisher's)"
        };
        let v = verdict(
            &[p.name, p.version, p.npm.map_or("other tool", |_| "npm")],
            compared,
            &[],
            &d,
        );
        if !v.is_empty() {
            failures.push(v);
        }
    }
    totals(FROM_REGISTRY.len(), failures.len());
    assert!(failures.is_empty(), "\n{}", failures.join("\n"));
}

#[test]
#[ignore = "network: clones twelve repositories and fetches their tarballs"]
fn git_checkouts_pack_like_the_published_tarball() {
    summary_table(
        "Git checkouts at the release commit, against the registry tarball",
        &["package", "version", "publisher", "compared", "result"],
    );
    let mut failures = Vec::new();
    for case in FROM_GIT {
        let p = &case.published;
        let dir = clone_at(case);
        let theirs = entries(&fetch(p), None);
        let (packed, bytes) = ours(dir.path());
        let mine = entries(&bytes, None);
        let mut d = divergences(&mine, &theirs, true, true, case.known);
        d.extend(fixed_headers(&mine));
        // Unconditional: a publisher's rewrite strips fields, never the version, so a checkout
        // one commit short of the release shows here even when package.json is exempt.
        if packed.version != p.version {
            d.push(format!(
                "version {} in git, {} published",
                packed.version, p.version
            ));
        }
        if pack::list(dir.path()).unwrap()
            != pack::list_with(dir.path(), &pack::Settings { quirks: true }).unwrap()
        {
            d.push("the strict and the quirks listing differ".into());
        }
        let v = verdict(
            &[p.name, p.version, p.npm.map_or("other tool", |_| "npm")],
            "files, order, sizes, modes, content, fixed headers, version",
            case.known,
            &d,
        );
        if !v.is_empty() {
            failures.push(v);
        }
    }
    totals(FROM_GIT.len(), failures.len());
    assert!(failures.is_empty(), "\n{}", failures.join("\n"));
}

/// npm's `pack --dry-run --json` report for `dir`, either shape.
fn npm_report(dir: &Path) -> serde_json::Value {
    let out = Command::new("npm")
        .args(["pack", "--dry-run", "--json", "--ignore-scripts"])
        .current_dir(dir)
        .output()
        .expect("npm on PATH");
    assert!(
        out.status.success(),
        "npm pack in {}: {}",
        dir.display(),
        String::from_utf8_lossy(&out.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    match value {
        serde_json::Value::Array(mut a) => a.remove(0),
        serde_json::Value::Object(o) => o.into_iter().next().unwrap().1,
        other => panic!("unexpected report: {other}"),
    }
}

#[test]
#[ignore = "needs npm on PATH: compares twelve real packages with npm pack --dry-run --json"]
fn the_pinned_npm_agrees_on_real_packages() {
    summary_table(
        "Git checkouts, this crate's report against npm 12.1.0's",
        &["package", "version", "files", "compared", "result"],
    );
    let mut failures = Vec::new();
    for case in FROM_GIT {
        let p = &case.published;
        let dir = clone_at(case);
        let plan = pack::Plan::with(dir.path(), &pack::Settings { quirks: true }).unwrap();
        let report = plan.write(std::io::sink()).unwrap().report();
        let npm = npm_report(dir.path());
        let mut d = Vec::new();
        let files = |r: &serde_json::Value| -> Vec<(String, u64, u64)> {
            r["files"]
                .as_array()
                .unwrap()
                .iter()
                .map(|f| {
                    (
                        f["path"].as_str().unwrap().to_string(),
                        f["size"].as_u64().unwrap(),
                        f["mode"].as_u64().unwrap(),
                    )
                })
                .collect()
        };
        let (mine, theirs) = (files(&report), files(&npm));
        if mine != theirs {
            let at = mine
                .iter()
                .zip(&theirs)
                .position(|(a, b)| a != b)
                .unwrap_or(mine.len().min(theirs.len()));
            d.push(format!(
                "files differ at #{at}: ours {:?}, npm {:?}",
                mine.get(at),
                theirs.get(at)
            ));
        }
        for key in ["entryCount", "unpackedSize", "filename", "name", "version"] {
            if report[key] != npm[key] {
                d.push(format!("{key}: ours {}, npm {}", report[key], npm[key]));
            }
        }
        let files = mine.len().to_string();
        let v = verdict(
            &[p.name, p.version, &files],
            "report files in order with sizes and modes, entryCount, unpackedSize, filename",
            &[],
            &d,
        );
        if !v.is_empty() {
            failures.push(v);
        }
    }
    totals(FROM_GIT.len(), failures.len());
    assert!(failures.is_empty(), "\n{}", failures.join("\n"));
}

#[test]
#[ignore = "needs npm on PATH: installs three of our tarballs with npm"]
fn our_tarballs_install_with_npm() {
    summary_table(
        "This crate's tarballs through npm install",
        &["package", "version", "compared", "result"],
    );
    for name in ["ms", "picocolors", "chalk"] {
        let case = FROM_GIT.iter().find(|c| c.published.name == name).unwrap();
        let dir = clone_at(case);
        let project = tempfile::tempdir().unwrap();
        let plan = pack::Plan::with(dir.path(), &pack::Settings { quirks: true }).unwrap();
        let packed = plan
            .write_to_path(&project.path().join(plan.filename()))
            .unwrap();
        let out = Command::new("npm")
            .args([
                "install",
                "--ignore-scripts",
                "--no-audit",
                "--no-fund",
                "--loglevel=error",
                &format!("./{}", packed.filename),
            ])
            .current_dir(project.path())
            .output()
            .expect("npm on PATH");
        assert!(
            out.status.success(),
            "npm install {}: {}",
            packed.filename,
            String::from_utf8_lossy(&out.stderr)
        );
        let installed: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(
                project
                    .path()
                    .join("node_modules")
                    .join(name)
                    .join("package.json"),
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(
            installed["version"], packed.version,
            "{name} installs as packed"
        );
        let row = format!(
            "| {name} | {} | npm install of the tarball, the installed version | installs as packed |",
            packed.version
        );
        println!("{row}");
        summary(&row);
    }
    totals(3, 0);
}
