# Netsanctum Client Architecture

## Product boundary

Netsanctum Client connects to an existing NetSanctum node. It does not run PostgreSQL, Redis, Celery, Docker, or module Python code locally.

The application has two trust domains:

1. The local shell owns connection setup, the offline library, downloads, and settings.
2. Node-provided HTML runs in a separate sandbox with no filesystem access and no Tauri command capability.

After authentication, a dedicated child WebView replaces the visible local shell in the same native window. Its temporary server session is installed directly as an HttpOnly cookie by Rust. The node page never receives Tauri capabilities; the `main` capability is scoped only to the local `main` WebView label. A guarded document-start script exposes the desktop presence flag and narrow navigation actions, but no credential or generic native IPC.

The Rust core owns credentials, temporary sessions, downloads, NSP parsing, SQLite, and the loopback offline gateway. Secrets are never injected into JavaScript, local storage, page URLs, or logs.

## Credential vault

The master token is stored in an application-owned, cross-platform encrypted envelope:

- Argon2id v1.3 derives a 256-bit key from the user's vault password and a random 128-bit salt.
- Production KDF parameters are 64 MiB memory, three iterations, and one lane.
- XChaCha20-Poly1305 encrypts and authenticates the master token with a random 192-bit nonce.
- Ciphertext is bound to the Netsanctum Client vault format through fixed associated data; modified KDF parameters produce authentication failure.
- KDF parameters read from disk are bounded before allocation to prevent modified-file resource exhaustion.
- The vault file is written through fsync and atomic rename; Unix permissions are `0600` in a `0700` directory.
- Passwords, derived keys, plaintext, and the in-memory master token use zeroizing containers.

No encryption key or password is persisted, so the user unlocks the vault once after each process start. This intentionally avoids platform keyring dependencies and fake machine-ID encryption.

## Authentication

The current NetSanctum compatibility path posts the master token to `POST /auth/ui/login`, captures the resulting HttpOnly-style temporary session identifier inside Rust, and verifies it through `GET /auth/me`. The cookie is not installed into the local shell or exposed to JavaScript. The Rust gateway will attach it to upstream requests.

The preferred versioned contract is:

`POST /auth/desktop/session`

Request:

```json
{
  "master_token": "secret",
  "client": {
    "name": "Netsanctum Client",
    "version": "0.1.0",
    "protocol_version": 1
  }
}
```

Response:

```json
{
  "access_token": "short-lived bearer token",
  "token_type": "bearer",
  "expires_in": 900,
  "node_name": "Home Sanctum"
}
```

Requirements:

- The access token is scoped to desktop API and package downloads.
- Maximum lifetime is 24 hours; 15 minutes is recommended.
- The master token is never returned.
- Invalid master tokens return `401` or `403`.
- Rate limiting applies independently from browser login.
- Future WebView launch uses a single-use, short-lived code exchanged for an HttpOnly cookie. Bearer tokens must not be exposed to node JavaScript.

If the versioned endpoint is absent, Desktop falls back to the existing cookie exchange. This fallback session is refreshed after at most 24 hours even though the server cookie currently lives longer.

## Package manifest v1

Every offline-capable module provides explicit metadata. Desktop code must never infer a module from URL segments.

```json
{
  "schema_version": 1,
  "package_id": "video_123",
  "package_title": "Example video",
  "module": {
    "id": "video_archiver",
    "title": "Видеоархив",
    "root_url": "/video-archiver/dashboard"
  },
  "root_url": "/video-archiver/dashboard?package_id=video_123",
  "resources": [
    {
      "url": "/api/packages/video_123/nsp",
      "type": "container",
      "size": 12345,
      "sha256": "hex digest"
    },
    {
      "url": "/api/video-archiver/videos/123/stream",
      "type": "binary",
      "size": 987654321,
      "sha256": "hex digest"
    }
  ]
}
```

Resource URLs must be same-origin relative URLs. Downloads reject redirects to another origin. When manifests provide expected sizes and hashes, the client verifies them in addition to always calculating a local SHA-256 content address. Hard resource and package limits protect the device when legacy manifests omit those fields.

## Storage

- SQLite stores metadata and resource references.
- `.nsp` containers remain packed and are read by indexed offsets.
- Large binaries use content-addressed files under `objects/<hash-prefix>/<sha256>`.
- Writes use temporary files, hash verification, fsync, and atomic rename.
- The local HTTP gateway binds to a random loopback port and requires an unguessable per-process capability token.
- Range requests stream bounded file regions without loading media into memory.

## Current limitations

- Releases are not yet code-signed or notarized.
- The application is not yet single-instance.
- Updating an already-ready package is not generation-based yet; a failed replacement can leave that package marked failed until it is downloaded again.
- The Linux Tauri stack currently carries upstream gtk-rs maintenance and soundness advisories tracked in `README.md` and CI output.
