# Contributing

## Setup

```bash
npm ci
npm run version:check
npm run check
cargo test --locked --manifest-path src-tauri/Cargo.toml
```

Linux development additionally requires WebKitGTK 4.1 and AppIndicator development packages. See the README for distribution-specific commands.

## Before a pull request

```bash
npm run build
cargo fmt --check --manifest-path src-tauri/Cargo.toml
cargo clippy --locked --manifest-path src-tauri/Cargo.toml --all-targets --all-features -- -D warnings
cargo test --locked --manifest-path src-tauri/Cargo.toml --all-features
```

Keep remote WebViews untrusted. Do not grant node or offline pages generic Tauri IPC, filesystem access, or shell access.

## Releases

Versions follow SemVer. Set all version files with:

```bash
npm run version:set -- 0.2.0
```

Merge the version change into `main`, then push an annotated `vX.Y.Z` tag. GitHub Actions creates a draft release with platform installers.
