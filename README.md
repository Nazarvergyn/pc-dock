# PC Dock

Демоны для управления Android-телефонами с ПК: экран (картинка и звук), ввод (касания с площадью и силой, мышь, клавиатура), файлы и
автоматизация сценариями на Lua как автотесты. Телефонов может быть много; подключение, отключение и выбор пути обрабатываются автоматически.

**Статус: альфа (0.1.0-alpha.1).** Основа работает на реальных телефонах (Linux, Android 15); Windows и macOS ждут проверки. См. «Что проверено» в
[`sdk/docs/README.md`](sdk/docs/README.md) и ограничения в [`sdk/docs/operations.md`](sdk/docs/operations.md).

## Что внутри

```
pcdockd        демон фермы: находит телефоны, держит подключения, раздаёт кадры, принимает ввод и файлы
pcdock-autod   демон автоматизации: сценарии на Lua, проверки, логи, прямой эфир, отчёты JSON
pcdock-check   самопроверка на вашей машине (Linux, macOS, Windows)
```

Единственный внешний интерфейс это протокол демонов (JSON и бинарные пакеты; TCP, WebSocket, локальная труба): [`sdk/docs/protocol.md`](sdk/docs/protocol.md).
Телефон подключается тремя путями: USB-аксессуар (AOA, нужно приложение), ADB (ничего не устанавливается, диалога записи экрана нет), serial.

## Быстрый старт

```bash
cd sdk && cargo build --release                    # нужен Rust и libusb (Linux: libusb-1.0-0-dev)
cd ../devices/android/app-screencast && ./gradlew :server-adb:assembleDebug   # сервер для ADB-режима
```

Включите на телефоне отладку по USB, подключите кабель и проверьте окружение:

```bash
sdk/target/release/pcdock-check --server-apk devices/android/app-screencast/server-adb/build/outputs/apk/debug/server-adb-debug.apk --input
```

Запуск:

```bash
sdk/target/release/pcdockd --auto --server-apk .../server-adb-debug.apk       # ферма (TCP 9001, WebSocket 9002)
sdk/target/release/pcdock-autod                                                # автоматизация (TCP 9101, WebSocket 9102)
```

Демо-клиенты протокола: `demos/web/index.html` (один файл, откройте в браузере) и `demos/gui` (pygame: `uv run app.py`).

## Документация

[`sdk/docs/`](sdk/docs/README.md): руководство по эксплуатации, протокол, демоны, сценарии Lua, пути подключения, архитектура, разработка.
Решения и причины: [`devices/android/DECISIONS.md`](devices/android/DECISIONS.md). План: [`devices/android/ROADMAP.md`](devices/android/ROADMAP.md).

## Состав репозитория

```
sdk/                     Rust: ядро, транспорты, ферма, демоны, клиент, самопроверка (+ docs/)
devices/android/         приложение (AOA) и сервер (ADB) для телефона на Kotlin, протокол ПК-телефон, решения, сценарий проверки
demos/                   демо-клиенты протокола (web, gui)
.github/workflows/       CI (Linux, Windows, macOS, Android) и релиз по тегу
```

## Лицензия

`MIT OR Apache-2.0` (файлы `LICENSE-MIT`, `LICENSE-APACHE`).
