//! npm-packlist's walk: which files of a package directory ship.
//!
//! Every level carries rule sets in ignore-walk's order: the defaults, the `files` allowlist
//! (root only), `.npmignore`, `.gitignore`, the strict set. The `files` entries expand as globs
//! against the package root in manifest order, as npm-packlist 11 does; an allowlist silences
//! the root ignore files, a `.npmignore` silences the `.gitignore` beside it, and a silenced
//! file is not read. A child level first asks its parent about `dir/entry`, then applies its
//! own rules; a match sets the verdict to the rule's negation flag, so the last match wins.
//! Only regular files ship.
//!
//! Above the rules sits the never-ship veto ([`never_ship`]): `.npmrc`, the VCS directories,
//! `node_modules`, the root lockfiles, a root `.npm-extension.*` and the patch files of
//! `patchedDependencies` are skipped before any rule runs and filtered again at the end.
//! npm 9.2 and npm-packlist 11.3.0 ship them through their rule ordering; this crate deviates
//! on purpose, see docs/pack.md.

use std::path::Path;
use std::rc::Rc;

use serde_json::Value;

use super::collate;
use super::pattern::Rule;
use super::Settings;
use crate::Result;

/// The depth the walk descends to. The walk and the rule filter recurse per level, and a few
/// hundred levels overflow a small caller-side stack; real trees have a few dozen. A deeper
/// tree is an error naming the directory. The cap bounds the `directories.bin` expansion too.
const MAX_DEPTH: usize = 64;

/// npm-packlist's default rules, applied at every level.
const DEFAULTS: &[&str] = &[
    ".npmignore",
    ".gitignore",
    "**/.git",
    "**/.svn",
    "**/.hg",
    "**/CVS",
    "**/.git/**",
    "**/.svn/**",
    "**/.hg/**",
    "**/CVS/**",
    "/.lock-wscript",
    "/.wafpickle-*",
    "/build/config.gypi",
    "npm-debug.log",
    "**/.npmrc",
    ".*.swp",
    ".DS_Store",
    "**/.DS_Store/**",
    "._*",
    "**/._*/**",
    "*.orig",
    "/archived-packages/**",
];

/// The strict rules every level starts with.
const STRICT_DEFAULTS: &[&str] = &["/.git"];

/// The root's strict rules after the defaults: what npm always ships and never ships. The
/// never-ship rules here are advisory; the hard veto below is the guarantee.
const ROOT_STRICT: &[&str] = &[
    "!/package.json",
    "!/readme{,.*[^~$]}",
    "!/copying{,.*[^~$]}",
    "!/license{,.*[^~$]}",
    "!/licence{,.*[^~$]}",
    "/.git",
    "/node_modules",
    ".npmrc",
    "/package-lock.json",
    "/npm-shrinkwrap.json",
    "/yarn.lock",
    "/pnpm-lock.yaml",
    "/bun.lockb",
    "/bun.lock",
    "/.npm-extension.mjs",
    "/.npm-extension.cjs",
];

/// The never-ship set, a hard veto no `main`, `browser`, `bin`, `files` entry or nested
/// `.npmignore` negation overrides. npm 9.2 and npm-packlist 11.3.0 ship these through their
/// rule ordering; they carry credentials and VCS metadata, and a package that means to publish
/// them is broken or hostile. Case-folded with the matcher's [`fold`].
const VETO_COMPONENTS: &[&str] = &[".npmrc", ".git", ".svn", ".hg", "cvs", "node_modules"];

/// The lockfiles vetoed at the root only: a nested `packages/x/yarn.lock` is ordinary content
/// (npm anchors these at `/`), but a root lockfile leaks resolved registry URLs and pins.
const VETO_ROOT_FILES: &[&str] = &[
    "package-lock.json",
    "npm-shrinkwrap.json",
    "yarn.lock",
    "pnpm-lock.yaml",
    "bun.lockb",
    "bun.lock",
];

/// The prefix of the root files vetoed by name: npm 12's `.npm-extension.mjs` and `.cjs`.
const VETO_ROOT_PREFIX: &str = ".npm-extension.";

/// One lowercase form per character for the never-ship veto, so a vetoed spelling cannot slip
/// past in a different case.
fn fold(c: char) -> char {
    c.to_lowercase().next().unwrap_or(c)
}

fn folded_eq(a: &str, b: &str) -> bool {
    a.chars().map(fold).eq(b.chars().map(fold))
}

fn folded_starts_with(a: &str, prefix: &str) -> bool {
    let mut a = a.chars().map(fold);
    prefix.chars().map(fold).all(|p| a.next() == Some(p))
}

