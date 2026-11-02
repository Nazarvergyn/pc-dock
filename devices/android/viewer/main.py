"""PC Dock viewer: телефон как USB-аксессуар, экран в окне, ввод обратно.

Протокол живёт в Rust-ядре (`sdk/`): Питон делает только транспорт (pyusb),
окно (pygame) и звук. Сессия: `sdk.SdkSession`.
"""
import signal
import sys
import time
import traceback

import cv2
import numpy as np
import usb.core
import usb.util

from audio import AudioPlayer
from sdk import (
    DC_CLOSED,
    DC_PEER_ERROR,
    DC_TIMEOUT,
    DC_VERSION,
    DEV_KEYBOARD,
    DEV_MOUSE,
    DEV_TOUCH,
    STATE_ACTIVE,
    SdkError,
    SdkSession,
    now_ms,
)
from ui import PhoneWindow

T0 = time.time()


def log(msg=""):
    print(f"[{time.time() - T0:7.2f}s] {msg}", flush=True)


class _Reconnect(Exception):
    """Устройство ушло (кабель выдернут): быстро выйти в цикл переподключения, без traceback."""


def _is_gone(e):
    """Похоже ли исключение на выдернутый кабель (а не на временный сбой)."""
    if isinstance(e, usb.core.USBError):
        if getattr(e, "errno", None) == -4:  # LIBUSB_ERROR_NO_DEVICE
            return True
        s = str(e).lower()
        if "no such device" in s or "no device" in s or "disconnected" in s:
            return True
    return False


def safe_reset(dev, timeout=5.0):
    """dev.reset() на полумёртвом usbip (WSL2) может висеть — зовём в демоне-потоке с таймаутом."""
    import threading
    done = threading.Event()
    err = []

    def _run():
        try:
            dev.reset()
        except Exception as e:
            err.append(e)
        finally:
            done.set()

    t = threading.Thread(target=_run, daemon=True)
    t.start()
    if not done.wait(timeout):
        log(f"[!] reset завис (>={timeout:.0f}с) — устройство, видимо, уже ушло; продолжаю без него")
        return False
    if err:
        log(f"[!] reset: {err[0]!r}")
        return False
    return True


def _arm_timeout(dev, ms=1500):
    """Чтобы control transfer без явного таймаута не висел вечно на трупе."""
    try:
        dev.default_timeout = ms
    except Exception:
        pass


def dump_usb_devices():
    """Печатает ВСЕ USB-устройства, которые видит pyusb (помогает понять, что за режим у телефона)"""
    log("--- USB-устройства ---")
    try:
        for d in usb.core.find(find_all=True):
            _arm_timeout(d)
            try:
                name = usb.util.get_string(d, d.iProduct) if d.iProduct else "?"
            except Exception:
                name = "? (нет доступа к строкам)"
            log(f"  {d.idVendor:04x}:{d.idProduct:04x} bus={d.bus} addr={d.address} {name}")
    except Exception as e:
        log(f"  не удалось перечислить устройства: {e!r}")
    log("----------------------")


def dump_descriptors(dev):
    """Печатает конфигурации, интерфейсы и endpoint устройства"""
    try:
        for cfg in dev:
            log(f"  config {cfg.bConfigurationValue}: интерфейсов={cfg.bNumInterfaces}")
            for intf in cfg:
                try:
                    drv = dev.is_kernel_driver_active(intf.bInterfaceNumber)
                except Exception as e:
                    drv = f"? ({e})"
                log(f"    interface {intf.bInterfaceNumber} alt={intf.bAlternateSetting} "
                    f"class={intf.bInterfaceClass:#04x} sub={intf.bInterfaceSubClass:#04x} "
                    f"proto={intf.bInterfaceProtocol:#04x} kernel_driver={drv}")
                for ep in intf:
                    direction = "IN " if usb.util.endpoint_direction(ep.bEndpointAddress) == usb.util.ENDPOINT_IN else "OUT"
                    kind = {0: "control", 1: "iso", 2: "bulk", 3: "interrupt"}[usb.util.endpoint_type(ep.bmAttributes)]
                    log(f"      ep {ep.bEndpointAddress:#04x} {direction} {kind} maxpacket={ep.wMaxPacketSize}")
    except Exception as e:
        log(f"  не удалось прочитать дескрипторы: {e!r}")


AOA_GET_PROTOCOL = 51
AOA_SEND_STRING = 52
AOA_START = 53

REQ_OUT = usb.util.CTRL_OUT | usb.util.CTRL_TYPE_VENDOR | usb.util.CTRL_RECIPIENT_DEVICE
REQ_IN = usb.util.CTRL_IN | usb.util.CTRL_TYPE_VENDOR | usb.util.CTRL_RECIPIENT_DEVICE

