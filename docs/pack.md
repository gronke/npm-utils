# `npm-utils pack`

`pack` lists the files a package directory publishes and writes npm's tarball for them, in Rust.

- Input: one package directory with its `package.json`, packed as it lies on disk; a build runs before `pack`, a workspace member packs from its own directory.
- Manifest fields read: `name`, `version`, `files`, `main`, `browser`, `bin`, `directories.bin`, `patchedDependencies`, `bundleDependencies`.
- Rules come from the manifest and the ignore files inside the directory.
- A manifest declaring bundled dependencies is refused with an error naming the field.
- Library: `pack::list` (the paths), `pack::Plan` (the frozen listing), `pack::write` (into any writer), `pack::write_to_path` (into a file), `pack::tarball` (into memory); each returns npm's report.
- Verb: `npm-utils pack [DIR] [--dry-run] [--json] [--pack-destination DIR] [--npm-quirks]`.

## Compatibility

- Specification: npm 12.1.0, npm-packlist 11.3.0 over ignore-walk 9 and minimatch 10.2.5; `files` globs as glob 13 on a case-sensitive filesystem.
- Pin: `tests/fixtures/pack/npm.json` records npm's listing for the trees in `tests/pack_npm.rs`; the ordinary test holds the crate to it, the ignored `record_the_pinned_npm` test rewrites it from the npm on PATH.
- npm 12 rules: `files` entries apply in manifest order (`["foo.js", "!foo.js"]` excludes), and they match case-sensitively against the whole path (`"LIB"` leaves `lib/` alone, `"*.js"` names root-level files).
- `--json` prints npm 12's shape, an object keyed by the package name.

## The listing

The walk ports npm-packlist over ignore-walk.
Each level consults its rule sets in order, and the last match decides.

1. The defaults, at every level: `.npmignore`, `.gitignore`, `**/.git`, `**/.svn`, `**/.hg`, `**/CVS` with their contents, `/.lock-wscript`, `/.wafpickle-*`, `/build/config.gypi`, `npm-debug.log`, `**/.npmrc`, `.*.swp`, `.DS_Store`, `._*`, `*.orig`, `/archived-packages/**`.
2. The `files` allowlist, at the root: everything is excluded, then each entry expands as a glob against the package root in manifest order.
   - One leading `./` or `/` and trailing slashes are stripped.
   - A match is an inclusion, a directory with its contents; a `!` entry's matches are exclusions; the later rule wins; an entry matching nothing adds no rule.
   - Globs are case-sensitive, `dot` is on, symlinks match nothing, a pattern without `**` reaches its own depth.
   - An allowlist silences the root's ignore files.
3. `.npmignore`, else `.gitignore`, of the directory, at every level; a file precedence drops is not read.
4. The strict set, at the root:
   - Always in: `package.json`, `README*`, `COPYING*`, `LICENSE*` and `LICENCE*` in any case (`~` and `$` backups excepted), and the `main`, `browser` and `bin` targets.
   - `main` and `browser` count as written: `./lib/index.js` stays unnormalized, so a rule on `lib` still excludes it (the `copying-and-dot-slash-entry-points` case).
   - `bin` as npm normalizes it: a string is the one bin; an array is keyed by basename, a later entry replacing an earlier one; an object by its keys; paths made relative.
     Without a `bin`, the entries beneath `directories.bin` stand in, dotfiles excluded, symlinks unfollowed; a `bin` of any kind, an empty object included, leaves `directories.bin` unexpanded.
   - Always out: `.git`, `node_modules`, `.npmrc`, the root lockfiles, `.npm-extension.mjs`, `.npm-extension.cjs` and the patch files of `patchedDependencies`, the last with a warning when a `files` entry pulled one in.
   - Below the root, `/.git` stays out.

Rules are minimatch patterns under npm's options:

- A slash-less pattern matches a basename at any depth, a leading `/` anchors it, `**` spans directories.
- Bracket classes with ranges and POSIX names, the extglob groups `@()`, `?()`, `*()`, `+()` and `!()`, braces `{a,b}` and `{1..3}`; case is folded; a leading `!` makes an inclusion.
- The matcher is the crate's `minimatch` module, minimatch 10.2.5 with negations evaluated as zero-width checks, pinned by `tests/minimatch.rs`.