/// `true` when a root-level `name` is a vetoed lockfile or `.npm-extension.*` file.
fn vetoed_root_file(name: &str) -> bool {
    VETO_ROOT_FILES.iter().any(|v| folded_eq(name, v)) || folded_starts_with(name, VETO_ROOT_PREFIX)
}

/// `true` when the entry `name` at level `rel` may neither ship nor be descended into.
fn vetoed_name(rel: &str, name: &str) -> bool {
    VETO_COMPONENTS.iter().any(|v| folded_eq(name, v)) || (rel.is_empty() && vetoed_root_file(name))
}

/// `true` when a walked path is in the never-ship set: a vetoed component at any depth, or a
/// root-level lockfile or `.npm-extension.*` file.
fn never_ship(rel: &str) -> bool {
    rel.split('/')
        .any(|c| VETO_COMPONENTS.iter().any(|v| folded_eq(c, v)))
        || (!rel.contains('/') && vetoed_root_file(rel))
}

/// One directory level of the walk.
struct Level {
    /// The directory's own name; a child's parent matches `basename/entry`.
    basename: String,
    /// ignore-walk's `exact`: the directory passed as a file or as `dir/`, so an ancestor's
    /// exclusion does not settle its entries before this level's own rules run.
    exact: bool,
    defaults: Rc<Vec<Rule>>,
    /// The `files` allowlist, root only.
    allowlist: Option<Vec<Rule>>,
    npmignore: Option<Vec<Rule>>,
    gitignore: Option<Vec<Rule>>,
    strict: Rc<Vec<Rule>>,
}

impl Level {
    /// The rule sets in consultation order, absent ones skipped.
    fn sets(&self) -> impl Iterator<Item = &Vec<Rule>> {
        [
            Some(&*self.defaults),
            self.allowlist.as_ref(),
            self.npmignore.as_ref(),
            self.gitignore.as_ref(),
            Some(&*self.strict),
        ]
        .into_iter()
        .flatten()
    }

    /// ignore-walk's precedence: an allowlist silences both ignore files, a `.npmignore` silences
    /// the `.gitignore` beside it.
    fn silence(&mut self) {
        if self.allowlist.is_some() {
            self.npmignore = None;
            self.gitignore = None;
        } else if self.npmignore.is_some() {
            self.gitignore = None;
        }
    }
}

fn rules<S: AsRef<str>>(lines: impl IntoIterator<Item = S>, quirks: bool) -> Result<Vec<Rule>> {
    lines
        .into_iter()
        .map(|line| Rule::parse(line.as_ref(), quirks))
        .filter_map(Result::transpose)
        .collect()
}

/// The rules of `dir/<name>` when that ignore file exists.
fn ignore_file(dir: &Path, name: &str, quirks: bool) -> Result<Option<Vec<Rule>>> {
    let path = dir.join(name);
    if !path.is_file() {
        return Ok(None);
    }
    let text =
        std::fs::read_to_string(&path).map_err(|e| format!("reading {}: {e}", path.display()))?;
    Ok(Some(
        rules(text.lines(), quirks).map_err(|e| format!("{}: {e}", path.display()))?,
    ))
}

/// The ignore files of one directory that precedence keeps.
struct IgnoreFiles {
    npmignore: Option<Vec<Rule>>,
    gitignore: Option<Vec<Rule>>,
}

/// The ignore rules of `dir` in ignore-walk's precedence: none under an allowlist, the
/// `.npmignore` when one is present, else the `.gitignore`. A file precedence drops is not read.
fn ignore_files(dir: &Path, allowlisted: bool, quirks: bool) -> Result<IgnoreFiles> {
    let (npmignore, gitignore) = if allowlisted {
        (None, None)
    } else if ignore_file_present(dir, ".npmignore") {
        (ignore_file(dir, ".npmignore", quirks)?, None)
    } else {
        (None, ignore_file(dir, ".gitignore", quirks)?)
    };
    Ok(IgnoreFiles {
        npmignore,
        gitignore,
    })
}

/// Whether `dir/<name>` is a file or a symlink, which is what precedence counts.
fn ignore_file_present(dir: &Path, name: &str) -> bool {
    std::fs::symlink_metadata(dir.join(name))
        .map(|meta| meta.is_file() || meta.file_type().is_symlink())
        .unwrap_or(false)
}

