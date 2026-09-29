//! `npm pack`, pure Rust: the files a package directory publishes and the tarball they form.
//!
//! [`list`] walks the directory by npm-packlist's rules (docs/pack.md). A [`Plan`] freezes that
//! listing before any output exists; [`write()`] streams the tarball into any writer, hashing as
//! it goes, [`write_to_path`] stages it beside the destination and renames it on success, and
//! [`tarball`] buffers it. The layout is pacote's: `package/`-prefixed regular files in packlist
//! order, npm's fixed mtime, node-tar's portable modes with the bins executable, gzip at level
//! 9, reported with the sha1 shasum and sha512 integrity of this crate's stream.

mod pattern;
mod walker;

use std::cmp::Ordering;
use std::fs::File;
use std::io::{self, Write};
use std::ops::Deref;
use std::path::Path;

use base64::Engine;
use flate2::write::GzEncoder;
use flate2::Compression;
use serde_json::{json, Value};
use sha1::Sha1;
use sha2::{Digest, Sha512};

use crate::package_json::{validate_package_name, validate_version};
use crate::Result;

/// npm's fixed entry mtime, 1985-10-26T08:15:00Z, so the bytes never depend on the clock.
const MTIME: u64 = 499_162_500;

/// One file of the tarball.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// The path inside the package (without the `package/` prefix).
    pub path: String,
    pub size: u64,
    /// The tar mode after node-tar's portable-mode normalization (`0o644` and `0o755` for the
    /// usual files, a `0o600` or `0o750` kept as such), with the executable bits set for a `bin`
    /// target.
    pub mode: u32,
}

/// What was packed: the report npm prints for a tarball, without the bytes.
#[derive(Debug, Clone)]
pub struct Packed {
    pub name: String,
    pub version: String,
    /// `<name>-<version>.tgz`, a scope's `@` dropped and its `/` turned into `-`.
    pub filename: String,
    /// The files in tar order.
    pub files: Vec<Entry>,
    pub unpacked_size: u64,
    /// The gzipped tarball's size in bytes.
    pub size: u64,
    /// The sha1 of the tarball, hex.
    pub shasum: String,
    /// The sha512 of the tarball as a Subresource Integrity string.
    pub integrity: String,
}

/// A packed tarball held in memory, with the report npm prints for it.
#[derive(Debug, Clone)]
pub struct Tarball {
    pub packed: Packed,
    /// The gzipped tar.
    pub bytes: Vec<u8>,
}

impl Deref for Tarball {
    type Target = Packed;

    fn deref(&self) -> &Packed {
        &self.packed
    }
}

/// The files of the package at `dir` that npm would publish, `/`-separated and relative to `dir`,
/// in npm-packlist's order (extension, then basename, then path).
pub fn list(dir: &Path) -> Result<Vec<String>> {
    walker::walk(dir, &manifest(dir)?)
}

/// The tarball's filename for the package at `dir`, `<name>-<version>.tgz`, once the manifest's
/// name and version passed their allowlists.
pub fn filename(dir: &Path) -> Result<String> {
    let manifest = manifest(dir)?;
    Ok(tarball_filename(
        field(&manifest, "name"),
        field(&manifest, "version"),
    ))
}

/// The frozen contents of a pack: name, version, listing and bin targets, computed before any
/// output exists, so the output never enters the listing and every target sees the same files.
#[derive(Debug, Clone)]
pub struct Plan {
    dir: std::path::PathBuf,
    name: String,
    version: String,
    /// The files that will ship, in tar order.
    files: Vec<String>,
    bins: Vec<String>,
}

impl Plan {
    /// Read the manifest at `dir`, validate its name and version, and freeze the listing.
    pub fn new(dir: &Path) -> Result<Plan> {
        let manifest = manifest(dir)?;
        Ok(Plan {
            dir: dir.to_path_buf(),
            name: field(&manifest, "name").to_string(),
            version: field(&manifest, "version").to_string(),
            files: walker::walk(dir, &manifest)?,
            bins: walker::bin_targets(dir, &manifest),
        })
    }

    /// The paths that will ship, `/`-separated and relative to the package, in tar order.
    pub fn files(&self) -> &[String] {
        &self.files
    }

    /// `<name>-<version>.tgz`.
    pub fn filename(&self) -> String {
        tarball_filename(&self.name, &self.version)
    }

