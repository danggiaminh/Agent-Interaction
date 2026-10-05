# Provenance: vendor/alpine-aports

Verbatim copy of the Alpine Linux `aports` source tree at release **v3.24.2**. The directory `vendor/alpine-aports/` is the archive's single top-level directory with its name stripped. Nothing inside it has been modified, added or removed.

| Field | Value |
|---|---|
| Upstream project | Alpine Linux `aports` (https://gitlab.alpinelinux.org/alpine/aports) |
| Release | v3.24.2 (released 2026-09-17; branch v3.24, EOL 2028-06-01 per https://alpinelinux.org/releases.json) |
| Tag | `v3.24.2` (tag object `ef12a0f0c2e1596a99f544b63eeac27689bb86ca`) |
| Commit | `d9d560d5de74ff9a7a73f3c903d6f126a0bf3142` ("===== release 3.24.2 =====", Natanael Copa, 2026-09-17) |
| Source archive URL | `https://gitlab.alpinelinux.org/api/v4/projects/alpine%2Faports/repository/archive.tar.gz?sha=v3.24.2` |
| Archive retrieved | 2026-10-04 23:33 GMT, HTTP 200, 13979162 bytes |
| Archive SHA-256 | `54bdc72acfe934955eca814acd48835ea9bd35387e9dc7a1e2e10a3a7ad23acd` (computed locally; Alpine publishes no checksum for this archive) |
| Vendored on | 2026-10-05 |
| Vendored tree id | `475841678d601c7c30d0d93e406463547e6e3dd0` (git tree of `vendor/alpine-aports/`) |
| Contents | 23337 files, 112 symlinks, 12637 directories under the vendored root |

## Verification performed
- Archive: SHA-256 recomputed, `gzip -t` clean, and the commit id embedded in the tar header equals `d9d560d5…`, the commit the tag resolves to via the GitLab API.
- Extraction: every archive member (type, content hash, symlink target, executable bit) matches the extracted tree, and no extra paths exist.
- Git index: all 23449 entries (files and symlinks) match the archive in mode and blob hash.
- Upstream cross-check: all 15 root entries of the vendored tree have git object ids identical to the GitLab API tree listing for commit `d9d560d5…`, so the tree is byte-identical to upstream at that commit.
- The commit is not signed upstream (GitLab signature API returns `404 Signature Not Found`), so there is no signature to verify.

## Licensing
The archive has no top-level COPYING or LICENSE file, only `README.md`. Licensing is declared per package in each `APKBUILD` (`license=` set in 12629 of 12629 APKBUILD files), and per-package license files, patches and sources are preserved exactly as shipped. Consult the individual package directories for the terms that apply to each.

## Notes
- Upstream's `.gitignore` (patterns such as `*.gz`, `*.xz`, `*.tar`, `src`, `pkg`, `tmp`, `core`) would silently drop some upstream-tracked files from a plain `git add`, so the tree was added with `git add --force`. Do not re-vendor with a plain `git add`.
- 13 files are upstream-tracked binary assets (images, test fixtures, a signature, a keystore). They are kept so the tree stays identical to upstream. No build artifacts, package caches or extraction leftovers are included.
- To update, replace the whole directory from a newly verified archive. Do not patch files in place.
