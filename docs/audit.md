# `npm-utils audit`

`audit` checks packages against vulnerability advisories — like `npm audit`, but querying **multiple sources** behind one trait.
Two ship by default: npm's native registry endpoint and [OSV](https://osv.dev).
Findings are deduped across sources (by GHSA/CVE alias) and filtered to the versions you actually have, so the npm endpoint's over-broad ranges don't cry wolf.

## Sources

The positional source names what to audit:

```console
$ npm-utils audit web/                        # a project directory: lockfile preferred
$ npm-utils audit /tmp/package.json           # an explicit package.json or package-lock.json path
$ npm-utils audit lit=^3                      # a package and its full transitive dependency tree
```

A directory source prefers its `package-lock.json` and falls back to its `package.json`.
The files it discovers may be symlinks as long as they resolve inside that directory; an explicit file path is used as given.
Path sources take the usual spellings — relative, absolute, `~/…` (expanded even when the shell didn't), and Windows drive/UNC prefixes.
An explicit file path is classified by basename, else by content — a lock under a nonstandard name is still recognized by its `lockfileVersion` key.
A registry spec is written `name=range` (the `=` form is never ambiguous with a path; an empty range means latest) and audits the package's full transitive dependency tree — "what would this package pull into my app?".

## Resolution

A `package.json` or `name=range` source resolves its registry-reachable `dependencies` **and** `optionalDependencies` tree against the registry in memory — audit never writes a `package-lock.json` (nor needs one on disk).
Optional deps of *every* platform are included (no `os`/`cpu` filtering: their advisories matter regardless of the auditing machine), and an optional dep that fails to resolve is tolerated, npm-style.
That resolution is nested like npm's: when requirements disagree (one parent wants `glob@^4`, another pins `5.0.15`), every resolved version is kept and audited — an audit flags issues rather than installs, and if the tree would contain both versions, both versions' advisories matter.
`npm:` aliases resolve and audit their target.

## Omissions

What a manifest resolution **cannot** cover is never dropped silently: git / path / tarball / `workspace:` / `link:` deps, workspace packages, and failed optional deps are reported as **omissions** — a `note:` line above the summary, an `omissions` array in `--format json`, and an `AUDIT INCOMPLETE` marker.
devDependencies are not resolved from a manifest.
A lockfile source audits exactly what the lock pins, dev dependencies included, with no omissions; prefer it when one exists.

## Exit semantics and flags

It mirrors npm's exit semantics with one strictness on top: `--audit-level <low|moderate|high|critical>` sets the bar, and the command exits `1` when a finding at or above it exists (default `low` — any vuln fails).
A finding is a result, not an error: it prints the report and exits `1`.
An **incomplete** audit fails closed with exit `2` unless `--allow-incomplete` opts back into fail-open — incomplete meaning every selected advisory source failed, or (with no findings to report first) some dependencies were omissions and thus never checked.
One unreachable source merely degrades and marks the report incomplete.
A missing/unreadable source is a hard error.
`--format json` emits an `npm audit --json`-shaped report, `--sources npm,osv` selects sources, and `--registry <url>` points the npm advisory source *and* manifest/spec resolution at a private mirror.

Resolving and querying can take a while; every fetch is bounded by `--timeout` (120 s by default).
Status output goes to stderr and never affects the report on stdout (including `--format json`) or `npm-utils:` errors, so piping stdout stays clean.
