//! Archive extraction, hardened against hostile archives.
//!
//! Both [`tar_gz`] and [`zip()`] iterate an archive in memory and write selected entries beneath
//! `dest`. `strip_prefix` (e.g. `Some("package/")` for npm tarballs) is removed from each entry
//! path before [`Select`] is applied.
//!
//! Archive contents are untrusted input, so extraction is defended in layers:
//!
//! - **Entry-type allowlist** — only regular files and directories are written; symlinks,
//!   hardlinks, device nodes, FIFOs and sockets are skipped, so an archive can't plant a link or
//!   special file.
//! - **Structural path check** ([`crate::path_safety::ensure_within`]) — the entry name as the
//!   archive wrote it is validated before any selection maps it: `..`, absolute, root/drive,
//!   backslash, NUL, interior `.` and empty segments are errors in every mode, never
//!   relativized or cleaned. The selected destination passes the same check again
//!   ([`crate::path_safety::safe_join`]).
//! - **Symlink-resolved containment** ([`crate::path_safety::contained_target`]) — each write's
//!   parent is canonicalized and required to stay within the canonicalized `dest`, before any
//!   directory is created beneath it, so even a symlink already on disk (pre-existing, or from a
//!   destination shared across calls) can't redirect a write outside it.
//! - **Exclusive creation** ([`crate::path_safety::create_contained_file`]) — a file is created
//!   `create_new`; a regular file already there is unlinked rather than written through, and a
//!   directory, FIFO, device or socket there is refused by name instead of followed or blocked on.
//! - **Size caps** — entries are streamed (never buffered whole), the bytes written are bounded,
//!   and so is the inflated stream itself, header bodies and skipped entries included, so a
//!   decompression bomb can't exhaust memory or disk whichever part of the archive carries it.
//!
//! Limits:
//!
//! - The containment check and the open are two steps; `create_new` closes the window for the
//!   file itself, and a directory swapped for a symlink in between redirects a later write.
//! - On a case-insensitive filesystem two entries differing only in case land on one file; the
//!   last one wins.
//! - The gzip reader takes one member; bytes after it are ignored.
//! - Contiguous (type `7`) tar entries are skipped.

use flate2::read::GzDecoder;
use std::fs::create_dir_all;
use std::io::{self, Cursor, Read, Write};
use std::path::Path;
use tar::Archive;

use crate::path_safety::{contained_target, create_contained_file, ensure_within, safe_join};

/// The ceilings one extraction may not cross.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Limits {
    /// Bytes one archive may write to disk.
    pub total_bytes: u64,
    /// Entries one archive may hold, counted before the type filter.
    pub entries: u64,
    /// Bytes the decompressor may produce for one archive: the files, the entries a selection
    /// skips and the extension headers tar reads whole, which no other cap sees.
    pub inflated_bytes: u64,
}

impl Limits {
    pub(crate) const DEFAULT: Limits = Limits {
        total_bytes: MAX_TOTAL_BYTES,
        entries: MAX_ENTRIES,
        inflated_bytes: MAX_TOTAL_BYTES + 256 * 1024 * 1024,
    };
}

/// A reader that ends with an error once `left` bytes have passed and more follow: the inflated
/// stream's cap, applied beneath the archive reader so every byte it pulls is counted.
struct Capped<R: Read> {
    inner: R,
    left: u64,
}

impl<R: Read> Read for Capped<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        if self.left == 0 {
            // One probe byte tells an archive that ends exactly at the cap from one that goes on.
            let mut probe = [0u8; 1];
            return match self.inner.read(&mut probe)? {
                0 => Ok(0),
                _ => Err(io::Error::other(
                    "archive inflates past the extraction size limit (possible decompression bomb)",
                )),
            };
        }
        let want = buf
            .len()
            .min(usize::try_from(self.left).unwrap_or(usize::MAX));
        let n = self.inner.read(&mut buf[..want])?;
        self.left -= n as u64;
        Ok(n)
    }
}