A child level asks its parent about `dir/entry` first, then applies its own rules.
Regular files ship, in npm-packlist's order: extension, then basename, then path; a symlink, a special file or a name carrying `*` is skipped.

## The never-ship veto

A veto sits above the rules, case-folded like the matcher:

- `.npmrc` at any depth.
- `.git`, `.svn`, `.hg`, `CVS` and `node_modules` as any path component.
- At the root: `package-lock.json`, `npm-shrinkwrap.json`, `yarn.lock`, `pnpm-lock.yaml`, `bun.lockb`, `bun.lock` and `.npm-extension.*`.
- The patch files of `patchedDependencies`, by exact path.

It outranks `main`, `browser`, `bin`, every `files` entry and every nested `.npmignore` negation; a nested `packages/x/yarn.lock` is ordinary content.
This is a deliberate deviation: npm 9.2 and npm-packlist 11.3.0 ship `.npmrc` and `.git/config` when `main` or `bin` names them and honour a nested `!.npmrc`, npm anchors `node_modules` at the root, and npm 12.1.0 ships `.npm-extension.mjs` when `main` names it and a patch file a nested negation re-includes, with a warning.

## Budgets

Each budget is an error naming the rule line.

| Budget | Value | minimatch 10.2.5 |
| --- | --- | --- |
| Brace alternatives per line | 10 000 | truncates silently past its own cap |
| Brace groups per line | 100 | |
| Expanded characters per line | 4 000 000 | |
| Evaluator steps per path segment | 1 000 000 | |
| Directory levels | 64 | |
| Nested extglob groups per pattern | 128 | a RangeError past a few hundred |
| Extglob nodes per line across its brace expansions, counted once every `!()` group has absorbed what follows it | 10 000 | the tree doubles per sequential `!()` group |
| `**` sections crossed while one path is matched | 200 | answers `false` |
| Characters per rule line | 65 536 | the same limit |

Many distinct just-under-budget rules cost linearly.
Negation-free runs compile to one linear regex each and `!()` groups evaluate as zero-width checks, so `*b*b*b*b*b*b*b*c`, `+(b|bb)+(b|bb)+…` and `*(!(a))y` against a long name answer in linear time.
Where the port answers differently from the JavaScript on purpose (characters instead of UTF-16 units, Unicode case folding under `nocase`, a POSIX class beside an escaped `-`), `tests/minimatch.rs` lists the cases.

## Quirks

The rules are read strictly by default: the matcher refuses by name what minimatch guesses at, and escapes hold everywhere.
`--npm-quirks` (`pack::Settings { quirks: true }`) reads them as npm does, for a listing identical to npm's on any input; ordinary rules read the same either way, and the pinned npm listing checks both modes.
The veto and the budgets hold in both modes.
`npm_utils::minimatch::Quirk` names each quirk:

- `negation-before-group`: npm reads a leading `!(` as negation, so `!(a|b)` negates a literal `(a|b)`, which no path matches.
  Strict refuses it; `!@(a|b)` negates a group match, `@(!(a|b))` is the group.
- `raw-extension-fast-path`: `*<ext>` and `?<ext>` compare the raw extension text, so `*\.js` matches `a\.js`, and under `dot` `*.` matches `..`.
  Strict applies the regex: `*\.js` matches `a.js`, `.` and `..` stay out.
- `escaped-pipe-alternates`: with other magic in the pattern, `\|` reaches the regex as an alternation, so `a*\|b` matches `a` and `xb`.
  Strict keeps it a literal `|`.
- `braces-strip-escapes`: brace expansion strips `\\`, `\{`, `\}`, `\,` and `\.` when a `{…}` pair exists, so `a\\*` and `a\\*{b,c}` read the backslashes differently.
  Strict lets the escapes survive expansion.
- `posix-print-is-control`: `[[:print:]]` is `\p{C}`, the control and unassigned characters.
  Strict: everything but `\p{C}` and the line and paragraph separators.
