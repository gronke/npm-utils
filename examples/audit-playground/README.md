# audit-playground

A deliberately outdated project to point `npm-utils audit` at.
Three direct dependencies are pinned to versions with well-known advisories — lodash 4.17.11, minimist 1.2.0, and body-parser 1.19.0, which drags in a vulnerable `qs` transitively — so one command reproduces a rich, multi-severity report:

<!-- regenerate: cargo run --features cli --bin npm-utils -- audit examples/audit-playground -->

```console
$ npm-utils audit examples/audit-playground
found 13 vulnerabilities (2 critical, 5 high, 5 moderate, 1 low) in 4 package(s)

body-parser@1.19.0
  HIGH     GHSA-qwcr-r2fm-qrc7  body-parser vulnerable to denial of service when url encoding is enabled
    range <1.20.3 · CWE-405 · https://github.com/advisories/GHSA-qwcr-r2fm-qrc7

lodash@4.17.11
  HIGH     GHSA-35jh-r3h4-6jhm  Command Injection in lodash
    range <4.17.21 · CWE-77, CWE-94 · https://github.com/advisories/GHSA-35jh-r3h4-6jhm
  CRITICAL GHSA-jf85-cpcp-j695  Prototype Pollution in lodash
    range <4.17.12 · CWE-20, CWE-1321 · https://github.com/advisories/GHSA-jf85-cpcp-j695
  ...

minimist@1.2.0
  CRITICAL GHSA-xvch-5gv4-984h  Prototype Pollution in minimist
    range >=1.0.0 <1.2.6 · CWE-1321 · https://github.com/advisories/GHSA-xvch-5gv4-984h
  ...

qs@6.7.0
  HIGH     GHSA-hrpp-h998-j3pp  qs vulnerable to Prototype Pollution
    range >=6.7.0 <6.7.3 · CWE-1321 · https://github.com/advisories/GHSA-hrpp-h998-j3pp
  ...
```

The report exits `1`, and `--audit-level critical` still fails — two findings are critical.
The exact counts are a snapshot: advisories are only ever added against these frozen versions, so the numbers can grow.

Both JSON files were written by npm-utils itself — `npm-utils init`, then `npm-utils add lodash=4.17.11 minimist=1.2.0 body-parser=1.19.0` — and `npm audit` reads the resulting lock and reports the identical advisory set.
Nothing needs to be installed: `audit` reads the manifest and lock and resolves in memory.
