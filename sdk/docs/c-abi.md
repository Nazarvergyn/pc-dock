# C ABI (`pcdock-ffi`)

Сборка: `cargo build -p pcdock-ffi` создаёт `libpcdock_ffi.so` (Linux), `.dylib` (macOS) или `pcdock_ffi.dll` (Windows) в `sdk/target/`.
Заголовок: `sdk/pcdock-ffi/include/pcdock.h` (101 функция и 59 констант). Генерируется командой `python3 tools/gen_header.py` из
исходников; `--check` проверяет, что он не устарел.

```bash
gcc -I sdk/pcdock-ffi/include prog.c -L sdk/target/debug -lpcdock_ffi -Wl,-rpath,$PWD/sdk/target/debug
```

В библиотеке три уровня; основной для приложений **`pcdock_farm_*`**:

| Префикс | Что это |
|---|---|
| `pcdock_farm_*` | живая ферма: сама находит, подключает и переподключает телефоны |
| `pcdock_registry_*` | реестр сессий без потоков и транспорта (байты возите вы) |
| `pcdock_session_*` | одна сессия протокола |

## Соглашения

- Функции возвращают `int32`: `0` ок, `1` есть элемент (для `poll`), отрицательные: `-1` нулевой указатель, `-2` неверный аргумент
  (например, нет такого телефона), `-3` ошибка ввода, `-4` мал буфер, `-99` паника внутри (паника не выходит за границу).
- Буферы выделяет вызывающий. Если буфер мал, функция возвращает `-4` и пишет в `*inout_len` нужный размер; вызовите ещё раз.
- Строки: UTF-8, без завершающего нуля, длина передаётся отдельно.
- Дескриптор фермы **однопоточный**: все вызовы с одним дескриптором делайте из одного потока (потоки телефонов ферма запускает
  внутри себя сама).
- Время вы не передаёте: ферма сама ведёт часы.

## Жизненный цикл

```c
const char *cfg = "{\"server_apk\":\"/opt/pcdock/server-adb-debug.apk\"}";   // "{}" = всё по умолчанию
PcdockFarm *f = pcdock_farm_new((const uint8_t *)cfg, strlen(cfg));
if (!f) {                                    // ошибка конфига: текст в last_error
    uint8_t msg[300]; size_t len = sizeof msg;
    pcdock_farm_last_error(msg, &len);
    fprintf(stderr, "%.*s\n", (int)len, msg);
    return 1;
}
...
pcdock_farm_free(f);                         // останавливает потоки, возвращает телефоны в обычный режим
```

