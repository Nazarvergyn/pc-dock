/* Сгенерировано tools/gen_header.py из sdk/pcdock-ffi — не править руками. */
#ifndef PCDOCK_H
#define PCDOCK_H

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef struct PcdockFarm PcdockFarm;
typedef struct PcdockRegistry PcdockRegistry;
typedef struct PcdockSession PcdockSession;

#define PCDOCK_OK 0
#define PCDOCK_ITEM 1
#define PCDOCK_ERR_NULL -1
#define PCDOCK_ERR_ARG -2
#define PCDOCK_ERR_INPUT -3
#define PCDOCK_ERR_SMALL_BUF -4
#define PCDOCK_ERR_PANIC -99
#define PCDOCK_TX_STREAM 0
#define PCDOCK_TX_CONTROL 1
#define PCDOCK_DIR_OUT 0
#define PCDOCK_DIR_IN 1
#define PCDOCK_EV_CONNECTED 1
#define PCDOCK_EV_DISPLAY 2
#define PCDOCK_EV_VIDEO 3
#define PCDOCK_EV_AUDIO_CONFIG 4
#define PCDOCK_EV_AUDIO 5
#define PCDOCK_EV_STATUS 6
#define PCDOCK_EV_PEER_SILENT 7
#define PCDOCK_EV_MALFORMED 8
#define PCDOCK_EV_DISCONNECTED 9
#define PCDOCK_STATE_IDLE 0
#define PCDOCK_STATE_CONNECTING 1
#define PCDOCK_STATE_ACTIVE 2
#define PCDOCK_DEV_TOUCH 0
#define PCDOCK_DEV_MOUSE 1
#define PCDOCK_DEV_KEYBOARD 2
#define PCDOCK_TOUCH_DOWN 0
#define PCDOCK_TOUCH_MOVE 1
#define PCDOCK_TOUCH_UP 2
#define PCDOCK_TOUCH_CANCEL 3
#define PCDOCK_BTN_LEFT 0
#define PCDOCK_BTN_RIGHT 1
#define PCDOCK_BTN_MIDDLE 2
#define PCDOCK_DC_TIMEOUT 0
#define PCDOCK_DC_VERSION 1
#define PCDOCK_DC_PEER_ERROR 2
#define PCDOCK_DC_CLOSED 3
#define PCDOCK_FARM_EV_APPEARED 1
#define PCDOCK_FARM_EV_STATE 2
#define PCDOCK_FARM_EV_LOST 3
#define PCDOCK_FARM_EV_CONNECTED 4
#define PCDOCK_FARM_EV_DISPLAY 5
#define PCDOCK_FARM_EV_VIDEO 6
#define PCDOCK_FARM_EV_AUDIO_CONFIG 7
#define PCDOCK_FARM_EV_AUDIO 8
#define PCDOCK_FARM_EV_STATUS 9
#define PCDOCK_FARM_EV_PEER_SILENT 10
#define PCDOCK_FARM_EV_MALFORMED 11
#define PCDOCK_FARM_EV_DISCONNECTED 12
#define PCDOCK_FARM_EV_LINK_DOWN 13
#define PCDOCK_FARM_EV_WARNING 14
#define PCDOCK_PHONE_APPEARED 0
#define PCDOCK_PHONE_CONNECTING 1
#define PCDOCK_PHONE_READY 2
#define PCDOCK_PHONE_NEEDS_PERMISSION 3
#define PCDOCK_PHONE_LOST 4
#define PCDOCK_PHONE_NEEDS_APP 5
#define PCDOCK_PHONE_NEEDS_ACCESS 6
#define PCDOCK_PHONE_NEEDS_AUTH 7

/*
 * Создаёт сессию с настройками по умолчанию. Освободить через `pcdock_session_free`.
 */
PcdockSession *pcdock_session_new(void);

/*
 * Освобождает сессию. После вызова указатель недействителен.
 */
void pcdock_session_free(PcdockSession *p);

/*
 * Имя реализации для `HELLO` (UTF-8, например "pcdock-viewer/1.0").
 */
int32_t pcdock_session_set_name(PcdockSession *p, const uint8_t *name, size_t len);

/*
 * Настройки потока: отправятся телефону сразу после согласования (вызывать до `start`;
 * вживую меняется через `pcdock_session_set_stream`).
 */
int32_t pcdock_session_set_initial_stream(PcdockSession *p, bool video, bool audio);

/*
 * Транспорт открыт (режим аксессуара): шлёт `HELLO` и регистрирует устройства ввода.
 */
int32_t pcdock_session_start(PcdockSession *p, uint64_t now_ms);