/// A manifest declaring bundled dependencies is refused; npm packs their `node_modules` trees,
/// this walk does not gather them.
fn refuse_bundles(root: &Path, manifest: &Value) -> Result<()> {
    for key in ["bundleDependencies", "bundledDependencies"] {
        let declared = match manifest.get(key) {
            Some(Value::Array(list)) => !list.is_empty(),
            Some(Value::Bool(all)) => *all,
            Some(Value::Object(map)) => !map.is_empty(),
            _ => false,
        };
        if declared {
            return Err(format!(
                "{}: {key} declares bundled dependencies, which this packer does not gather",
                root.join("package.json").display()
            )
            .into());
        }
    }
    Ok(())
}

/// The files of the package at `root` that ship, `/`-separated and relative to it, in
/// npm-packlist's order (extension, then basename, then path).
pub(crate) fn walk(root: &Path, manifest: &Value, settings: &Settings) -> Result<Vec<String>> {
    refuse_bundles(root, manifest)?;
    let quirks = settings.quirks;
    let RootRules {
        allowlist,
        strict,
        vetoed,
    } = package_rules(root, manifest, quirks)?;
    // The stock sets compile once; every level below the root shares them.
    let shared = Shared {
        defaults: Rc::new(rules(DEFAULTS, quirks)?),
        strict: Rc::new(rules(STRICT_DEFAULTS, quirks)?),
        quirks,
        vetoed,
    };
    let IgnoreFiles {
        npmignore,
        gitignore,
    } = ignore_files(root, allowlist.is_some(), quirks)?;
    if gitignore.is_some() {
        crate::warn::warn(
            "no .npmignore file found, using .gitignore for file exclusion; a .npmignore \
             controls the published files explicitly",
        );
    }
    let level = Level {
        basename: String::new(),
        exact: false,
        defaults: Rc::clone(&shared.defaults),
        allowlist,
        npmignore,
        gitignore,
        strict: Rc::new(strict),
    };
    let mut levels = vec![level];
    let mut out = Vec::new();
    walk_dir(root, "", &mut levels, &shared, &mut out)?;
    // Backstop at the single choke point every inclusion mechanism flows through: nothing
    // vetoed leaves `walk`, whatever the rules decided.
    out.retain(|path| !never_ship(path) && !shared.vetoed.contains(path));
    out.sort_by(|a, b| packlist_order(a, b));
    Ok(out)
}

/// What the manifest contributes at the root.
struct RootRules {
    /// The `files` allowlist, when the manifest has one.
    allowlist: Option<Vec<Rule>>,
    strict: Vec<Rule>,
    /// The exact paths the veto holds beyond the names: the patch files of `patchedDependencies`.
    vetoed: Vec<String>,
}

fn package_rules(root: &Path, manifest: &Value, quirks: bool) -> Result<RootRules> {
    let mut strict: Vec<String> = STRICT_DEFAULTS
        .iter()
        .chain(ROOT_STRICT)
        .map(|s| s.to_string())
        .collect();
    let mut ignores: Vec<String> = Vec::new();
    let mut vetoed: Vec<String> = Vec::new();
    let files = manifest.get("files").and_then(Value::as_array);
    if let Some(files) = files {
        // npm-packlist 11: every entry is a glob against the package root, expanded in
        // manifest order; a positive entry un-ignores what it matches (a directory with its
        // contents), a `!` entry re-ignores it, and an entry matching nothing drops silently.
        for entry in files.iter().filter_map(Value::as_str) {
            let negation = entry.starts_with('!');
            let pattern = strip_files_entry(if negation {
                entry.trim_start_matches('!')
            } else {
                entry
            });
            let Some(glob) = Rule::parse_glob(pattern, quirks)? else {
                continue;
            };
            let prefix = if negation { "" } else { "!" };
            for (rel, is_dir) in expand_glob(root, &glob)? {
                ignores.push(format!("{prefix}/{rel}"));
                if is_dir {
                    ignores.push(format!("{prefix}/{rel}/**"));
                }
            }
        }
    }
    let allowlist = if files.is_some() {
        let mut lines = vec!["*".to_string()];
        lines.extend(ignores.iter().cloned());
        Some(rules(&lines, quirks)?)
    } else {
        None
    };
    if let Some(browser) = manifest.get("browser").and_then(Value::as_str) {
        strict.push(format!("!/{browser}"));
    }
    if let Some(main) = manifest.get("main").and_then(Value::as_str) {
        strict.push(format!("!/{main}"));
    }
    for bin in bin_targets(root, manifest) {
        strict.push(format!("!/{bin}"));
    }
    // The patch files of `patchedDependencies` are project-local fixes and never ship, even
    // when a `files` entry pulled them in; only the exact files, never their directory.
    if let Some(patches) = manifest
        .get("patchedDependencies")
        .and_then(Value::as_object)
    {
        for patch in patches.values().filter_map(Value::as_str) {
            let unixified = patch.replace('\\', "/");
            let rel = strip_files_entry(&unixified);
            if rel.is_empty()
                || rel.starts_with('/')
                || rel == ".."
                || rel.starts_with("../")
                || rel.contains("/../")
            {
                continue;
            }
            let parent = match rel.rsplit_once('/') {
                Some((dir, _)) => format!("!/{dir}/**"),
                None => "!/./**".to_string(),
            };
            if files.is_some()
                && (ignores.contains(&format!("!/{rel}")) || ignores.contains(&parent))
            {
                crate::warn::warn(&format!(
                    "excluding {rel:?} from the package tarball: patch files in \
                     patchedDependencies must not be published"
                ));
            }
            strict.push(format!("/{rel}"));
            vetoed.push(rel.to_string());
        }
    }
    Ok(RootRules {
        allowlist,
        strict: rules(&strict, quirks)?,
        vetoed,
    })
}