- `posix-punct-skips-symbols`: `[[:punct:]]` is `\p{P}`, so `$`, `+`, `<`, `=`, `>`, `^`, `` ` ``, `|` and `~` are no punctuation.
  Strict: `\p{P}\p{S}`.
- `unmatchable-class-poisons`: a class that can match nothing (`[z-a]`, `[a-[:alpha:]]`) makes its whole segment match nothing.
  Strict refuses it.
- `unclosed-class-is-literal`: an unclosed `[` is a literal `[`.
  Strict refuses it.
- `unclosed-group-is-literal`: an unclosed group (`x*(`) is literal text.
  Strict refuses it.
- `unknown-posix-class-is-literal`: `[[:nope:]]` is a class of `[`, `:`, `n`, `o`, `p`, `e` and `:`, then a literal `]`.
  Strict refuses it.

The quirks live in minimatch (`isaacs/minimatch`) and brace-expansion (`juliangruber/brace-expansion`); the listing rules the veto deviates from live in `npm/npm-packlist` and `npm/ignore-walk`.

## The tarball

- `name` and `version` pass the path-safety allowlists first; they become the filename.
- A `Plan` freezes the listing before any output exists: `pack .` never packs its own output, and every target sees the same contents.
- `write_to_path` streams into an exclusively created temporary sibling (`.<name>.<pid>.<n>.part`) and renames it onto the destination on success; a failed pack removes the temporary file and keeps the previous artifact.
  The verb writes through the containment guards an extraction uses.
- Entries: `package/`-prefixed regular files with npm's fixed mtime, uid and gid zero, node-tar's portable mode (`(mode | 0600) & ~0022`: `0644` and `0755` for the usual files, a `0600` or `0750` kept), the executable bits for a `bin` target, the setuid, setgid and sticky bits masked; gzip level 9.
- A path beyond ustar's 255-byte split becomes a GNU longname entry.
- Each listed path is re-stat'ed without following symlinks before it is packed; the header takes its size from the open handle and the bytes are counted against it, so a file that changes size while packed fails the pack.
  The final open still races a symlink swap: pack trees you trust.
- The report is npm's object for one tarball: `id`, `name`, `version`, `size`, `unpackedSize`, `shasum` (sha1), `integrity` (sha512), `filename`, `files` with size and mode (uppercase-led paths first), `entryCount`, `bundled` (empty).
- `write` hashes and counts the bytes as they pass; `--dry-run` streams into a sink.
- The digests describe this crate's gzip stream; npm's zlib writes different bytes, installers accept both.

## Limits

- A top-level entry starting with `@` is listed under its own name (npm prefixes `./`).
- Bundled dependencies are the gap worth closing next, on the install machinery this crate has.
- The recorded fixture and the live comparison stay within what npm and this crate agree on; the veto is covered by `tests/pack.rs`.

## Tests

- `tests/pack_npm.rs`: twenty-one trees pinned to npm 12.1.0's listing in `tests/fixtures/pack/npm.json`; the ignored recorder rewrites the record from the npm on PATH.
- `tests/minimatch.rs`: the matcher pinned to answers recorded from minimatch 10.2.5; the ignored recorder rewrites them from the minimatch on PATH.
- `tests/pack_e2e.rs`: twelve packages from git at their release commit, held file by file to npm 12.1.0's listing and to the registry's tarball, and twenty registry tarballs unpacked and packed again, build steps and scopes included, three of them through `npm install`.
- `tests/minimatch_props.rs`: properties over patterns drawn from `tests/common/glob_grammar.rs` with proptest: a compile ends with a result or a named error, a path from the pattern's own language matches, `!` is the complement, `nocase` and `dot` only add matches, strict and quirks agree where no quirk is involved, escapes round-trip.
  256 cases per property on every run, 4096 in CI; a failing case lands in `proptest-regressions/` and replays first.
- `tests/pack.rs`: the veto, symlinks, special files, the destination guards.
- The `pack-e2e` CI job runs the recorders, the grammar-fed differential and the corpora with npm 12.1.0 and minimatch 10.2.5 in a digest-pinned node container with the network off (`ci/sealed-node` shims on PATH, the minimatch tree installed from the committed lockfile by the crate's own `ci` verb), and requires the committed records to reproduce.