/*
 * Транспорт потерян (кабель выдернули): сессия закрывается, ввод сбрасывается.
 */
int32_t pcdock_session_close(PcdockSession *p);

/*
 * Состояние: 0 idle, 1 connecting, 2 active.
 */
int32_t pcdock_session_state(PcdockSession *p);

/*
 * Байты, прочитанные из транспорта.
 */
int32_t pcdock_session_handle_input(PcdockSession *p, uint64_t now_ms, const uint8_t *data, size_t len);

/*
 * Вызвать, когда наступило время из `pcdock_session_next_timeout`.
 */
int32_t pcdock_session_handle_timeout(PcdockSession *p, uint64_t now_ms);

/*
 * Когда вызвать `handle_timeout` в следующий раз. Возвращает 1 и пишет метку, либо 0 (нет).
 */
int32_t pcdock_session_next_timeout(PcdockSession *p, uint64_t *out_ms);

/*
 * Забирает одну единицу на отправку. Возвращает 1 (заполнены `out_*`), 0 (пусто) или ошибку.
 *
 * `kind`: 0 = байты потока (bulk OUT), 1 = USB control transfer.
 * Для control: `dir` 0 = OUT (данные в `out_buf`), 1 = IN (`in_len` = сколько байт ждать).
 */
int32_t pcdock_session_poll_transmit(PcdockSession *p, uint32_t *out_kind, uint32_t *out_dir, uint8_t *out_request, uint16_t *out_value, uint16_t *out_index, uint16_t *out_in_len, uint8_t *out_buf, size_t *inout_len);

/*
 * Забирает одно событие. Возвращает 1 (вид в `out_kind`, детали — через `pcdock_event_*`),
 * 0 (пусто) или ошибку.
 */
int32_t pcdock_session_poll_event(PcdockSession *p, uint32_t *out_kind);

/*
 * Детали `Connected`: версия, общие возможности, имя телефона (UTF-8 в буфере).
 */
int32_t pcdock_event_connected(PcdockSession *p, uint16_t *out_version, uint32_t *out_caps, uint8_t *out_buf, size_t *inout_len);

/*
 * Детали `Display`: размеры потока и экрана, плотность, поворот.
 */
int32_t pcdock_event_display(PcdockSession *p, uint16_t *out_stream_w, uint16_t *out_stream_h, uint16_t *out_screen_w, uint16_t *out_screen_h, uint16_t *out_dpi, uint8_t *out_rotation);

/*
 * Детали `Video`: кодек, ключевой ли кадр, метка времени, данные кадра в буфере.
 */
int32_t pcdock_event_video(PcdockSession *p, uint8_t *out_codec, bool *out_key, uint64_t *out_pts_us, uint8_t *out_buf, size_t *inout_len);

/*
 * Детали `AudioConfig`: кодек, частота, число каналов.
 */
int32_t pcdock_event_audio_config(PcdockSession *p, uint8_t *out_codec, uint32_t *out_rate, uint8_t *out_channels);

/*
 * Детали `Audio`: метка времени, PCM-данные в буфере.
 */
int32_t pcdock_event_audio(PcdockSession *p, uint64_t *out_pts_us, uint8_t *out_buf, size_t *inout_len);

/*
 * Детали `Status`/`Malformed`: код и текст в буфере. Для `Malformed` код всегда 0.
 */
int32_t pcdock_event_status(PcdockSession *p, uint16_t *out_code, uint8_t *out_buf, size_t *inout_len);

/*
 * Детали `Disconnected`: причина (0 таймаут, 1 версия, 2 ошибка телефона, 3 закрыто),
 * для ошибки телефона — её текст в буфере.
 */
int32_t pcdock_event_disconnected(PcdockSession *p, uint32_t *out_reason, uint8_t *out_buf, size_t *inout_len);

/*
 * Включает или отключает устройство ввода (отключение снимает его с телефона).
 */
int32_t pcdock_session_device_enable(PcdockSession *p, uint32_t kind, bool enabled);

/*
 * Касание: `id` — номер пальца, `phase` 0/1/2/3 = down/move/up/cancel,
 * координаты — доли отображаемой картинки 0.0..1.0.
 */
int32_t pcdock_session_touch(PcdockSession *p, uint8_t id, uint32_t phase, float x, float y);

/*
 * Касание с явными пятном и силой: `width`/`height` — доли ширины экрана 0.0..1.0,
 * `pressure` — 0.0..1.0. Отрицательное значение = default из конфига.
 * Скрипт автотестов задаёт их сам на каждый вызов: клик и свайп собираются
 * из DOWN/MOVE/UP с нужными паузами и координатами на стороне вызывающего.
 */
