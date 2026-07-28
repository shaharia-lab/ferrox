---
name: release
description: >-
  Cut a new tagged release of Ferrox. Picks the next semver version from the
  commits since the last tag, publishes a GitHub Release (which triggers the
  release workflow: cross-platform binaries + GHCR Docker images for ferrox and
  ferrox-cp + Homebrew tap update), then watches the workflow to green and
  verifies the published artifacts. Use when you want to ship a new version —
  e.g. "release Ferrox", "cut v0.5.0", "make a new release".
---

# Release Ferrox

Releasing is **tag-driven**. Publishing a GitHub Release for a `vX.Y.Z` tag fires
`.github/workflows/release.yml` (`on: release: [published]`), which fans out to:

- **binaries** — builds `ferrox` + `ferrox-cp` for all four targets
  (`{x86_64,aarch64}-unknown-linux-gnu`, `{x86_64,aarch64}-apple-darwin`) and
  attaches `.tar.gz` + `.sha256` sidecars to the release.
- **docker** / **docker-cp** — builds and pushes
  `ghcr.io/shaharia-lab/ferrox` and `ghcr.io/shaharia-lab/ferrox-cp`, each tagged
  `{{version}}`, `{{major}}.{{minor}}`, `{{major}}`, and `latest`.
- **homebrew** — updates the tap formula (`ferrox.rb` + a versioned
  `ferrox@X.Y.Z.rb`).

There is **no manual version-bump commit**: the `version` fields in
`ferrox/Cargo.toml` / `ferrox-cp/Cargo.toml` are not the source of truth — the
**git tag is**. So a release is: pick the version → publish the release → watch
the workflow → verify. That's it.

---

## Flow

```
0 preflight (gh auth · on main · synced · CI green)
→ 1 pick the next semver version from commits since the last tag (confirm with the user)
→ 2 publish the GitHub Release (creates the tag, auto-generates notes) — this triggers release.yml
→ 3 watch the release workflow to success (binaries · docker · docker-cp · homebrew)
→ 4 verify artifacts (GHCR tags · release assets + .sha256)
→ 5 report
```

---

## Phase 0 — Preflight

All from a clean `gh`-authenticated shell in the repo:

```bash
gh auth status                                   # authenticated for shaharia-lab, else STOP
git fetch origin --tags --prune
git rev-parse --abbrev-ref HEAD                  # should be the default branch (main)
git rev-parse HEAD; git rev-parse origin/main    # must be equal — release the pushed HEAD
```

- **Release from `main` at `origin/main`.** Local ahead/behind or a dirty tree →
  STOP and say so; never tag un-pushed or uncommitted work.
- **CI must be green on the target commit** — a release compiles and ships it:
  ```bash
  gh run list --repo shaharia-lab/ferrox --branch main --limit 3 \
    --json conclusion,status,displayTitle,headSha
  ```
  Red or still-running required checks on `origin/main`'s SHA → STOP (releasing a
  red commit produces a broken image). CodeQL/analyze jobs pending are fine to
  note and proceed; the build/test/clippy gate must be green.

## Phase 1 — Pick the version

```bash
LAST=$(git tag --sort=-v:refname | head -1)      # e.g. v0.4.0
git log "$LAST"..origin/main --pretty='%s'       # everything shipping in this release
```

Choose the next **semver** tag `vX.Y.Z` from what changed (repo is pre-1.0):

- **patch** (`v0.4.0 → v0.4.1`) — only `fix:`/`chore:`/`build(deps)` since the tag.
- **minor** (`v0.4.0 → v0.5.0`) — any `feat:` / user-visible new capability.
- **major** (`→ v1.0.0`) — only when the user explicitly wants to declare API
  stability. Never assume it.

The version is the user's call and a published tag is annoying to undo —
**confirm the chosen version with the user** (AskUserQuestion, recommended option
first) unless they already named it. Then sanity-check the tag is new
(`git tag -l vX.Y.Z` must be empty).

## Phase 2 — Publish the release (triggers the workflow)

`gh release create` makes the tag **and** publishes in one step, and
`--generate-notes` produces the PR-list + full-changelog body the repo's prior
releases use (see `gh release view <LAST>`):

```bash
gh release create vX.Y.Z \
  --repo shaharia-lab/ferrox \
  --target main \
  --title "vX.Y.Z" \
  --generate-notes \
  --latest
```

The command prints the release URL on success. (Ignore any non-zero exit from a
follow-up `--json` field name; verify with a plain
`gh release view vX.Y.Z --json tagName,publishedAt` instead.)

## Phase 3 — Watch the release workflow

Publishing fires `release.yml`. Find and follow the run to completion:

```bash
gh run list --repo shaharia-lab/ferrox --workflow release.yml --limit 3 \
  --json databaseId,status,conclusion,displayTitle,headBranch
gh run watch <RUN_ID> --repo shaharia-lab/ferrox --exit-status   # blocks until done
```

- The build is slow (cross-compiles + multi-arch images) — expect several
  minutes. `--exit-status` returns non-zero if any job fails.
- **A failed job → the release tag exists but artifacts are missing/partial.**
  Read the failing job log (`gh run view <RUN_ID> --log-failed`), fix forward on
  `main`, then either re-run the failed jobs (`gh run rerun <RUN_ID> --failed`) if
  the fix was infra/transient, or — if the fix is a code change — delete the
  release+tag (`gh release delete vX.Y.Z --cleanup-tag --yes`) and re-cut from the
  new commit. Never leave a published release with a broken/missing image.

## Phase 4 — Verify the artifacts

```bash
# GHCR image for this version is pullable (both binaries)
docker manifest inspect ghcr.io/shaharia-lab/ferrox:X.Y.Z    >/dev/null && echo "ferrox:X.Y.Z OK"
docker manifest inspect ghcr.io/shaharia-lab/ferrox-cp:X.Y.Z >/dev/null && echo "ferrox-cp:X.Y.Z OK"
# release assets: 4 platforms × (.tar.gz + .sha256) attached
gh release view vX.Y.Z --repo shaharia-lab/ferrox --json assets \
  -q '.assets[].name'
```

Confirm: the workflow is green, `:X.Y.Z` (and the moved `:latest`) images exist on
GHCR, and the release has its binary + `.sha256` assets. Anything missing → treat
as a failed release (Phase 3 recovery).

## Phase 5 — Report

One compact summary: version tagged, release URL, workflow conclusion, images
pushed (tags), assets attached. Note anything skipped (e.g. Homebrew tap PAT
absent) so it's visible.

---

## Guardrails

- **Never tag un-pushed, dirty, or red commits.** The release ships exactly the
  target commit; preflight gates exist for that reason.
- **The tag is the version.** Don't hand-edit `Cargo.toml` versions to "match" —
  the workflow derives everything from the tag.
- **Don't declare `v1.0.0`** or any major bump on your own — that's an explicit
  API-stability decision for the user.
- **A published release is public.** Double-check the version and target before
  publishing; recovering from a bad tag means deleting a release people may have
  already pulled.
