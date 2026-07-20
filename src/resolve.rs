//! Locate files inside an installed dependency under `node_modules/`.
//!
//! [`package_dir`] finds `node_modules/<name>` by walking up from a starting
//! directory, the way Node resolves a bare specifier; [`package_file`] then maps a
//! `<name>/<subpath>` reference to the real file on disk, honoring the package's
//! `exports` (and its encapsulation) when declared and addressing the file directly
//! when not. Every result is confined to the resolved package directory via
//! [`crate::path_safety`] — nothing here writes, fetches over the network, or
//! resolves a reference outside the package it located.

use std::path::{Path, PathBuf};

use crate::package_json::{validate_package_name, PackageJson};
use crate::path_safety::safe_join;
use crate::Result;

/// Find an installed package by walking `node_modules/<name>` upward from `from_dir`
/// (Node's module-resolution order), returning the first directory that exists.
/// `name` may be scoped (`@scope/pkg`). Errors when the package is installed nowhere
/// on the way up — run `npm install` / `web-modules ci` first.
pub fn package_dir(from_dir: &Path, name: &str) -> Result<PathBuf> {
    validate_package_name(name)?;
    let mut cursor = Some(from_dir);
    while let Some(dir) = cursor {
        let candidate = dir.join("node_modules").join(name);
        if candidate.is_dir() {
            return Ok(candidate);
        }
        cursor = dir.parent();
    }
    Err(format!(
        "package {name:?} is not installed in any node_modules above {}",
        from_dir.display()
    )
    .into())
}

