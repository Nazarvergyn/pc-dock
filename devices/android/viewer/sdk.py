"""Питон-обвязка над Rust-ядром (pcdock-ffi): сессия, ввод, события.

Протокола на Питоне здесь нет: байты из USB уходят в `handle_input`,
байты в USB и HID-запросы забираются из `poll_transmit`, события — из `poll_event`.
Питон делает только транспорт (pyusb), окно (pygame) и звук.
"""
import ctypes
import os
import time

OK = 0
ITEM = 1
ERR_NULL = -1
ERR_ARG = -2
ERR_INPUT = -3
ERR_SMALL_BUF = -4
ERR_PANIC = -99

TX_STREAM = 0
TX_CONTROL = 1
DIR_OUT = 0
DIR_IN = 1

EV_CONNECTED = 1
EV_DISPLAY = 2
EV_VIDEO = 3
EV_AUDIO_CONFIG = 4
EV_AUDIO = 5
EV_STATUS = 6
EV_PEER_SILENT = 7
EV_MALFORMED = 8
EV_DISCONNECTED = 9

STATE_IDLE = 0
STATE_CONNECTING = 1
STATE_ACTIVE = 2

DEV_TOUCH = 0
DEV_MOUSE = 1
DEV_KEYBOARD = 2

TOUCH_DOWN = 0
TOUCH_MOVE = 1
TOUCH_UP = 2
TOUCH_CANCEL = 3

BTN_LEFT = 0
BTN_RIGHT = 1
BTN_MIDDLE = 2

DC_TIMEOUT = 0
DC_VERSION = 1
DC_PEER_ERROR = 2
DC_CLOSED = 3

_ERRORS = {
    ERR_NULL: "нулевой указатель",
    ERR_ARG: "неверный аргумент",
    ERR_INPUT: "ввод отклонён ядром (устройство выключено?)",
    ERR_SMALL_BUF: "мал буфер",
    ERR_PANIC: "паника в Rust-ядре",
}


class SdkError(RuntimeError):
    def __init__(self, code, where=""):
        super().__init__(f"pcdock: {where}: {_ERRORS.get(code, code)}")
        self.code = code


def _lib_path():
    if "PCDOCK_LIB" in os.environ:
        return os.environ["PCDOCK_LIB"]
    here = os.path.dirname(os.path.abspath(__file__))
    root = os.path.join(here, "..", "..", "..")  # viewer -> android -> devices -> pc-dock
    for cand in (
        os.path.join(root, "sdk", "target", "debug", "libpcdock_ffi.so"),
        os.path.join(root, "sdk", "target", "debug", "pcdock_ffi.dll"),
        os.path.join(root, "sdk", "target", "debug", "libpcdock_ffi.dylib"),
    ):
        if os.path.exists(cand):
            return cand
    return "libpcdock_ffi.so"


