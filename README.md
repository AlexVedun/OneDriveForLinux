# OneDriveForLinux

A Rust desktop client for OneDrive on Linux. This repository was developed in
small, independently tested stages. The source tree at each commit shows the
features available at that point; later features may not yet exist here.

## Build and test

On Fedora, install Rust, Cargo, GTK 4 and libadwaita development packages:

```sh
sudo dnf install rust cargo rustfmt clippy gcc pkgconf-pkg-config gtk4-devel libadwaita-devel
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

The initial commit contains a background daemon, a desktop status window and
core checks. Microsoft sign-in and synchronization are added in later commits.