# PID аксессуаров Android
ACCESSORY_PIDS = {0x2D00, 0x2D01, 0x2D02, 0x2D03, 0x2D04, 0x2D05}

# Обычные Android PID
ANDROID_VIDS = [0x18D1, 0x04E8, 0x0BB4, 0x12D1, 0x2717, 0x22B8]


def enabled_devices():
    """Какие устройства ввода включать при старте: --no-touch, --no-mouse, --no-keyboard отключают"""
    return tuple(n for n in ("touch", "mouse", "keyboard") if f"--no-{n}" not in sys.argv[1:])


def audio_enabled():
    """Звук телефона на ПК: выключается флагом --no-audio"""
    return "--no-audio" not in sys.argv[1:]


def find_device():
    """Находит Android-устройство и определяет его режим"""
    dump_usb_devices()
    for dev in usb.core.find(find_all=True):
        if dev.idVendor in ANDROID_VIDS:
            _arm_timeout(dev)
            is_accessory = dev.idProduct in ACCESSORY_PIDS
            log(f"[+] Найдено: VID={hex(dev.idVendor)}, PID={hex(dev.idProduct)}")
            if is_accessory:
                log("    -> Режим аксессуара (handshake не нужен)")
            else:
                log("    -> Обычный режим (нужен handshake)")
            return dev, is_accessory
    return None, False


def send_handshake(dev, strings):
    """Отправляет AOA handshake (строки — из SDK, единый источник) и ПОЛНОСТЬЮ освобождает USB"""
    import subprocess

    # Убиваем MTP и ADB
    try:
        subprocess.run(['killall', '-9', 'gvfsd-mtp'], stderr=subprocess.DEVNULL)
        subprocess.run(['killall', '-9', 'gvfs-mtp-volume-monitor'], stderr=subprocess.DEVNULL)
        subprocess.run(['adb', 'kill-server'], stderr=subprocess.DEVNULL)
    except Exception:
        pass

    cfg = dev.get_active_configuration()
    intf = cfg[(0, 0)]
    log("Дескрипторы до handshake:")
    dump_descriptors(dev)

    # Отключаем драйверы ядра
    for i in range(cfg.bNumInterfaces):
        try:
            if dev.is_kernel_driver_active(i):
                dev.detach_kernel_driver(i)
        except Exception:
            pass

    try:
        usb.util.claim_interface(dev, intf.bInterfaceNumber)
    except usb.core.USBError as e:
        log(f"[!] Не удалось захватить интерфейс: {e}")
        raise

    protocol = dev.ctrl_transfer(REQ_IN, AOA_GET_PROTOCOL, 0, 0, 2, timeout=2000)
    log(f"AOA protocol version: {int.from_bytes(bytes(protocol), 'little')}")
    if int.from_bytes(bytes(protocol), "little") < 2:
        raise RuntimeError("Нужен AOA 2 (HID): обновите телефон или проверьте кабель")

    for index, payload in enumerate(strings):
        log(f"AOA string[{index}] = {bytes(payload)!r}")
        dev.ctrl_transfer(REQ_OUT, AOA_SEND_STRING, 0, index, bytes(payload), timeout=2000)

    dev.ctrl_transfer(REQ_OUT, AOA_START, 0, 0, [], timeout=2000)

    # ВАЖНО: Полностью освобождаем устройство, чтобы Android мог открыть accessory
    usb.util.release_interface(dev, intf.bInterfaceNumber)
    usb.util.dispose_resources(dev)
    # reset() не нужен: телефон сам переподключится в режиме аксессуара

    HANDSHAKE["at"] = time.time()
    log("[+] Handshake отправлен, USB освобожден")


def open_endpoints(dev):
    """Открывает Bulk-эндпоинты для чтения/записи"""
    log(f"Устройство {dev.idVendor:04x}:{dev.idProduct:04x}, дескрипторы в режиме аксессуара:")
    dump_descriptors(dev)
    cfg = dev.get_active_configuration()
    for intf in cfg:
        eps_in = [e for e in intf
                  if usb.util.endpoint_direction(e.bEndpointAddress) == usb.util.ENDPOINT_IN
                  and usb.util.endpoint_type(e.bmAttributes) == usb.util.ENDPOINT_TYPE_BULK]
        eps_out = [e for e in intf
                   if usb.util.endpoint_direction(e.bEndpointAddress) == usb.util.ENDPOINT_OUT
                   and usb.util.endpoint_type(e.bmAttributes) == usb.util.ENDPOINT_TYPE_BULK]

        if eps_in and eps_out:
            if dev.is_kernel_driver_active(intf.bInterfaceNumber):
                dev.detach_kernel_driver(intf.bInterfaceNumber)
            usb.util.claim_interface(dev, intf.bInterfaceNumber)
            log(f"[+] Выбран interface {intf.bInterfaceNumber}")
            log(f"[+] Endpoints: IN={hex(eps_in[0].bEndpointAddress)}, OUT={hex(eps_out[0].bEndpointAddress)}")
            return eps_in[0], eps_out[0], intf.bInterfaceNumber

    raise RuntimeError("Bulk endpoints не найдены")