/// npm-packlist's normalization of a `files` entry: one leading `./` or `/` and any trailing
/// slashes go, the rest is the glob.
fn strip_files_entry(entry: &str) -> &str {
    let entry = entry
        .strip_prefix("./")
        .or_else(|| entry.strip_prefix('/'))
        .unwrap_or(entry);
    entry.trim_end_matches('/')
}

/// The paths beneath `root` a `files` glob matches, as (`/`-separated relative path, is a
/// directory), sorted: regular files and directories only, symlinks and special files never
/// (npm's glob runs `follow:false` and packlist drops what is neither file nor directory), the
/// never-ship names skipped since they could not ship anyway. A directory is descended only
/// while the glob can still match beneath it.
fn expand_glob(root: &Path, glob: &Rule) -> Result<Vec<(String, bool)>> {
    let mut out = Vec::new();
    expand_glob_dir(root, "", glob, 0, &mut out)?;
    out.sort();
    Ok(out)
}

fn expand_glob_dir(
    dir: &Path,
    rel: &str,
    glob: &Rule,
    depth: usize,
    out: &mut Vec<(String, bool)>,
) -> Result<()> {
    if depth > MAX_DEPTH {
        return Err(format!("{}: deeper than the {MAX_DEPTH}-level limit", dir.display()).into());
    }
    let mut names: Vec<String> = Vec::new();
    for entry in std::fs::read_dir(dir).map_err(|e| format!("reading {}: {e}", dir.display()))? {
        if let Ok(name) = entry?.file_name().into_string() {
            names.push(name);
        }
    }
    names.sort();
    for name in names {
        if name.contains('*') || vetoed_name(rel, &name) {
            continue;
        }
        let path = dir.join(&name);
        let meta =
            std::fs::symlink_metadata(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        let child_rel = if rel.is_empty() {
            name.clone()
        } else {
            format!("{rel}/{name}")
        };
        if meta.is_dir() {
            if glob.matches(&child_rel, false)? {
                out.push((child_rel.clone(), true));
            }
            if glob.matches(&child_rel, true)? {
                expand_glob_dir(&path, &child_rel, glob, depth + 1, out)?;
            }
        } else if meta.is_file() && glob.matches(&child_rel, false)? {
            out.push((child_rel, false));
        }
    }
    Ok(())
}

/// The `bin` targets as npm normalizes them (`@npmcli/package-json`): a string is the one bin
/// of a named package, an array is keyed by basename, an object by its keys; every key's
/// basename and every target pass [`secure_path`], an entry whose key or target secures to
/// nothing is dropped, and a later entry replaces an earlier one under the same basename. Any
/// other `bin` value means no bins. Without a `bin`, the entries beneath `directories.bin`
/// stand in, dotfiles excluded; a `bin` of any kind, an empty object included, leaves
/// `directories.bin` unexpanded.
pub(super) fn bin_targets(root: &Path, manifest: &Value) -> Vec<String> {
    let bin = manifest.get("bin");
    if bin_is_falsy(bin) {
        return directory_bins(root, manifest);
    }
    let pairs: Vec<(String, String)> = match bin {
        Some(Value::String(path)) => manifest
            .get("name")
            .and_then(Value::as_str)
            .map(|name| vec![(name.to_string(), path.clone())])
            .unwrap_or_default(),
        Some(Value::Array(list)) => list
            .iter()
            .filter_map(Value::as_str)
            .map(|path| (path.to_string(), path.to_string()))
            .collect(),
        Some(Value::Object(map)) => map
            .iter()
            .filter_map(|(key, value)| value.as_str().map(|v| (key.clone(), v.to_string())))
            .collect(),
        _ => Vec::new(),
    };
    let mut by_name: Vec<(String, String)> = Vec::new();
    for (key, value) in pairs {
        let base = basename(&secure_path(&key)).to_string();
        let target = secure_path(&value);
        if base.is_empty() || target.is_empty() {
            continue;
        }
        match by_name.iter_mut().find(|(name, _)| *name == base) {
            Some(slot) => slot.1 = target,
            None => by_name.push((base, target)),
        }
    }
    by_name.into_iter().map(|(_, target)| target).collect()
}

/// npm's `!data.bin`: absent, `null`, `false`, `0` and `""`.
fn bin_is_falsy(bin: Option<&Value>) -> bool {
    match bin {
        None | Some(Value::Null) | Some(Value::Bool(false)) => true,
        Some(Value::String(s)) => s.is_empty(),
        Some(Value::Number(n)) => n.as_f64() == Some(0.0),
        _ => false,
    }
}

/// The entries beneath `directories.bin`, sorted; none without the field or when it secures to
/// nothing.
fn directory_bins(root: &Path, manifest: &Value) -> Vec<String> {
    let Some(dir) = manifest
        .pointer("/directories/bin")
        .and_then(Value::as_str)
        .map(secure_path)
        .filter(|dir| !dir.is_empty())
    else {
        return Vec::new();
    };
    let mut out = Vec::new();
    collect_entries(&root.join(&dir), &dir, 0, &mut out);
    out.sort();
    out
}

/// Every file and directory beneath `dir`, recursively, `/`-joined onto `prefix`; entries whose
/// name starts with a dot stay out, as npm's glob leaves them. Recursion stops at
/// [`MAX_DEPTH`], where the walk's own depth error takes over.
fn collect_entries(dir: &Path, prefix: &str, depth: usize, out: &mut Vec<String>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        if name.starts_with('.') {
            continue;
        }
        let path = format!("{prefix}/{name}");
        // `file_type` does not follow symlinks; npm's glob expands directories.bin with
        // follow:false, so a linked directory's entries stay out of the bin set.
        if depth < MAX_DEPTH && entry.file_type().is_ok_and(|t| t.is_dir()) {
            collect_entries(&entry.path(), &path, depth + 1, out);
        }
        out.push(path);
    }
}

