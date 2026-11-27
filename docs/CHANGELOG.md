# suonelo changelog

## [1.0.11] - 2026-11-26

### Added

- [linux] Build suonelo as a native WebKitGTK desktop app and produce an x86-64 Debian package using the Ubuntu 22.04 baseline.
- [linux] Add a local build script that collects the Debian package, portable AppImage, and checksums in a versioned output folder.
- [linux] Save Qobuz credentials in the desktop Secret Service keyring and retain the verified account identity at sign-out for account-switch protection.
- [linux] Store profiles under XDG data directories, honor the configured Music directory, and open verified download folders and approved links through the desktop.
- [win/mac/linux] Added a looping marquee with edge fade and hover/focus pause for overflowing release subtitles in Downloads and Bookmarks, falling back to static ellipsis when reduced motion is enabled.

### Changed

- [win/mac/linux] Grouped tracks from the same release under a single album row in Downloads across Search, Add link, link imports, and Bookmarks, displaying the artist and partial track count (`{Artist} · Album · X of Y tracks`) with tracks sorted in album disc/track order.
- [win/mac/linux] Merged additional tracks from the same release into existing matching non-playlist release rows across all queue states and upgraded partial rows in place when the full release is queued, skipping already-included tracks as duplicates.

### Fixed

- [win/mac/linux] Fixed the Downloads panel shifting horizontally when expanding or collapsing rows toggles the vertical scrollbar by reserving a thin scrollbar gutter (`scrollbar-gutter: stable` and `scrollbar-width: thin`, with an `overflow-y: scroll` fallback for older macOS and Linux WebKit builds).

### Compatibility

- [linux] Credential storage needs a persistent default collection in GNOME Keyring or a compatible Secret Service provider. The Debian package is the initial distribution format.
- [linux] The local Ubuntu 22.04 WSL test suite covers credential records, profile isolation, and the credential lifecycle against a private GNOME Keyring. See [Linux verification](linux-verification.md) for desktop and live-account checks.

Validation: `npm run typecheck`, `npm run build`, `cargo check --locked --all-targets`, `cargo build --locked`, and 138 Rust unit tests (`--skip profile_migration`) passed on Windows x64; the 4 `profile_migration` tests (Windows sandbox access error) and `scripts/check-ux.windows.mjs` (sandbox loopback restriction) did not complete inside the sandbox.

## [1.0.10] - 2026-11-20

### Changed

- [win/mac] Show one row per album for artist discographies and playlists. Row actions affect that album, and a playlist still exports one M3U in the original track order, including repeats.
- [win/mac] Show outlined Remove buttons and icon-only file actions with tooltips. Center the file controls vertically and give them 12 px of spacing.

### Fixed

- [win/mac] Enable Open folder as soon as the first track is saved so each downloading release opens its own folder directly.
- [win/mac] Let release-level Retry recover failed files of any type, including timed-out lyrics, while keeping successfully saved files and the release's original file-type settings.

## Note

- These are unsigned binaries, your system may try to block it from executing. When prompted on Windows, click **Run Anyway**, on macOS go to Settings > Privacy Security then scroll down to find **suonelo** and click **Open anyway**.
- There is a known issue with Windows wherein RTSS may cause webview2 to crash, as a workaround open up RTSS profile editor, add the executable file and set the app detection level to *None*.
- Windows x64 portable ZIP and unsigned macOS Intel/Apple Silicon DMG. SHA256SUMS.txt covers both downloads.

## [1.0.9] - 2026-11-19

### Changed

- [win/mac] Repair missing FLAC audio checksums automatically and verify the tagged file before saving it.
- [win/mac] Fixed spacing and styling issues
- [win/mac] Added tooltips to release row buttons

### Removed

- [win/mac] Removed optional FLAC re-encoding, external FLAC tool setup, and the FLAC settings tab. Checksum repair retains the original encoded audio frames.

## [1.0.8] - 2026-11-18

### Changed

- Combined the Windows and macOS apps in one React and Rust codebase while preserving each platform's native webview and credential storage.
- Packaged Windows x64 as a portable ZIP with the app's profile stored beside the executable.
- Targeted Intel Macs with an x86_64 DMG that requires Rosetta on supported Apple silicon Macs.