# Если телефон уже в режиме аксессуара, система не запустит приложение (нет события подключения).
# Тогда после тишины сбрасываем USB, чтобы телефон переподключился и приложение стартовало само.
NO_DATA_RESET_AFTER = 6.0
RESET_MIN_INTERVAL = 60.0
RESET_MAX_TRIES = 3
KICK = {"last": 0.0, "tries": 0}

STATS = {"bytes": 0, "timeouts": 0, "chunks": 0, "frames": 0, "last_report": 0.0}

HANDSHAKE = {"at": 0.0}

# Детектор зависшего видео: PONGи идут, кадров нет — просим свежий кадр.
FREEZE_AFTER = 3.0
REQUEST_FRAME_EVERY = 2.0


def exec_control(dev, c):
    """Выполняет ControlRequest ядра как vendor control transfer."""
    from sdk import DIR_OUT
    out = c["direction"] == DIR_OUT
    bm = (usb.util.CTRL_OUT if out else usb.util.CTRL_IN) | usb.util.CTRL_TYPE_VENDOR | usb.util.CTRL_RECIPIENT_DEVICE
    if out:
        dev.ctrl_transfer(bm, c["request"], c["value"], c["index"], c["data"], timeout=300)
    else:
        dev.ctrl_transfer(bm, c["request"], c["value"], c["index"], c["in_len"], timeout=300)


def return_to_normal_mode():
    """Выводит телефон из режима аксессуара сбросом USB.

    Иначе при следующем запуске система не пришлёт событие подключения и приложение не запустится само.
    Телефон ищем заново: старый дескриптор после переподключения уже недействителен.
    """
    # Если handshake отправлен только что, телефон ещё переходит в режим аксессуара: дождёмся его
    wait_until = HANDSHAKE["at"] + 8.0
    try:
        while True:
            for d in usb.core.find(find_all=True):
                if d.idVendor in ANDROID_VIDS and d.idProduct in ACCESSORY_PIDS:
                    log("[*] Выход: сбрасываю USB, телефон вернётся в обычный режим")
                    _arm_timeout(d)
                    safe_reset(d)
                    return
            if time.time() >= wait_until:
                return
            time.sleep(0.5)
    except usb.core.USBError as e:
        log(f"[!] Не удалось сбросить USB при выходе: {e!r}")


def exit_on_sigterm(signum, frame):
    """SIGTERM (kill, systemd stop) обрабатываем как Ctrl+C, чтобы выйти с уборкой за собой"""
    raise KeyboardInterrupt


