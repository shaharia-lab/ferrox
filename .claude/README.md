# Claude Code Agents

Project-scoped sub-agents for the Ferrox repository. Invoke with `@<agent-name>` in any Claude Code session.

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
