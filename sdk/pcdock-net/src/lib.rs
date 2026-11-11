//! Общая сетевая основа демонов PC Dock (`pcdockd`, `pcdock-autod`): носители (TCP, WebSocket, локальная
//! труба), исходящий ящик клиента, приветствие, пароль и разбор запросов.
//!
//! Конверт пакета в потоковых трубах (TCP, локальная труба): `u32` длина (little-endian, считая байт типа и
//! тело), байт типа, тело. Тип `0` — JSON; остальные типы определяет демон (`1` видео, `2` звук, `3` данные
//! файла и т. д.). В WebSocket текстовый фрейм — JSON, бинарный — `[тип][нагрузка]`.
//!
//! Каждый запрос — JSON-объект с полем `op` и необязательным `id`; ответ на него несёт тот же `id`.
//! Пароль (`{"op":"Auth","token":"…"}`) и приветствие обрабатывает сама основа.

mod hub;
mod outbox;

pub use hub::{Client, Handler, Hub};
pub use outbox::Outbox;

/// Тип пакета: JSON.
pub const KIND_JSON: u8 = 0;

/// Собирает конверт потоковой трубы.
pub fn envelope(kind: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + 1 + payload.len());
    out.extend_from_slice(&((1 + payload.len()) as u32).to_le_bytes());
    out.push(kind);
    out.extend_from_slice(payload);
    out
}
