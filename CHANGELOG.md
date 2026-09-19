# Changelog

All notable changes will be documented here. The project follows Semantic Versioning.

## [Unreleased]

## [2.0.6] - 2026-09-19

### Fixed

- Routed headset and system previous/next commands to the active NetSanctum module player.
- Refreshed Android media metadata immediately when tracks, videos, or episodes change.

## [2.0.5] - 2026-09-16

### Fixed

- Prevented Android WebView from suspending active module media when the screen is locked.
- Retried interrupted background video playback without overriding system media pause commands.

## [2.0.4] - 2026-09-16

### Added

- Added automatic Android picture-in-picture for playing module videos when leaving the app.

### Fixed

- Kept module video playback active with the same background service and wake lock used for audio.

## [2.0.3] - 2026-09-16

### Changed

- Restyled Android media artwork with a dark matte background and turquoise neo-brutalist accents.

### Fixed

- Added Android media controls for detached and autoplaying audio players.
- Prevented native audio focus handling from repeatedly pausing WebView video playback.

## [2.0.2] - 2026-09-16

### Added

- Android media controls with shared audio and video metadata, artwork, seeking, audio focus, and background playback.

### Changed

- Replaced module-specific Android player styling with generic responsive media handling.

### Fixed

- Prevented missing temporary or stale object files from breaking offline package saves and playback.

## [2.0.1] - 2026-09-15

### Added

- Smart Android module shortcuts with customizable names and icons.
- Online session validation, vault unlock, and per-package offline fallback for shortcuts.
- Background media playback support in the Android node WebView.

### Changed

- Renamed the product to Netsanctum Client and updated release artifact names.
- Added immersive Android video fullscreen with dynamic system-bar insets.

### Fixed

- Preserved access to credential vaults created under the previous product name.

## [2.0.0] - 2026-09-15

### Added

- Android native authentication, isolated node WebView, and offline package runtime.
- Android package progress, notifications, library management, and launcher shortcuts.
- Optional passwordless Android vault unlock backed by the system Keystore.

### Changed

- Added safe-area-aware Android navigation, settings, language controls, and native menus.
- Added signed Android ARM64 APK builds to tagged GitHub releases.

### Fixed

- Resolved offline NSP asset lookups containing cache query parameters.

## [1.0.0] - 2026-08-13

### Changed

- Licensed the public desktop client under MPL-2.0.

### Added

- Secure connection to an existing NetSanctum node.
- Password-protected local credential vault.
- Sandboxed node WebView with desktop bridge detection.
- Versioned NSP package downloads and SQLite-backed offline library.
- Authenticated loopback runtime with media Range support.
- Linux tray integration and custom flat desktop chrome.