/// Resolve `<name>/<subpath>` to a real file inside the installed package.
///
/// When the package declares `exports`, only the subpaths it maps resolve — one it
/// does not is refused, mirroring Node's `ERR_PACKAGE_PATH_NOT_EXPORTED`; otherwise
/// the file is addressed directly (Node's behavior for a package without `exports`,
/// e.g. `bootstrap-icons`). The result is canonicalized and guaranteed to sit inside the
/// package's real directory — an in-package symlink resolving outside it is refused.
pub fn package_file(from_dir: &Path, name: &str, subpath: &str) -> Result<PathBuf> {
    let dir = package_dir(from_dir, name)?;
    let manifest = dir.join("package.json");
    let relative = if manifest.is_file() {
        let package = PackageJson::from_path(&manifest)?;
        match package.resolve_asset(subpath) {
            Some(relative) => relative,
            None if package.has_exports() => {
                return Err(format!(
                    "{name}/{subpath}: not exported by {name} \
                     (its package.json `exports` does not map this subpath)"
                )
                .into())
            }
            None => {
                return Err(format!("{name}/{subpath}: refuses an unsafe or empty subpath").into())
            }
        }
    } else {
        // A package without a manifest is unusual; address the file directly, with
        // path-safety applied by `safe_join` below.
        subpath
            .strip_prefix("./")
            .unwrap_or(subpath)
            .trim_start_matches('/')
            .to_string()
    };
    let candidate = safe_join(&dir, &relative)?;
    // Resolve through the package's real on-disk location and require the result to stay
    // inside it. `subpath` is attacker-influenced (an `npm://` symlink target, or a
    // dependency's own layout), and a symlink *inside* the package could otherwise
    // redirect the read outside it — so containment is enforced on the canonical path,
    // not on the joined string `safe_join` only checks structurally.
    let package_root = dir.canonicalize()?;
    let real = candidate
        .canonicalize()
        .map_err(|e| -> crate::Error { format!("{name}/{subpath}: {e}").into() })?;
    if !real.starts_with(&package_root) {
        return Err(format!("{name}/{subpath}: resolves outside {name}").into());
    }
    if !real.is_file() {
        return Err(format!("{name}/{subpath}: not a file in the package").into());
    }
    Ok(real)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    /// Lay out `<root>/node_modules/<name>/` with a `package.json` (`manifest`) and a
    /// set of `(relative-path, contents)` files.
    fn install(root: &Path, name: &str, manifest: &str, files: &[(&str, &str)]) {
        let pkg = root.join("node_modules").join(name);
        fs::create_dir_all(&pkg).unwrap();
        fs::write(pkg.join("package.json"), manifest).unwrap();
        for (rel, body) in files {
            let path = pkg.join(rel);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, body).unwrap();
        }
    }

    #[test]
    fn package_dir_walks_up_to_node_modules() {
        let tmp = tempdir().unwrap();
        install(
            tmp.path(),
            "bootstrap-icons",
            r#"{"name":"bootstrap-icons"}"#,
            &[],
        );
        let deep = tmp.path().join("web").join("icons").join("bi");
        fs::create_dir_all(&deep).unwrap();
        assert_eq!(
            package_dir(&deep, "bootstrap-icons").unwrap(),
            tmp.path().join("node_modules").join("bootstrap-icons")
        );
        assert!(package_dir(&deep, "not-installed").is_err());
    }

    #[test]
    fn package_file_addresses_directly_without_exports() {
        let tmp = tempdir().unwrap();
        install(
            tmp.path(),
            "bootstrap-icons",
            r#"{"name":"bootstrap-icons","files":["icons/*.svg"]}"#,
            &[("icons/eye.svg", "<svg/>")],
        );
        let start = tmp.path().join("web");
        fs::create_dir_all(&start).unwrap();
        let file = package_file(&start, "bootstrap-icons", "icons/eye.svg").unwrap();
        assert_eq!(fs::read_to_string(&file).unwrap(), "<svg/>");
        // A file the package does not contain errors rather than returning a missing path.
        assert!(package_file(&start, "bootstrap-icons", "icons/missing.svg").is_err());
        // Traversal is refused.
        assert!(package_file(&start, "bootstrap-icons", "../escape.svg").is_err());
    }

    #[test]
    fn package_file_honors_exports_encapsulation() {
        let tmp = tempdir().unwrap();
        install(
            tmp.path(),
            "guarded",
            r#"{"exports":{"./icons/*":"./dist/icons/*.svg"}}"#,
            &[("dist/icons/eye.svg", "<svg/>"), ("secret.txt", "nope")],
        );
        let file = package_file(tmp.path(), "guarded", "icons/eye").unwrap();
        assert!(file.ends_with("dist/icons/eye.svg"));
        // Present on disk but outside `exports` → refused.
        assert!(package_file(tmp.path(), "guarded", "secret.txt").is_err());
    }

    #[test]
    fn scoped_package_resolves() {
        let tmp = tempdir().unwrap();
        install(
            tmp.path(),
            "@scope/pkg",
            r#"{"name":"@scope/pkg"}"#,
            &[("a/b.svg", "x")],
        );
        let file = package_file(tmp.path(), "@scope/pkg", "a/b.svg").unwrap();
        assert_eq!(fs::read_to_string(file).unwrap(), "x");
    }

    #[test]
    fn package_dir_refuses_an_absolute_name_before_touching_the_disk() {
        // `Path::join` with an absolute name would replace the base — the name guard
        // fires first, so `/etc` is a validation error, not a lookup.
        let tmp = tempdir().unwrap();
        assert!(package_dir(tmp.path(), "/etc").is_err());
        assert!(package_file(tmp.path(), "/", "etc/passwd").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn package_file_refuses_an_in_package_symlink_that_escapes() {
        use std::os::unix::fs::symlink;
        let tmp = tempdir().unwrap();
        install(
            tmp.path(),
            "pkg",
            r#"{"name":"pkg"}"#,
            &[("ok.svg", "<svg/>")],
        );
        // A secret outside the package, and an in-package symlink pointing at it.
        std::fs::write(tmp.path().join("secret.txt"), "top secret").unwrap();
        let pkg = tmp.path().join("node_modules").join("pkg");
        symlink(tmp.path().join("secret.txt"), pkg.join("leak.txt")).unwrap();

        // The escaping symlink is refused; an ordinary in-package file still resolves.
        assert!(package_file(tmp.path(), "pkg", "leak.txt").is_err());
        assert!(package_file(tmp.path(), "pkg", "ok.svg").is_ok());
    }
}