/// `@npmcli/package-json`'s `secureAndUnixifyPath`: backslashes and colons become slashes, `.`
/// and `..` segments resolve without climbing above the root, the result is relative; a path
/// that resolves to the root is empty.
fn secure_path(path: &str) -> String {
    let mut segments: Vec<&str> = Vec::new();
    let unixified = path.replace(['\\', ':'], "/");
    for segment in unixified.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                segments.pop();
            }
            other => segments.push(other),
        }
    }
    segments.join("/")
}

/// The rule sets every level below the root uses as they are, and the mode its ignore files
/// are read in.
struct Shared {
    defaults: Rc<Vec<Rule>>,
    strict: Rc<Vec<Rule>>,
    quirks: bool,
    /// The exact paths the veto holds: the patch files of `patchedDependencies`.
    vetoed: Vec<String>,
}

fn walk_dir(
    dir: &Path,
    rel: &str,
    levels: &mut Vec<Level>,
    shared: &Shared,
    out: &mut Vec<String>,
) -> Result<()> {
    if levels.len() > MAX_DEPTH {
        return Err(format!("{}: deeper than the {MAX_DEPTH}-level limit", dir.display()).into());
    }
    let mut names: Vec<String> = Vec::new();
    for entry in std::fs::read_dir(dir).map_err(|e| format!("reading {}: {e}", dir.display()))? {
        // A name that is not UTF-8 cannot be matched by any rule; npm would not ship it either.
        if let Ok(name) = entry?.file_name().into_string() {
            names.push(name);
        }
    }
    names.sort();
    let here = levels.len() - 1;
    for name in names {
        // node-tar cannot represent `*` in a name on Windows; npm-packlist skips such entries.
        if name.contains('*') {
            continue;
        }
        // The never-ship veto outranks every rule set and runs before them: a vetoed name is
        // never listed and never descended, whatever `main`/`bin`, `files` or a nested ignore
        // file says.
        if vetoed_name(rel, &name) {
            continue;
        }
        let pass_file = filter_entry(levels, here, &name, false, None)?;
        let pass_dir = filter_entry(levels, here, &name, true, None)?;
        if !pass_file && !pass_dir {
            continue;
        }
        let path = dir.join(&name);
        let meta =
            std::fs::symlink_metadata(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        let child_rel = if rel.is_empty() {
            name.clone()
        } else {
            format!("{rel}/{name}")
        };
        if meta.is_dir() {
            if !pass_dir {
                continue;
            }
            let exact = pass_file || filter_entry(levels, here, &format!("{name}/"), false, None)?;
            let IgnoreFiles {
                npmignore,
                gitignore,
            } = ignore_files(&path, false, shared.quirks)?;
            levels.push(Level {
                basename: name.clone(),
                exact,
                defaults: Rc::clone(&shared.defaults),
                allowlist: None,
                npmignore,
                gitignore,
                strict: Rc::clone(&shared.strict),
            });
            walk_dir(&path, &child_rel, levels, shared, out)?;
            levels.pop();
        } else if meta.is_file() && pass_file && !shared.vetoed.contains(&child_rel) {
            out.push(child_rel);
        }
    }
    Ok(())
}

/// ignore-walk's `filterEntry`: the parent's verdict on `basename/entry` first (unless this level
/// is exact), then every rule set of this level in order, a match setting the verdict to the
/// rule's negation flag. A rule whose evaluation runs past its step budget fails the walk.
fn filter_entry(
    levels: &[Level],
    index: usize,
    entry: &str,
    partial: bool,
    entry_basename: Option<&str>,
) -> Result<bool> {
    let level = &levels[index];
    let mut included = true;
    if index > 0 {
        let parent_entry = format!("{}/{entry}", level.basename);
        let parent_basename = entry_basename.unwrap_or(entry);
        included = filter_entry(
            levels,
            index - 1,
            &parent_entry,
            partial,
            Some(parent_basename),
        )?;
        if !included && !level.exact {
            return Ok(false);
        }
    }
    let slashed = format!("/{entry}");
    for rule in level.sets().flatten() {
        // A negated rule can only turn an exclusion around, a plain one only an inclusion.
        if rule.negate == included {
            continue;
        }
        let relative = entry_basename.filter(|_| rule.is_relative());
        let mut hit = rule.matches(&slashed, false)? || rule.matches(entry, false)?;
        if !hit && partial {
            hit = rule.matches(&format!("{slashed}/"), false)?
                || rule.matches(&format!("{entry}/"), false)?
                || (rule.negate && (rule.matches(&slashed, true)? || rule.matches(entry, true)?));
            if !hit {
                if let Some(base) = relative {
                    hit = rule.matches(&format!("/{base}/"), false)?
                        || rule.matches(&format!("{base}/"), false)?
                        || (rule.negate
                            && (rule.matches(&format!("/{base}"), true)?
                                || rule.matches(base, true)?));
                }
            }
        }
        if hit {
            included = rule.negate;
        }
    }
    Ok(included)
}

/// npm-packlist's order, chosen for compressibility: extension, then basename, then the path.
fn packlist_order(a: &str, b: &str) -> std::cmp::Ordering {
    let (base_a, base_b) = (basename(a), basename(b));
    collate(
        &extname(base_a).to_lowercase(),
        &extname(base_b).to_lowercase(),
        false,
    )
    .then_with(|| collate(&base_a.to_lowercase(), &base_b.to_lowercase(), false))
    .then_with(|| collate(a, b, false))
}

fn basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// Node's `extname`: from the last dot of the basename, unless that dot leads it.
fn extname(base: &str) -> &str {
    match base.rfind('.') {
        Some(0) | None => "",
        Some(i) => &base[i..],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn package(files: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for (path, content) in files {
            let full = dir.path().join(path);
            fs::create_dir_all(full.parent().unwrap()).unwrap();
            fs::write(full, content).unwrap();
        }
        dir
    }

    fn listed(dir: &Path, manifest: &str) -> Vec<String> {
        fs::write(dir.join("package.json"), manifest).unwrap();
        walk(
            dir,
            &serde_json::from_str(manifest).unwrap(),
            &crate::pack::Settings::default(),
        )
        .unwrap()
    }

    #[test]
    fn the_files_allowlist_silences_the_ignore_files_and_keeps_the_required_set() {
        let dir = package(&[
            ("dist/index.js", ""),
            ("dist/nested/x.d.ts", ""),
            ("dist/.DS_Store", ""),
            ("src/index.ts", ""),
            ("LICENSE-MIT", ""),
            ("README.md", ""),
            ("CHANGELOG.md", ""),
            (".npmignore", "dist\n"),
            ("node_modules/dep/index.js", ""),
            ("package-lock.json", "{}"),
            (".git/HEAD", ""),
            ("bin/demo.js", ""),
            ("lib/a.js", ""),
            ("lib/deep/b.js", ""),
            ("lib/c.ts", ""),
        ]);
        let files = listed(
            dir.path(),
            r#"{"name":"demo","version":"1.2.3","files":["dist","LICENSE-MIT","lib/*.js"],
                "main":"dist/index.js","bin":{"demo":"./bin/demo.js"}}"#,
        );
        assert_eq!(
            files,
            [
                "LICENSE-MIT",
                "lib/a.js",
                "bin/demo.js",
                "dist/index.js",
                "package.json",
                "README.md",
                "dist/nested/x.d.ts",
            ]
        );
    }

    #[test]
    fn npmignore_wins_over_gitignore_and_nested_ignores_apply() {
        let dir = package(&[
            ("index.js", ""),
            ("build/out.js", ""),
            ("docs/a.md", ""),
            ("docs/.npmignore", "*.md\n!keep.md\n"),
            ("docs/keep.md", ""),
            ("scratch.orig", ""),
            (".gitignore", "build\n"),
            (".npmignore", "docs/a.md\n"),
            ("LICENCE", ""),
            ("Readme", ""),
            ("readme.md~", ""),
        ]);
        let files = listed(dir.path(), r#"{"name":"demo","version":"0.0.1"}"#);
        assert_eq!(
            files,
            [
                "LICENCE",
                "Readme",
                "index.js",
                "build/out.js",
                "package.json",
                "docs/keep.md",
                "readme.md~",
            ]
        );
    }

    #[test]
    fn an_empty_allowlist_ships_the_strict_set_only() {
        let dir = package(&[("index.js", ""), ("README", ""), ("LICENSE", "")]);
        let files = listed(
            dir.path(),
            r#"{"name":"demo","version":"0.0.1","files":[],"main":"index.js"}"#,
        );
        assert_eq!(files, ["LICENSE", "README", "index.js", "package.json"]);
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_and_starred_names_never_ship() {
        let dir = package(&[("real.js", ""), ("we*ird.js", "")]);
        std::os::unix::fs::symlink("real.js", dir.path().join("link.js")).unwrap();
        let files = listed(dir.path(), r#"{"name":"demo","version":"0.0.1"}"#);
        assert_eq!(files, ["real.js", "package.json"]);
    }

    #[test]
    fn directories_bin_expands_when_no_bin_is_declared() {
        let dir = package(&[
            ("bin/cli.js", ""),
            ("bin/tools/fix.js", ""),
            ("bin/.hidden", ""),
            ("index.js", ""),
        ]);
        let manifest =
            r#"{"name":"demo","version":"0.0.1","files":[],"directories":{"bin":"./bin"}}"#;
        let targets = bin_targets(dir.path(), &serde_json::from_str(manifest).unwrap());
        assert_eq!(targets, ["bin/cli.js", "bin/tools", "bin/tools/fix.js"]);
        let files = listed(dir.path(), manifest);
        assert_eq!(files, ["bin/cli.js", "bin/tools/fix.js", "package.json"]);
        // A declared bin wins over the directory.
        let declared =
            r#"{"name":"demo","version":"0.0.1","bin":"index.js","directories":{"bin":"bin"}}"#;
        assert_eq!(
            bin_targets(dir.path(), &serde_json::from_str(declared).unwrap()),
            ["index.js"]
        );
    }

    #[cfg(unix)]
    #[test]
    fn directories_bin_does_not_expand_through_a_symlink() {
        // A linked directory's target entries stay out of the bin set: they are names read
        // from outside the package. The links themselves are listed and never ship.
        let dir = package(&[("bin/cli.js", "")]);
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("target-name.js"), "").unwrap();
        std::os::unix::fs::symlink(outside.path(), dir.path().join("bin/exit")).unwrap();
        std::os::unix::fs::symlink(".", dir.path().join("bin/loop")).unwrap();
        let manifest = r#"{"name":"demo","version":"0.0.1","directories":{"bin":"bin"}}"#;
        let targets = bin_targets(dir.path(), &serde_json::from_str(manifest).unwrap());
        assert_eq!(targets, ["bin/cli.js", "bin/exit", "bin/loop"]);
    }

    #[test]
    fn extglob_ignore_rules_exclude_like_npm() {
        let dir = package(&[
            ("index.js", ""),
            ("secret.pem", ""),
            ("certs/server.key", ""),
            ("keep.log", ""),
            ("debug.log", ""),
            (".npmignore", "*.@(pem|key)\ndebug.!(txt)\n"),
        ]);
        let files = listed(dir.path(), r#"{"name":"demo","version":"0.0.1"}"#);
        assert_eq!(files, ["index.js", "package.json", "keep.log"]);
    }

    #[test]
    fn bin_paths_normalize_like_npm() {
        assert_eq!(secure_path("./bin/x.js"), "bin/x.js");
        assert_eq!(secure_path("../../bin/x.js"), "bin/x.js");
        assert_eq!(secure_path("bin//x.js"), "bin/x.js");
        assert_eq!(secure_path("C:\\bin\\x.js"), "C/bin/x.js");
        assert_eq!(secure_path(".."), "");
    }

    #[test]
    fn the_never_ship_veto_covers_its_whole_set() {
        for vetoed in [
            ".npmrc",
            ".git",
            ".svn",
            ".hg",
            "CVS",
            "node_modules",
            "a/.npmrc",
            "a/b/.git",
            "lib/node_modules/dep/index.js",
            "package-lock.json",
            "npm-shrinkwrap.json",
            "yarn.lock",
            "pnpm-lock.yaml",
            "bun.lockb",
            "bun.lock",
        ] {
            assert!(never_ship(vetoed), "{vetoed} must never ship");
        }
        for fine in [
            "sub/yarn.lock", // a nested lockfile is ordinary content
            "node_modulesx", // a prefix is not the name
            ".npmrc2",       // a suffix is not the name
            "git/config",    // no dot, not the VCS dir
            "index.js",
            "packages/app/package-lock.json",
        ] {
            assert!(!never_ship(fine), "{fine} is ordinary content");
        }
        // The veto folds case with the matcher's own fold.
        assert!(never_ship(".NPMRC"));
        assert!(never_ship("NODE_MODULES/dep/index.js"));
        assert!(never_ship("Package-Lock.JSON"));
        assert!(vetoed_name("", "package-lock.json"));
        assert!(!vetoed_name("sub", "package-lock.json"));
    }

    #[test]
    fn the_never_ship_set_survives_every_inclusion_mechanism() {
        // The walk-level mirror of the tests/pack.rs veto matrix: entry-point fields, a nested
        // negation and the allowlist all fail to ship `.npmrc` or `.git`.
        let dir = package(&[
            (".npmrc", "token"),
            (".git/config", "token"),
            ("lib/.npmrc", "token"),
            ("lib/.npmignore", "!.npmrc\n"),
            ("lib/x.js", ""),
            ("index.js", ""),
        ]);
        for manifest in [
            r#"{"name":"demo","version":"0.0.1","main":".npmrc"}"#,
            r#"{"name":"demo","version":"0.0.1","bin":{"x":".git/config"}}"#,
            r#"{"name":"demo","version":"0.0.1","browser":".npmrc"}"#,
            r#"{"name":"demo","version":"0.0.1","directories":{"bin":".git"}}"#,
            r#"{"name":"demo","version":"0.0.1","files":["**"]}"#,
        ] {
            let files = listed(dir.path(), manifest);
            assert_eq!(
                files
                    .iter()
                    .filter(|f| f.contains("npmrc") || f.contains(".git"))
                    .count(),
                0,
                "{manifest}: {files:?}"
            );
            assert!(
                files.iter().any(|f| f == "index.js"),
                "{manifest}: {files:?}"
            );
        }
        // The nested negation cannot re-include lib/.npmrc either.
        let files = listed(dir.path(), r#"{"name":"demo","version":"0.0.1"}"#);
        assert!(files.contains(&"lib/x.js".to_string()), "{files:?}");
        assert!(!files.contains(&"lib/.npmrc".to_string()), "{files:?}");
    }

    #[test]
    fn the_order_is_extension_then_basename_then_path() {
        let mut files = vec![
            "b/index.js".to_string(),
            "README.md".to_string(),
            "a/index.js".to_string(),
            "package.json".to_string(),
            "LICENSE".to_string(),
        ];
        files.sort_by(|a, b| packlist_order(a, b));
        assert_eq!(
            files,
            [
                "LICENSE",
                "a/index.js",
                "b/index.js",
                "package.json",
                "README.md"
            ]
        );
        assert_eq!(extname(".npmrc"), "");
        assert_eq!(extname("a.b.c"), ".c");
        assert_eq!(extname("plain"), "");
    }
}
