# `npm-utils pack`

`pack` is a pure-Rust local package packer: npm-packlist file selection and npm's tarball layout, without Node.
The input is one package directory.
Lifecycle scripts never run, workspaces and package specs are not resolved, a manifest declaring bundled dependencies is refused, no global ignore file applies.
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
3. `.npmignore`, else `.gitignore`, of the directory, at every level; a file precedence drops is not read.
4. The strict set at the root: `package.json`, `README*`, `COPYING*`, `LICENSE*` and `LICENCE*` (any case, `~` and `$` backups not among them) and the `main`, `browser` and `bin` targets are always in.
   `main` and `browser` count as written: a `./lib/index.js` is not normalized, so an ignore rule on `lib` still excludes it, as npm-packlist 11.3.0 reads it (the `copying-and-dot-slash-entry-points` case pins this).
   `.git`, `node_modules`, `.npmrc`, `package-lock.json`, `npm-shrinkwrap.json`, `yarn.lock`, `pnpm-lock.yaml`, `bun.lockb`, `bun.lock`, `.npm-extension.mjs`, `.npm-extension.cjs` and the patch files of `patchedDependencies` are always out, the last with a warning when a `files` entry pulled one in.
   `bin` is read as npm normalizes it: a string is the one bin, an array is keyed by basename and a later entry replaces an earlier one under the same basename, an object by its keys, paths made relative; without a `bin`, the entries beneath `directories.bin` stand in, dotfiles excluded, symlinks not followed; a `bin` of any kind, an empty object included, leaves `directories.bin` unexpanded.
   Below the root only `/.git` stays out.

Rules are minimatch patterns under npm's options: a slash-less pattern matches a basename at any depth, a leading `/` anchors it, `**` spans directories, bracket classes with ranges and POSIX names apply, so do the extglob groups `@()`, `?()`, `*()`, `+()` and `!()`, braces `{a,b}` and `{1..3}` expand, case is folded, a leading `!` makes an inclusion.
The matcher is the crate's `minimatch` module, minimatch 10.2.5 with negations evaluated without a backtracking engine, pinned by its own recorded fixture (`tests/minimatch.rs`).
A child level first asks its parent about `dir/entry`, then applies its own rules; the last match decides.
Only regular files ship; symlinks, special files and names carrying `*` are skipped.
The order is npm-packlist's: extension, then basename, then path.

## The never-ship veto

A hard veto sits above the rules: `.npmrc` at any depth, `.git`, `.svn`, `.hg`, `CVS` and `node_modules` as any path component, the root lockfiles, a root `.npm-extension.*` and the patch files of `patchedDependencies` never ship.
No `main`, `browser` or `bin` include, no `files` entry and no nested `.npmignore` negation overrides it, and it folds case like the matcher.
A nested `packages/x/yarn.lock` is ordinary content.

This deviates from npm on purpose.
npm 9.2 and npm-packlist 11.3.0 ship `.npmrc` when `main` or `bin` names it, ship `.git/config` the same way and honor a nested `!.npmrc`, because their include rules come last; npm 9.2 also ships it through a `files` entry.
npm 12.1.0 ships `.npm-extension.mjs` when `main` names it, and ships a patch file a nested `.npmignore` negation re-includes, with a warning.
A package that means to publish these is broken or hostile.
`node_modules` is vetoed at any depth (npm anchors it at the root) since bundled dependencies are not gathered here.

## Budgets

The packer fails closed past generous budgets, each error naming the rule line:

- 10 000 brace alternatives, 100 brace groups and 4 000 000 expanded characters per line; `{1..1000000000}` or sixteen `{a,b,c}` groups are an immediate error, where minimatch silently truncates past its own cap.
- 1 000 000 evaluator steps per path segment, a backstop for the polynomial worst case; the exponential classes of the JavaScript (`*b*b*b*b*b*b*b*c`, `+(b|bb)+(b|bb)+…`, `*(!(a))y` against a long name) do not exist here, because negation-free runs compile to linear regexes and `!()` groups evaluate as zero-width checks, so these patterns simply answer.
- 64 directory levels; the walk and the filter recurse per level.
- 128 nested extglob groups per pattern; adoption chains bypass the grammar's own depth guard, and a few hundred nested groups overflow a small stack where npm throws a RangeError.
- 10 000 extglob nodes per line across its brace expansions, counted once every `!()` group has absorbed what follows it; sequential `!()` groups double that tree per group in npm's algorithm, so a twenty-group line costs millions of regexes there and is an error here.
- 200 `**` sections crossed while one path is matched, where minimatch answers `false`.
- 65 536 characters per rule line, minimatch's own limit.

Many distinct just-under-budget rules cost linearly, not exponentially.
Where the port answers differently from the JavaScript on purpose (characters instead of UTF-16 units, Unicode case folding under `nocase`, a POSIX class beside an escaped `-`), `tests/minimatch.rs` lists the cases.

## Quirks

The rules are read strictly by default: the matcher refuses by name what minimatch guesses at and honours escapes everywhere.
`--npm-quirks` (`pack::Settings { quirks: true }`) reads them as npm does, quirks included, for a listing identical to npm's on any input; ordinary rules read the same either way, and the pinned npm listing checks both.
The never-ship veto and the budgets hold in both modes.
`npm_utils::minimatch::Quirk` names each quirk:

- `negation-before-group`: npm reads a leading `!(` as negation, so `!(a|b)` negates a literal `(a|b)`, which no path matches.
  Strict refuses it; `!@(a|b)` negates a group match, `@(!(a|b))` is the group.
- `raw-extension-fast-path`: `*<ext>` and `?<ext>` compare the raw extension text, so `*\.js` matches `a\.js` and not `a.js`, and under `dot` the comparison takes `.` and `..`, so `*.` matches `..`.
  Strict applies the regex, so the escape holds, `*\.js` matches `a.js`, and `.` and `..` stay out.
- `escaped-pipe-alternates`: with other magic in the pattern, `\|` reaches the regex as an alternation, so `a*\|b` matches `a` and `xb`.
  Strict keeps it a literal `|`.
- `braces-strip-escapes`: brace expansion strips `\\`, `\{`, `\}`, `\,` and `\.`, but only when a `{…}` pair exists, so `a\\*` and `a\\*{b,c}` read the backslashes differently.
  Strict lets the escapes survive expansion.
- `posix-print-is-control`: `[[:print:]]` is `\p{C}`, the control and unassigned characters.
  Strict: everything but `\p{C}` and the line and paragraph separators.
- `posix-punct-skips-symbols`: `[[:punct:]]` is `\p{P}`, so `$`, `+`, `<`, `=`, `>`, `^`, `` ` ``, `|` and `~` are no punctuation.
  Strict: `\p{P}\p{S}`.
- `unmatchable-class-poisons`: a class that can match nothing (`[z-a]`, `[a-[:alpha:]]`) silently makes its whole segment match nothing.
  Strict refuses it.
- `unclosed-class-is-literal`: an unclosed `[` is a literal `[`.
  Strict refuses it.
- `unclosed-group-is-literal`: an unclosed group (`x*(`) is literal text.
  Strict refuses it.
- `unknown-posix-class-is-literal`: `[[:nope:]]` is a class of `[`, `:`, `n`, `o`, `p`, `e` and `:`, then a literal `]`.
  Strict refuses it.

The quirks live in minimatch (`isaacs/minimatch`) and brace-expansion (`juliangruber/brace-expansion`); the listing rules the never-ship veto deviates from live in `npm/npm-packlist` and `npm/ignore-walk`.

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

No workspaces, no lifecycle scripts, no package specs, no global ignore file; a manifest declaring bundled dependencies is refused; a top-level entry starting with `@` is listed under its own name (npm prefixes `./`).
Bundled dependencies are the gap worth closing next, on the install machinery this crate has.
The recorded fixture and the ignored live comparison stay within what npm and this crate agree on; the veto is covered by `tests/pack.rs`.