def _load():
    lib = ctypes.CDLL(_lib_path())
    u8p = ctypes.POINTER(ctypes.c_uint8)
    u16p = ctypes.POINTER(ctypes.c_uint16)
    u32p = ctypes.POINTER(ctypes.c_uint32)
    u64p = ctypes.POINTER(ctypes.c_uint64)
    boolp = ctypes.POINTER(ctypes.c_bool)
    sizep = ctypes.POINTER(ctypes.c_size_t)
    voidp = ctypes.c_void_p

    def fn(name, restype, *argtypes):
        f = getattr(lib, name)
        f.restype = restype
        f.argtypes = list(argtypes)
        return f

    lib.pcdock_session_new.restype = voidp
    lib.pcdock_session_new.argtypes = []
    lib.pcdock_session_free.restype = None
    lib.pcdock_session_free.argtypes = [voidp]
    lib.pcdock_version.restype = ctypes.c_char_p
    lib.pcdock_version.argtypes = []
    fns = {
        "new": lib.pcdock_session_new,
        "free": lib.pcdock_session_free,
        "version": lib.pcdock_version,
        "set_name": fn("pcdock_session_set_name", ctypes.c_int32, voidp, voidp, ctypes.c_size_t),
        "set_initial_stream": fn("pcdock_session_set_initial_stream", ctypes.c_int32, voidp, ctypes.c_bool, ctypes.c_bool),
        "start": fn("pcdock_session_start", ctypes.c_int32, voidp, ctypes.c_uint64),
        "close": fn("pcdock_session_close", ctypes.c_int32, voidp),
        "state": fn("pcdock_session_state", ctypes.c_int32, voidp),
        "handle_input": fn("pcdock_session_handle_input", ctypes.c_int32, voidp, ctypes.c_uint64, voidp, ctypes.c_size_t),
        "handle_timeout": fn("pcdock_session_handle_timeout", ctypes.c_int32, voidp, ctypes.c_uint64),
        "next_timeout": fn("pcdock_session_next_timeout", ctypes.c_int32, voidp, u64p),
        "poll_transmit": fn("pcdock_session_poll_transmit", ctypes.c_int32, voidp, u32p, u32p, u8p, u16p, u16p, u16p, voidp, sizep),
        "poll_event": fn("pcdock_session_poll_event", ctypes.c_int32, voidp, u32p),
        "event_connected": fn("pcdock_event_connected", ctypes.c_int32, voidp, u16p, u32p, voidp, sizep),
        "event_display": fn("pcdock_event_display", ctypes.c_int32, voidp, u16p, u16p, u16p, u16p, u16p, u8p),
        "event_video": fn("pcdock_event_video", ctypes.c_int32, voidp, voidp, boolp, u64p, voidp, sizep),
        "event_audio_config": fn("pcdock_event_audio_config", ctypes.c_int32, voidp, u8p, u32p, u8p),
        "event_audio": fn("pcdock_event_audio", ctypes.c_int32, voidp, u64p, voidp, sizep),
        "event_status": fn("pcdock_event_status", ctypes.c_int32, voidp, u16p, voidp, sizep),
        "event_disconnected": fn("pcdock_event_disconnected", ctypes.c_int32, voidp, u32p, voidp, sizep),
        "device_enable": fn("pcdock_session_device_enable", ctypes.c_int32, voidp, ctypes.c_uint32, ctypes.c_bool),
        "touch": fn("pcdock_session_touch", ctypes.c_int32, voidp, ctypes.c_uint8, ctypes.c_uint32, ctypes.c_float, ctypes.c_float),
        "touch_ex": fn("pcdock_session_touch_ex", ctypes.c_int32, voidp, ctypes.c_uint8, ctypes.c_uint32, ctypes.c_float, ctypes.c_float, ctypes.c_float, ctypes.c_float, ctypes.c_float),
        "cursor_move_abs": fn("pcdock_session_cursor_move_abs", ctypes.c_int32, voidp, ctypes.c_float, ctypes.c_float),
        "cursor_move_rel": fn("pcdock_session_cursor_move_rel", ctypes.c_int32, voidp, ctypes.c_int32, ctypes.c_int32),
        "cursor_resync": fn("pcdock_session_cursor_resync", ctypes.c_int32, voidp),
        "mouse_button": fn("pcdock_session_mouse_button", ctypes.c_int32, voidp, ctypes.c_uint32, ctypes.c_bool),
        "scroll": fn("pcdock_session_scroll", ctypes.c_int32, voidp, ctypes.c_float),
        "scroll_h": fn("pcdock_session_scroll_h", ctypes.c_int32, voidp, ctypes.c_float),
        "key": fn("pcdock_session_key", ctypes.c_int32, voidp, ctypes.c_uint16, ctypes.c_bool),
        "release_all": fn("pcdock_session_release_all", ctypes.c_int32, voidp),
        "request_frame": fn("pcdock_session_request_frame", ctypes.c_int32, voidp),
        "wake": fn("pcdock_session_wake", ctypes.c_int32, voidp),
        "set_stream": fn("pcdock_session_set_stream", ctypes.c_int32, voidp, ctypes.c_bool, ctypes.c_bool),
        "dropped_bytes": fn("pcdock_session_dropped_bytes", ctypes.c_uint64, voidp),
        "aoa_string": fn("pcdock_aoa_string", ctypes.c_int32, ctypes.c_uint32, voidp, sizep),
    }
    return fns


def now_ms():
    return int(time.monotonic() * 1000)


