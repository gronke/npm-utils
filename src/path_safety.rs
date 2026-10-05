//! Path-traversal hardening shared by [`crate::extract`] and [`crate::install`].
//!
//! Archive and lockfile paths are untrusted. Two layers keep every write inside its intended
//! directory:
//!
//! 1. a cheap **structural** check ([`ensure_within`] / [`safe_join`]) that rejects `..`
//!    (`ParentDir`), absolute / root / drive-prefixed paths, and any segment containing a
//!    backslash (a Windows separator Unix would treat as a filename); and
//! 2. a **filesystem** check ([`contained_target`]) that canonicalizes the resolved parent so a
//!    symlink — one planted by the archive or already present on disk — can't redirect a write
//!    out of the destination; the deepest existing ancestor is checked before any directory is
//!    created beneath it, so a link to the outside never gets directories created at its target
//!    on the way to the refusal; and
//! 3. an **exclusive creation** ([`create_contained_file`]) that looks at what already sits at
//!    the destination: a regular file is unlinked first rather than written through (a
//!    hardlink's twin keeps its bytes), anything else is refused by name (an open would follow a
//!    symlink, fail on a directory or block on a FIFO), and the open itself is `create_new`.
//!
//! Names that *look* dangerous but don't actually traverse — `...`, `~`, or one literally
//! containing `file://` — are ordinary filenames; we never interpret them, so they're allowed
//! and stay contained rather than rejected (rejecting them would break legitimate packages).

use std::fs::File;
use std::io;
use std::path::{Component, Path, PathBuf};

fn unsafe_path(relative: &str) -> Box<dyn std::error::Error + Send + Sync> {
    format!("unsafe path {relative:?}: refuses to escape the destination").into()
}

/// Validate that `relative` cannot escape a base directory. Rejects an empty path, a `..`
/// (`ParentDir`) component, an absolute / root / drive-prefixed path, any segment containing a
/// backslash or a NUL, and a `.` segment anywhere but at the start (`./x`): `Path::components`
/// normalizes `a/./b` away, so a caller counting the written segments would count a depth the
/// path does not have. A leading `.` and ordinary segments — including odd-but-legal names
/// like `...`, `~`, or `file:` — are allowed; none of them traverse.
pub fn ensure_within(relative: &str) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    if relative.is_empty() || relative.contains('\0') {
        return Err(unsafe_path(relative));
    }
    if relative.split('/').skip(1).any(|segment| segment == ".") {
        return Err(unsafe_path(relative));
    }
    for component in Path::new(relative).components() {
        match component {
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(unsafe_path(relative));
            }
            Component::Normal(segment) if segment.to_string_lossy().contains('\\') => {
                return Err(unsafe_path(relative));
            }
            _ => {}
        }
    }
    Ok(())
}

/// `base` joined with a `relative` first validated by [`ensure_within`].
pub fn safe_join(
    base: &Path,
    relative: &str,
) -> Result<PathBuf, Box<dyn std::error::Error + Send + Sync>> {
    ensure_within(relative)?;
    Ok(base.join(relative))
}

/// Resolve where `out`'s parent really points — creating it, then following symlinks — and
/// require it to stay within `root` (which must already be canonicalized). Returns the real,
/// contained path to write to. This is the symlink-traversal guard: neither a link planted by
/// an archive nor one already on disk in the destination can redirect a write outside it.
pub fn contained_target(
    root: &Path,
    out: &Path,
) -> Result<PathBuf, Box<dyn std::error::Error + Send + Sync>> {
    let parent = out
        .parent()
        .ok_or_else(|| -> Box<dyn std::error::Error + Send + Sync> {
            "path has no parent".into()
        })?;
    let escapes = || -> Box<dyn std::error::Error + Send + Sync> {
        format!("unsafe path {out:?}: parent resolves outside the destination (symlink traversal?)")
            .into()
    };
    // Containment first, creation second: the deepest ancestor that exists is resolved and must
    // lie within `root` before any directory is created beneath it, so a planted
    // `link -> /outside` never gets `/outside/a/b` created on the way to the refusal.
    let existing = parent
        .ancestors()
        .find(|ancestor| std::fs::symlink_metadata(ancestor).is_ok())
        .unwrap_or(Path::new("."));
    if !existing.canonicalize()?.starts_with(root) {
        return Err(escapes());
    }
    std::fs::create_dir_all(parent)?;
    let real_parent = parent.canonicalize()?;
    if !real_parent.starts_with(root) {
        return Err(escapes());
    }
    let name = out
        .file_name()
        .ok_or_else(|| -> Box<dyn std::error::Error + Send + Sync> {
            "path has no file name".into()
        })?;
    // Write into the *resolved* directory, so the final write can't be re-redirected.
    let target = real_parent.join(name);
    if let Ok(meta) = std::fs::symlink_metadata(&target) {
        if meta.file_type().is_symlink() {
            return Err(format!(
                "unsafe path {out:?}: destination already exists as a symlink \
                 (refuses to write through it)"
            )
            .into());
        }
    }
    Ok(target)
}

