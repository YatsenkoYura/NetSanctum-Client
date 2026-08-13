# Security Policy

## Supported versions

Only the newest 1.x release receives security fixes.

## Reporting

Do not publish credential, path traversal, sandbox escape, package validation, or local gateway issues in a public proof of concept before the maintainer has had a reasonable chance to respond. Use GitHub private vulnerability reporting when it is enabled for the repository.

Never include a real NetSanctum master token, encrypted vault, downloaded package, or personal server URL in a report.

## Current security boundaries

- The master token is stored in an Argon2id/XChaCha20-Poly1305 password vault.
- Remote node pages run without Tauri capabilities.
- Package and resource URLs must remain on the configured node origin.
- Offline content is served through an authenticated loopback gateway.
- Release installers are currently unsigned. Verify that downloads come from this repository's GitHub Releases page.
