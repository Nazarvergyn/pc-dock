# SDK (рабочее название; название ещё не выбрано)

Библиотека управления **Android-телефоном по USB Accessory (AOA)**: получать картинку (и звук) и отправлять ввод (касания, клавиатуру, мышь).
Протокол описан в [`devices/android/PROTOCOL.md`](../devices/android/PROTOCOL.md).

## Состав

| Крейт | Что это | Статус |
|---|---|---|
| `pcdock-core` | Ядро: пакеты, HID-отчёты, ввод, сессия. **Не знает об ОС и не делает ввод-вывод** (sans-IO) | готово, 50 тестов |
| C ABI (`pcdock-ffi`) | Оболочка для вызова из Python, C и игр: непрозрачный дескриптор сессии, явные буферы, коды возврата | готово, 3 теста |
| USB-адаптер (`pcdock-usb`) | Находит телефон, AOA handshake, выполняет запросы ядра через libusb/rusb; утилита `pcdock-probe` | готово, проверено на телефоне |

## Как устроено ядро

Ядро ничего не читает и не пишет само. Хост-адаптер крутит цикл:

```rust
let mut session = Session::new(Config::default());   // настройки: строки AOA, HID, таймауты, поток
// ... адаптер выполнил AOA handshake (hid::get_protocol_request(), hid::start_requests(&cfg.aoa))
// ... телефон перешёл в режим аксессуара, транспорт (bulk) открыт
session.start(now_ms);

loop {
    // 1. байты из транспорта -> ядро
    session.handle_input(now_ms, &bytes_read);
    // 2. по таймеру
    if now_ms >= session.next_timeout().unwrap_or(u64::MAX) { session.handle_timeout(now_ms); }
    // 3. что нужно отправить
    while let Some(t) = session.poll_transmit() {
        match t {
            Transmit::Stream(bytes)   => write_bulk_out(&bytes),
            Transmit::Control(req)    => usb_control_transfer(&req), // HID-запросы AOA
        }
    }
    // 4. события для фронтенда
    while let Some(e) = session.poll_event() { /* Video, Display, Audio, Status, ... */ }
}

// Команды фронтенда:
session.touch(TouchPoint { pressure: Some(0.7), ..TouchPoint::new(0, TouchPhase::Down, 0.5, 0.25) })?;
session.cursor_move_abs(0.5, 0.5)?;
session.mouse_button(MouseButton::Right, true)?;
session.scroll(-1.0)?;
session.key(0x04, true)?;                       // клавиша A (USB HID usage)
session.device_enable(DeviceKind::Mouse, false); // снимает мышь с телефона
```

Время (`now_ms`) — любые монотонные миллисекунды, ядро часы не читает. Это делает всё тестируемым без телефона.

## Что делает ядро

- Собирает и разбирает пакеты протокола, перестраивается после мусора в потоке.
- Согласует версию и возможности, шлёт `PING`, повторяет `HELLO`, сообщает `PeerSilent`, если телефон молчит.
- Формирует HID-отчёты: многоточечный тачскрин (до 10 пальцев, площадь, сила), клавиатура, мышь.
- Пересчитывает координаты касаний по повороту экрана (Android поворачивает виртуальный тачскрин вместе с экраном).
- Оценивает положение курсора мыши и «прижимает» его к углу для точного отсчёта; есть поправка `mouse_gain` на ускорение указателя.
- Включает и отключает устройства ввода командой (снятие с телефона, отпускание нажатого).
- Все идентификаторы настраиваются: строки AOA, id и дескрипторы HID, число пальцев, значения по умолчанию.

## Что делает адаптер (не ядро)

USB-транспорт, AOA handshake, выполнение `ControlRequest`, часы, потоки, отрисовка, звук.
Решение «сбросить USB, если телефон молчит» принимает адаптер по событию `PeerSilent`.

## USB-адаптер (`pcdock-usb`)

`Dock` соединяет ядро с реальным телефоном:

```rust
let mut dock = Dock::new(Config::default(), Settings::default())?;
dock.connect()?;                       // найти телефон, handshake, открыть канал, запустить сессию
loop {
    if !dock.is_connected() { dock.connect()?; continue; }
    for event in dock.poll()? { /* Video, Display, ... */ }
    dock.session().touch(/* ... */)?;  // команды ввода
}
dock.shutdown();                       // снять ввод и вернуть телефон в обычный режим
```

- Сам переждёт переподключение телефона и повторит handshake.
- Если телефон молчит после `HELLO` (приложение-приёмник не запущено), сбрасывает USB (не чаще раза в минуту, до 3 раз): так система заново запускает приложение.
- `shutdown()` (и `Drop`) отпускает ввод, снимает HID, сбрасывает USB; если выход пришёлся сразу после handshake, ждёт перехода в режим аксессуара.
- Нефатальные проблемы (например, телефон отклонил запрос) собираются в `take_warnings()`.
- libusb собирается из исходников (`libusb1-sys`), системный пакет не нужен.

Проверка на живом телефоне: `cargo run -p pcdock-usb --bin pcdock-probe -- --seconds 20 --tap`.

## C ABI (`pcdock-ffi`)

```bash
cd sdk
cargo build -p pcdock-ffi   # -> target/debug/libpcdock_ffi.so (.dll/.dylib на Win/Mac)
cargo test -p pcdock-ffi
```

Использование из Python (см. `devices/android/viewer/sdk.py`): создать сессию, задать имя
и начальный поток, включить нужные устройства, `start(now_ms)`; дальше цикл: байты из bulk —
в `handle_input`, по `next_timeout` — `handle_timeout`, `poll_transmit` — в bulk OUT или
control transfer, `poll_event` — кадры/звук/статусы фронтенду. Время — монотонные миллисекунды
вызывающего, поток один. Все функции возвращают `0` (ок), `1` (есть элемент) или отрицательный
код ошибки; паники ловятся внутри.

## Сборка и тесты

```bash
cd sdk
cargo test      # ядро: 35 модульных + 14 сценариев сессии с имитацией телефона; адаптер: 3
cargo clippy --all-targets
```

Зависимостей у ядра нет, `unsafe` запрещён (`#![forbid(unsafe_code)]`).