    /// Stream the tarball into `out`, hashing the bytes as they pass.
    pub fn write<W: Write>(&self, out: W) -> Result<Packed> {
        let mut builder =
            tar::Builder::new(GzEncoder::new(Digesting::new(out), Compression::best()));
        let mut files = Vec::new();
        let mut unpacked_size = 0;
        for path in &self.files {
            let full = self.dir.join(path);
            let meta = checked_meta(&full)?;
            let mode = mode_fix(unix_mode(&meta), self.bins.contains(path));
            let mut header = tar::Header::new_ustar();
            header.set_size(meta.len());
            header.set_mode(mode);
            header.set_mtime(MTIME);
            header.set_uid(0);
            header.set_gid(0);
            header.set_entry_type(tar::EntryType::Regular);
            let file = File::open(&full).map_err(|e| format!("{}: {e}", full.display()))?;
            builder
                .append_data(&mut header, format!("package/{path}"), file)
                .map_err(|e| format!("packing {path}: {e}"))?;
            unpacked_size += meta.len();
            files.push(Entry {
                path: path.clone(),
                size: meta.len(),
                mode,
            });
        }
        let mut digesting = builder.into_inner()?.finish()?;
        digesting.flush()?;
        Ok(Packed {
            name: self.name.clone(),
            version: self.version.clone(),
            filename: self.filename(),
            files,
            unpacked_size,
            size: digesting.written,
            shasum: format!("{:x}", digesting.sha1.finalize()),
            integrity: format!(
                "sha512-{}",
                base64::engine::general_purpose::STANDARD.encode(digesting.sha512.finalize())
            ),
        })
    }

    /// Stream the tarball into a temporary sibling of `path` and rename it onto `path` on
    /// success; a failed pack removes the temporary file and keeps what was at `path`.
    pub fn write_to_path(&self, path: &Path) -> Result<Packed> {
        let (temporary, file) = temporary_sibling(path)?;
        let result = self.write(io::BufWriter::new(file)).and_then(|packed| {
            std::fs::rename(&temporary, path)
                .map_err(|e| format!("moving the tarball onto {}: {e}", path.display()))?;
            Ok(packed)
        });
        if result.is_err() {
            let _ = std::fs::remove_file(&temporary);
        }
        result
    }
}

/// A fresh file beside `path` (`create_new`), named after the destination so a stray one is
/// recognizable; retried on a name clash.
fn temporary_sibling(path: &Path) -> Result<(std::path::PathBuf, File)> {
    let parent = path.parent().filter(|p| !p.as_os_str().is_empty());
    let stem = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "tarball".to_string());
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    for attempt in 0..16u32 {
        let name = format!(
            ".{stem}.{}.{}.part",
            std::process::id(),
            nanos.wrapping_add(attempt)
        );
        let candidate = match parent {
            Some(parent) => parent.join(&name),
            None => std::path::PathBuf::from(&name),
        };
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(file) => return Ok((candidate, file)),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(format!("creating {}: {e}", candidate.display()).into()),
        }
    }
    Err(format!("no free temporary name beside {}", path.display()).into())
}

/// Build the tarball npm would publish for the package at `dir`, in memory.
pub fn tarball(dir: &Path) -> Result<Tarball> {
    let mut bytes = Vec::new();
    let packed = Plan::new(dir)?.write(&mut bytes)?;
    Ok(Tarball { packed, bytes })
}

/// Stream the tarball npm would publish for the package at `dir` into `path` through a
/// temporary sibling, renamed on success; an existing artifact survives a failure and the
/// output never packs itself.
pub fn write_to_path(dir: &Path, path: &Path) -> Result<Packed> {
    Plan::new(dir)?.write_to_path(path)
}

/// Stream the tarball npm would publish for the package at `dir` into `out`, hashing the bytes
/// as they pass, so the package never has to fit in memory.
pub fn write<W: Write>(dir: &Path, out: W) -> Result<Packed> {
    Plan::new(dir)?.write(out)
}

/// A writer that hashes and counts what passes through it; npm's digests come out of the stream.
struct Digesting<W: Write> {
    inner: W,
    sha1: Sha1,
    sha512: Sha512,
    written: u64,
}

impl<W: Write> Digesting<W> {
    fn new(inner: W) -> Self {
        Digesting {
            inner,
            sha1: Sha1::new(),
            sha512: Sha512::new(),
            written: 0,
        }
    }
}

impl<W: Write> Write for Digesting<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.sha1.update(&buf[..n]);
        self.sha512.update(&buf[..n]);
        self.written += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

