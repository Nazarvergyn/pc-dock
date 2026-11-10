//! Протокол уровня приложения: один набор сообщений на всех носителях.
//!
//! Команды и события — JSON, кадры и звук — бинарные пакеты с шапкой.
//! Различаются по конверту, а не по содержимому: в веб-сокетах текстовый
//! фрейм — JSON, бинарный — кадр; в остальных трубах каждый пакет идёт
//! с заголовком (длина, тип, тело): 0 — JSON, 1 — видео, 2 — звук.

use serde::{Deserialize, Serialize};

/// Команда клиента. `phone` — трубка-получатель.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "op")]
pub enum ClientMsg {
    /// Пароль. Если демон запущен с токеном, это должно быть первое сообщение клиента.
    Auth {
        token: String,
    },
    /// Список трубок.
    List,
    /// Подписаться на кадры трубки / отписаться.
    Watch {
        phone: u32,
    },
    Unwatch {
        phone: u32,
    },
    /// Касание с пятном/силой. `None` = default ядра.
    TouchEx {
        phone: u32,
        cid: u8,
        phase: u32,
        x: f32,
        y: f32,
        width: Option<f32>,
        height: Option<f32>,
        pressure: Option<f32>,
    },
    CursorMoveAbs {
        phone: u32,
        x: f32,
        y: f32,
    },
    CursorMoveRel {
        phone: u32,
        dx: i32,
        dy: i32,
    },
    CursorResync {
        phone: u32,
    },
    MouseButton {
        phone: u32,
        button: u32,
        down: bool,
    },
    Scroll {
        phone: u32,
        ticks: f32,
    },
    ScrollH {
        phone: u32,
        ticks: f32,
    },
    Key {
        phone: u32,
        usage: u16,
        down: bool,
    },
    DeviceEnable {
        phone: u32,
        kind: u32,
        on: bool,
    },
    ReleaseAll {
        phone: u32,
    },
    RequestFrame {
        phone: u32,
    },
    Wake {
        phone: u32,
    },
    SetStream {
        phone: u32,
        video: bool,
        audio: bool,
    },
}

/// Событие сервера в JSON. Видео и звук идут бинарными пакетами, не здесь.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "ev")]
pub enum ServerMsg {
    /// Пароль принят.
    Authed,
    Phones {
        phones: Vec<u32>,
    },
    Watched {
        phone: u32,
        on: bool,
    },
    Connected {
        phone: u32,
        version: u16,
        caps: u32,
        name: String,
    },
    Display {
        phone: u32,
        stream_w: u16,
        stream_h: u16,
        screen_w: u16,
        screen_h: u16,
        dpi: u16,
        rotation: u8,
    },
    AudioConfig {
        phone: u32,
        codec: u8,
        rate: u32,
        channels: u8,
    },
    Status {
        phone: u32,
        code: u16,
        detail: String,
    },
    PeerSilent {
        phone: u32,
    },
    Malformed {
        phone: u32,
        detail: String,
    },
    Disconnected {
        phone: u32,
        reason: String,
    },
    LinkDown {
        phone: u32,
        detail: String,
    },
    Warning {
        phone: u32,
        detail: String,
    },
    /// Обнаружен телефон: номер, путь (`usb`/`adb`/`serial`), адрес и серийник, если известен.
    Appeared {
        phone: u32,
        transport: String,
        address: String,
        serial: Option<String>,
    },
    /// Состояние: `appeared`, `connecting`, `ready`, `needs_permission`, `needs_app`, `needs_access`,
    /// `needs_auth`, `lost`; `detail` — подсказка человеку (может быть пустой).
    State {
        phone: u32,
        state: String,
        detail: String,
    },
    Lost {
        phone: u32,
    },
    Error {
        detail: String,
    },
}

/// Типы пакетов в потоковых трубах (TCP, unix-сокет, пайп).
pub const PKG_JSON: u8 = 0;
pub const PKG_VIDEO: u8 = 1;
pub const PKG_AUDIO: u8 = 2;

/// Шапка видеопакета: phone u32 LE, seq u64 LE, codec u8, key u8, pts u64 LE.
pub const VIDEO_HEADER_LEN: usize = 4 + 8 + 1 + 1 + 8;

/// Шапка аудиопакета: phone u32 LE, pts u64 LE.
pub const AUDIO_HEADER_LEN: usize = 4 + 8;

