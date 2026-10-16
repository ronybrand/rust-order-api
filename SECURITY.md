# Security Policy

## Supported versions

Single-branch project (no maintained release lines): security fixes land on `main` only. There is no
LTS or backport policy.

## Reporting a vulnerability

Please **do not** open a public GitHub issue for a suspected vulnerability. Use
[GitHub's private vulnerability reporting](../../security/advisories/new) for this repository
(Security tab, "Report a vulnerability"), or contact the maintainer through the email on the
[GitHub profile](https://github.com/ronybrand).

Include, where applicable: a description and the potential impact, steps to reproduce (a minimal
request is ideal), and the affected endpoint or component.

The project is maintained on a best-effort basis (no SLA), but reports are taken seriously.

## Scope

In scope: the code in this repository, the notification worker, and the Dockerfile and CI workflows that
build and ship them.

Out of scope: third-party services used for local development (PostgreSQL, RabbitMQ, Mailpit); report
those upstream. Dependency vulnerabilities are tracked by Dependabot, `cargo-deny` and the dependency
review of every pull request instead of manual reports.

## What the project already does

- Weekly Dependabot updates, auto-merged (patch and minor only) after CI passes; actions pinned by SHA.
- `cargo-deny` on every change: RustSec advisories, licenses, where crates may come from, and a ban on a
  second TLS crypto provider (two of them in one process make rustls panic at the first handshake).
- Dependency review on every pull request; CodeQL on pull requests and weekly.
- A CycloneDX bill of materials of the resolved `Cargo.lock` graph is scanned for known vulnerabilities on
  every change and weekly, and kept as a build artifact.
- `unsafe` is forbidden in the crate; `clippy` runs in pedantic mode with warnings as errors.
- The image is one static binary on `scratch`: no shell, no package manager, no libc, and a numeric non-root user.
- Personal data (tax id, passport, e-mail) is never logged (`Debug` of the types that carry it shows only
  the id) and is never returned by the customer endpoints.
- A conflict, a validation error or a failed notification never echoes the value that caused it; an
  unexpected failure reaches the client only as `INTERNAL-00`, without its text.
- The API only verifies tokens (RS256, with `aud`, `iss` and `exp` required) and refuses to start if the
  configured key is a private one.
- No secrets in the repository: configuration comes from environment variables.
- Every column is bounded and every business rule has a database constraint behind it, so a bypass of the
  application layer cannot store invalid data.

## Known limits

- Rate limiting is per process: with several replicas each one counts for itself (a shared limiter would need Redis).
- The per-address limit uses the connection's own address and has no trusted-proxy setting, so behind a proxy
  every client looks like the proxy.
- Publishing the notification is best effort (no outbox, ADR 0003).