impl Packed {
    /// npm's report for one tarball: id, name, version, sizes, digests, filename, the files with
    /// their sizes and modes (uppercase-led paths first, as npm lists them), the entry count and
    /// the (never gathered) bundled dependencies. npm 12's `pack --json` prints it keyed by the
    /// package name, which is what the `pack` verb does too.
    pub fn report(&self) -> Value {
        let mut uppers: Vec<&Entry> = Vec::new();
        let mut others: Vec<&Entry> = Vec::new();
        for entry in &self.files {
            let first = entry.path.chars().next().unwrap_or(' ');
            if first.to_uppercase().next() == Some(first) {
                uppers.push(entry);
            } else {
                others.push(entry);
            }
        }
        uppers.sort_by(|a, b| collate(&a.path, &b.path, true));
        others.sort_by(|a, b| collate(&a.path, &b.path, true));
        let files: Vec<Value> = uppers
            .into_iter()
            .chain(others)
            .map(|entry| json!({ "path": entry.path, "size": entry.size, "mode": entry.mode }))
            .collect();
        json!({
            "id": format!("{}@{}", self.name, self.version),
            "name": self.name,
            "version": self.version,
            "size": self.size,
            "unpackedSize": self.unpacked_size,
            "shasum": self.shasum,
            "integrity": self.integrity,
            "filename": self.filename,
            "files": files,
            "entryCount": self.files.len(),
            "bundled": [],
        })
    }
}

/// The manifest, which must carry a `name` and a `version` to pack; both pass the crate's
/// path-safety allowlists first, since they become the tarball's filename.
fn manifest(dir: &Path) -> Result<Value> {
    let path = dir.join("package.json");
    let text =
        std::fs::read_to_string(&path).map_err(|e| format!("reading {}: {e}", path.display()))?;
    let manifest: Value =
        serde_json::from_str(&text).map_err(|e| format!("parsing {}: {e}", path.display()))?;
    for key in ["name", "version"] {
        if manifest.get(key).and_then(Value::as_str).is_none() {
            return Err(format!("{}: a package needs a {key} to pack", path.display()).into());
        }
    }
    let name = field(&manifest, "name");
    validate_package_name(name).map_err(|e| format!("{}: {e}", path.display()))?;
    if name.matches('/').count() > 1 {
        return Err(format!(
            "{}: package name {name:?} has more than one scope separator",
            path.display()
        )
        .into());
    }
    validate_version(field(&manifest, "version"))
        .map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(manifest)
}

/// `<name>-<version>.tgz`: a scope's `@` dropped and its `/` turned into `-`, so a validated
/// name and version yield one path segment.
fn tarball_filename(name: &str, version: &str) -> String {
    let filename = format!(
        "{}-{version}.tgz",
        name.replacen('@', "", 1).replacen('/', "-", 1)
    );
    debug_assert!(crate::path_safety::ensure_within(&filename).is_ok() && !filename.contains('/'));
    filename
}

fn field<'a>(manifest: &'a Value, key: &str) -> &'a str {
    manifest
        .get(key)
        .and_then(Value::as_str)
        .expect("the manifest was checked")
}

#[cfg(unix)]
fn unix_mode(meta: &std::fs::Metadata) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode()
}

#[cfg(not(unix))]
fn unix_mode(_meta: &std::fs::Metadata) -> u32 {
    0o644
}

/// The metadata of a listed file, re-checked without following symlinks: a path that stopped
/// being a regular file since the walk fails the pack instead of shipping its target. The
/// `File::open` after this still races in theory; std has no `O_NOFOLLOW` without libc.
fn checked_meta(full: &Path) -> Result<std::fs::Metadata> {
    let meta = std::fs::symlink_metadata(full).map_err(|e| format!("{}: {e}", full.display()))?;
    if !meta.is_file() {
        return Err(format!(
            "{}: not a regular file (changed since the walk?)",
            full.display()
        )
        .into());
    }
    Ok(meta)
}

/// node-tar's portable mode fix, `(mode | 0o600) & !0o022` within the permission bits (`0644`
/// and `0755` for the usual files, a `0600` or `0750` kept), plus the executable bits pacote
/// adds for a `bin`. The setuid, setgid and sticky bits are masked off; node-tar carries them,
/// a publishable tarball must not.
fn mode_fix(mode: u32, bin: bool) -> u32 {
    let mut mode = ((mode & 0o777) | 0o600) & !0o022;
    if bin {
        mode |= 0o111;
    }
    mode
}