`pcdock_farm_new` принимает JSON-настройку (ключи те же, что в [farm.md](farm.md#настройка-из-json)). Неизвестный ключ — ошибка, функция
вернёт NULL. Предупреждения запуска: `pcdock_farm_warnings(f, buf, &len)` (строки через `\n`).

## События

```c
uint32_t kind, phone;
while (pcdock_farm_poll(f, &kind, &phone) == 1) {      // 1 = есть событие
    uint8_t buf[4 * 1024 * 1024]; size_t len = sizeof buf;
    if (kind == PCDOCK_FARM_EV_VIDEO) {
        pcdock_farm_event_data(f, buf, &len);           // JPEG
    } else {
        pcdock_farm_event_json(f, buf, &len);           // подробности, JSON-объект
    }
}
```

`pcdock_farm_poll` берёт следующее событие и делает его текущим; обнаружение выполняется внутри. Вызывайте регулярно (десятки раз в
секунду). `event_json` и `event_data` относятся к текущему событию.

| Вид | Константа | JSON (`event_json`) |
|---|---|---|
| 1 | `PCDOCK_FARM_EV_APPEARED` | `{"phone","transport","address","serial"}` |
| 2 | `..._STATE` | `{"phone","state","detail"}` |
| 3 | `..._LOST` | `{"phone"}` |
| 4 | `..._CONNECTED` | `{"phone","version","capabilities","name"}` |
| 5 | `..._DISPLAY` | `{"phone","stream_w","stream_h","screen_w","screen_h","dpi","rotation"}` |
| 6 | `..._VIDEO` | `{"phone","seq","codec","key","pts_us","size"}`; данные кадра: `event_data` |
| 7 | `..._AUDIO_CONFIG` | `{"phone","codec","rate","channels"}` |
| 8 | `..._AUDIO` | `{"phone","pts_us","size"}`; данные PCM: `event_data` |
| 9 | `..._STATUS` | `{"phone","code","detail"}` |
| 10 | `..._PEER_SILENT` | `{"phone"}` |
| 11 | `..._MALFORMED` | `{"phone","detail"}` |
| 12 | `..._DISCONNECTED` | `{"phone","reason"}` |
| 13 | `..._LINK_DOWN` | `{"phone","detail"}` |
| 14 | `..._WARNING` | `{"phone","detail"}` |

Состояние в JSON: строка `appeared`, `connecting`, `ready`, `needs_permission`, `needs_app`, `needs_access`, `needs_auth`, `lost`.
Числом: `pcdock_farm_state(f, id)` возвращает 0 appeared, 1 connecting, 2 ready, 3 needs_permission, 4 lost, 5 needs_app,
6 needs_access, 7 needs_auth.

Видео приходит в события **только от телефонов, за которыми следят**: `pcdock_farm_set_watching(f, id, true)`. Последний кадр без
очереди: `pcdock_farm_last_frame(...)`.

## Телефоны

| Функция | Назначение |
|---|---|
| `pcdock_farm_phone_count(f)` | число телефонов |
| `pcdock_farm_phone_at(f, index, &id)` | номер по порядку (1 есть, 0 нет) |
| `pcdock_farm_state(f, id)` | состояние числом |
| `pcdock_farm_set_watching(f, id, on)` / `is_watching` | подписка на видео |
| `pcdock_farm_last_frame(f, id, &seq, &codec, &key, &pts, buf, &len)` | последний кадр |

## Ввод

Координаты и размеры доли `0..1`; отрицательное значение размера, пятна или силы означает «по умолчанию».

| Функция | Назначение |
|---|---|
| `pcdock_farm_device_enable(f, id, kind, on)` | `kind`: `PCDOCK_DEV_TOUCH` 0, `PCDOCK_DEV_MOUSE` 1, `PCDOCK_DEV_KEYBOARD` 2 |
| `pcdock_farm_touch_ex(f, id, cid, phase, x, y, w, h, pressure)` | касание; `phase`: `PCDOCK_TOUCH_DOWN` 0, `MOVE` 1, `UP` 2, `CANCEL` 3 |
| `pcdock_farm_cursor_move_abs(f, id, x, y)` / `cursor_move_rel(f, id, dx, dy)` / `cursor_resync(f, id)` | курсор мыши |
| `pcdock_farm_mouse_button(f, id, button, down)` | `PCDOCK_BTN_LEFT` 0, `RIGHT` 1, `MIDDLE` 2 |
| `pcdock_farm_scroll(f, id, ticks)` / `scroll_h(f, id, ticks)` | прокрутка |
| `pcdock_farm_key(f, id, usage, down)` | код USB HID |
| `pcdock_farm_release_all(f, id)` | отпустить всё |
| `pcdock_farm_request_frame(f, id)` | повторить последний кадр |
| `pcdock_farm_wake(f, id)` | включить экран |
| `pcdock_farm_set_stream(f, id, video, audio)` | включить или выключить видео и звук |

Команда неизвестному телефону возвращает `-2`, нулевому дескриптору `-1`.

## Пример на Python (ctypes)

```python
import ctypes, json
lib = ctypes.CDLL("sdk/target/debug/libpcdock_ffi.so")
lib.pcdock_farm_new.restype = ctypes.c_void_p
lib.pcdock_farm_new.argtypes = [ctypes.c_char_p, ctypes.c_size_t]

cfg = json.dumps({"server_apk": "server-adb-debug.apk"}).encode()
farm = lib.pcdock_farm_new(cfg, len(cfg))

kind, phone = ctypes.c_uint32(), ctypes.c_uint32()
buf = ctypes.create_string_buffer(4 * 1024 * 1024)
while True:
    while lib.pcdock_farm_poll(ctypes.c_void_p(farm), ctypes.byref(kind), ctypes.byref(phone)) == 1:
        n = ctypes.c_size_t(len(buf))
        lib.pcdock_farm_event_json(ctypes.c_void_p(farm), buf, ctypes.byref(n))
        print(kind.value, json.loads(buf.raw[:n.value]))
```

Для других функций задайте `argtypes`/`restype` по `pcdock.h` (`uint32` → `c_uint32`, `float` → `c_float`, `bool` → `c_bool`, указатель на
дескриптор → `c_void_p`).

## Реестр без потоков (`pcdock_registry_*`)

Для тех, кто возит байты сам: `pcdock_registry_add(r, id)`, затем для каждого телефона `start(now_ms)`, `handle_input(bytes)`,
`handle_timeout(now_ms)`, `poll_transmit(...)` (что отправить в транспорт: поток или управляющий запрос), `poll_event(...)` и
`event_*` для разбора. Состояние сессии (`idle`/`connecting`/`active`) отличается от `PhoneState` живой фермы. См. `pcdock.h` и
тесты `pcdock-ffi`.
