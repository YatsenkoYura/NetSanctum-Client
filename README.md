# Netsanctum Client

Native desktop and mobile client for [NetSanctum](https://github.com/YatsenkoYura/NetSanctum).

Netsanctum Client connects to an existing node. It does not run Docker, PostgreSQL, Redis, Celery, or module Python code on the user's computer. Online modules are rendered in a sandboxed WebView; selected module packages can be downloaded and opened through the local offline runtime.

> [!WARNING]
> Netsanctum Client 2.0 unifies the desktop and Android clients in one release line. Offline package contracts are versioned, but the project is still evolving. Keep a backup of important data stored on the server.

## Features

- Connect to a self-hosted NetSanctum node using its master token.
- Store the master token in an application-owned encrypted vault.
- Use the node's existing Jinja/HTMX module interfaces in a native desktop window.
- Save versioned NSP packages and large binary resources to the device.
- Group downloaded data by module instead of hardcoding Music, Video Archive, Vault, or future modules.
- Open downloaded packages through an authenticated loopback HTTP runtime.
- Stream offline audio and video with HTTP Range support.
- Keep remote node pages outside the trusted Tauri IPC boundary.
- Minimize to the system tray.

## Downloads

Installers are published on the repository's [GitHub Releases](https://github.com/YatsenkoYura/netsanctum-desktop/releases) page after a `vX.Y.Z` tag passes all platform builds.

| Platform | Release files |
| --- | --- |
| Linux x86_64 | `.AppImage`, `.deb`, `.rpm` |
| Windows x86_64 | NSIS `.exe`, `.msi` |
| macOS Apple Silicon | ARM64 `.dmg` |
| macOS Intel | x86_64 `.dmg` |
| Android | signed ARM64 `.apk` |

Early releases are not code-signed. Windows may show an Unknown Publisher/SmartScreen warning. macOS may require allowing the application in Privacy & Security. Download builds only from this repository.

### Arch Linux repository

Netsanctum Client publishes an unsigned pacman repository generated from the official Debian release package. Add this block to `/etc/pacman.conf`:

```ini
[netsanctum]
SigLevel = Never
Server = https://github.com/YatsenkoYura/NetSanctum-Desktop/releases/download/arch-repo
```

Install the application and perform a full system upgrade:

```bash
sudo pacman -Syu netsanctum-desktop
```

Future versions are installed by the usual `sudo pacman -Syu`. The repository is delivered over HTTPS and pacman verifies package hashes from its database, but packages are not currently signed.

## First Start

1. Start or open an existing NetSanctum node.
2. Enter its HTTPS URL and master token.
3. Create a local vault password of at least 10 characters.
4. Re-enter the vault password after each application restart.
5. Open a supported item on the node and use **Save to device**.
6. Return to Client Home and open the saved module from the local archive.

The vault password is never persisted. Losing it requires reconnecting the node; it does not alter server data.

## Security Model

- Argon2id derives a vault key from the user's password.
- XChaCha20-Poly1305 encrypts and authenticates the master token.
- Node HTML runs without filesystem, shell, or generic Tauri command access.
- Native downloads accept only same-origin relative manifest and resource URLs.
- Redirects are disabled for authenticated package downloads.
- Offline resources are available only through a random per-process loopback capability.
- SQLite stores metadata; binary objects are addressed by SHA-256.

See [the architecture document](docs/architecture.md) and [security policy](SECURITY.md) for details.

## Development

Requirements:

- Node.js 24
- Rust 1.88 (installed automatically by `rust-toolchain.toml`)
- npm
- platform WebView/build dependencies

Ubuntu/Debian dependencies:

```bash
sudo apt-get install build-essential libayatana-appindicator3-dev \
  librsvg2-dev libssl-dev libwebkit2gtk-4.1-dev patchelf
```

Arch-based dependencies:

```bash
sudo pacman -S base-devel webkit2gtk-4.1 libayatana-appindicator librsvg
```

Run the application:

```bash
npm ci
npm run version:check
npm run tauri dev
```

Run checks:

```bash
npm run check
npm run build
npm audit --audit-level=high
cargo fmt --check --manifest-path src-tauri/Cargo.toml
cargo clippy --locked --manifest-path src-tauri/Cargo.toml --all-targets --all-features -- -D warnings
cargo test --locked --manifest-path src-tauri/Cargo.toml --all-features
```

### Android

The Android target shares the Rust vault,
connection validation, and library shell with desktop. Live node pages open in
a separate native WebView with no Tauri IPC bridge. Package manifests are
validated and downloaded by the Rust core into app-private Android storage.
Downloaded modules can be opened through the app-private offline runtime.

Install Android Studio with the Android SDK Platform, Platform-Tools, NDK,
Build-Tools, and Command-line Tools. Install `rustup`, then initialize the
generated Android project and build an arm64 release APK:

```bash
rustup target add aarch64-linux-android
npm run tauri -- android init
npm run tauri -- android build --apk --target aarch64
```

The local unsigned APK is written to
`src-tauri/gen/android/app/build/outputs/apk/universal/release/`. The generated
Android Studio project remains untracked and can be recreated with
`npm run tauri -- android init`; `npm run build` copies the tracked native
WebView plugin into it before each Android build.

Tagged GitHub releases publish a signed ARM64 APK. Configure these repository
secrets before pushing a release tag:

- `ANDROID_KEYSTORE_BASE64`: base64-encoded release keystore
- `ANDROID_KEYSTORE_PASSWORD`: keystore password
- `ANDROID_KEY_ALIAS`: signing key alias
- `ANDROID_KEY_PASSWORD`: signing key password

Keep the release keystore backed up securely. Android updates must be signed by
the same key as the installed application.

Optional repository hooks:

```bash
pre-commit install
```

## Releases

All version sources are kept in sync by `scripts/version.mjs`.

```bash
npm run version:set -- 2.0.1
npm run version:check
git commit -am "chore(release): prepare v2.0.1"
git tag -a v2.0.1 -m "Netsanctum Client v2.0.1"
git push origin main v2.0.1
```

The tag must exactly match the version in `package.json`, `package-lock.json`, `src-tauri/Cargo.toml`, `src-tauri/Cargo.lock`, and `src-tauri/tauri.conf.json`. GitHub Actions builds every installer into a draft and publishes the release only after all jobs succeed.

## Repository Layout

```text
src/                  Svelte desktop shell
src-tauri/src/        Rust application core
src-tauri/icons/      platform application icons
docs/architecture.md  security and package contracts
.github/workflows/    CI and tagged release automation
```

## License

Mozilla Public License 2.0. See `LICENSE`.
