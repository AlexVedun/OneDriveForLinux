# OneDrive для Linux

Разработка по [плану](onedrive-linux-plan.md). Сейчас реализуется этап 0: каркас
движка, окна и входа. Синхронизация ещё не реализована.

## Разработка на Fedora 44

```sh
sudo dnf install rust cargo rustfmt clippy gcc pkgconf-pkg-config gtk4-devel libadwaita-devel
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo build -p onedrive-daemon
dbus-run-session -- sh scripts/smoke-dbus.sh
```

## Тестовая регистрация Microsoft

В [Microsoft Entra App registrations](https://learn.microsoft.com/en-us/entra/identity-platform/quickstart-register-app)
создайте приложение для личных аккаунтов Microsoft. В разделе Authentication
добавьте платформу **Mobile and desktop applications** с URI
`http://localhost/callback`. В API permissions добавьте делегированные
`Files.ReadWrite` и `offline_access`. Клиентский секрет не создавайте.

Укажите полученный Application (client) ID переменной `ONEDRIVE_CLIENT_ID` в
окружении фонового процесса. Для проверки входа нужен работающий Secret Service
(например, GNOME Keyring или KWallet). Токены в файлы проекта и журнал не пишутся.

В терминале с пользовательской шиной D-Bus запустите процессы отдельно:

```sh
ONEDRIVE_CLIENT_ID=<client-id> cargo run -p onedrive-daemon
cargo run -p onedrive-desktop
```

Окно запрашивает `Status` у фонового процесса. Кнопка входа открывает системный
браузер; демон проверяет `state`, обменивает код с PKCE, проверяет доступ к
`/me/drive` и сохраняет refresh token в системном хранилище секретов.
Кнопка «Обновить состояние» покажет `connected` после успешного входа.
При недоступной шине или процессе окно сообщает `daemon-unavailable`.

Для будущего виртуального диска понадобятся `fuse3 fuse3-devel`. AppImage
также требует отдельного инструмента упаковки и проверки на целевых системах.
