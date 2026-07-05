//! `Source` — the shared grammar for the CLI's positional source arguments.
//!
//! One token names a local directory, a local `package.json` / `package-lock.json` file, or a
//! registry package spec `name=range` (e.g. `lit=^3`). `audit` reads all three; the installer
//! verbs consume specs (directory sources are a planned follow-up); future source-taking verbs
//! parse the same type. Path markers always win, so `./lit=^3` names a file even though it looks
//! like a spec, and `=` (which no package name may contain) unambiguously marks a spec.

use std::path::{Path, PathBuf};

use super::common::split_name_range;
use super::Res;
use crate::package_json::{spec::Range, validate_package_name};

/// How a bare token (no `=`, no path marker) reads — verbs differ.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum BareToken {
    /// `audit`: a bare token is a filesystem path (a spec requires `=`).
    Path,
    /// The installer verbs: a bare token is a package spec — `name` (latest) or the legacy
    /// `name@range`.
    Spec,
}

/// A classified positional source argument.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Source {
    /// An existing local directory (a project root, e.g. `.` or `web/`).
    Dir(PathBuf),
    /// An existing local file — a manifest or a lockfile ([`classify_file`] tells them apart).
    File(PathBuf),
    /// A registry package spec; `range: None` means "resolve latest".
    Spec { name: String, range: Option<String> },
}

/// What a source file turned out to be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum FileKind {
    Manifest,
    Lockfile,
}

/// What the filesystem says a token names (the injectable probe result).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PathKind {
    Dir,
    File,
    Missing,
}

/// The real-filesystem probe behind [`Source::parse`]; follows symlinks, like the verbs' reads.
/// Only "does not exist" reads as [`PathKind::Missing`] — a permission or I/O failure is a real
/// error, not a missing path.
fn probe_fs(path: &Path) -> Res<PathKind> {
    match std::fs::metadata(path) {
        Ok(m) if m.is_dir() => Ok(PathKind::Dir),
        Ok(_) => Ok(PathKind::File),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(PathKind::Missing),
        Err(e) => Err(format!("stat {}: {e}", path.display()).into()),
    }
}

impl Source {
    /// Parse one positional token, probing the real filesystem for path tokens.
    pub(super) fn parse(token: &str, bare: BareToken) -> Res<Source> {
        Self::parse_with(token, bare, std::env::home_dir(), probe_fs)
    }

    /// [`Source::parse`] with an injectable home directory and path probe — the crate's usual
    /// offline-test seam.
    ///
    /// Precedence: path markers, then `name=range`, then the bare-token reading `bare` selects.
    fn parse_with(
        token: &str,
        bare: BareToken,
        home: Option<PathBuf>,
        probe: impl Fn(&Path) -> Res<PathKind>,
    ) -> Res<Source> {
        let token = token.trim();
        if token.is_empty() {
            return Err("empty source".into());
        }

        // 1. Path markers win over everything — the escape hatch for a filename containing `=`.
        if has_path_marker(token) {
            return path_source(token, home, &probe, false);
        }

        // 2. `name=range`, split at the *first* `=`: a package name can never contain `=`, while a
        //    range legitimately can (`lit=>=1 <2` → name `lit`, range `>=1 <2`).
        if let Some((name, range)) = token.split_once('=') {
            validate_spec_name(name)?;
            return Ok(Source::Spec {
                name: name.to_string(),
                range: validate_range(range)?,
            });
        }

        // 3. A bare token: a path for `audit`, a spec (`name` / legacy `name@range`) for the
        //    installer verbs — with no filesystem probe there (npm-faithful: `npm install lodash`
        //    is a registry install even when `./lodash` exists).
        match bare {
            BareToken::Path => path_source(token, home, &probe, true),
            BareToken::Spec => {
                let (name, range) = split_name_range(token);
                if !name.starts_with('@') && name.contains('/') {
                    return Err(format!(
                        "{token:?} is not a package name — for a local directory use ./{token} \
                         (directory sources aren't supported yet)"
                    )
                    .into());
                }
                validate_spec_name(name)?;
                Ok(Source::Spec {
                    name: name.to_string(),
                    range: validate_range(range.unwrap_or(""))?,
                })
            }
        }
    }