class SdkSession:
    """Сессия PC Dock поверх Rust-ядра. Не потокобезопасна: вызывать из одного потока."""

    def __init__(self, name="pcdock-viewer/1.0", video=True, audio=False):
        try:
            self._f = _load()
        except OSError as e:
            raise SdkError(ERR_NULL,
                           f"не найдена libpcdock_ffi ({e}); соберите: cd sdk && cargo build -p pcdock-ffi")
        self._h = self._f["new"]()
        if not self._h:
            raise SdkError(ERR_PANIC, "new")
        self._tx_buf = ctypes.create_string_buffer(65536)
        self._video_buf = ctypes.create_string_buffer(4 * 1024 * 1024)
        self._text_buf = ctypes.create_string_buffer(4096)
        self.set_name(name)
        self.set_initial_stream(video, audio)

    # --- служебное ---

    def _check(self, rc, where):
        if rc < 0:
            raise SdkError(rc, where)
        return rc

    @staticmethod
    def lib_version():
        return _load()["version"]().decode("utf-8", "replace")

    def aoa_strings(self):
        """6 строк AOA с завершающим нулём (производитель, модель, описание, версия, URI, серийник)."""
        out = []
        for i in range(6):
            n = ctypes.c_size_t(256)
            rc = self._f["aoa_string"](i, self._text_buf, ctypes.byref(n))
            self._check(rc, "aoa_string")
            out.append(bytes(self._text_buf[: n.value]))
        return out

    # --- жизненный цикл ---

    def set_name(self, name):
        b = name.encode("utf-8")
        self._check(self._f["set_name"](self._h, b, len(b)), "set_name")

    def set_initial_stream(self, video, audio):
        self._check(self._f["set_initial_stream"](self._h, bool(video), bool(audio)), "set_initial_stream")

    def start(self, now=None):
        self._check(self._f["start"](self._h, now_ms() if now is None else now), "start")

    def close(self):
        if self._h:
            try:
                self._f["close"](self._h)
            finally:
                self._f["free"](self._h)
                self._h = None

    def __del__(self):
        try:
            self.close()
        except Exception:
            pass

    def state(self):
        return self._check(self._f["state"](self._h), "state")

    def dropped_bytes(self):
        return self._f["dropped_bytes"](self._h)

    # --- ввод-вывод ---

    def handle_input(self, data, now=None):
        if not data:
            return
        self._check(
            self._f["handle_input"](self._h, now_ms() if now is None else now, data, len(data)),
            "handle_input",
        )

    def handle_timeout(self, now=None):
        self._check(
            self._f["handle_timeout"](self._h, now_ms() if now is None else now), "handle_timeout"
        )

    def next_timeout(self):
        t = ctypes.c_uint64(0)
        rc = self._check(self._f["next_timeout"](self._h, ctypes.byref(t)), "next_timeout")
        return t.value if rc == ITEM else None

    def poll_transmit(self):
        """('stream', bytes) | ('control', {...}) | None."""
        kind = ctypes.c_uint32(0)
        direction = ctypes.c_uint32(0)
        request = ctypes.c_uint8(0)
        value = ctypes.c_uint16(0)
        index = ctypes.c_uint16(0)
        in_len = ctypes.c_uint16(0)
        n = ctypes.c_size_t(len(self._tx_buf))
        rc = self._check(
            self._f["poll_transmit"](
                self._h,
                ctypes.byref(kind),
                ctypes.byref(direction),
                ctypes.byref(request),
                ctypes.byref(value),
                ctypes.byref(index),
                ctypes.byref(in_len),
                self._tx_buf,
                ctypes.byref(n),
            ),
            "poll_transmit",
        )
        if rc != ITEM:
            return None
        if kind.value == TX_STREAM:
            return ("stream", bytes(self._tx_buf[: n.value]))
        return (
            "control",
            {
                "direction": direction.value,
                "request": request.value,
                "value": value.value,
                "index": index.value,
                "in_len": in_len.value,
                "data": bytes(self._tx_buf[: n.value]),
            },
        )

    def poll_event(self):
        """(kind, dict) | None. kind: connected/display/video/audio_config/audio/status/
        peer_silent/malformed/disconnected."""
        kind = ctypes.c_uint32(0)
        rc = self._check(self._f["poll_event"](self._h, ctypes.byref(kind)), "poll_event")
        if rc != ITEM:
            return None
        k = kind.value
        if k == EV_CONNECTED:
            ver, caps = ctypes.c_uint16(0), ctypes.c_uint32(0)
            n = ctypes.c_size_t(len(self._text_buf))
            self._check(
                self._f["event_connected"](self._h, ctypes.byref(ver), ctypes.byref(caps), self._text_buf, ctypes.byref(n)),
                "event_connected",
            )
            return ("connected", {"version": ver.value, "caps": caps.value, "name": bytes(self._text_buf[: n.value]).decode("utf-8", "replace")})
        if k == EV_DISPLAY:
            v = [ctypes.c_uint16(0) for _ in range(5)]
            rot = ctypes.c_uint8(0)
            self._check(
                self._f["event_display"](self._h, *[ctypes.byref(x) for x in v], ctypes.byref(rot)),
                "event_display",
            )
            sw, sh, dw, dh, dpi = (x.value for x in v)
            return ("display", {"stream": (sw, sh), "screen": (dw, dh), "dpi": dpi, "rotation": rot.value})
        if k == EV_VIDEO:
            codec, key, pts = ctypes.c_uint8(0), ctypes.c_bool(False), ctypes.c_uint64(0)
            n = ctypes.c_size_t(len(self._video_buf))
            self._check(
                self._f["event_video"](self._h, ctypes.byref(codec), ctypes.byref(key), ctypes.byref(pts), self._video_buf, ctypes.byref(n)),
                "event_video",
            )
            return ("video", {"codec": codec.value, "key": bool(key.value), "pts_us": pts.value, "data": bytes(self._video_buf[: n.value])})
        if k == EV_AUDIO_CONFIG:
            codec, rate, ch = ctypes.c_uint8(0), ctypes.c_uint32(0), ctypes.c_uint8(0)
            self._check(
                self._f["event_audio_config"](self._h, ctypes.byref(codec), ctypes.byref(rate), ctypes.byref(ch)),
                "event_audio_config",
            )
            return ("audio_config", {"codec": codec.value, "rate": rate.value, "channels": ch.value})
        if k == EV_AUDIO:
            pts = ctypes.c_uint64(0)
            n = ctypes.c_size_t(len(self._video_buf))
            self._check(
                self._f["event_audio"](self._h, ctypes.byref(pts), self._video_buf, ctypes.byref(n)),
                "event_audio",
            )
            return ("audio", {"pts_us": pts.value, "data": bytes(self._video_buf[: n.value])})
        if k in (EV_STATUS, EV_MALFORMED):
            code = ctypes.c_uint16(0)
            n = ctypes.c_size_t(len(self._text_buf))
            self._check(
                self._f["event_status"](self._h, ctypes.byref(code), self._text_buf, ctypes.byref(n)),
                "event_status",
            )
            text = bytes(self._text_buf[: n.value]).decode("utf-8", "replace")
            return ("malformed" if k == EV_MALFORMED else "status", {"code": code.value, "detail": text})
        if k == EV_PEER_SILENT:
            return ("peer_silent", {})
        if k == EV_DISCONNECTED:
            reason = ctypes.c_uint32(0)
            n = ctypes.c_size_t(len(self._text_buf))
            self._check(
                self._f["event_disconnected"](self._h, ctypes.byref(reason), self._text_buf, ctypes.byref(n)),
                "event_disconnected",
            )
            return ("disconnected", {"reason": reason.value, "detail": bytes(self._text_buf[: n.value]).decode("utf-8", "replace")})
        return ("unknown", {"kind": k})

    # --- ввод ---

    def device_enable(self, kind, enabled):
        self._check(self._f["device_enable"](self._h, kind, bool(enabled)), "device_enable")

    def touch(self, cid, phase, x, y):
        """Совместимость: идёт через touch_ex с default пятном/силой."""
        self.touch_ex(cid, phase, x, y, -1.0, -1.0, -1.0)

    def touch_ex(self, cid, phase, x, y, width=-1.0, height=-1.0, pressure=-1.0):
        """Касание с пятном/силой. Отрицательное = default ядра."""
        self._check(
            self._f["touch_ex"](
                self._h, cid, phase, float(x), float(y),
                float(width), float(height), float(pressure),
            ),
            "touch_ex",
        )

    def cursor_move_abs(self, x, y):
        self._check(self._f["cursor_move_abs"](self._h, float(x), float(y)), "cursor_move_abs")

    def cursor_move_rel(self, dx, dy):
        self._check(self._f["cursor_move_rel"](self._h, int(dx), int(dy)), "cursor_move_rel")

    def cursor_resync(self):
        self._check(self._f["cursor_resync"](self._h), "cursor_resync")

    def mouse_button(self, button, down):
        self._check(self._f["mouse_button"](self._h, button, bool(down)), "mouse_button")

    def scroll(self, ticks):
        self._check(self._f["scroll"](self._h, float(ticks)), "scroll")

    def scroll_h(self, ticks):
        self._check(self._f["scroll_h"](self._h, float(ticks)), "scroll_h")

    def key(self, usage, down):
        self._check(self._f["key"](self._h, usage, bool(down)), "key")

    def release_all(self):
        self._check(self._f["release_all"](self._h), "release_all")

    def request_frame(self):
        self._check(self._f["request_frame"](self._h), "request_frame")

    def wake(self):
        self._check(self._f["wake"](self._h), "wake")

    def set_stream(self, video, audio):
        self._check(self._f["set_stream"](self._h, bool(video), bool(audio)), "set_stream")
