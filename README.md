# OneDrive для Linux

Разработка по [плану](onedrive-linux-plan.md). Сейчас реализуется этап 0: каркас
движка и окна. Синхронизация и вход в Microsoft ещё не реализованы.

## Разработка на Fedora 44

```sh
sudo dnf install rust cargo rustfmt clippy gcc pkgconf-pkg-config gtk4-devel libadwaita-devel
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

В терминале с пользовательской шиной D-Bus запустите процессы отдельно:

```sh
cargo run -p onedrive-daemon
cargo run -p onedrive-desktop
```

Окно запрашивает `Status` у фонового процесса. На данном этапе процесс сообщает
`needs-login`; при недоступной шине или процессе окно сообщает `daemon-unavailable`.

Для будущих этапов понадобятся `fuse3 fuse3-devel` (виртуальный диск) и
`libsecret-devel` (системное хранилище секретов). AppImage также требует
отдельного инструмента упаковки и проверки на целевых системах.
