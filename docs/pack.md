# `npm-utils pack`

`pack` is a pure-Rust local package packer: npm-packlist file selection and npm's tarball layout, without Node.
The input is one package directory.
Lifecycle scripts never run, workspaces and package specs are not resolved, bundled dependencies are not gathered, no global ignore file applies.
Library entry points: `pack::list` (the paths), `pack::Plan` (the frozen listing), `pack::write` (the tarball into any writer), `pack::write_to_path` (into a file), `pack::tarball` (into memory).
Each returns npm's report.

## Compatibility

The specification is npm 12.1.0: npm-packlist 11.3.0 over ignore-walk 9 and minimatch 10.2.5, `files` entries expanded like glob 13 on a case-sensitive filesystem.
`tests/fixtures/pack/npm.json` records npm's listing for the cases in `tests/pack_npm.rs`; the ordinary test holds the crate to it, the ignored `record_the_pinned_npm` test rewrites it from a host npm.
npm 12 changed two things and this crate follows: `files` entries apply in manifest order (`["foo.js", "!foo.js"]` excludes, npm 11 included), and they match case-sensitively against the whole path (`"LIB"` does not name `lib/`, `"*.js"` names root-level files only).
`--json` prints npm 12's shape, an object keyed by the package name; npm 9 to 11 printed an array.

## The listing

The walk ports npm-packlist over ignore-walk, rule set by rule set:

1. The defaults at every level: `.npmignore`, `.gitignore`, `**/.git`, `**/.svn`, `**/.hg`, `**/CVS` and their contents, `/.lock-wscript`, `/.wafpickle-*`, `/build/config.gypi`, `npm-debug.log`, `**/.npmrc`, `.*.swp`, `.DS_Store`, `._*`, `*.orig`, `/archived-packages/**`.
2. The `files` allowlist at the root: everything is excluded, then each entry is expanded as a glob against the package root in manifest order (one leading `./` or `/` and trailing slashes stripped).
   A match becomes an inclusion rule, a directory with its contents; a `!` entry's matches become exclusion rules; the later rule wins; an entry matching nothing drops silently.
   Globs are case-sensitive, `dot` is on, symlinks match nothing, a pattern without `**` reaches its own depth only.
   An allowlist silences the root's `.npmignore` and `.gitignore`.
3. `.npmignore`, else `.gitignore`, of the directory, at every level.
4. The strict set at the root: `package.json`, `README*`, `COPYING*`, `LICENSE*` and `LICENCE*` (any case, `~` and `$` backups excluded) and the `main`, `browser` and `bin` targets are always in.
   `.git`, `node_modules`, `.npmrc`, `package-lock.json`, `npm-shrinkwrap.json`, `yarn.lock`, `pnpm-lock.yaml`, `bun.lockb`, `bun.lock`, `.npm-extension.mjs`, `.npm-extension.cjs` and the patch files of `patchedDependencies` are always out, the last with a warning when a `files` entry pulled one in.
   `bin` is read as npm normalizes it (string, object or array, paths made relative); without one, the entries beneath `directories.bin` stand in, dotfiles excluded, symlinks not followed.
   Below the root only `/.git` stays out.

Rules are minimatch patterns under npm's options: a slash-less pattern matches a basename at any depth, a leading `/` anchors it, `**` spans directories, bracket classes with ranges and POSIX names apply, so do the extglob groups `@()`, `?()`, `*()`, `+()` and `!()`, braces `{a,b}` and `{1..3}` expand, case is folded, a leading `!` makes an inclusion.
A child level first asks its parent about `dir/entry`, then applies its own rules; the last match decides.
Only regular files ship; symlinks, special files and names carrying `*` are skipped.
The order is npm-packlist's: extension, then basename, then path.

## The never-ship veto

A hard veto sits above the rules: `.npmrc` at any depth, `.git`, `.svn`, `.hg`, `CVS` and `node_modules` as any path component, and the root lockfiles never ship.
No `main`, `browser` or `bin` include, no `files` entry and no nested `.npmignore` negation overrides it, and it folds case like the matcher.
A nested `packages/x/yarn.lock` is ordinary content.

This deviates from npm on purpose.
npm 9.2 and npm-packlist 11.3.0 ship `.npmrc` when `main` or `bin` names it, ship `.git/config` the same way and honor a nested `!.npmrc`, because their include rules come last; npm 9.2 also ships it through a `files` entry.
A package that means to publish these is broken or hostile.
`node_modules` is vetoed at any depth (npm anchors it at the root) since bundled dependencies are not gathered here.

## Budgets

The packer fails closed past generous budgets, each error naming the rule line:

- 10 000 brace alternatives and 100 brace groups per line; `{1..1000000000}` or sixteen `{a,b,c}` groups are an immediate error, where minimatch silently truncates past its own cap.
- 1 000 000 backtracking steps per rule evaluation; `*b*b*b*b*b*b*b*c` or `+(b|bb)+(b|bb)+…` against a long name hangs minimatch 10.2.6 and `npm pack` 9.2.0 and 12.1.0, here it errors in milliseconds.
- 64 directory levels; the walk and the filter recurse per level.

Many distinct just-under-budget rules cost linearly, not exponentially.

## The tarball

The manifest's `name` and `version` pass the path-safety allowlists first (no `..`, no separators, one scope separator at most), since they become the filename; the verb writes through the same containment guards as an extraction.
A `Plan` freezes the listing before any output exists, so `pack .` never packs its own output and every target sees the same contents.
`write_to_path` streams into a temporary sibling (`.<name>.<pid>.<n>.part`, created exclusively) and renames it onto the destination on success; a failed pack removes the temporary file and keeps the previous artifact.
Every file is a `package/`-prefixed regular entry with npm's fixed mtime (1985-10-26T08:15:00Z), uid and gid zero and node-tar's portable mode (`(mode | 0600) & ~0022`, so `0644` and `0755` for the usual files, a `0600` or `0750` kept), the executable bits set for a `bin` target, gzipped at level 9; the setuid, setgid and sticky bits are masked off.
A path beyond ustar's 255-byte split becomes a GNU longname entry.
Each listed path is re-stat'ed without following symlinks before it is packed; the final open still races in theory, so pack trees you trust.
The report is npm's object for one tarball: `id`, `name`, `version`, `size`, `unpackedSize`, `shasum` (sha1), `integrity` (sha512), `filename`, `files` with size and mode (uppercase-led paths first), `entryCount`, an empty `bundled`.
`write` hashes and counts the bytes as they pass, `--dry-run` streams into a sink.
The digests describe this crate's gzip stream; npm's zlib writes different bytes, installers accept both.

## Limits

No bundled dependencies, no workspaces, no lifecycle scripts, no package specs, no global ignore file; a top-level entry starting with `@` is listed under its own name (npm prefixes `./`).
Bundled dependencies are the gap worth closing next, on the install machinery this crate has.
The recorded fixture and the ignored live comparison stay within what npm and this crate agree on; the veto is covered by `tests/pack.rs`.