/// A stand-in for the `en` locale comparison npm sorts with: punctuation before digits before
/// letters, letters compared case-insensitively first and lowercase first on a tie, and, when
/// `numeric`, digit runs compared by value.
pub(crate) fn collate(a: &str, b: &str, numeric: bool) -> Ordering {
    keys(a, numeric)
        .cmp(&keys(b, numeric))
        .then_with(|| case_order(a, b))
        .then_with(|| a.cmp(b))
}

#[derive(PartialEq, Eq, PartialOrd, Ord)]
enum Key {
    Char(u8, char),
    Number(u128),
}

fn keys(text: &str, numeric: bool) -> Vec<Key> {
    let mut out = Vec::new();
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if numeric && c.is_ascii_digit() {
            let mut value = u128::from(c.to_digit(10).unwrap_or(0));
            while let Some(d) = chars.peek().and_then(|d| d.to_digit(10)) {
                value = value.saturating_mul(10).saturating_add(u128::from(d));
                chars.next();
            }
            out.push(Key::Number(value));
            continue;
        }
        let rank = if c.is_whitespace() {
            0
        } else if c.is_ascii_punctuation() || (!c.is_alphanumeric() && !c.is_whitespace()) {
            1
        } else if c.is_numeric() {
            2
        } else {
            3
        };
        out.push(Key::Char(rank, c.to_lowercase().next().unwrap_or(c)));
    }
    out
}