/// The file at `out`, created exclusively within `root`: the containment of
/// [`contained_target`], then a look at what already sits there. A regular file is unlinked
/// first rather than written through, so a hardlink's twin elsewhere keeps its bytes; anything
/// else that exists (a directory, a FIFO, a device, a socket) is refused by name, since an open
/// would fail on the one and block on the other. The open itself is `create_new`, so a link
/// planted between the look and the open is refused too.
pub fn create_contained_file(
    root: &Path,
    out: &Path,
) -> Result<File, Box<dyn std::error::Error + Send + Sync>> {
    let target = contained_target(root, out)?;
    match std::fs::symlink_metadata(&target) {
        Ok(meta) if meta.is_file() => std::fs::remove_file(&target)?,
        Ok(meta) => {
            let what = if meta.is_dir() {
                "a directory"
            } else {
                "a special file"
            };
            return Err(format!(
                "unsafe path {out:?}: destination already exists as {what} (refuses to write \
                 over it)"
            )
            .into());
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    Ok(std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&target)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ensure_within_rejects_traversal() {
        for bad in [
            "../flag.txt",
            "./../flag.txt",
            "a/../../flag.txt",
            "/etc/passwd",  // absolute
            "..",           // bare parent
            "",             // empty
            "..\\flag.txt", // backslash (a Windows separator) in a single Unix segment
            "a/..\\..\\b",  // backslash-escapes hidden inside a segment
            "a/./b",        // an interior `.` that components() would drop
            "a\0b",         // a NUL, which no filesystem call accepts
        ] {
            assert!(ensure_within(bad).is_err(), "{bad:?} must be rejected");
        }
    }

    #[test]
    fn ensure_within_allows_legal_but_unusual_names() {
        // None of these traverse — they're ordinary (if odd) filenames, so they're allowed and
        // stay contained under the base. We never interpret `~` or `file://` specially.
        for ok in [
            "flag.txt",
            "a/b/c.js",
            "@scope/pkg/index.js",
            ".../flag.txt",         // a directory literally named "..."
            "~/flag.txt",           // a directory literally named "~"
            "file:///tmp/flag.txt", // contains "file://" — just a filename to us
            "a..b/c",               // ".." inside a name is not a parent reference
            "./flag.txt",           // a leading "." is fine
            "dir/",                 // a trailing slash names a directory
        ] {
            assert!(
                ensure_within(ok).is_ok(),
                "{ok:?} is a normal name, must be contained"
            );
        }
    }

    #[test]
    fn safe_join_stays_under_base() {
        let base = Path::new("/srv/node_modules");
        assert_eq!(
            safe_join(base, "@scope/pkg/index.js").unwrap(),
            base.join("@scope/pkg/index.js")
        );
        assert!(safe_join(base, "../escape").is_err());
        assert!(safe_join(base, "a/../b").is_err());
        assert!(safe_join(base, "/abs").is_err());
        assert!(safe_join(base, "").is_err());
    }

    #[test]
    fn contained_target_refuses_the_root_itself() {
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("pkg");
        std::fs::create_dir_all(&dest).unwrap();
        let root = dest.canonicalize().unwrap();
        // Writing *at* the root (out == root) is refused — its parent is above the root, so a
        // `.`-style entry can never replace the package directory.
        assert!(contained_target(&root, &dest).is_err());
        // A child under the root is allowed.
        assert!(contained_target(&root, &dest.join("file.js")).is_ok());
    }

    #[test]
    #[cfg(unix)]
    fn contained_target_refuses_before_creating_anything_through_a_symlinked_parent() {
        use std::os::unix::fs::symlink;
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("dest");
        let outside = tmp.path().join("outside");
        std::fs::create_dir_all(&dest).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        symlink(&outside, dest.join("link")).unwrap();
        let root = dest.canonicalize().unwrap();
        assert!(contained_target(&root, &dest.join("link/a/b/file")).is_err());
        assert!(
            !outside.join("a").exists(),
            "no directory may be created outside before the refusal"
        );
    }

    #[test]
    #[cfg(unix)]
    fn create_contained_file_replaces_a_hardlinked_file_instead_of_writing_through_it() {
        use std::io::Write;
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("dest");
        std::fs::create_dir_all(&dest).unwrap();
        let twin = tmp.path().join("twin.txt");
        std::fs::write(&twin, b"original").unwrap();
        std::fs::hard_link(&twin, dest.join("file.txt")).unwrap();
        let root = dest.canonicalize().unwrap();
        let mut file = create_contained_file(&root, &dest.join("file.txt")).unwrap();
        file.write_all(b"replaced").unwrap();
        drop(file);
        assert_eq!(std::fs::read(dest.join("file.txt")).unwrap(), b"replaced");
        assert_eq!(
            std::fs::read(&twin).unwrap(),
            b"original",
            "the hardlink's twin keeps its bytes"
        );
    }

    #[test]
    #[cfg(unix)]
    fn create_contained_file_refuses_a_directory_and_a_fifo() {
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("dest");
        std::fs::create_dir_all(dest.join("dir")).unwrap();
        let root = dest.canonicalize().unwrap();
        let error = create_contained_file(&root, &dest.join("dir"))
            .unwrap_err()
            .to_string();
        assert!(error.contains("a directory"), "{error}");
        // A FIFO where a file should land: an ordinary open would block on it forever, so the
        // refusal, and this test finishing, is the point.
        let fifo = dest.join("fifo");
        let made = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !made {
            eprintln!("mkfifo unavailable; the FIFO half is skipped");
            return;
        }
        let error = create_contained_file(&root, &fifo).unwrap_err().to_string();
        assert!(error.contains("a special file"), "{error}");
    }

    #[test]
    #[cfg(unix)]
    fn contained_target_refuses_a_preexisting_leaf_symlink() {
        use std::os::unix::fs::symlink;
        // The parent is contained, but the *leaf* itself is a pre-existing symlink pointing
        // outside `root`. `File::create` would follow it, so contained_target must refuse.
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("dest");
        let outside = tmp.path().join("outside.txt");
        std::fs::create_dir_all(&dest).unwrap();
        std::fs::write(&outside, b"original").unwrap();
        let root = dest.canonicalize().unwrap();
        symlink(&outside, dest.join("leaf")).unwrap();
        assert!(
            contained_target(&root, &dest.join("leaf")).is_err(),
            "a pre-existing leaf symlink must be refused"
        );
    }
}