int32_t pcdock_session_touch_ex(PcdockSession *p, uint8_t id, uint32_t phase, float x, float y, float width, float height, float pressure);

/*
 * Абсолютное положение курсора в долях картинки 0.0..1.0.
 */
int32_t pcdock_session_cursor_move_abs(PcdockSession *p, float x, float y);

/*
 * Относительное движение курсора в пикселях.
 */
int32_t pcdock_session_cursor_move_rel(PcdockSession *p, int32_t dx, int32_t dy);

/*
 * Забыть оценку положения курсора: следующее движение прижмёт его к углу.
 */
int32_t pcdock_session_cursor_resync(PcdockSession *p);

/*
 * Кнопка мыши: 0 левая, 1 правая, 2 средняя.
 */
int32_t pcdock_session_mouse_button(PcdockSession *p, uint32_t button, bool down);

/*
 * Прокрутка в «щелчках» колеса (дробь копится внутри ядра).
 */
int32_t pcdock_session_scroll(PcdockSession *p, float ticks);

/*
 * Горизонтальная прокрутка (наклон колеса / жест трекпада).
 */
int32_t pcdock_session_scroll_h(PcdockSession *p, float ticks);

/*
 * Клавиша: USB HID usage (страница Keyboard), как SDL scancode.
 */
int32_t pcdock_session_key(PcdockSession *p, uint16_t usage, bool down);

/*
 * Отпускает всё нажатое (потеря фокуса окна).
 */
int32_t pcdock_session_release_all(PcdockSession *p);

/*
 * Просит телефон изменить параметры потока (вживую; до старта — как начальные).
 */
int32_t pcdock_session_set_stream(PcdockSession *p, bool video, bool audio);

/*
 * Просит телефон прислать текущее содержимое экрана (вживую; до согласования — ничего).
 */
int32_t pcdock_session_request_frame(PcdockSession *p);

/*
 * Просит телефон включить экран на минуту (разблокировка вручную).
 */
int32_t pcdock_session_wake(PcdockSession *p);

/*
 * Сколько байт отброшено при перестройке потока. `u64::MAX` при нулевом дескрипторе.
 */
uint64_t pcdock_session_dropped_bytes(PcdockSession *p);

/*
 * Строка AOA по индексу 0..5 (производитель, модель, описание, версия, URI, серийник).
 * Возвращает 1 (строка в буфере с завершающим нулём), 0 (нет такой) или ошибку.
 */
int32_t pcdock_aoa_string(uint32_t index, uint8_t *out_buf, size_t *inout_len);

/*
 * Версия библиотеки (NUL-терминированная строка, живёт всегда).
 */
const uint8_t *pcdock_version(void);

/*
 * Создаёт пустую ферму. Освободить через `pcdock_registry_free`.
 */
PcdockRegistry *pcdock_registry_new(void);

/*
 * Освобождает ферму вместе со всеми сессиями.
 */
void pcdock_registry_free(PcdockRegistry *p);

/*
 * Добавляет трубку. Имя/поток задаются отдельно до `start`.
 */
int32_t pcdock_registry_add(PcdockRegistry *p, uint32_t id);

/*
 * Убирает трубку (сессия закрывается).
 */
int32_t pcdock_registry_remove(PcdockRegistry *p, uint32_t id);

/*
 * Число трубок в ферме.
 */
int32_t pcdock_registry_count(PcdockRegistry *p);

/*
 * Идентификатор трубки по порядковому номеру 0..count (порядок сортированный).
 * Возвращает 1 и пишет id, либо 0 (нет такой) или ошибку.
 */
int32_t pcdock_registry_id_at(PcdockRegistry *p, uint32_t index, uint32_t *out_id);

/*
 * Имя реализации для `HELLO` (вызывать до `start`).
 */
int32_t pcdock_registry_set_name(PcdockRegistry *p, uint32_t id, const uint8_t *name, size_t len);

/*
 * Начальные настройки потока трубки (вызывать до `start`).
 */
int32_t pcdock_registry_set_initial_stream(PcdockRegistry *p, uint32_t id, bool video, bool audio);

/*
 * Флаг «трубку сейчас смотрят»: сокет-мост шлёт её кадры, анализ берёт по запросу.
 */
int32_t pcdock_registry_set_watching(PcdockRegistry *p, uint32_t id, bool watching);

int32_t pcdock_registry_is_watching(PcdockRegistry *p, uint32_t id);