/// Lowercase before uppercase at the first position where only the case differs.
fn case_order(a: &str, b: &str) -> Ordering {
    for (x, y) in a.chars().zip(b.chars()) {
        if x != y && x.to_lowercase().eq(y.to_lowercase()) {
            return x.is_lowercase().cmp(&y.is_lowercase()).reverse();
        }
    }
    Ordering::Equal
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn package(files: &[(&str, &str)], manifest: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for (path, content) in files {
            let full = dir.path().join(path);
            fs::create_dir_all(full.parent().unwrap()).unwrap();
            fs::write(full, content).unwrap();
        }
        fs::write(dir.path().join("package.json"), manifest).unwrap();
        dir
    }

    #[test]
    fn the_tarball_carries_prefixed_entries_fixed_mtime_and_normalized_modes() {
        let dir = package(
            &[
                ("dist/index.js", "export {}\n"),
                ("bin/cli.js", "#!/usr/bin/env node\n"),
            ],
            r#"{"name":"@scope/demo","version":"1.0.0","files":["dist"],"bin":{"demo":"./bin/cli.js"}}"#,
        );
        let tarball = tarball(dir.path()).unwrap();
        assert_eq!(tarball.filename, "scope-demo-1.0.0.tgz");
        let paths: Vec<&str> = tarball.files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(paths, ["bin/cli.js", "dist/index.js", "package.json"]);
        let cli = &tarball.files[0];
        assert_eq!(cli.mode, 0o755);
        assert_eq!(tarball.files[1].mode, 0o644);
        assert_eq!(
            tarball.unpacked_size,
            10 + 20 + fs::metadata(dir.path().join("package.json")).unwrap().len()
        );
        assert!(tarball.integrity.starts_with("sha512-"));
        assert_eq!(tarball.shasum.len(), 40);

        // The bytes are a gzipped tar the crate's own extractor unpacks under `package/`.
        let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(tarball.bytes.as_slice()));
        let entries: Vec<(String, u64, u32)> = archive
            .entries()
            .unwrap()
            .map(|e| {
                let e = e.unwrap();
                (
                    e.path().unwrap().to_string_lossy().into_owned(),
                    e.header().mtime().unwrap(),
                    e.header().mode().unwrap(),
                )
            })
            .collect();
        assert_eq!(
            entries,
            [
                ("package/bin/cli.js".to_string(), MTIME, 0o755),
                ("package/dist/index.js".to_string(), MTIME, 0o644),
                ("package/package.json".to_string(), MTIME, 0o644),
            ]
        );
        let out = tempfile::tempdir().unwrap();
        let written = crate::extract::tar_gz(
            &tarball.bytes,
            out.path(),
            Some("package/"),
            crate::extract::Select::All,
        )
        .unwrap();
        assert_eq!(written, 3);
        assert!(out.path().join("dist/index.js").is_file());

        // The report is npm's shape, uppercase-led paths first.
        let report = tarball.report();
        assert_eq!(report["id"], "@scope/demo@1.0.0");
        assert_eq!(report["entryCount"], 3);
        assert_eq!(report["bundled"], json!([]));
        assert_eq!(report["files"][0]["path"], "bin/cli.js");
        assert_eq!(report["files"][0]["mode"], 0o755);
        assert_eq!(report["size"], tarball.bytes.len());
    }

    #[test]
    fn a_name_or_version_that_could_traverse_is_refused() {
        for (manifest, needle) in [
            (r#"{"name":"demo","version":"1.0.0/../../escape"}"#, "'..'"),
            (
                r#"{"name":"demo","version":"1.0.0/escape"}"#,
                "disallowed characters",
            ),
            (r#"{"name":"../demo","version":"1.0.0"}"#, "'..'"),
            (r#"{"name":"@a/b/c","version":"1.0.0"}"#, "scope separator"),
            (
                r#"{"name":"demo","version":"1.0.0 && rm"}"#,
                "disallowed characters",
            ),
            (
                r#"{"name":"@scope/","version":"1.0.0"}"#,
                "not a relative name",
            ),
            (
                r#"{"name":"de\\mo","version":"1.0.0"}"#,
                "disallowed characters",
            ),
            (
                r#"{"name":"demo","version":"1.0.0\t"}"#,
                "disallowed characters",
            ),
        ] {
            let dir = package(&[], manifest);
            let error = tarball(dir.path()).unwrap_err().to_string();
            assert!(error.contains(needle), "{manifest}: {error}");
        }
        // A name at the length limit's far side is refused too.
        let long = format!(r#"{{"name":"{}","version":"1.0.0"}}"#, "a".repeat(201));
        let dir = package(&[], &long);
        let error = tarball(dir.path()).unwrap_err().to_string();
        assert!(error.contains("invalid length"), "{error}");
        assert_eq!(
            tarball_filename("@scope/demo", "1.2.3-rc.1+build"),
            "scope-demo-1.2.3-rc.1+build.tgz"
        );
    }

    #[cfg(unix)]
    #[test]
    fn checked_meta_refuses_a_symlinked_path() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real.js");
        fs::write(&real, "x").unwrap();
        assert!(checked_meta(&real).is_ok());
        let link = dir.path().join("link.js");
        symlink(&real, &link).unwrap();
        let error = checked_meta(&link).unwrap_err().to_string();
        assert!(error.contains("not a regular file"), "{error}");
    }

    #[test]
    fn a_directories_bin_target_is_executable() {
        let dir = package(
            &[("bin/cli.js", "#!/usr/bin/env node\n")],
            r#"{"name":"demo","version":"1.0.0","files":[],"directories":{"bin":"bin"}}"#,
        );
        let tarball = tarball(dir.path()).unwrap();
        let cli = tarball
            .files
            .iter()
            .find(|f| f.path == "bin/cli.js")
            .expect("the bin ships");
        assert_eq!(cli.mode, 0o755);
    }

    #[test]
    fn the_stream_and_the_buffer_agree() {
        let dir = package(
            &[("dist/index.js", "export {}\n")],
            r#"{"name":"demo","version":"1.0.0","files":["dist"]}"#,
        );
        let buffered = tarball(dir.path()).unwrap();
        let mut streamed = Vec::new();
        let packed = write(dir.path(), &mut streamed).unwrap();
        assert_eq!(streamed, buffered.bytes);
        assert_eq!(packed.size, buffered.bytes.len() as u64);
        assert_eq!(packed.shasum, buffered.shasum);
        assert_eq!(packed.integrity, buffered.integrity);
        let sunk = write(dir.path(), std::io::sink()).unwrap();
        assert_eq!(
            sunk.integrity, packed.integrity,
            "the digests come out of the stream"
        );
        let out = tempfile::tempdir().unwrap();
        let target = out.path().join("demo-1.0.0.tgz");
        let on_disk = write_to_path(dir.path(), &target).unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), buffered.bytes);
        assert_eq!(on_disk.report()["size"], buffered.bytes.len());
        assert_eq!(filename(dir.path()).unwrap(), "demo-1.0.0.tgz");
        assert_eq!(
            Plan::new(dir.path()).unwrap().files(),
            ["dist/index.js", "package.json"]
        );
    }

    #[test]
    fn packing_into_the_package_directory_never_packs_the_output() {
        // `pack .` writes beside the sources: the listing is frozen before the output exists,
        // so neither the temporary sibling nor the tarball can enter the archive.
        let dir = package(
            &[("index.js", "console.log(1)\n")],
            r#"{"name":"demo","version":"1.0.0"}"#,
        );
        let target = dir.path().join("demo-1.0.0.tgz");
        let first = write_to_path(dir.path(), &target).unwrap();
        let paths: Vec<&str> = first.files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(paths, ["index.js", "package.json"]);
        let leftovers: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".part"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
        // A second pack sees the earlier artifact as ordinary package content, as npm does,
        // but still never its own output.
        let second = write_to_path(dir.path(), &target).unwrap();
        let paths: Vec<&str> = second.files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(paths, ["index.js", "package.json", "demo-1.0.0.tgz"]);
        let earlier = second
            .files
            .iter()
            .find(|f| f.path == "demo-1.0.0.tgz")
            .unwrap();
        assert_eq!(earlier.size, first.size, "the earlier artifact, whole");
    }

    #[test]
    fn a_failed_pack_leaves_the_existing_artifact_untouched() {
        let dir = package(
            &[("index.js", "console.log(1)\n")],
            r#"{"name":"demo","version":"1.0.0"}"#,
        );
        let out = tempfile::tempdir().unwrap();
        let target = out.path().join("demo-1.0.0.tgz");
        std::fs::write(&target, b"the previous artifact").unwrap();
        std::fs::write(dir.path().join(".npmignore"), "{1..100000000}\n").unwrap();
        let error = write_to_path(dir.path(), &target).unwrap_err().to_string();
        assert!(error.contains("{1..100000000}"), "{error}");
        assert_eq!(std::fs::read(&target).unwrap(), b"the previous artifact");
        assert_eq!(
            std::fs::read_dir(out.path()).unwrap().count(),
            1,
            "no temporary file lingers"
        );
    }

    #[test]
    fn a_pack_failing_mid_stream_cleans_up_and_keeps_the_previous_artifact() {
        // The plan freezes the listing, then a listed file vanishes: the stream fails after the
        // temporary sibling exists, and the cleanup branch must remove it.
        let dir = package(
            &[("index.js", "console.log(1)\n")],
            r#"{"name":"demo","version":"1.0.0"}"#,
        );
        let out = tempfile::tempdir().unwrap();
        let target = out.path().join("demo-1.0.0.tgz");
        std::fs::write(&target, b"the previous artifact").unwrap();
        let plan = Plan::new(dir.path()).unwrap();
        std::fs::remove_file(dir.path().join("index.js")).unwrap();
        let error = plan.write_to_path(&target).unwrap_err().to_string();
        assert!(error.contains("index.js"), "{error}");
        assert_eq!(std::fs::read(&target).unwrap(), b"the previous artifact");
        assert_eq!(
            std::fs::read_dir(out.path()).unwrap().count(),
            1,
            "no temporary file lingers"
        );
    }

    #[test]
    fn packing_needs_a_name_and_a_version() {
        let dir = package(&[], r#"{"name":"demo"}"#);
        let error = tarball(dir.path()).unwrap_err().to_string();
        assert!(error.contains("needs a version"), "{error}");
        assert!(list(dir.path()).is_err());
    }

    #[test]
    fn modes_follow_node_tar_portable_rules() {
        assert_eq!(mode_fix(0o664, false), 0o644);
        assert_eq!(mode_fix(0o600, false), 0o600);
        assert_eq!(mode_fix(0o775, false), 0o755);
        assert_eq!(mode_fix(0o644, true), 0o755);
        assert_eq!(mode_fix(0o100644, false), 0o644);
        // The privilege bits never reach an entry: a publishable tarball is `0644`/`0755`.
        assert_eq!(mode_fix(0o4755, false), 0o755);
        assert_eq!(mode_fix(0o4755, true), 0o755);
        assert_eq!(mode_fix(0o2750, false), 0o750);
        assert_eq!(mode_fix(0o1777, false), 0o755);
    }

    #[test]
    fn the_collation_puts_punctuation_first_and_reads_numbers_by_value() {
        let mut names = vec!["b.js", "B.js", "a10.js", "a2.js", "_x.js", "a.js"];
        names.sort_by(|a, b| collate(a, b, true));
        assert_eq!(names, ["_x.js", "a.js", "a2.js", "a10.js", "b.js", "B.js"]);
        let mut plain = vec!["a10.js", "a2.js"];
        plain.sort_by(|a, b| collate(a, b, false));
        assert_eq!(plain, ["a10.js", "a2.js"]);
    }
}
