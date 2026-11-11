"""Клиент протокола демонов PC Dock на чистом Питоне (без зависимостей).

Протокол: sdk/docs/protocol.md. Пакет в потоковых трубах: u32 длина (little-endian, считая байт типа и тело),
байт типа, тело. Тип 0 — JSON, 1 — видеокадр, 2 — звук, 3 — данные файла, 4 — вложение прогона.
"""
import json
import socket
import struct
import threading

PROTOCOL = 1
KIND_JSON, KIND_VIDEO, KIND_AUDIO, KIND_FILE = 0, 1, 2, 3


class RemoteError(Exception):
    pass


def _open(addr):
    """tcp://host:port | host:port | local:имя (локальная труба)."""
    if addr.startswith("local:"):
        name = addr[6:]
        if hasattr(socket, "AF_UNIX"):
            s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
            try:
                s.connect("\0" + name)           # Linux: абстрактное пространство имён
            except OSError:
                s.close()
                s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
                s.connect("/tmp/" + name)        # macOS: файл в /tmp
            return s.makefile("rwb", buffering=0), s
        f = open(r"\\.\pipe" + "\\" + name, "r+b", buffering=0)   # Windows: именованный канал
        return f, f
    host, _, port = addr.removeprefix("tcp://").rpartition(":")
    s = socket.create_connection((host, int(port)))
    s.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
    return s.makefile("rwb", buffering=0), s


class Daemon:
    def __init__(self, addr, token=None):
        self._io, self._sock = _open(addr)
        self._wlock = threading.Lock()
        self._next = 1
        self._pending = {}
        self.events = []                 # события без id (State, Appeared, RunEvent...), новые в конце
        self.frames = {}                 # телефон -> (номер кадра, JPEG): последний кадр
        self.on_event = None
        hello = self._read()
        if hello[0] != KIND_JSON or json.loads(hello[1]).get("ev") != "Hello":
            raise RemoteError("сервер не прислал Hello")
        self.hello = json.loads(hello[1])
        if self.hello["protocol"] != PROTOCOL:
            raise RemoteError(f"версия протокола {self.hello['protocol']}, клиент знает {PROTOCOL}")
        self.closed = False
        threading.Thread(target=self._reader, daemon=True).start()
        if token:
            self.request("Auth", token=token)

    def _read(self):
        head = self._io.read(4)
        if len(head) < 4:
            raise ConnectionError("соединение закрыто")
        (n,) = struct.unpack("<I", head)
        body = self._io.read(n)
        return body[0], body[1:]

    def _reader(self):
        try:
            while True:
                kind, body = self._read()
                if kind == KIND_VIDEO and len(body) >= 22:
                    phone, seq = struct.unpack_from("<IQ", body)
                    self.frames[phone] = (seq, body[22:])
                elif kind == KIND_JSON:
                    v = json.loads(body)
                    slot = self._pending.pop(v.get("id"), None) if "id" in v else None
                    if slot:
                        slot[1].append(v)
                        slot[0].set()
                    else:
                        self.events.append(v)
                        if self.on_event:
                            self.on_event(v)
        except (ConnectionError, OSError, ValueError):
            pass
        self.closed = True
        for ev, _ in list(self._pending.values()):
            ev.set()

    def send(self, op, **fields):
        """Команда без ответа."""
        self._write(KIND_JSON, json.dumps({"op": op, **fields}).encode())

    def send_binary(self, kind, payload):
        self._write(kind, payload)

    def _write(self, kind, payload):
        with self._wlock:
            self._io.write(struct.pack("<IB", 1 + len(payload), kind) + payload)

    def request(self, op, timeout=10, **fields):
        """Команда с ответом: назначает id и ждёт ответ с тем же id."""
        rid, self._next = self._next, self._next + 1
        slot = (threading.Event(), [])
        self._pending[rid] = slot
        self._write(KIND_JSON, json.dumps({"op": op, "id": rid, **fields}).encode())
        if not slot[0].wait(timeout) or not slot[1]:
            self._pending.pop(rid, None)
            raise RemoteError("нет ответа" if not self.closed else "соединение закрыто")
        reply = slot[1][0]
        if reply.get("ev") == "Error":
            raise RemoteError(reply.get("detail", ""))
        return reply

    def close(self):
        try:
            self._sock.close()
        except OSError:
            pass
