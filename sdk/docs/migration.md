# Переход со старых версий

Для тех, кто подключал крейты с коммита `201937e` и раньше (до 2026-11-10). До версии 1 API менялся без совместимости.

## Что изменилось

Крейт-библиотека **`pcdock-daemon` удалён**. Его содержимое (потоки телефонов, переподключения, кэш кадров) теперь в `pcdock-farm`, а общие
интерфейсы транспорта в `pcdock-core`. **Демон как программа (`pcdockd`) остался** и работает поверх фермы. Решение и причины:
`devices/android/DECISIONS.md`, пункт 30.

| Было | Стало |
|---|---|
| крейт `pcdock-daemon` | удалён; используйте `pcdock-farm` |
| `pcdock_daemon::Daemon` | `pcdock_farm::Farm` |
| `pcdock_daemon::DaemonEvent` | `pcdock_farm::FarmEvent` (добавились `Appeared`, `State`, `Lost`) |
| `pcdock_daemon::{PhoneLink, LinkError, InputCmd, PhoneId}` | `pcdock_core::link::{...}` |
| `pcdock_daemon::UsbLink` | `pcdock_usb::UsbLink` |
| `pcdock_farm::Farm` (sans-IO реестр сессий) | `pcdock_farm::Registry` |
| C: `pcdock_farm_*` (реестр без потоков) | C: `pcdock_registry_*`; имя `pcdock_farm_*` теперь у **живой** фермы |
| `Daemon::add_phone(id, link)` | `Farm::add_phone(id, link)` (то же); либо автообнаружение: `Farm::from_config(FarmConfig)` |
| `pcdockd --auto` брал только первый USB-телефон | берёт все телефоны, USB и ADB, с запасным переходом |
| `Settings::target` / выбор телефона | без изменений (PR #1), плюс `PhoneSpec`, `prefer` |
| ADB через приложение `app-adb` (порт 47631) | серверный процесс; нужен `server_apk` |

## Минимальные правки кода

```rust
// было
use pcdock_daemon::{Daemon, DaemonEvent, InputCmd, PhoneLink, UsbLink};
let mut d = Daemon::new();
d.add_phone(1, Box::new(UsbLink::new(cfg, settings)?))?;
for ev in d.drain() { /* DaemonEvent::... */ }

// стало
use pcdock_core::link::{InputCmd, PhoneLink};
use pcdock_farm::{Farm, FarmEvent};
use pcdock_usb::UsbLink;
let mut d = Farm::new();
d.add_phone(1, Box::new(UsbLink::new(cfg, settings)?))?;
for ev in d.drain() { /* FarmEvent::... */ }
```

Если ваш код сам крутит цикл «USB ↔ ядро» для каждого телефона (одна поток на телефон), его можно заменить на `Farm`: ферма делает
то же самое и сверх того обнаружение, состояния и запасные переходы. Начните с [farm.md](farm.md) и примера
`pcdock-farm/examples/minimal.rs`.

Прямая зависимость только от `pcdock-core` продолжает работать: сессия, HID и пакеты не менялись.

## Удалён C ABI (2026-11-12)

Продукт теперь только демоны (решение 34). Крейт `pcdock-ffi`, заголовок `pcdock.h`, `docs/c-abi.md` и старый Python-viewer удалены.
Вместо них: протокол демонов ([protocol.md](protocol.md)) и демо-клиенты в `demos/` (веб и pygame-GUI). Для Python есть готовый
клиент без зависимостей: `demos/gui/client.py`.