/// Which archive entries to extract, and where each lands (relative to `dest`).
pub enum Select<'a> {
    /// Every file, keeping its (prefix-stripped) path. Directory entries create
    /// directories; non-regular entries (symlinks, hardlinks, devices) are skipped.
    All,
    /// Only entries whose (prefix-stripped) path equals a listed source; written
    /// to the paired destination.
    Files(&'a [(&'a str, &'a str)]),
    /// Each entry's (prefix-stripped) path is handed to the closure, which
    /// returns the destination path or `None` to skip the entry.
    Matching(&'a dyn Fn(&str) -> Option<String>),
}

impl Select<'_> {
    /// Resolve an entry's (prefix-stripped) archive path to a destination
    /// relative path, or `None` to skip it.
    fn dest_for(&self, rel: &str) -> Option<String> {
        match self {
            Select::All => Some(rel.to_string()),
            Select::Files(files) => files
                .iter()
                .find(|(src, _)| *src == rel)
                .map(|(_, dst)| dst.to_string()),
            Select::Matching(f) => f(rel),
        }
    }
}

/// Extract a gzipped tarball into `dest`. Returns the number of files written.
pub fn tar_gz(
    bytes: &[u8],
    dest: &Path,
    strip_prefix: Option<&str>,
    select: Select<'_>,
) -> Result<usize, Box<dyn std::error::Error + Send + Sync>> {
    tar_gz_with(bytes, dest, strip_prefix, select, &Limits::DEFAULT)
}

pub(crate) fn tar_gz_with(
    bytes: &[u8],
    dest: &Path,
    strip_prefix: Option<&str>,
    select: Select<'_>,
    limits: &Limits,
) -> Result<usize, Box<dyn std::error::Error + Send + Sync>> {
    let mut archive = Archive::new(Capped {
        inner: GzDecoder::new(Cursor::new(bytes)),
        left: limits.inflated_bytes,
    });
    let mut count = 0;
    let mut total: u64 = 0;
    let mut entries: u64 = 0;
    // The real (symlink-resolved) absolute path every write must stay under.
    create_dir_all(dest)?;
    let root = dest.canonicalize()?;
    for entry in archive.entries()? {
        let mut entry = entry?;
        entries += 1;
        if entries > limits.entries {
            return Err(too_many_entries(limits.entries));
        }
        let entry_type = entry.header().entry_type();
        let is_dir = entry_type.is_dir();
        // Materialize only regular files and (for `Select::All`) directories. Symlinks,
        // hardlinks, device nodes, FIFOs and sockets are skipped — an archive must not create a
        // link or special file that could redirect a later write or otherwise surprise the caller.
        if !is_dir && !entry_type.is_file() {
            continue;
        }
        // Take the entry path as UTF-8 or reject it: a lossy conversion would map invalid bytes
        // to U+FFFD, which could alias a different name in `Select::Files` matching.
        let path_str = {
            let entry_path = entry.path()?;
            match entry_path.to_str() {
                Some(s) => s.to_owned(),
                None => return Err(non_utf8_entry(&entry_path)),
            }
        };
        let rel = strip(&path_str, strip_prefix);
        // Skip the archive root itself (`.` or empty after the prefix strip): an entry naming
        // the destination directory must never replace it or be written over it.
        if is_root_entry(rel) {
            continue;
        }
        // The name as the archive wrote it must already be a contained relative path, before
        // any selection maps it: a `../x` or `/etc/passwd` is an error, never relativized.
        ensure_within(rel)?;
        if is_dir {
            if matches!(select, Select::All) {
                // Create the directory through the same symlink-resolved containment guard the file
                // writes use, so a pre-existing symlink can't redirect dir creation out of `dest`.
                let target = contained_target(&root, &safe_join(dest, rel)?)?;
                create_dir_all(target)?;
            }
            continue;
        }
        let Some(dest_rel) = select.dest_for(rel) else {
            continue;
        };
        let out = safe_join(dest, &dest_rel)?;
        let mut file = create_contained_file(&root, &out)?;
        total += copy_capped(
            &mut entry,
            &mut file,
            limits.total_bytes.saturating_sub(total),
        )?;
        count += 1;
    }
    Ok(count)
}

/// Extract a zip archive into `dest`. Returns the number of files written.
pub fn zip(
    bytes: &[u8],
    dest: &Path,
    strip_prefix: Option<&str>,
    select: Select<'_>,
) -> Result<usize, Box<dyn std::error::Error + Send + Sync>> {
    zip_with(bytes, dest, strip_prefix, select, &Limits::DEFAULT)
}

