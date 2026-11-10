# Rust API: `pcdock-farm`

```toml
[dependencies]
pcdock-farm = { path = "sdk/pcdock-farm" }   # по умолчанию с USB, ADB и serial
pcdock-core = { path = "sdk/pcdock-core" }   # InputCmd, PhoneState и др.
```

Без готовых транспортов (только интерфейсы и ядро): `default-features = false`. Тогда доступны `Farm`, `Registry` и интерфейсы,
а настройку и транспорты вы приносите сами.

## Минимальный пример

Проверяемый пример лежит в `sdk/pcdock-farm/examples/minimal.rs` (запуск: `cargo run -p pcdock-farm --example minimal`):

```rust
//! Минимальный потребитель фермы: печатает события и нажимает в центре экрана первого готового телефона.
//! Запуск: `PCDOCK_SERVER_APK=.../server-adb-debug.apk cargo run -p pcdock-farm --example minimal`

use std::time::Duration;

use pcdock_core::input::TouchPhase;
use pcdock_core::link::{InputCmd, PhoneState};
use pcdock_farm::{Farm, FarmConfig, FarmEvent};

fn main() -> Result<(), String> {
    // Автообнаружение USB и ADB; APK сервера берётся из PCDOCK_SERVER_APK.
    let (mut farm, warnings) = Farm::from_config(FarmConfig::default())?;
    for w in warnings {
        eprintln!("предупреждение: {w}");
    }
    let mut tapped = false;
    loop {
        for ev in farm.drain() {
            match ev {
                FarmEvent::Appeared { phone, transport, serial, .. } => {
                    println!("телефон {phone}: {transport} {serial:?}");
                    farm.set_watching(phone, true)?; // хотим видео от этого телефона
                }
                FarmEvent::State { phone, state, detail } => println!("{phone}: {state:?} {detail}"),
                FarmEvent::Video { phone, frame } => {
                    println!("{phone}: кадр {} ({} байт)", frame.seq, frame.data.len())
                }
                FarmEvent::Lost { phone } => println!("{phone}: потерян"),
                _ => {}
            }
        }
        if !tapped {
            if let Some(&id) = farm.phones().first() {
                if farm.state(id) == Ok(PhoneState::Ready) {
                    farm.command(id, InputCmd::DeviceEnable { kind: pcdock_core::input::DeviceKind::Touch, on: true })?;
                    farm.command(id, InputCmd::touch_ex(0, TouchPhase::Down, 0.5, 0.5, None, None, None))?;
                    farm.command(id, InputCmd::touch_ex(0, TouchPhase::Up, 0.5, 0.5, None, None, None))?;
                    tapped = true;
                }
            }
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}
```

`drain()` нужно вызывать регулярно (десятки раз в секунду): он выполняет обнаружение по таймеру и возвращает накопленные
события. Если не вызывать, события копятся в очереди.

## `FarmConfig`

Все поля с умолчаниями (`FarmConfig::default()`):

| Поле | Тип | По умолчанию | Назначение |
|---|---|---|---|
| `name` | `String` | `pcdock/1.0` | имя хоста в рукопожатии (видно телефону) |
| `video` | `bool` | `true` | включить видео у новых телефонов |
| `audio` | `bool` | `false` | включить звук у новых телефонов |
| `auto` | `bool` | `true` | принимать всё найденное (кроме `except`) |
| `except` | `Vec<String>` | пусто | исключения для `auto` |
| `pinned` | `Vec<PhoneSpec>` | пусто | закреплённые телефоны (принимаются всегда) |
| `usb`, `adb` | `bool` | `true` | искать эти семейства |
| `serial` | `bool` | `false` | искать serial-трубы (включать явно) |
| `vendor_ids` | `Vec<u16>` | 15 производителей | какие USB VID считать телефонами |
| `adb_server` | `Option<String>` | локальный `127.0.0.1:5037` | адрес сервера ADB |
| `adb_local_port` | `u16` | `47000` | первый локальный порт проброса ADB |
| `server_apk` | `Option<PathBuf>` | из `PCDOCK_SERVER_APK` | APK серверного процесса для ADB (обязателен для ADB) |
| `serial_baud` | `u32` | `921600` | скорость serial (у USB-CDC не влияет) |
| `every` | `Duration` | 2 с | период опроса источников |
| `lost_after` | `Duration` | 45 с | через сколько отсутствия телефон считается потерянным |
| `order` | `Vec<Transport>` | USB, ADB, Serial | порядок предпочтения путей |
| `prefer` | `HashMap<String, Transport>` | пусто | путь для отдельного телефона: серийник → путь |
| `fallback_after` | `Option<Duration>` | 20 с | через сколько перейти на другой путь; `None` — не переходить |

`Farm::from_config` возвращает ошибку, если нет ни `auto`, ни закреплённых телефонов, и предупреждения, если часть источников не
поднялась (например, у USB-источника нет доступа к libusb). Ферма при этом работает с тем, что поднялось.

### Настройка из JSON

`FarmConfig::from_json(text)` (фича `json`, включена в C ABI) читает объект; все ключи необязательны, **неизвестный ключ
является ошибкой**:

