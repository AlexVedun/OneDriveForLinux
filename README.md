# OneDriveForLinux

A Linux desktop client for a personal Microsoft OneDrive account. It combines a
Rust background daemon, a GTK desktop window, an optional FUSE virtual drive,
and a local cache. The project is under active development; review the
limitations below before relying on it for important files.

## Features

- Browser sign-in with OAuth PKCE and tokens stored in the system Secret Service.
- A local OneDrive catalog, manual downloads, and a preview of changes for a
  separately configured sync folder.
- A FUSE drive with versioned caching and durable local drafts for new or
  modified files. Eligible closed drafts upload while the drive stays mounted.
- Manual draft export, upload, cloud comparison, and confirmed discard.
- Offline pinning for multiple files or folders and safe cache clearing.
- Desktop and tray status, with notifications when attention is needed.

The sync folder currently previews changes and maintains a guarded operation
queue. It does not perform general automatic two-way synchronization. Configure
only one mode at a time when using virtual-drive uploads.

## Requirements

On Fedora:

```sh
sudo dnf install rust cargo rustfmt clippy gcc pkgconf-pkg-config gtk4-devel libadwaita-devel fuse3
```

The virtual drive needs `/dev/fuse` and `fusermount3`. Sign-in needs a running
Secret Service, such as GNOME Keyring or KWallet. A StatusNotifierItem host is
needed for the tray icon, but the main window works without one.

## Microsoft application registration

In [Microsoft Entra App registrations](https://learn.microsoft.com/en-us/entra/identity-platform/quickstart-register-app),
create an application for personal Microsoft accounts. Under **Authentication**,
add a **Mobile and desktop applications** redirect URI of
`http://localhost/callback`. Grant delegated `Files.ReadWrite` and
`offline_access` permissions. Do not create a client secret.

Put the Application (client) ID in
`$XDG_CONFIG_HOME/onedrive-linux/config.env` (normally
`~/.config/onedrive-linux/config.env`):

```sh
ONEDRIVE_CLIENT_ID=your-application-id
```

For development from the repository root, a local `.env` file also works and
is ignored by Git. An AppImage launched elsewhere does not read that file.
The daemon checks an explicit environment variable first, then XDG config,
then `.env` in its working directory, then a build-time value. An explicitly
empty environment variable disables the fallback for tests.

## Run from source

Start the daemon and desktop in separate terminals on the same user D-Bus session:

```sh
cargo run -p onedrive-daemon
cargo run -p onedrive-desktop
```

Choose **Account → Sign in with Microsoft**. After the browser flow, use
**Sync → Refresh OneDrive** to fetch the catalog. **Sync → OneDrive files**
opens a separate browser for manual downloads. To mount a drive, choose
**Virtual drive → Mount drive** and select an existing empty directory outside
the sync folder and cache. A configured mount directory is remembered and
remounted on the next daemon start after the catalog is ready.

English is the default interface language. Choose Ukrainian from the
**Language** menu, then restart the desktop application. The selection
is saved under `$XDG_CONFIG_HOME/onedrive-linux/language`.

SQLite state is stored under `$XDG_STATE_HOME/onedrive-linux/` (normally
`~/.local/state/onedrive-linux/`). Keep that directory when upgrading: it
contains the catalog, queues, and local drafts.

## Virtual-drive writes and conflicts

FUSE saves changes locally before upload. A successful local save does not
mean the cloud copy has changed. Closed drafts upload only while the daemon
is connected, the drive is mounted, and a sync folder is not configured.
New folders must be confirmed in OneDrive before their files can upload.
Drafts with an uncertain upload result are retained for review.
After a completed catalog refresh, cloud-deleted files disappear from an
already mounted drive. Open read handles and local drafts remain available.
New cloud files still appear after remounting the drive.

Use **Virtual drive → Local changes** to inspect drafts. You can export a copy
without removing its draft, compare small files with their cloud copy, retry
an eligible upload, or explicitly discard a draft. Discard permanently removes
local edits; it does not delete the cloud file. Upload diagnostics are written
to `$XDG_STATE_HOME/onedrive-linux/virtual-upload.log` without tokens or
upload session URLs:

```sh
tail -n 30 ~/.local/state/onedrive-linux/virtual-upload.log
```

If an old mount remains busy after a crash, inspect it with `findmnt` before
unmounting it. The daemon will not take over an occupied mount directory.

## Offline files and cache

**Virtual drive → Choose files for offline access** opens a collapsed tree.
Select several files or folders; selecting a folder includes its descendant
files. Pinned versions remain available without a network connection.
**Clear cache** removes only unpinned temporary copies. Local drafts are kept.
The cache limit can be changed in the desktop window.

## Develop and test

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
dbus-run-session -- sh scripts/smoke-dbus.sh
dbus-run-session -- sh scripts/smoke-tray.sh
```

The tray smoke test needs `gtk4-broadwayd`. FUSE integration tests require a
real Linux FUSE device and can be run explicitly:

```sh
ONEDRIVE_FUSE_SMOKE=1 cargo test -p onedrive-mount mounted_tree_reads_cached_file_without_network
ONEDRIVE_FUSE_SMOKE=1 cargo test -p onedrive-mount writable_mount_persists_truncating_replacement
```

To build an AppImage, provide locally verified `linuxdeploy` and
`appimagetool` executables and run `scripts/build-appimage.sh`. The version
comes from `[workspace.package].version` in the root `Cargo.toml`.

## Publish a release

After setting the workspace version and merging the release commit, push a
tag with the exact `X.Y.Z` version (without a `v` prefix):

```sh
git tag 1.0.0
git push origin 1.0.0
```

The tag must match the Cargo version. The release workflow runs source and
smoke checks, builds and checks an x86_64 AppImage, writes its SHA-256 checksum,
and publishes both files to a GitHub Release. A tag such as `v1.0.0` or
`1.0.0-rc1` does not produce a release. The ordinary CI workflow runs on
branch pushes and pull requests; tag pushes use the release workflow.

If a tagged release fails and the fix keeps the same application version,
open **Actions → Release AppImage → Run workflow**. Select the branch with
the fix and enter the existing `X.Y.Z` tag. The workflow checks that the
branch still declares that version, runs the tests, and builds and verifies
the AppImage. Only then does it move the tag to the verified commit and
publish the release. It refuses to move a tag that already has a release.
The moved tag makes the release source archive match the AppImage source.

Local tests cover recovery and conflicts, but a full live-account and
cross-distribution AppImage validation is still needed. Keep an independent
backup of important cloud files while testing write operations.
