# Sources

What each part of npm-utils follows, and the terms the registry sets for a client like it.
Ported code keeps its upstream licence through REUSE (`REUSE.toml`, `LICENSES/`); behaviour reimplemented from documentation and recorded output names its reference here instead.

## What each module follows

| Module | Follows | Upstream and its licence |
| --- | --- | --- |
| `package_json` | npm's [package.json](https://docs.npmjs.com/cli/v8/configuring-npm/package-json), [package-lock.json](https://docs.npmjs.com/cli/v8/configuring-npm/package-lock-json) and [package-spec](https://docs.npmjs.com/cli/v8/using-npm/package-spec) documentation; ranges follow [node-semver's grammar](https://github.com/npm/node-semver#ranges) on the `semver` crate. | Documentation of the npm CLI (Artistic-2.0) and of node-semver (ISC); no code was copied. |
| `registry` | The registry API as the npm CLI uses it: the packument, by default in its [abbreviated install form](https://github.com/npm/registry/blob/main/docs/responses/package-metadata.md), the tarball URL it advertises, and `/-/v1/search`. | npm's API documentation, archived at [npm/registry](https://github.com/npm/registry); no code was copied. |
| `integrity` | [W3C Subresource Integrity](https://www.w3.org/TR/SRI/), in the `sha512-` form npm writes. | A specification. |
| `extract` | The npm tarball layout: every entry under `package/`, as pacote writes it and npm reads it. | pacote (ISC); the layout only. |
| `install`, `project` | `npm install`, `npm ci` and `npm update`: the flat `node_modules/` tree, `.bin` shims, workspace links, lockfile v3 output. | The npm CLI (Artistic-2.0); behaviour only. |
| `resolve` | Node's package resolution: the `node_modules` ascent and the `exports` field, per [Node's packages documentation](https://nodejs.org/api/packages.html). | Node.js documentation (MIT); behaviour only. |
| `sbom` | [CycloneDX 1.6](https://cyclonedx.org/docs/1.6/json/), [SPDX 2.3](https://spdx.github.io/spdx-spec/v2.3/) and [purl](https://github.com/package-url/purl-spec). | Specifications. |
| `audit` | npm's bulk advisory endpoint as the npm CLI calls it, and the [OSV API](https://google.github.io/osv.dev/api/). | Endpoints; the data's terms are below. |
| `cli` | npm's verbs and their `--json` shapes, for the subset it offers. | The npm CLI (Artistic-2.0); behaviour only. |

`download`, `cache`, `path_safety` and `warn` follow nothing in particular.
The two recorded fixtures under `tests/fixtures/` are advisory responses: their shapes are npm's bulk endpoint's and OSV's, their content is the GitHub Advisory Database's, CC-BY 4.0 and attributed by the record links inside them, which `REUSE.toml` declares.

## The registry's terms

npm's [Open-Source Terms](https://docs.npmjs.com/policies/open-source-terms) allow searching, downloading and managing packages "using software other than CLI via application programming interfaces that npm publicly documents or makes available for public use".
npm-utils uses four such APIs and nothing on the website: `GET /<name>` for the packument, by default the abbreviated install document (`Accept: application/vnd.npm.install-v1+json`), the tarball URL a packument advertises, `GET /-/v1/search` with `size` at most 250, and `POST /-/npm/v1/security/advisories/bulk`, the npm CLI's own audit endpoint.
The load stays small: the resolver fetches at most eight packuments at a time, tarballs download one after another, and a failed request is retried once.
The [crawler policy](https://docs.npmjs.com/policies/crawlers) concerns the website and names downloading tarballs for inspection as acceptable; a mirror or proxy is pointed at through the registry URL, never crawled.

Vulnerability data from npm may be used "only for your own personal or internal business purposes", and the terms forbid providing it to others "directly or as part of other products or services".
An `audit` run from your machine against your project is that use; a service that answers third parties from npm's data would not be.
The GitHub Advisory Database, where npm's advisories come from, is [CC-BY 4.0](https://docs.github.com/en/site-policy/github-terms/github-terms-for-additional-products-and-features), and GitHub names a link to the record as sufficient attribution; every finding carries its advisory link.
OSV aggregates databases under their own licences, listed on its [data sources page](https://google.github.io/osv.dev/data/); its API has no rate limit, and queries go out in pages of at most 1000.

The policy pages were read on 2026-10-05.
