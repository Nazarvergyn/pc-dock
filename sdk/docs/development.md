# Разработка

## Структура репозитория (Rust-часть)

```
sdk/
  pcdock-core/     ядро (протокол, сессия, HID, ввод, интерфейсы транспорта)
  pcdock-usb/      AOA по USB (libusb), pcdock-probe
  pcdock-adb/      ADB: клиент сервера ADB, запуск серверного процесса
  pcdock-serial/   serial-труба
  pcdock-farm/     Farm, FarmConfig, Registry; examples/minimal.rs
  pcdock-ffi/      C ABI; include/pcdock.h (генерируется)
  pcdock-serve/    протокол приложения и носители
  pcdockd/         демон
  docs/            эта документация
tools/gen_header.py  генератор pcdock.h
devices/android/   протокол (PROTOCOL.md), решения (DECISIONS.md), приложение и сервер для телефона, сценарий проверки
```

## Сборка и проверка

```bash
cd sdk
cargo build                       # всё
cargo test                        # все тесты (около 115)
cargo clippy --all-targets        # без предупреждений
cargo fmt --check
python3 ../tools/gen_header.py --check    # заголовок pcdock.h актуален
```

Тесты `pcdock-usb`, которым нужен libusb (`Dock::new`), пропускаются, если контекст USB недоступен (CI, контейнер). Тесты фермы и
демона работают на фейковых каналах и не требуют телефонов. Проверка на телефонах: [`TESTING.md`](../../devices/android/TESTING.md).

Часть приложения и сервера для телефона (Kotlin): `cd devices/android/app-screencast && ./gradlew :app:assembleDebug
:server-adb:assembleDebug :lib-protocol:test`.

## Принципы

- Ядро (`pcdock-core`) не делает ввод-вывод, не читает часы и не содержит `unsafe` (`#![forbid(unsafe_code)]`). Время приходит параметром.
- Решения принимает ферма, а не транспорт и не демон. Транспорт сообщает факты и выполняет байты.
- Демон тонкий: никакой логики про телефоны.
- `unsafe` допускается только на границе C ABI (`pcdock-ffi`), с комментарием `SAFETY`.
- Решения и их причины записываются в `devices/android/DECISIONS.md`.

## Как добавить транспорт

1. Реализуйте `PhoneLink` (подключить, прочитать и отправить байты, выполнить `InputCmd`, отпустить). Внутри держите
   `Session` из `pcdock-core`: ей отдаёте входящие байты, забираете то, что надо отправить (`Transmit::Stream` и
   `Transmit::Control`), и события. Образец: `StreamLink` в `pcdock-serial`.
2. Реализуйте `Discovery`, выдающий `Sighting` (путь, адрес, серийник, помеха). Если есть события (hotplug), верните `true` из
   `changed()`.
3. Добавьте вариант `Transport` в `pcdock-core::link` и ветку в фабрике `pcdock-farm::config` (или своя `LinkFactory`).
4. Ферма сама сделает слияние по серийнику, запасные переходы, состояния и потерю. Проверьте на фейках, как в тестах
   `pcdock-farm/src/runtime.rs`.

## Тесты фермы

`runtime.rs` содержит `FakeLink`, `FakeScan`, `FakeFactory` и тесты сценариев: появление, два пути, переключение, потеря и
возврат, закрепление, исключения, состояния с причинами, запасной переход, предпочтение, hotplug. Добавляйте сценарии
туда же, на фейках.

## C ABI

После изменения `#[no_mangle]`-функций или констант в `pcdock-ffi` перегенерируйте заголовок: `python3 tools/gen_header.py`.
Новые типы параметров нужно добавить в таблицу `SCALARS` генератора.