pub(crate) fn zip_with(
    bytes: &[u8],
    dest: &Path,
    strip_prefix: Option<&str>,
    select: Select<'_>,
    limits: &Limits,
) -> Result<usize, Box<dyn std::error::Error + Send + Sync>> {
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes))?;
    if archive.len() as u64 > limits.entries {
        return Err(too_many_entries(limits.entries));
    }
    let mut count = 0;
    let mut total: u64 = 0;
    // The real (symlink-resolved) absolute path every write must stay under.
    create_dir_all(dest)?;
    let root = dest.canonicalize()?;
    for i in 0..archive.len() {
        let mut file = archive.by_index(i)?;
        if file.is_dir() || file.is_symlink() {
            continue;
        }
        // The name as written, not the zip crate's cleaned `enclosed_name`, which drops a
        // leading `/` or drive, resolves `a/../b` and reads `\` as a separator: each of those
        // is an error here, as it is for a tarball.
        let name = file.name().to_owned();
        let rel = strip(&name, strip_prefix);
        // Skip the archive root itself (`.`/empty), as in `tar_gz`.
        if is_root_entry(rel) {
            continue;
        }
        ensure_within(rel)?;
        let Some(dest_rel) = select.dest_for(rel) else {
            continue;
        };
        let out = safe_join(dest, &dest_rel)?;
        let mut writer = create_contained_file(&root, &out)?;
        total += copy_capped(
            &mut file,
            &mut writer,
            limits.total_bytes.saturating_sub(total),
        )?;
        count += 1;
    }
    Ok(count)
}

fn strip<'a>(path: &'a str, prefix: Option<&str>) -> &'a str {
    match prefix {
        Some(p) => path.strip_prefix(p).unwrap_or(path),
        None => path,
    }
}

/// Whether a (prefix-stripped) entry path refers to the destination root itself — `.` or the
/// empty string. Such an entry names the package directory, so it is skipped: the root must
/// never be written or linked over.
fn is_root_entry(rel: &str) -> bool {
    rel.is_empty() || rel == "."
}

/// Ceiling on the total bytes one archive may expand to on disk. A compressed archive can
/// inflate enormously (a "decompression bomb"); without a cap a small download could exhaust
/// memory or disk. Generous for real packages — even a large `node_modules` is a few hundred
/// MB — while a bomb is orders of magnitude bigger.
const MAX_TOTAL_BYTES: u64 = 4 * 1024 * 1024 * 1024; // 4 GiB

/// Ceiling on the number of entries one archive may contain. Bounds inode-exhaustion archives
/// (millions of tiny files or directories) that the byte cap alone wouldn't catch. Far above
/// any real single package, which has at most a few thousand files.
const MAX_ENTRIES: u64 = 200_000;

fn too_many_entries(limit: u64) -> Box<dyn std::error::Error + Send + Sync> {
    format!("archive has more than {limit} entries (possible archive bomb)").into()
}

/// Reject an archive entry whose path is not valid UTF-8, rather than lossily mangling it.
fn non_utf8_entry(path: &Path) -> Box<dyn std::error::Error + Send + Sync> {
    format!("archive entry path is not valid UTF-8: {path:?}").into()
}