/// Собирает видеопакет: шапка + JPEG.
pub fn encode_video(phone: u32, seq: u64, codec: u8, key: bool, pts: u64, jpeg: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(VIDEO_HEADER_LEN + jpeg.len());
    out.extend_from_slice(&phone.to_le_bytes());
    out.extend_from_slice(&seq.to_le_bytes());
    out.push(codec);
    out.push(u8::from(key));
    out.extend_from_slice(&pts.to_le_bytes());
    out.extend_from_slice(jpeg);
    out
}

/// Разобранный видеопакет: (phone, seq, codec, key, pts, jpeg).
pub type DecodedVideo<'a> = (u32, u64, u8, bool, u64, &'a [u8]);

/// Разбирает видеопакет. Возвращает (phone, seq, codec, key, pts, jpeg).
pub fn decode_video(buf: &[u8]) -> Option<DecodedVideo<'_>> {
    if buf.len() < VIDEO_HEADER_LEN {
        return None;
    }
    let phone = u32::from_le_bytes(buf[0..4].try_into().ok()?);
    let seq = u64::from_le_bytes(buf[4..12].try_into().ok()?);
    let codec = buf[12];
    let key = buf[13] != 0;
    let pts = u64::from_le_bytes(buf[14..22].try_into().ok()?);
    Some((phone, seq, codec, key, pts, &buf[VIDEO_HEADER_LEN..]))
}

/// Собирает аудиопакет: шапка + PCM.
pub fn encode_audio(phone: u32, pts: u64, pcm: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(AUDIO_HEADER_LEN + pcm.len());
    out.extend_from_slice(&phone.to_le_bytes());
    out.extend_from_slice(&pts.to_le_bytes());
    out.extend_from_slice(pcm);
    out
}

/// Разбирает аудиопакет. Возвращает (phone, pts, pcm).
pub fn decode_audio(buf: &[u8]) -> Option<(u32, u64, &[u8])> {
    if buf.len() < AUDIO_HEADER_LEN {
        return None;
    }
    let phone = u32::from_le_bytes(buf[0..4].try_into().ok()?);
    let pts = u64::from_le_bytes(buf[4..12].try_into().ok()?);
    Some((phone, pts, &buf[AUDIO_HEADER_LEN..]))
}

/// Конверт потоковой трубы: длина тела (тип + нагрузка) u32 LE, затем тип и тело.
pub fn encode_envelope(kind: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + 1 + payload.len());
    out.extend_from_slice(&((1 + payload.len()) as u32).to_le_bytes());
    out.push(kind);
    out.extend_from_slice(payload);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_commands_roundtrip() {
        let msgs = vec![
            ClientMsg::List,
            ClientMsg::Watch { phone: 7 },
            ClientMsg::TouchEx {
                phone: 7,
                cid: 0,
                phase: 0,
                x: 0.5,
                y: 0.5,
                width: None,
                height: Some(0.05),
                pressure: Some(0.9),
            },
            ClientMsg::Scroll { phone: 7, ticks: 2.5 },
            ClientMsg::Key { phone: 7, usage: 0xE1, down: true },
        ];
        for m in msgs {
            let s = serde_json::to_string(&m).unwrap();
            let back: ClientMsg = serde_json::from_str(&s).unwrap();
            assert_eq!(m, back);
        }
    }

    #[test]
    fn server_events_roundtrip() {
        let m = ServerMsg::Display {
            phone: 3,
            stream_w: 360,
            stream_h: 820,
            screen_w: 720,
            screen_h: 1640,
            dpi: 320,
            rotation: 1,
        };
        let s = serde_json::to_string(&m).unwrap();
        assert!(s.contains("\"ev\":\"Display\""));
        let back: ServerMsg = serde_json::from_str(&s).unwrap();
        assert_eq!(m, back);
    }

    #[test]
    fn binary_packets_roundtrip() {
        let v = encode_video(7, 42, 1, true, 1000, &[9, 8, 7]);
        let (phone, seq, codec, key, pts, jpeg) = decode_video(&v).unwrap();
        assert_eq!((phone, seq, codec, key, pts, jpeg), (7, 42, 1, true, 1000, &[9, 8, 7][..]));
        assert!(decode_video(&v[..10]).is_none());
        let a = encode_audio(7, 2000, &[1, 2]);
        let (phone, pts, pcm) = decode_audio(&a).unwrap();
        assert_eq!((phone, pts, pcm), (7, 2000, &[1, 2][..]));
        let e = encode_envelope(PKG_VIDEO, &v);
        assert_eq!(e.len(), 4 + 1 + v.len());
        assert_eq!(u32::from_le_bytes(e[0..4].try_into().unwrap()) as usize, 1 + v.len());
        assert_eq!(e[4], PKG_VIDEO);
    }
}
