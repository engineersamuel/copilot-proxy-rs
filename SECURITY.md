# Security Policy

## Supported versions

`copilot-proxy-rs` is currently alpha software. Security fixes are applied to
the main branch until versioned release branches exist.

## Reporting a vulnerability

Please report vulnerabilities privately through GitHub Security Advisories for
the published repository. If advisories are not enabled, contact the maintainer
privately before opening a public issue.

Include:

- Affected commit or release.
- Reproduction steps.
- Expected impact.
- Whether credentials, prompt content, or network exposure are involved.

## Safe operation

This proxy uses the operator's GitHub/Copilot credentials for upstream calls.
Keep it bound to loopback unless it is protected by trusted inbound
authentication and network controls.