```json
{
  "name": "my-app/1.0", "video": true, "audio": false,
  "auto": true, "except": ["usb:1-2.3", "SERIAL123"],
  "phones": ["adb:SERIAL456", "usb:SERIAL789"],
  "usb": true, "adb": true, "serial": false,
  "vendor_ids": [10864, 8921],
  "adb_server": "127.0.0.1:5037", "adb_local_port": 47000,
  "server_apk": "/opt/pcdock/server-adb-debug.apk",
  "serial_baud": 921600,
  "every_ms": 2000, "lost_after_ms": 45000,
  "order": ["usb", "adb", "serial"],
  "prefer": {"SERIAL456": "adb"},
  "fallback_after_ms": 20000
}
```

`phones` соответствует `pinned`; `fallback_after_ms: 0` отключает переход; `vendor_ids` задаются числами (`0x2A70` = 10864).

## `Farm`

| Метод | Назначение |
|---|---|
| `Farm::from_config(cfg)` | создать ферму с обнаружением (возвращает `(Farm, предупреждения)`) |
| `Farm::new()` | пустая ферма без обнаружения: телефоны добавляете сами |
| `enable_discovery(providers, factory, DiscoveryConfig)` | включить обнаружение со своими источниками и фабрикой |
| `drain() -> Vec<FarmEvent>` | выполнить обнаружение (если пора) и забрать события |
| `discover_now() -> Vec<FarmEvent>` | принудительный проход обнаружения (для тестов) |
| `add_phone(id, link)` / `remove_phone(id)` | добавить свой канал под своим номером / убрать (поток останавливается, телефон возвращается в обычный режим) |
| `phones() -> Vec<PhoneId>` | номера телефонов по возрастанию |
| `state(id) -> PhoneState` | текущее состояние |
| `command(id, InputCmd)` | команда телефону (ввод, поток, побудка) |
| `set_watching(id, bool)` / `is_watching(id)` | подписка на видео |
| `last_frame(id) -> Option<CachedFrame>` | последний кадр без очереди событий |
| `events() -> &Receiver<FarmEvent>` | канал событий напрямую (вместо `drain`, без обнаружения и кэша) |

Освобождение: удаление `Farm` останавливает потоки телефонов и возвращает телефоны в обычный режим.

### `InputCmd`

| Вариант | Параметры |
|---|---|
| `DeviceEnable { kind, on }` | `kind`: `Touch`, `Mouse`, `Keyboard` |
| `Touch { cid, phase, x, y, width, height, pressure }` (конструктор `touch_ex`) | `cid` номер пальца; `phase`: `Down`, `Move`, `Up`, `Cancel`; `x`, `y` доли 0..1; остальное доли или `None` |
| `CursorMoveAbs { x, y }` / `CursorMoveRel { dx, dy }` / `CursorResync` | курсор мыши |
| `MouseButton { button, down }` | `Left`, `Right`, `Middle` |
| `Scroll { ticks }` / `ScrollH { ticks }` | вертикальная и горизонтальная прокрутка |
| `Key { usage, down }` | код USB HID |
| `ReleaseAll` | отпустить всё нажатое |
| `RequestFrame` | повторить последний кадр |
| `Wake` | включить экран |
| `SetStream { video, audio }` | включить или выключить видео и звук |

## Свой транспорт или источник

Интерфейсы лежат в `pcdock_core::link`:

```rust
pub trait PhoneLink: Send {
    fn is_connected(&self) -> bool;
    fn connect(&mut self) -> Result<(), LinkError>;      // Gone = повторить позже; Blocked = нужны действия человека
    fn poll(&mut self) -> Result<Vec<Event>, LinkError>; // один шаг: отправить, прочитать, вернуть события
    fn input(&mut self, cmd: &InputCmd) -> Result<(), LinkError>;
    fn shutdown(&mut self);
}
pub trait Discovery: Send {
    fn scan(&mut self) -> Vec<Sighting>;
    fn changed(&mut self) -> bool { false }              // true = опросить раньше срока (события hotplug)
}
pub trait LinkFactory: Send {
    fn make(&mut self, s: &Sighting) -> Result<Box<dyn PhoneLink>, String>;
}
```

Вместо готовой `Farm::from_config` создайте `Farm::new()` и вызовите `enable_discovery(vec![Box::new(мой_источник)],
Box::new(моя_фабрика), DiscoveryConfig::default())`. Ферма сама сделает слияние по серийнику, запасные переходы, состояния и
потерю. Готовый пример канала на любом потоке байтов: `StreamLink` в `pcdock-serial`.

Ошибки канала: `LinkError::Gone` (устройства нет, ферма повторяет через секунду), `Fatal` (ошибка, о ней сообщается
`Warning`), `Blocked(состояние, текст)` (нужны действия человека: ферма выставляет состояние и подсказку и не повторяет
сообщение каждую секунду).

## `Registry` (без потоков)

Если нужно возить байты самому (встраиваемая платформа, свой рантайм), используйте `Registry`: реестр сессий без потоков и
транспорта. Вы передаёте ему входящие байты и время, забираете то, что надо отправить, и события. Из C доступен как
`pcdock_registry_*`.