    /// Unwrap into `(name, range)` for the spec-consuming verbs; path sources get a clear error
    /// naming `verb` until directory/file sources land there.
    pub(super) fn into_spec(self, verb: &str) -> Res<(String, Option<String>)> {
        match self {
            Source::Spec { name, range } => Ok((name, range)),
            Source::Dir(p) => Err(format!(
                "directory source {} isn't supported by `{verb}` yet — local directories as \
                 dependencies are a planned follow-up",
                p.display()
            )
            .into()),
            Source::File(p) => Err(format!(
                "file source {} isn't supported by `{verb}` — only `audit` reads a \
                 package.json / package-lock.json path",
                p.display()
            )
            .into()),
        }
    }
}

/// `package.json` vs lockfile: by basename first (`package-lock.json` / `npm-shrinkwrap.json` are
/// locks, `package.json` a manifest), else by content — a top-level `lockfileVersion` key marks a
/// lock. (Only that key is sniffed: a workspace-style manifest can carry a `packages` field.)
/// Pure over `(path, text)`, so tests need no files on disk.
pub(super) fn classify_file(path: &Path, text: &str) -> FileKind {
    match path.file_name().and_then(|n| n.to_str()) {
        Some("package-lock.json") | Some("npm-shrinkwrap.json") => FileKind::Lockfile,
        Some("package.json") => FileKind::Manifest,
        _ => match serde_json::from_str::<serde_json::Value>(text) {
            Ok(doc) if doc.get("lockfileVersion").is_some() => FileKind::Lockfile,
            _ => FileKind::Manifest,
        },
    }
}

/// `.`/`..`, a leading `./`, `../`, `/`, `~` or `~/`, or a Windows drive/UNC prefix — the
/// explicit-path grammar [`crate::package_json::spec`] also recognizes for dependency values.
fn has_path_marker(token: &str) -> bool {
    token == "."
        || token == ".."
        || token == "~"
        || token.starts_with("./")
        || token.starts_with("../")
        || token.starts_with('/')
        || token.starts_with("~/")
        || is_windows_path(token)
}

/// `X:\`/`X:/` drive paths and `\\server` UNC paths — Windows-only forms, recognized on every
/// platform so classification (and its errors) never differ per OS.
fn is_windows_path(token: &str) -> bool {
    let bytes = token.as_bytes();
    let drive = bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && (bytes[2] == b'/' || bytes[2] == b'\\');
    drive || token.starts_with(r"\\")
}

/// Expand a leading tilde (the whole token, or `~/rest`) against `home`. The shell already does
/// this for unquoted arguments; a quoted `"~/x"` reaches us verbatim and must mean the same path.
/// `~user` (someone else's home) passes through untouched — resolving other users is shell
/// business — and a tilde token with no known home directory is an error, not a literal path.
fn expand_home(token: &str, home: Option<PathBuf>) -> Res<PathBuf> {
    let Some(rest) = token.strip_prefix('~') else {
        return Ok(PathBuf::from(token));
    };
    if !(rest.is_empty() || rest.starts_with('/')) {
        return Ok(PathBuf::from(token)); // `~user…`
    }
    let home = home.ok_or_else(|| format!("cannot expand {token:?}: no home directory"))?;
    match rest.strip_prefix('/') {
        None => Ok(home), // `~`
        Some(sub) => Ok(home.join(sub)),
    }
}

/// Probe `token` as a path (tilde-expanded first — the `Dir`/`File` sources carry the expanded
/// path, which is what the verbs read). `spec_hint` adds the "a spec is written name=range"
/// pointer to the not-found error — wanted for a bare token (a likely misspelled spec), noise for
/// a `./`-marked one.
fn path_source(
    token: &str,
    home: Option<PathBuf>,
    probe: &impl Fn(&Path) -> Res<PathKind>,
    spec_hint: bool,
) -> Res<Source> {
    let path = expand_home(token, home)?;
    match probe(&path)? {
        PathKind::Dir => Ok(Source::Dir(path)),
        PathKind::File => Ok(Source::File(path)),
        PathKind::Missing if spec_hint => Err(format!(
            "source path {token:?} does not exist (a package spec is written name=range, e.g. lit=^3)"
        )
        .into()),
        PathKind::Missing => Err(format!("source path {token:?} does not exist").into()),
    }
}

