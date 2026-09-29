//! `pack`: the files a package directory publishes and their tarball (= `npm pack`), pure Rust.

use std::path::Path;

use super::Res;
use crate::pack;

/// Pack the package at `dir`. The listing is frozen first; with `dry_run` the tarball then
/// streams into a sink and nothing is written, else into a temporary sibling that replaces the
/// destination once the pack succeeded. With `json` the report is npm 12's `pack --json` object
/// on stdout, keyed by the package name, else the contents go to stderr as npm's notice block
/// and the tarball's filename to stdout.
pub(super) fn run(dir: &Path, dry_run: bool, json: bool, destination: Option<&Path>) -> Res {
    let plan = pack::Plan::new(dir)?;
    let tarball = if dry_run {
        plan.write(std::io::sink())?
    } else {
        let destination = destination.unwrap_or(Path::new("."));
        std::fs::create_dir_all(destination)?;
        // The filename derives from the manifest; both checks fail closed even though the
        // name and version passed their allowlists.
        let root = destination.canonicalize()?;
        let target = crate::path_safety::contained_target(
            &root,
            &crate::path_safety::safe_join(&root, &plan.filename())?,
        )?;
        plan.write_to_path(&target)?
    };
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({ &tarball.name: tarball.report() }))?
        );
        return Ok(());
    }
    eprintln!("package: {}@{}", tarball.name, tarball.version);
    eprintln!("Tarball Contents");
    for entry in &tarball.files {
        eprintln!("{} {}", format_bytes(entry.size), entry.path);
    }
    eprintln!("Tarball Details");
    eprintln!("name: {}", tarball.name);
    eprintln!("version: {}", tarball.version);
    eprintln!("filename: {}", tarball.filename);
    eprintln!("package size: {}", format_bytes(tarball.size));
    eprintln!("unpacked size: {}", format_bytes(tarball.unpacked_size));
    eprintln!("shasum: {}", tarball.shasum);
    eprintln!("integrity: {}", tarball.integrity);
    eprintln!("total files: {}", tarball.files.len());
    println!("{}", tarball.filename);
    Ok(())
}

/// npm's byte rendering: `B`, `kB`, `MB`, `GB` with one decimal.
fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "kB", "MB", "GB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1000.0 && unit < UNITS.len() - 1 {
        value /= 1000.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::format_bytes;

    #[test]
    fn bytes_render_like_npm() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(999), "999 B");
        assert_eq!(format_bytes(1234), "1.2 kB");
        assert_eq!(format_bytes(12_345_678), "12.3 MB");
    }
}