/// Stream `reader` into `writer`, writing at most `budget` bytes and erroring if the source
/// has more — i.e. if the archive's running total would exceed [`MAX_TOTAL_BYTES`]. Streaming
/// (rather than buffering the whole entry) means a single huge entry can't OOM the process,
/// and the budget bounds total disk use. Returns the number of bytes written.
fn copy_capped<R: Read, W: Write>(
    reader: &mut R,
    writer: &mut W,
    budget: u64,
) -> Result<u64, Box<dyn std::error::Error + Send + Sync>> {
    // Read one byte past the budget, so an over-budget entry is detected rather than silently
    // truncated to the limit.
    let written = std::io::copy(&mut reader.take(budget.saturating_add(1)), writer)?;
    if written > budget {
        return Err(
            "archive exceeds the extraction size limit (possible decompression bomb)".into(),
        );
    }
    Ok(written)
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::write::GzEncoder;
    use flate2::Compression;
    use std::io::Cursor as IoCursor;
    use tempfile::tempdir;

    /// Build an in-memory `.tar.gz` from `(path, contents)` pairs.
    fn make_tar_gz(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut builder = tar::Builder::new(GzEncoder::new(Vec::new(), Compression::fast()));
        for (path, contents) in entries {
            let mut header = tar::Header::new_gnu();
            header.set_size(contents.len() as u64);
            header.set_mode(0o644);
            header.set_entry_type(tar::EntryType::Regular);
            builder
                .append_data(&mut header, *path, IoCursor::new(*contents))
                .unwrap();
        }
        builder.finish().unwrap();
        builder.into_inner().unwrap().finish().unwrap()
    }

    /// Build an in-memory `.tar.gz` carrying a single directory entry at `path`.
    fn make_tar_gz_dir(path: &str) -> Vec<u8> {
        let mut builder = tar::Builder::new(GzEncoder::new(Vec::new(), Compression::fast()));
        let mut header = tar::Header::new_gnu();
        header.set_size(0);
        header.set_mode(0o755);
        header.set_entry_type(tar::EntryType::Directory);
        builder
            .append_data(&mut header, path, IoCursor::new(&b""[..]))
            .unwrap();
        builder.finish().unwrap();
        builder.into_inner().unwrap().finish().unwrap()
    }

    #[test]
    #[cfg(unix)]
    fn rejects_creating_a_dir_through_a_preexisting_symlink() {
        use std::os::unix::fs::symlink;
        // A pre-existing symlink dir inside `dest`; a directory entry would otherwise create a
        // subdir through it. The containment guard must refuse, and nothing may land outside `dest`.
        let tmp = tempdir().unwrap();
        let dest = tmp.path().join("dest");
        let outside = tmp.path().join("outside");
        std::fs::create_dir_all(&dest).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        symlink(&outside, dest.join("link")).unwrap();

        let tgz = make_tar_gz_dir("package/link/sub");
        let result = tar_gz(&tgz, &dest, Some("package/"), Select::All);
        assert!(
            result.is_err(),
            "must refuse to create a dir through a symlink"
        );
        assert!(
            !outside.join("sub").exists(),
            "nothing created outside dest"
        );
    }

    #[test]
    #[cfg(unix)]
    fn rejects_non_utf8_entry_names() {
        use std::os::unix::ffi::OsStrExt;
        // An entry whose path is not valid UTF-8 is rejected, not lossily mangled to U+FFFD.
        let mut builder = tar::Builder::new(GzEncoder::new(Vec::new(), Compression::fast()));
        let mut header = tar::Header::new_gnu();
        header.set_size(3);
        header.set_mode(0o644);
        header.set_entry_type(tar::EntryType::Regular);
        let bad = std::ffi::OsStr::from_bytes(b"bad\xff.txt");
        builder
            .append_data(&mut header, bad, IoCursor::new(&b"abc"[..]))
            .unwrap();
        builder.finish().unwrap();
        let tgz = builder.into_inner().unwrap().finish().unwrap();

        let tmp = tempdir().unwrap();
        assert!(
            tar_gz(&tgz, tmp.path(), None, Select::All).is_err(),
            "a non-UTF-8 entry path must be rejected"
        );
    }

    #[test]
    fn tar_gz_all_strips_prefix() {
        let tgz = make_tar_gz(&[("package/index.js", b"a"), ("package/sub/util.js", b"b")]);
        let tmp = tempdir().unwrap();
        let n = tar_gz(&tgz, tmp.path(), Some("package/"), Select::All).unwrap();
        assert_eq!(n, 2);
        assert!(tmp.path().join("index.js").exists());
        assert!(tmp.path().join("sub/util.js").exists());
    }

    #[test]
    fn tar_gz_files_picks_named_entries() {
        let tgz = make_tar_gz(&[
            ("package/dist/sprite.svg", b"<svg/>"),
            ("package/readme.md", b"x"),
        ]);
        let tmp = tempdir().unwrap();
        let n = tar_gz(
            &tgz,
            tmp.path(),
            Some("package/"),
            Select::Files(&[("dist/sprite.svg", "icons/sprite.svg")]),
        )
        .unwrap();
        assert_eq!(n, 1);
        assert!(tmp.path().join("icons/sprite.svg").exists());
        assert!(!tmp.path().join("readme.md").exists());
    }

    #[test]
    fn tar_gz_matching_predicate_and_prefix() {
        let tgz = make_tar_gz(&[
            ("package/a.js", b"x"),
            ("package/b.css", b"y"),
            ("package/c.mjs", b"z"),
        ]);
        let tmp = tempdir().unwrap();
        let keep_js = |rel: &str| -> Option<String> {
            (rel.ends_with(".js") || rel.ends_with(".mjs")).then(|| format!("lit/{rel}"))
        };
        let n = tar_gz(
            &tgz,
            tmp.path(),
            Some("package/"),
            Select::Matching(&keep_js),
        )
        .unwrap();
        assert_eq!(n, 2);
        assert!(tmp.path().join("lit/a.js").exists());
        assert!(tmp.path().join("lit/c.mjs").exists());
        assert!(!tmp.path().join("lit/b.css").exists());
    }

    #[test]
    fn tar_gz_errors_when_selection_escapes_dest() {
        // Benign archive, but the selection maps an entry to a path that escapes
        // `dest` — extraction must abort, not silently skip.
        let tgz = make_tar_gz(&[("package/x.js", b"x")]);
        let tmp = tempdir().unwrap();
        let escape = |_rel: &str| -> Option<String> { Some("../escape.js".to_string()) };
        let result = tar_gz(
            &tgz,
            tmp.path(),
            Some("package/"),
            Select::Matching(&escape),
        );
        assert!(result.is_err(), "extraction must error when a dest escapes");
    }

    #[test]
    #[cfg(unix)]
    fn rejects_writing_through_a_preexisting_symlink() {
        use std::os::unix::fs::symlink;
        // The footgun: a symlink already inside `dest` points outside it, and an archive
        // writes a file *through* it. The canonicalized-containment guard must refuse, and
        // nothing may land outside `dest`.
        let tmp = tempdir().unwrap();
        let dest = tmp.path().join("dest");
        let outside = tmp.path().join("outside");
        std::fs::create_dir_all(&dest).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        symlink(&outside, dest.join("evil")).unwrap();

        let tgz = make_tar_gz(&[("package/evil/pwned", b"owned")]);
        let result = tar_gz(&tgz, &dest, Some("package/"), Select::All);

        assert!(
            result.is_err(),
            "must refuse to write through an escaping symlink"
        );
        assert!(
            !outside.join("pwned").exists(),
            "nothing may be written outside the extract dir"
        );
    }

    #[test]
    #[cfg(unix)]
    fn rejects_writing_through_a_preexisting_leaf_symlink() {
        use std::os::unix::fs::symlink;
        // Like the test above, but the symlink is the *leaf* being written, not a parent dir.
        // `File::create` would follow it; the containment guard must refuse and leave the
        // pointed-at file untouched.
        let tmp = tempdir().unwrap();
        let dest = tmp.path().join("dest");
        let outside = tmp.path().join("outside.txt");
        std::fs::create_dir_all(&dest).unwrap();
        std::fs::write(&outside, b"original").unwrap();
        symlink(&outside, dest.join("evil")).unwrap();

        let tgz = make_tar_gz(&[("package/evil", b"owned")]);
        let result = tar_gz(&tgz, &dest, Some("package/"), Select::All);

        assert!(
            result.is_err(),
            "must refuse to write through a leaf symlink"
        );
        assert_eq!(
            std::fs::read(&outside).unwrap(),
            b"original",
            "the symlink's target must be untouched"
        );
    }

    #[test]
    fn odd_but_legal_entry_names_stay_contained() {
        // Scary-looking but non-traversal entry names must land *under* `dest`, never escape:
        // `...` and `~` are ordinary directory names, and `file://` is just part of a filename
        // (we never interpret it as a URL).
        let tmp = tempdir().unwrap();
        let dest = tmp.path().join("dest");
        let tgz = make_tar_gz(&[
            (".../flag.txt", b"a"),
            ("~/flag.txt", b"b"),
            ("file:///tmp/flag.txt", b"c"),
        ]);
        let n = tar_gz(&tgz, &dest, None, Select::All).unwrap();
        assert_eq!(n, 3);
        assert!(dest.join("...").join("flag.txt").is_file());
        assert!(dest.join("~").join("flag.txt").is_file());
        // "file:///tmp/flag.txt" → a dir named "file:", then tmp/flag.txt — all under dest.
        assert!(dest.join("file:").join("tmp").join("flag.txt").is_file());
        // Crucially, nothing escaped to dest's parent (no `/tmp` write, no parent-dir write).
        assert!(!tmp.path().join("flag.txt").exists());
    }

    /// A tarball carrying a symlink entry, a hardlink entry, and one regular file.
    fn tar_with_links() -> Vec<u8> {
        let mut b = tar::Builder::new(GzEncoder::new(Vec::new(), Compression::fast()));
        let mut reg = tar::Header::new_gnu();
        reg.set_size(4);
        reg.set_mode(0o644);
        reg.set_entry_type(tar::EntryType::Regular);
        b.append_data(&mut reg, "real.txt", IoCursor::new(&b"data"[..]))
            .unwrap();

        let mut sym = tar::Header::new_gnu();
        sym.set_size(0);
        sym.set_mode(0o777);
        sym.set_entry_type(tar::EntryType::Symlink);
        b.append_link(&mut sym, "evil-symlink", "real.txt").unwrap();

        let mut hard = tar::Header::new_gnu();
        hard.set_size(0);
        hard.set_mode(0o644);
        hard.set_entry_type(tar::EntryType::Link);
        b.append_link(&mut hard, "evil-hardlink", "real.txt")
            .unwrap();

        b.finish().unwrap();
        b.into_inner().unwrap().finish().unwrap()
    }

    #[test]
    fn skips_symlink_and_hardlink_entries() {
        // Only regular files and directories are materialized; link entries (which could
        // redirect a later write or point outside the tree) are never created.
        let tmp = tempdir().unwrap();
        let dest = tmp.path().join("dest");
        let n = tar_gz(&tar_with_links(), &dest, None, Select::All).unwrap();
        assert_eq!(n, 1, "only the regular file is written");
        assert!(dest.join("real.txt").is_file());
        assert!(!dest.join("evil-symlink").exists());
        assert!(!dest.join("evil-hardlink").exists());
    }

    #[test]
    fn copy_capped_streams_within_budget_and_rejects_a_bomb() {
        let src = vec![7u8; 1000];
        // Within budget: the whole stream is copied.
        let mut ok = Vec::new();
        assert_eq!(
            copy_capped(&mut src.as_slice(), &mut ok, 2000).unwrap(),
            1000
        );
        assert_eq!(ok, src);
        // Over budget (the decompression-bomb case): errors rather than truncating silently.
        let mut overflow = Vec::new();
        assert!(copy_capped(&mut src.as_slice(), &mut overflow, 100).is_err());
    }

    /// A `.tar.gz` with one regular file whose header carries `raw_name` verbatim, bypassing
    /// `Header::set_path`, which refuses the names an attacker writes by hand.
    fn make_tar_gz_raw_name(raw_name: &str, contents: &[u8]) -> Vec<u8> {
        let mut builder = tar::Builder::new(GzEncoder::new(Vec::new(), Compression::fast()));
        let mut header = tar::Header::new_gnu();
        header.set_size(contents.len() as u64);
        header.set_mode(0o644);
        header.set_entry_type(tar::EntryType::Regular);
        header.as_old_mut().name[..raw_name.len()].copy_from_slice(raw_name.as_bytes());
        header.set_cksum();
        builder.append(&header, IoCursor::new(contents)).unwrap();
        builder.finish().unwrap();
        builder.into_inner().unwrap().finish().unwrap()
    }

    /// A `.tar.gz` whose first entry is a GNU long-name header with `size` bytes of `body`, the
    /// kind tar reads whole before any file is seen, followed by the file it names.
    fn make_tar_gz_with_long_name_body(size: u64, body: impl Read) -> Vec<u8> {
        let mut builder = tar::Builder::new(GzEncoder::new(Vec::new(), Compression::fast()));
        let mut header = tar::Header::new_gnu();
        header.set_size(size);
        header.set_mode(0o644);
        header.set_entry_type(tar::EntryType::GNULongName);
        header.as_old_mut().name[..13].copy_from_slice(b"././@LongLink");
        header.set_cksum();
        builder.append(&header, body).unwrap();
        let mut file = tar::Header::new_gnu();
        file.set_size(1);
        file.set_mode(0o644);
        file.set_entry_type(tar::EntryType::Regular);
        builder
            .append_data(&mut file, "package/x.js", IoCursor::new(&b"x"[..]))
            .unwrap();
        builder.finish().unwrap();
        builder.into_inner().unwrap().finish().unwrap()
    }

    fn make_zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut writer = zip::ZipWriter::new(IoCursor::new(Vec::new()));
        for (name, contents) in entries {
            writer
                .start_file(*name, zip::write::SimpleFileOptions::default())
                .unwrap();
            writer.write_all(contents).unwrap();
        }
        writer.finish().unwrap().into_inner()
    }

    fn nothing_but(dir: &Path, allowed: &[&str]) -> bool {
        std::fs::read_dir(dir)
            .unwrap()
            .all(|e| allowed.contains(&e.unwrap().file_name().to_str().unwrap()))
    }

    #[test]
    fn an_oversized_extension_header_is_an_error_not_an_allocation() {
        // tar reads a GNU long-name or PAX body whole before the entry it describes exists, so
        // no per-file cap sees it; the inflated-stream cap beneath the reader does.
        let tmp = tempdir().unwrap();
        let bomb = make_tar_gz_with_long_name_body(
            8 * 1024 * 1024,
            std::io::repeat(b'x').take(8 * 1024 * 1024),
        );
        let limits = Limits {
            inflated_bytes: 1024 * 1024,
            ..Limits::DEFAULT
        };
        let error = tar_gz_with(&bomb, tmp.path(), Some("package/"), Select::All, &limits)
            .unwrap_err()
            .to_string();
        assert!(error.contains("inflates past"), "{error}");
        // A long name of ordinary length, within the cap, is read and names the file.
        let name = format!("package/{}", "x".repeat(150));
        let long = make_tar_gz_with_long_name_body(name.len() as u64, IoCursor::new(name.clone()));
        let written =
            tar_gz_with(&long, tmp.path(), Some("package/"), Select::All, &limits).unwrap();
        assert_eq!(written, 1);
        assert!(tmp.path().join(&name["package/".len()..]).is_file());
    }

    #[test]
    fn too_many_entries_is_an_error() {
        let tmp = tempdir().unwrap();
        let tgz = make_tar_gz(&[
            ("package/a", b"a"),
            ("package/b", b"b"),
            ("package/c", b"c"),
        ]);
        let limits = Limits {
            entries: 2,
            ..Limits::DEFAULT
        };
        let error = tar_gz_with(&tgz, tmp.path(), Some("package/"), Select::All, &limits)
            .unwrap_err()
            .to_string();
        assert!(error.contains("more than 2 entries"), "{error}");
    }

    #[test]
    fn rejects_parent_and_absolute_entry_names_before_selection() {
        // The name as written is validated before a selection maps it, so an install's
        // top-directory strip never turns `../x` into `x` or `/etc/passwd` into `etc/passwd`.
        let strip_top =
            |rel: &str| -> Option<String> { rel.split_once('/').map(|(_, rest)| rest.to_string()) };
        for raw in [
            "../evil.js",
            "/etc/passwd",
            "package/../evil.js",
            "package/./x",
        ] {
            let tmp = tempdir().unwrap();
            let dest = tmp.path().join("dest");
            let tgz = make_tar_gz_raw_name(raw, b"owned");
            for select in [Select::All, Select::Matching(&strip_top)] {
                let error = tar_gz(&tgz, &dest, None, select).unwrap_err().to_string();
                assert!(error.contains("refuses to escape"), "{raw}: {error}");
            }
            assert!(nothing_but(&dest, &[]), "{raw} wrote something");
            assert!(
                nothing_but(tmp.path(), &["dest"]),
                "{raw} wrote outside dest"
            );
        }
    }

    #[test]
    fn zip_extracts_a_benign_archive() {
        let tmp = tempdir().unwrap();
        let archive = make_zip(&[("package/a.txt", b"A"), ("package/dir/b.txt", b"B")]);
        let written = zip(&archive, tmp.path(), Some("package/"), Select::All).unwrap();
        assert_eq!(written, 2);
        assert_eq!(std::fs::read(tmp.path().join("a.txt")).unwrap(), b"A");
        assert_eq!(std::fs::read(tmp.path().join("dir/b.txt")).unwrap(), b"B");
    }

    #[test]
    fn zip_rejects_parent_absolute_and_backslash_names() {
        // zip's own `enclosed_name` would clean these into contained paths; the name as written
        // is an error here, as it is for a tarball.
        for raw in ["../evil", "/abs", "a\\..\\b", "dir/../x", "a/./b"] {
            let tmp = tempdir().unwrap();
            let dest = tmp.path().join("dest");
            let archive = make_zip(&[(raw, b"owned")]);
            let result = zip(&archive, &dest, None, Select::All);
            assert!(result.is_err(), "{raw} was accepted");
            assert!(nothing_but(&dest, &[]), "{raw} wrote something");
            assert!(
                nothing_but(tmp.path(), &["dest"]),
                "{raw} wrote outside dest"
            );
        }
    }

    #[test]
    fn zip_skips_symlink_and_directory_entries_and_keeps_case_variants() {
        let tmp = tempdir().unwrap();
        let mut writer = zip::ZipWriter::new(IoCursor::new(Vec::new()));
        let options = zip::write::SimpleFileOptions::default();
        writer.add_directory("dir", options).unwrap();
        writer.add_symlink("link", "/etc/passwd", options).unwrap();
        writer.start_file("a.txt", options).unwrap();
        writer.write_all(b"lower").unwrap();
        writer.start_file("A.txt", options).unwrap();
        writer.write_all(b"upper").unwrap();
        let archive = writer.finish().unwrap().into_inner();
        let written = zip(&archive, tmp.path(), None, Select::All).unwrap();
        assert_eq!(written, 2, "the two files, not the link or the directory");
        assert!(std::fs::symlink_metadata(tmp.path().join("link")).is_err());
        assert!(!tmp.path().join("dir").exists());
        // Case variants are distinct entries; on a case-insensitive filesystem the second write
        // replaces the first, never escapes.
        assert!(tmp.path().join("a.txt").is_file());
    }

    #[test]
    fn zip_caps_the_total_bytes() {
        let tmp = tempdir().unwrap();
        let archive = make_zip(&[("a", &[0u8; 1024]), ("b", &[0u8; 1024])]);
        let limits = Limits {
            total_bytes: 1500,
            ..Limits::DEFAULT
        };
        let error = zip_with(&archive, tmp.path(), None, Select::All, &limits)
            .unwrap_err()
            .to_string();
        assert!(error.contains("extraction size limit"), "{error}");
    }

    #[test]
    #[cfg(unix)]
    fn a_fifo_at_the_destination_is_refused_not_blocked_on() {
        // An ordinary `File::create` on a FIFO blocks until a reader appears; the extraction
        // refuses instead, which this test proves by finishing.
        let tmp = tempdir().unwrap();
        let dest = tmp.path().join("dest");
        std::fs::create_dir_all(&dest).unwrap();
        let made = std::process::Command::new("mkfifo")
            .arg(dest.join("x.js"))
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !made {
            eprintln!("mkfifo unavailable; skipped");
            return;
        }
        let tgz = make_tar_gz(&[("package/x.js", b"owned")]);
        let error = tar_gz(&tgz, &dest, Some("package/"), Select::All)
            .unwrap_err()
            .to_string();
        assert!(error.contains("refuses to write over it"), "{error}");
    }

    #[test]
    fn is_root_entry_flags_dot_and_empty() {
        // `.` and "" name the destination root itself and are skipped, so no entry — least of
        // all a symlink — can replace or be written over the package directory.
        assert!(is_root_entry("."));
        assert!(is_root_entry(""));
        assert!(!is_root_entry("index.js"));
        assert!(!is_root_entry("./index.js"));
        assert!(!is_root_entry("..."));
    }

    #[test]
    fn refuses_to_write_at_the_destination_root() {
        // A `.`/empty *entry* is skipped (is_root_entry); a selection mapping straight onto the
        // root is caught by the containment check (the root's parent is above it). Either way the
        // destination directory itself is never overwritten.
        let tmp = tempdir().unwrap();
        let dest = tmp.path().join("dest");
        let tgz = make_tar_gz(&[("package/x.js", b"x")]);
        let onto_root = |_rel: &str| -> Option<String> { Some(".".to_string()) };
        let result = tar_gz(&tgz, &dest, Some("package/"), Select::Matching(&onto_root));
        assert!(result.is_err(), "writing onto the root must be refused");
        assert!(
            dest.is_dir(),
            "the destination root remains a real directory"
        );
    }
}