/// A `name`d file inside a directory source, `None` when absent. The file may be a symlink —
/// npm tooling reads manifests through symlinks all the time — but only one that **resolves
/// inside** the directory: both paths are canonicalized first, so a symlinked
/// `package-lock.json` cannot steer `audit web/` into reading a file outside `web/`. (An
/// explicit *file* source is the user's own path and is deliberately used as given.)
pub(super) fn contained_file(dir: &Path, name: &str) -> Res<Option<PathBuf>> {
    let file = dir.join(name);
    // Follows symlinks, like `probe_fs`: a dangling link reads as absent, not as an error.
    match std::fs::metadata(&file) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("stat {}: {e}", file.display()).into()),
        Ok(_) => {}
    }
    let canonical = |p: &Path| -> Res<PathBuf> {
        p.canonicalize()
            .map_err(|e| format!("resolving {}: {e}", p.display()).into())
    };
    let file_real = canonical(&file)?;
    if !file_real.starts_with(canonical(dir)?) {
        return Err(format!(
            "{} resolves outside its source directory (to {})",
            file.display(),
            file_real.display()
        )
        .into());
    }
    Ok(Some(file))
}

/// A registry spec's name: the path-safety allowlist plus "no `/` outside a scope" (an unscoped
/// name with a slash is a directory path or a GitHub shorthand, never a registry name).
fn validate_spec_name(name: &str) -> Res {
    validate_package_name(name)?;
    if !name.starts_with('@') && name.contains('/') {
        return Err(
            format!("package name {name:?} contains '/' — scoped names start with '@'").into(),
        );
    }
    Ok(())
}