/*
 * Последний кадр трубки без потребления очереди событий.
 * Пишет seq/кодек/ключ/pts и данные в буфер. Возвращает 1 (есть кадр), 0 (нет) или ошибку.
 */
int32_t pcdock_registry_last_frame(PcdockRegistry *p, uint32_t id, uint64_t *out_seq, uint8_t *out_codec, bool *out_key, uint64_t *out_pts_us, uint8_t *out_buf, size_t *inout_len);

/*
 * Состояние сессии трубки: 0 idle, 1 connecting, 2 active.
 */
int32_t pcdock_registry_state(PcdockRegistry *p, uint32_t id);

int32_t pcdock_registry_start(PcdockRegistry *p, uint32_t id, uint64_t now_ms);

int32_t pcdock_registry_close(PcdockRegistry *p, uint32_t id);

int32_t pcdock_registry_handle_input(PcdockRegistry *p, uint32_t id, uint64_t now_ms, const uint8_t *data, size_t len);

int32_t pcdock_registry_handle_timeout(PcdockRegistry *p, uint32_t id, uint64_t now_ms);

int32_t pcdock_registry_next_timeout(PcdockRegistry *p, uint32_t id, uint64_t *out_ms);

int32_t pcdock_registry_poll_transmit(PcdockRegistry *p, uint32_t id, uint32_t *out_kind, uint32_t *out_dir, uint8_t *out_request, uint16_t *out_value, uint16_t *out_index, uint16_t *out_in_len, uint8_t *out_buf, size_t *inout_len);

int32_t pcdock_registry_poll_event(PcdockRegistry *p, uint32_t id, uint32_t *out_kind);

int32_t pcdock_registry_event_connected(PcdockRegistry *p, uint32_t id, uint16_t *out_version, uint32_t *out_caps, uint8_t *out_buf, size_t *inout_len);

int32_t pcdock_registry_event_display(PcdockRegistry *p, uint32_t id, uint16_t *out_stream_w, uint16_t *out_stream_h, uint16_t *out_screen_w, uint16_t *out_screen_h, uint16_t *out_dpi, uint8_t *out_rotation);

int32_t pcdock_registry_event_video(PcdockRegistry *p, uint32_t id, uint8_t *out_codec, bool *out_key, uint64_t *out_pts_us, uint8_t *out_buf, size_t *inout_len);

int32_t pcdock_registry_event_audio_config(PcdockRegistry *p, uint32_t id, uint8_t *out_codec, uint32_t *out_rate, uint8_t *out_channels);

int32_t pcdock_registry_event_audio(PcdockRegistry *p, uint32_t id, uint64_t *out_pts_us, uint8_t *out_buf, size_t *inout_len);

int32_t pcdock_registry_event_status(PcdockRegistry *p, uint32_t id, uint16_t *out_code, uint8_t *out_buf, size_t *inout_len);

int32_t pcdock_registry_event_disconnected(PcdockRegistry *p, uint32_t id, uint32_t *out_reason, uint8_t *out_buf, size_t *inout_len);

int32_t pcdock_registry_device_enable(PcdockRegistry *p, uint32_t id, uint32_t kind, bool enabled);

int32_t pcdock_registry_touch_ex(PcdockRegistry *p, uint32_t id, uint8_t cid, uint32_t phase, float x, float y, float width, float height, float pressure);

int32_t pcdock_registry_cursor_move_abs(PcdockRegistry *p, uint32_t id, float x, float y);

int32_t pcdock_registry_cursor_move_rel(PcdockRegistry *p, uint32_t id, int32_t dx, int32_t dy);

int32_t pcdock_registry_cursor_resync(PcdockRegistry *p, uint32_t id);

int32_t pcdock_registry_mouse_button(PcdockRegistry *p, uint32_t id, uint32_t button, bool down);

int32_t pcdock_registry_scroll(PcdockRegistry *p, uint32_t id, float ticks);

int32_t pcdock_registry_scroll_h(PcdockRegistry *p, uint32_t id, float ticks);

int32_t pcdock_registry_key(PcdockRegistry *p, uint32_t id, uint16_t usage, bool down);

int32_t pcdock_registry_release_all(PcdockRegistry *p, uint32_t id);

int32_t pcdock_registry_request_frame(PcdockRegistry *p, uint32_t id);

int32_t pcdock_registry_wake(PcdockRegistry *p, uint32_t id);

int32_t pcdock_registry_set_stream(PcdockRegistry *p, uint32_t id, bool video, bool audio);

/*
 * Создаёт ферму из JSON-настройки (`{}` — всё по умолчанию: автообнаружение USB и ADB).
 * При ошибке возвращает NULL, текст — в `pcdock_farm_last_error`. Освободить через `pcdock_farm_free`.
 */
