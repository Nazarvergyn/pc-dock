# Установка

Три способа поставить `pcdockd`, `pcdock-autod`, `pcdock-check` и `pcdock-probe`. Выберите один.

| Способ | Что нужно | Когда выбрать |
|---|---|---|
| [Скрипт](#1-скрипт-curl--sh) | `curl`, `tar` | обычная установка готовых бинарников, Rust не нужен |
| [`cargo install`](#2-cargo-install) | Rust 1.82+, libusb | уже есть Rust; хочется собрать под свою систему |
| [Из исходников](#3-из-исходников) | Rust, libusb, при желании JDK 17 | разработка, правки кода |

Результат везде одинаков: четыре исполняемых файла. `pcdockd` держит телефоны, `pcdock-autod` запускает сценарии, `pcdock-check` проверяет
вашу машину и телефон, `pcdock-probe` показывает USB-устройства.

## 1. Скрипт (curl | sh)

**Linux (x86_64) и macOS (Apple Silicon):**

```bash
curl -fsSL https://pc-dock.prozorov.me/install.sh | sh
```

**Windows (PowerShell):**

```powershell
irm https://pc-dock.prozorov.me/install.ps1 | iex
```

Скрипт:

1. находит последний релиз (в том числе альфу) на GitHub;
2. скачивает архив для вашей системы и `SHA256SUMS`;
3. **проверяет контрольную сумму**; при несовпадении ничего не устанавливает;
4. кладёт бинарники в `~/.pcdock/bin` (Windows: `%USERPROFILE%\.pcdock\bin`), документацию в `~/.pcdock/docs`;
5. на Windows добавляет каталог в PATH пользователя; на Linux и macOS печатает строку для PATH.

На Linux и macOS добавьте строку, которую напечатал скрипт, в `~/.profile` (или `~/.zshrc`):

```bash
export PATH="$HOME/.pcdock/bin:$PATH"
```

Чтобы прочитать скрипт перед запуском: скачайте его (`curl -fsSL https://pc-dock.prozorov.me/install.sh -o install.sh`), откройте и запустите `sh install.sh`.

Параметры:

| Что | Как |
|---|---|
| конкретная версия | `sh install.sh v0.1.0-alpha.3` (PowerShell: `.\install.ps1 -Version v0.1.0-alpha.3`) |
| другой каталог | переменная `PCDOCK_HOME` |
| версия через переменную | `PCDOCK_VERSION` |

Если сайт недоступен, тот же скрипт лежит в репозитории:
`curl -fsSL https://raw.githubusercontent.com/toxic-sunflower/pc-dock/main/install.sh | sh`.

**Обновление:** запустите скрипт ещё раз, он заменит бинарники. **Удаление:** удалите каталог `~/.pcdock` и строку PATH.

## 2. cargo install

Нужны Rust 1.82+ ([rustup.rs](https://rustup.rs)) и `libusb`:

- Debian/Ubuntu: `sudo apt install libusb-1.0-0-dev pkg-config`;
- macOS: `brew install libusb pkg-config`;
- Windows: ничего, libusb собирается вместе с программой (нужны Visual Studio Build Tools).

Установка из репозитория, без публикации на crates.io:

```bash
cargo install --locked --git https://github.com/toxic-sunflower/pc-dock \
    pcdockd pcdock-autod pcdock-check pcdock-usb
```

`pcdock-usb` даёт `pcdock-probe`. Бинарники окажутся в `~/.cargo/bin` (он уже в PATH, если Rust ставили через rustup).
Выбрать версию: добавьте `--tag v0.1.0-alpha.3`. Обновить: повторите команду с `--force`.

Крейты на crates.io не опубликованы, команда `cargo install pcdockd` пока не работает.

## 3. Из исходников

```bash
git clone https://github.com/toxic-sunflower/pc-dock && cd pc-dock/sdk
cargo build --release                  # sdk/target/release/: pcdockd, pcdock-autod, pcdock-check, pcdock-probe
```

Сервер для ADB-режима и приложение для AOA-режима (нужны JDK 17 и Android SDK):

```bash
cd ../devices/android/app-screencast
./gradlew :server-adb:assembleDebug    # server-adb/build/outputs/apk/debug/server-adb-debug.apk
./gradlew :app:assembleDebug           # app/build/outputs/apk/debug/app-debug.apk
```

## Что ещё скачать: файлы для телефона

Скрипт и `cargo install` ставят программы для ПК. Для ADB-режима нужен APK сервера, для AOA-режима приложение; оба лежат в
[релизе](https://github.com/toxic-sunflower/pc-dock/releases):

| Файл | Для чего |
|---|---|
| `pcdock-server-<версия>.apk` | ADB-режим: путь к нему передаётся в `pcdockd --server-apk`, ферма сама кладёт сервер на телефон |
| `pcdock-app-<версия>.apk` | AOA-режим: устанавливается на телефон (`adb install -r pcdock-app-<версия>.apk`) |

Пока в репозитории нет ключа подписи, APK в релизе **не подписаны**. Для них используйте сборку из исходников (`assembleDebug` подписывает отладочным ключом).

## Подготовка ПК и телефона

Права на USB (udev на Linux), WSL2 (usbipd), Windows, macOS: [transports.md](transports.md#подготовка-пк). Включение отладки по USB и
подтверждения на телефоне: [operations.md](operations.md#4-подготовка-телефона).

## Проверка

```bash
pcdock-check
```

Команда находит телефоны, проверяет USB и ADB и печатает отчёт. Если что-то не так, она говорит, что делать. Дальше: [getting-started.md](getting-started.md).