/// Validate a range against the npm grammar, keeping the **raw** text (it is written verbatim into
/// `package.json`; re-rendering through semver would corrupt npm space-ranges). Empty means latest.
fn validate_range(range: &str) -> Res<Option<String>> {
    let range = range.trim();
    if range.is_empty() {
        return Ok(None);
    }
    Range::parse(range)?;
    Ok(Some(range.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A probe that never touches the filesystem: answers `kind` for every path.
    fn always(kind: PathKind) -> impl Fn(&Path) -> Res<PathKind> {
        move |_| Ok(kind)
    }

    fn spec(name: &str, range: Option<&str>) -> Source {
        Source::Spec {
            name: name.into(),
            range: range.map(str::to_string),
        }
    }

    #[test]
    fn equals_splits_at_the_first_equals() {
        let parse = |t| Source::parse_with(t, BareToken::Path, None, always(PathKind::Missing));
        assert_eq!(parse("lit=^3").unwrap(), spec("lit", Some("^3")));
        // Ranges may contain `=`; the *first* `=` is the separator.
        assert_eq!(parse("lit=>=1 <2").unwrap(), spec("lit", Some(">=1 <2")));
        assert_eq!(
            parse("@lit/context=^1").unwrap(),
            spec("@lit/context", Some("^1"))
        );
        // An empty range means latest.
        assert_eq!(parse("lit=").unwrap(), spec("lit", None));
    }

    #[test]
    fn path_markers_force_a_path_even_when_it_looks_like_a_spec() {
        let src =
            Source::parse_with("./lit=^3", BareToken::Path, None, always(PathKind::File)).unwrap();
        assert_eq!(src, Source::File(PathBuf::from("./lit=^3")));
        // In spec mode too: markers outrank the spec grammar.
        let src =
            Source::parse_with("../web", BareToken::Spec, None, always(PathKind::Dir)).unwrap();
        assert_eq!(src, Source::Dir(PathBuf::from("../web")));
        // A marked path that doesn't exist is an error without the spec hint.
        let err = Source::parse_with("./gone", BareToken::Path, None, always(PathKind::Missing))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("does not exist") && !err.contains("name=range"),
            "{err}"
        );
    }

    #[test]
    fn bare_tokens_are_paths_for_audit() {
        let parse = |t, k| Source::parse_with(t, BareToken::Path, None, always(k));
        // Markerless relative paths work: `audit web`, `audit pkg/package.json`.
        assert_eq!(
            parse("web", PathKind::Dir).unwrap(),
            Source::Dir("web".into())
        );
        assert_eq!(
            parse("pkg/package.json", PathKind::File).unwrap(),
            Source::File("pkg/package.json".into())
        );
        // A missing bare path hints at the spec grammar (a likely misspelled spec).
        let err = parse("lit@^3", PathKind::Missing).unwrap_err().to_string();
        assert!(err.contains("name=range"), "{err}");
    }

    #[test]
    fn bare_tokens_are_specs_for_installers_without_probing() {
        // `PathKind::Dir` everywhere proves no probe outcome can turn a bare spec into a path.
        let parse = |t| Source::parse_with(t, BareToken::Spec, None, always(PathKind::Dir));
        assert_eq!(parse("lit").unwrap(), spec("lit", None));
        assert_eq!(parse("lit@^3").unwrap(), spec("lit", Some("^3")));
        assert_eq!(
            parse("@lit/context@^1").unwrap(),
            spec("@lit/context", Some("^1"))
        );
        assert_eq!(parse("@scope/pkg").unwrap(), spec("@scope/pkg", None));
        // The legacy empty-range form also means latest.
        assert_eq!(parse("lit@").unwrap(), spec("lit", None));
    }

    #[test]
    fn tilde_expands_against_the_injected_home() {
        let home = || Some(PathBuf::from("/home/tester"));
        let src =
            Source::parse_with("~/proj", BareToken::Path, home(), always(PathKind::Dir)).unwrap();
        assert_eq!(src, Source::Dir("/home/tester/proj".into()));
        let src = Source::parse_with("~", BareToken::Path, home(), always(PathKind::Dir)).unwrap();
        assert_eq!(src, Source::Dir("/home/tester".into()));
        // Markers still outrank the spec grammar after expansion: `~/lit=^3` names a file.
        let src = Source::parse_with("~/lit=^3", BareToken::Path, home(), always(PathKind::File))
            .unwrap();
        assert_eq!(src, Source::File("/home/tester/lit=^3".into()));
        // `~user` is the shell's business, not ours — it stays a literal relative path.
        let src = Source::parse_with("~other/x", BareToken::Path, home(), always(PathKind::File))
            .unwrap();
        assert_eq!(src, Source::File("~other/x".into()));
        // A tilde token with no known home directory is an error, never a literal path.
        let err = Source::parse_with("~/proj", BareToken::Path, None, always(PathKind::Dir))
            .unwrap_err()
            .to_string();
        assert!(err.contains("no home directory"), "{err}");
    }

    #[test]
    fn windows_prefixes_are_path_markers_on_every_platform() {
        for token in [r"C:\proj\package.json", "c:/proj", r"\\server\share"] {
            assert!(has_path_marker(token), "{token}");
        }
        for token in ["C:", "X:name", "~user/x", "lit"] {
            assert!(!has_path_marker(token), "{token}");
        }
        // In spec mode a drive path is a path source, not an invalid package name.
        let src =
            Source::parse_with(r"C:\web", BareToken::Spec, None, always(PathKind::Dir)).unwrap();
        assert_eq!(src, Source::Dir(PathBuf::from(r"C:\web")));
    }

    #[test]
    fn probe_errors_propagate_instead_of_reading_as_missing() {
        let denied =
            |p: &Path| -> Res<PathKind> { Err(format!("stat {}: denied", p.display()).into()) };
        let err = Source::parse_with("./locked", BareToken::Path, None, denied)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("denied") && !err.contains("does not exist"),
            "{err}"
        );
    }

    /// The real probe: an unreadable parent is a `stat` error, not "does not exist".
    #[cfg(unix)]
    #[test]
    fn probe_fs_surfaces_permission_errors() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let parent = dir.path().join("parent");
        std::fs::create_dir(&parent).unwrap();
        let child = parent.join("package.json");
        std::fs::write(&child, "{}").unwrap();
        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o000)).unwrap();
        // Root ignores permission bits; nothing to assert there.
        if std::fs::metadata(&child).is_err() {
            let err = probe_fs(&child).unwrap_err().to_string();
            assert!(
                err.contains("stat") && !err.contains("does not exist"),
                "{err}"
            );
        }
        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[test]
    fn bare_spec_tokens_with_slashes_get_the_directory_hint() {
        // The old `install <dir>` grammar's footgun: a markerless directory reads as a spec token.
        let err = Source::parse_with("some/dir", BareToken::Spec, None, always(PathKind::Dir))
            .unwrap_err()
            .to_string();
        assert!(err.contains("./some/dir"), "{err}");
    }

    #[test]
    fn invalid_names_and_ranges_error() {
        let parse = |t| Source::parse_with(t, BareToken::Spec, None, always(PathKind::Missing));
        assert!(
            parse("foo..bar=1").is_err(),
            "path-safety allowlist applies"
        );
        assert!(parse("=^3").is_err(), "empty name");
        assert!(parse("web/pkg=^1").is_err(), "unscoped name with a slash");
        // A dist-tag range surfaces the spec module's guidance, not a bare parse failure.
        let err = parse("lit=next").unwrap_err().to_string();
        assert!(err.contains("dist-tag"), "{err}");
    }

    #[test]
    fn into_spec_rejects_path_sources_naming_the_verb() {
        let (name, range) = spec("lit", Some("^3")).into_spec("add").unwrap();
        assert_eq!((name.as_str(), range.as_deref()), ("lit", Some("^3")));
        let err = Source::Dir("web".into())
            .into_spec("install")
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("`install`") && err.contains("follow-up"),
            "{err}"
        );
        let err = Source::File("package.json".into())
            .into_spec("add")
            .unwrap_err()
            .to_string();
        assert!(err.contains("`add`") && err.contains("audit"), "{err}");
    }

    #[test]
    fn contained_file_finds_regular_files_and_reads_absence_as_none() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            contained_file(dir.path(), "package-lock.json").unwrap(),
            None
        );
        std::fs::write(dir.path().join("package-lock.json"), "{}").unwrap();
        assert_eq!(
            contained_file(dir.path(), "package-lock.json").unwrap(),
            Some(dir.path().join("package-lock.json"))
        );
    }

    #[cfg(unix)]
    #[test]
    fn contained_file_allows_symlinks_resolving_inside() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("real-lock.json"), "{}").unwrap();
        std::os::unix::fs::symlink("real-lock.json", dir.path().join("package-lock.json")).unwrap();
        // A relative symlink to a sibling stays inside the (canonicalized) directory.
        assert!(contained_file(dir.path(), "package-lock.json")
            .unwrap()
            .is_some());
        // A dangling symlink reads as absent, exactly like the old `exists()` probe.
        std::os::unix::fs::symlink("gone.json", dir.path().join("package.json")).unwrap();
        assert_eq!(contained_file(dir.path(), "package.json").unwrap(), None);
    }

    #[cfg(unix)]
    #[test]
    fn contained_file_rejects_symlinks_escaping_the_directory() {
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret.json"), "{}").unwrap();
        let dir = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(
            outside.path().join("secret.json"),
            dir.path().join("package-lock.json"),
        )
        .unwrap();
        let err = contained_file(dir.path(), "package-lock.json")
            .unwrap_err()
            .to_string();
        assert!(err.contains("resolves outside"), "{err}");
    }

    #[cfg(unix)]
    #[test]
    fn contained_file_canonicalizes_the_directory_before_comparing() {
        // The *directory* may itself be reached through a symlink (`audit ./link-to-project`):
        // the containment base is the resolved directory, so its regular files pass.
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real");
        std::fs::create_dir(&real).unwrap();
        std::fs::write(real.join("package.json"), "{}").unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        assert!(contained_file(&link, "package.json").unwrap().is_some());
    }

    #[test]
    fn classify_file_by_basename_then_content_sniff() {
        let lock_text = r#"{ "lockfileVersion": 3, "packages": {} }"#;
        let manifest_text = r#"{ "name": "demo", "version": "1.0.0" }"#;
        // Basenames are authoritative — content isn't consulted.
        for name in ["package-lock.json", "npm-shrinkwrap.json"] {
            assert_eq!(
                classify_file(Path::new(name), manifest_text),
                FileKind::Lockfile
            );
        }
        assert_eq!(
            classify_file(Path::new("package.json"), lock_text),
            FileKind::Manifest
        );
        // Anything else sniffs for `lockfileVersion`.
        assert_eq!(
            classify_file(Path::new("mylock.json"), lock_text),
            FileKind::Lockfile
        );
        assert_eq!(
            classify_file(Path::new("other.json"), manifest_text),
            FileKind::Manifest
        );
        assert_eq!(
            classify_file(Path::new("broken.json"), "not json"),
            FileKind::Manifest
        );
        // A workspace-style manifest with a `packages` field stays a manifest.
        let workspaceish = r#"{ "name": "demo", "packages": ["a", "b"] }"#;
        assert_eq!(
            classify_file(Path::new("weird.json"), workspaceish),
            FileKind::Manifest
        );
    }
}
