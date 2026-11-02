# PC Dock

Телефон подключается к ПК одним USB-кабелем и превращается в док-станцию: экран —
в окне на ПК, мышь/клавиатура/тачскрин ПК управляют телефоном, звук идёт на ПК,
телефон будится удалённо. Режим разработчика и отладка по USB на телефоне не нужны
(транспорт — Android Open Accessory).

## Из чего состоит

```
pc-dock/
├── sdk/                      Rust: протокол, сессия, ввод (ОС-независимо, sans-IO)
│   ├── pcdock-core/          пакеты, HID-отчёты, ввод, сессия; 50 тестов
│   ├── pcdock-ffi/           C ABI поверх ядра (сессия, события, ввод); 3 теста
│   └── pcdock-usb/           USB-адаптер (поиск, handshake, Dock) + утилита pcdock-probe
└── devices/android/          Android-направление
    ├── app-screencast/       приложение на телефон (Kotlin): захват экрана/звука, USB
    ├── viewer/               ПК-сторона (Python + pygame): окно, ввод, звук поверх pcdock-ffi
    ├── PROTOCOL.md           спецификация протокола v2 (пакеты, сессия, ввод, звук)
    ├── DECISIONS.md          принятые решения и причины
    ├── STATUS.md             что работает / не проверено / известные проблемы
    └── ROADMAP.md            план работ
```

Протокол живёт в одном месте — в ядре (`pcdock-core`) и `PROTOCOL.md`. Питон-протокола нет:
viewer говорит с ядром через `pcdock-ffi` (ctypes-обвязка `viewer/sdk.py`), транспорт
(pyusb), окно (pygame) и звук остаются на Питоне.

## Быстрый старт (Android)

```bash
cd devices/android
./dev.sh install   # собрать и поставить APK (подтвердить запись экрана один раз)
cd viewer && uv run main.py
```

Подробности — в `devices/android/README.md`, про SDK — в `sdk/README.md`.