def run():
    while True:  # Бесконечный цикл переподключения
        dev = None
        interface = None
        ui = None
        audio = None
        session = None
        try:
            log("[*] Поиск устройства...")
            dev, is_accessory = find_device()

            if not dev:
                log("[-] Устройство не найдено, ждем 5 секунд...")
                time.sleep(5)
                continue

            # Сессия ядра: единый владелец протокола. Строки AOA — из ядра.
            session = SdkSession(name="pcdock-viewer/1.0", video=True, audio=audio_enabled())
            strings = session.aoa_strings()

            if not is_accessory:
                log("[*] Отправка AOA handshake...")
                send_handshake(dev, strings)

                log("[*] Ожидание переподключения...")
                for attempt in range(30):
                    time.sleep(1)
                    dev, is_accessory = find_device()

                    if dev and is_accessory:
                        log(f"[+] Устройство переподключилось (попытка {attempt + 1})")
                        break

                    if attempt % 5 == 0:
                        log(f"    Жду... (попытка {attempt + 1}/30)")
                else:
                    log("[-] Не переподключилось, начинаем заново...")
                    continue

            log("[*] Открытие endpoints...")
            ep_in, ep_out, interface = open_endpoints(dev)

            log("[*] Старт сессии SDK...")
            audio = AudioPlayer(log=log)
            want_audio = audio_enabled()
            ui = PhoneWindow(session, log=log, enabled=enabled_devices(),
                             audio=audio, audio_on=want_audio,
                             on_audio=lambda on: session.set_stream(True, on))
            # start() регистрирует всё по умолчанию (перетирая призраков прошлых сессий),
            # затем гасим ненужное: так на телефоне не остаётся чужих HID.
            session.start(now_ms())
            for name, kind in (("touch", DEV_TOUCH), ("mouse", DEV_MOUSE), ("keyboard", DEV_KEYBOARD)):
                session.device_enable(kind, bool(ui.active.get(name)))

            frame_count = 0
            connected_at = time.time()
            bytes_at_connect = STATS["bytes"]
            last_video_rx = 0.0
            last_request_frame = 0.0
            freeze_warned = False
            audio_config_rx = 0.0
            audio_warned = False
            last_audio_on = ui.audio_on
            seen_codecs = set()

            while True:
                # 1. Байты из транспорта -> ядро
                try:
                    data = bytes(ep_in.read(65536, timeout=15))
                except usb.core.USBError as e:
                    if _is_gone(e):
                        log("[*] Телефон отключён (устройство пропало) — переподключаюсь")
                        raise _Reconnect()
                    if "timed" not in str(e).lower() and e.errno != 110:
                        log(f"[ERROR] USB: {e}")
                        raise
                    data = b""
                    STATS["timeouts"] += 1
                if data:
                    if STATS["bytes"] == 0:
                        log(f"[DBG] Первые данные: {len(data)} байт, начало: {data[:12].hex()}")
                    STATS["bytes"] += len(data)
                    STATS["chunks"] += 1
                    session.handle_input(data)

                # 2. По таймеру (PING, повтор HELLO, PeerSilent/Timeout)
                now = now_ms()
                nxt = session.next_timeout()
                if nxt is not None and now >= nxt:
                    session.handle_timeout(now)

                # 3. Что нужно отправить
                while True:
                    tx = session.poll_transmit()
                    if tx is None:
                        break
                    t, payload = tx
                    try:
                        if t == "stream":
                            ep_out.write(payload, timeout=1000)
                        else:
                            exec_control(dev, payload)
                    except usb.core.USBError as e:
                        if _is_gone(e):
                            log("[*] Телефон отключён при записи — переподключаюсь")
                            raise _Reconnect()
                        log(f"[!] Ошибка отправки: {e!r}")
                        break

                # 4. События для фронтенда
                frame = None
                while True:
                    ev = session.poll_event()
                    if ev is None:
                        break
                    kind, d = ev
                    if kind == "connected":
                        log(f"[sdk] Connected: версия {d['version']}, caps={d['caps']:#x}, {d['name']!r}")
                    elif kind == "display":
                        sw, sh = d["stream"]
                        dw, dh = d["screen"]
                        log(f"[sdk] Display: поток {sw}x{sh}, экран {dw}x{dh}, rot={d['rotation']}")
                    elif kind == "video":
                        if d["codec"] != 1:
                            if d["codec"] not in seen_codecs:
                                seen_codecs.add(d["codec"])
                                log(f"[!] Видеокодек {d['codec']} не поддерживается (нужен JPEG)")
                            continue
                        img = cv2.imdecode(np.frombuffer(d["data"], np.uint8), cv2.IMREAD_COLOR)
                        if img is None:
                            log(f"[!] imdecode не смог ({len(d['data'])} байт)")
                            continue
                        frame = img
                        STATS["frames"] += 1
                        last_video_rx = time.time()
                        freeze_warned = False
                    elif kind == "audio_config":
                        log(f"[sdk] Audio: codec={d['codec']} {d['rate']} Гц x{d['channels']}")
                        audio_config_rx = time.time()
                        if d["codec"] == 1:
                            audio.set_config(d["rate"], d["channels"])
                        else:
                            log(f"[!] Аудиокодек {d['codec']} не поддерживается (нужен PCM)")
                    elif kind == "audio":
                        audio.feed(d["data"])
                    elif kind == "status":
                        log(f"[sdk] STATUS {d['code']}: {d['detail']!r}")
                    elif kind == "malformed":
                        log(f"[!] Битый пакет пропущен: {d['detail']!r}")
                    elif kind == "peer_silent":
                        log("[!] Телефон молчит после HELLO: приложение-приёмник не запущено. Сбрасываю USB")
                        safe_reset(dev)
                        raise _Reconnect()
                    elif kind == "disconnected":
                        r = d["reason"]
                        names = {DC_TIMEOUT: "таймаут", DC_VERSION: "версия", DC_PEER_ERROR: "ошибка телефона", DC_CLOSED: "закрыто"}
                        log(f"[!] Сессия закрыта: {names.get(r, r)} {d['detail']!r} — переподключаюсь")
                        raise _Reconnect()
                    # unknown — пропускаем

                now_s = time.time()
                if now_s - STATS["last_report"] >= 2:
                    STATS["last_report"] = now_s
                    video_age = f", видео_назад={now_s - last_video_rx:.0f}с" if last_video_rx > 0 else ""
                    log(f"[stat] получено {STATS['bytes']} байт ({STATS['chunks']} чанков), "
                        f"кадров {STATS['frames']}, таймаутов {STATS['timeouts']}{video_age}")

                if frame is not None:
                    frame_count += 1
                    if frame_count % 30 == 0:
                        log(f"[OK] Кадр {frame_count}: {frame.shape[1]}x{frame.shape[0]}")
                    ui.update_frame(frame)

                # Звук включён, но телефон не прислал AUDIO_CONFIG — самодиагностика:
                # либо APK старый (без захвата звука), либо захват не завёлся.
                if ui.audio_on != last_audio_on:
                    last_audio_on = ui.audio_on
                    audio_config_rx = 0.0
                    audio_warned = False
                if ui.audio_on and audio_config_rx == 0.0 and not audio_warned \
                        and now_s - connected_at >= 6.0:
                    audio_warned = True
                    log("[!] Звук включён, но телефон не прислал AUDIO_CONFIG за 6 с. "
                        "Либо APK старый — обнови: ./dev.sh install + подтверди запись экрана. "
                        "Либо захват не завёлся — смотри STATUS в логе и hb-лог телефона.")

                # Детектор зависшего видео: сессия жива, кадров нет — просим свежий кадр.
                if (session.state() == STATE_ACTIVE and last_video_rx > 0
                        and now_s - last_video_rx >= FREEZE_AFTER):
                    if now_s - last_request_frame >= REQUEST_FRAME_EVERY:
                        last_request_frame = now_s
                        try:
                            session.request_frame()
                        except SdkError as e:
                            log(f"[!] request_frame: {e}")
                    if not freeze_warned:
                        freeze_warned = True
                        log(f"[!] Нет видеокадров {now_s - last_video_rx:.0f} с — "
                            f"запросил кадр. Если висит дольше — смотри hb-лог телефона.")

                if STATS["bytes"] > bytes_at_connect:
                    KICK["tries"] = 0
                elif (time.time() - connected_at > NO_DATA_RESET_AFTER
                      and time.time() - KICK["last"] > RESET_MIN_INTERVAL
                      and KICK["tries"] < RESET_MAX_TRIES):
                    KICK["last"] = time.time()
                    KICK["tries"] += 1
                    log(f"[!] От телефона нет данных {NO_DATA_RESET_AFTER:.0f} с. Скорее всего, приложение не запущено "
                        f"(телефон уже был в режиме аксессуара). Сбрасываю USB, попытка {KICK['tries']}/{RESET_MAX_TRIES}")
                    safe_reset(dev)
                    raise _Reconnect()

                if not ui.poll():
                    return
                ui.draw()

        except _Reconnect:
            log("[*] Переподключение...")
            time.sleep(1)
            continue
        except usb.core.USBError as e:
            log(f"[!] USB ошибка: {e!r} errno={getattr(e, 'errno', None)}")
            log(traceback.format_exc())
            log("[*] Переподключение через 3 секунды...")
            time.sleep(3)
            continue
        except SdkError as e:
            log(f"[!] Ошибка SDK: {e}")
            log(traceback.format_exc())
            log("[*] Переподключение через 3 секунды...")
            time.sleep(3)
            continue
        except Exception as e:
            log(f"[!] Критическая ошибка: {e!r}")
            log(traceback.format_exc())
            log("[*] Перезапуск через 5 секунд...")
            time.sleep(5)
            continue
        finally:
            if session is not None:
                try:
                    session.close()
                except Exception:
                    pass
                session = None
            if audio is not None:
                try:
                    audio.close()
                except Exception:
                    pass
                audio = None
            if ui is not None:
                ui.close()
                ui = None
            if dev is not None and interface is not None:
                try:
                    usb.util.release_interface(dev, interface)
                    usb.util.dispose_resources(dev)
                except usb.core.USBError:
                    pass


def main():
    for flag in ("--legacy", "--v1"):
        if flag in sys.argv[1:]:
            log(f"[!] Флаг {flag} больше не нужен: viewer всегда говорит на v2 через SDK")
    signal.signal(signal.SIGTERM, exit_on_sigterm)
    try:
        run()
    except KeyboardInterrupt:
        log("[*] Остановлено")
    finally:
        return_to_normal_mode()


if __name__ == "__main__":
    main()