PcdockFarm *pcdock_farm_new(const uint8_t *config_json, size_t len);

/*
 * Текст последней ошибки `pcdock_farm_new` или `-2` с этого же потока (UTF-8, без нуля в конце).
 */
int32_t pcdock_farm_last_error(uint8_t *out_buf, size_t *inout_len);

/*
 * Предупреждения запуска (например, USB-обнаружение не встало), строки через `\n`.
 */
int32_t pcdock_farm_warnings(PcdockFarm *p, uint8_t *out_buf, size_t *inout_len);

/*
 * Останавливает потоки телефонов, возвращает телефоны в обычный режим и освобождает ферму.
 */
void pcdock_farm_free(PcdockFarm *p);

/*
 * Берёт следующее событие и делает его текущим. Возвращает 1 (событие есть: вид и номер телефона записаны)
 * или 0 (событий нет). Видео приходит только от телефонов, за которыми «следят» (`pcdock_farm_set_watching`).
 */
int32_t pcdock_farm_poll(PcdockFarm *p, uint32_t *out_kind, uint32_t *out_phone);

/*
 * Подробности текущего события в виде JSON-объекта (UTF-8). Если буфер мал — `-4` и нужный размер.
 */
int32_t pcdock_farm_event_json(PcdockFarm *p, uint8_t *out_buf, size_t *inout_len);

/*
 * Двоичные данные текущего события: JPEG для видео, PCM для звука. Для остальных событий `-2`.
 */
int32_t pcdock_farm_event_data(PcdockFarm *p, uint8_t *out_buf, size_t *inout_len);

/*
 * Число телефонов в ферме.
 */
int32_t pcdock_farm_phone_count(PcdockFarm *p);

/*
 * Номер телефона по порядку 0..count. Возвращает 1 и пишет номер, либо 0, если такого нет.
 */
int32_t pcdock_farm_phone_at(PcdockFarm *p, uint32_t index, uint32_t *out_id);

/*
 * Состояние телефона: 0 appeared, 1 connecting, 2 ready, 3 needs_permission, 4 lost, 5 needs_app,
 * 6 needs_access, 7 needs_auth.
 */
int32_t pcdock_farm_state(PcdockFarm *p, uint32_t id);

/*
 * Следить за телефоном: видео идёт в события только от тех, за кем следят. Кэш кадра хранится всегда.
 */
int32_t pcdock_farm_set_watching(PcdockFarm *p, uint32_t id, bool watching);

int32_t pcdock_farm_is_watching(PcdockFarm *p, uint32_t id);

/*
 * Последний кадр телефона без потребления событий. Возвращает 1 (есть), 0 (нет) или ошибку.
 */
int32_t pcdock_farm_last_frame(PcdockFarm *p, uint32_t id, uint64_t *out_seq, uint8_t *out_codec, bool *out_key, uint64_t *out_pts_us, uint8_t *out_buf, size_t *inout_len);

int32_t pcdock_farm_device_enable(PcdockFarm *p, uint32_t id, uint32_t kind, bool enabled);

/*
 * Касание. Отрицательные ширина, высота или сила — значения по умолчанию.
 */
int32_t pcdock_farm_touch_ex(PcdockFarm *p, uint32_t id, uint8_t cid, uint32_t phase, float x, float y, float width, float height, float pressure);

int32_t pcdock_farm_cursor_move_abs(PcdockFarm *p, uint32_t id, float x, float y);

int32_t pcdock_farm_cursor_move_rel(PcdockFarm *p, uint32_t id, int32_t dx, int32_t dy);

int32_t pcdock_farm_cursor_resync(PcdockFarm *p, uint32_t id);

int32_t pcdock_farm_mouse_button(PcdockFarm *p, uint32_t id, uint32_t button, bool down);

int32_t pcdock_farm_scroll(PcdockFarm *p, uint32_t id, float ticks);

int32_t pcdock_farm_scroll_h(PcdockFarm *p, uint32_t id, float ticks);

int32_t pcdock_farm_key(PcdockFarm *p, uint32_t id, uint16_t usage, bool down);

int32_t pcdock_farm_release_all(PcdockFarm *p, uint32_t id);

int32_t pcdock_farm_request_frame(PcdockFarm *p, uint32_t id);

int32_t pcdock_farm_wake(PcdockFarm *p, uint32_t id);

int32_t pcdock_farm_set_stream(PcdockFarm *p, uint32_t id, bool video, bool audio);

#ifdef __cplusplus
}
#endif

#endif /* PCDOCK_H */
