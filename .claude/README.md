# Claude Code Agents & Skills

Project-scoped sub-agents (invoke with `@<agent-name>`) and skills (invoke with `/<skill-name>`) for the Ferrox repository, available in any Claude Code session.

---

## `@architecture-guardian`

Reviews proposed designs, audits the codebase for structural issues, and keeps `docs/developer/architecture.md` in sync with the implementation.

```
@architecture-guardian I want to add a per-route request timeout that overrides the global default. Review this design.
```
```
@architecture-guardian Audit the existing architecture for unnecessary complexity, inconsistencies, and non-optimal patterns.
```
```
@architecture-guardian We just added a new GLM provider adapter. Sync the architecture doc.
```
```
@architecture-guardian Is docs/developer/architecture.md up to date with the current implementation?
```

---

## `@security-reviewer`

Reviews PRs and audits the full codebase for vulnerabilities, attack vectors, cryptographic issues, and dependency advisories.

```
@security-reviewer Review the current branch for security issues before I open a PR.
```
```
@security-reviewer Run a full security audit of the Ferrox codebase.
```
```
@security-reviewer The last 3 commits touched the JWT validation and token issuance flow. Check them for security issues.
```
```
@security-reviewer Check our Dockerfiles and docker-compose.yml for container security problems.
```

---

## `@documentation-reviewer`

Audits documentation for staleness, gaps, inconsistencies, and duplication. Also maps recent code changes to docs that need updating.

```
@documentation-reviewer Audit all docs for stale content that no longer matches the code.
```
```
@documentation-reviewer What documentation needs updating based on the last 14 days of changes?
```
```
@documentation-reviewer Check docs/user/configuration.md for gaps and inconsistencies against the actual config source.
```
```
@documentation-reviewer Find any duplicated content across the user and developer docs.
```

---

# Skills

## `/sdk-compat-audit`

Re-runs the provider-SDK compatibility audit: clones the official OpenAI / Anthropic / Gemini SDKs into a temp dir, cross-references Ferrox's OpenAI ⇄ Anthropic translation layer (`ferrox/src/anthropic_types.rs`, `types.rs`, `providers/*.rs`) against their real wire contracts across the translation dimensions (streaming tool calls, reasoning, usage, multimodal, per-provider tool calling, stop_reason mapping, transparent field pass-through, error/protocol events), deduplicates findings against existing GitHub issues, and files new ones. Read-only on the codebase; the only writes are GitHub issues.

```
/sdk-compat-audit
```

---

## `/release`

Cuts a new tagged release. Picks the next semver version from the commits since the last tag (patch for fixes, minor for features — confirmed with you), publishes a GitHub Release with auto-generated notes, which triggers the release workflow (cross-platform binaries + `.sha256` sidecars, GHCR Docker images for `ferrox` and `ferrox-cp` tagged `X.Y.Z`/`major.minor`/`major`/`latest`, and the Homebrew tap update). Then watches the workflow to green and verifies the published images and assets. Releases are tag-driven — no manual version-bump commit.

```
/release
```
