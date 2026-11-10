//! C ABI живой фермы (`pcdock_farm_*`): ферма сама находит телефоны, подключает и переподключает их.
//! Вызывающему нужны три вещи: создать ферму из JSON-настройки, раз за разом брать события и отдавать команды.
//!
//! Правила те же, что у остальных: коды `0/1/отрицательные`, буферы выделяет вызывающий, дескриптор
//! однопоточный (потоки ферма запускает внутри себя). События: `pcdock_farm_poll` даёт вид и номер телефона,
//! подробности — `pcdock_farm_event_json`, двоичные данные кадров и звука — `pcdock_farm_event_data`.
//! Низкоуровневый реестр без потоков и транспорта — в `pcdock_registry_*`.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::panic::{catch_unwind, AssertUnwindSafe};

use pcdock_core::link::{InputCmd, PhoneState};
use pcdock_farm::{Farm, FarmConfig, FarmEvent};
use serde_json::json;

use super::registry::{device_kind, mouse_button, touch_phase};
use super::{check_slice, copy_out, guard};
use super::{PCDOCK_ERR_ARG, PCDOCK_ERR_NULL, PCDOCK_ITEM, PCDOCK_OK};

/// Виды событий `pcdock_farm_poll`.
pub const PCDOCK_FARM_EV_APPEARED: u32 = 1;
pub const PCDOCK_FARM_EV_STATE: u32 = 2;
pub const PCDOCK_FARM_EV_LOST: u32 = 3;
pub const PCDOCK_FARM_EV_CONNECTED: u32 = 4;
pub const PCDOCK_FARM_EV_DISPLAY: u32 = 5;
pub const PCDOCK_FARM_EV_VIDEO: u32 = 6;
pub const PCDOCK_FARM_EV_AUDIO_CONFIG: u32 = 7;
pub const PCDOCK_FARM_EV_AUDIO: u32 = 8;
pub const PCDOCK_FARM_EV_STATUS: u32 = 9;
pub const PCDOCK_FARM_EV_PEER_SILENT: u32 = 10;
pub const PCDOCK_FARM_EV_MALFORMED: u32 = 11;
pub const PCDOCK_FARM_EV_DISCONNECTED: u32 = 12;
pub const PCDOCK_FARM_EV_LINK_DOWN: u32 = 13;
pub const PCDOCK_FARM_EV_WARNING: u32 = 14;

/// Состояния телефона (`pcdock_farm_state`).
pub const PCDOCK_PHONE_APPEARED: i32 = 0;
pub const PCDOCK_PHONE_CONNECTING: i32 = 1;
pub const PCDOCK_PHONE_READY: i32 = 2;
pub const PCDOCK_PHONE_NEEDS_PERMISSION: i32 = 3;
pub const PCDOCK_PHONE_LOST: i32 = 4;

/// Непрозрачный дескриптор живой фермы.
pub struct PcdockFarm {
    /// Ферма однопоточна для вызывающего; паника внутри вызова превращается в код ошибки.
    farm: AssertUnwindSafe<Farm>,
    queue: VecDeque<FarmEvent>,
    current: Option<FarmEvent>,
    warnings: String,
}

thread_local! {
    static LAST_ERROR: RefCell<String> = const { RefCell::new(String::new()) };
}

fn set_error(text: String) {
    LAST_ERROR.with(|e| *e.borrow_mut() = text);
}

fn check(p: *mut PcdockFarm) -> Result<&'static mut PcdockFarm, i32> {
    if p.is_null() {
        return Err(PCDOCK_ERR_NULL);
    }
    // SAFETY: указатель выдан `pcdock_farm_new` и жив до `pcdock_farm_free`.
    Ok(unsafe { &mut *p })
}

fn err(e: String) -> i32 {
    set_error(e);
    PCDOCK_ERR_ARG
}

/// Создаёт ферму из JSON-настройки (`{}` — всё по умолчанию: автообнаружение USB и ADB).
/// При ошибке возвращает NULL, текст — в `pcdock_farm_last_error`. Освободить через `pcdock_farm_free`.
#[no_mangle]
pub extern "C" fn pcdock_farm_new(config_json: *const u8, len: usize) -> *mut PcdockFarm {
    let made = catch_unwind(|| -> Result<*mut PcdockFarm, String> {
        let bytes = check_slice(config_json, len).map_err(|_| "нулевой указатель конфига".to_string())?;
        let text = std::str::from_utf8(bytes).map_err(|_| "конфиг не UTF-8".to_string())?;
        let text = if text.trim().is_empty() { "{}" } else { text };
        let cfg = FarmConfig::from_json(text)?;
        let (farm, warnings) = Farm::from_config(cfg)?;
        Ok(Box::into_raw(Box::new(PcdockFarm {
            farm: AssertUnwindSafe(farm),
            queue: VecDeque::new(),
            current: None,
            warnings: warnings.join("\n"),
        })))
    });
    match made {
        Ok(Ok(p)) => p,
        Ok(Err(e)) => {
            set_error(e);
            std::ptr::null_mut()
        }
        Err(_) => {
            set_error("паника при создании фермы".into());
            std::ptr::null_mut()
        }
    }
}

/// Текст последней ошибки `pcdock_farm_new` или `-2` с этого же потока (UTF-8, без нуля в конце).
#[no_mangle]
pub extern "C" fn pcdock_farm_last_error(out_buf: *mut u8, inout_len: *mut usize) -> i32 {
    guard(|| Ok(LAST_ERROR.with(|e| copy_out(e.borrow().as_bytes(), out_buf, inout_len))))
}

/// Предупреждения запуска (например, USB-обнаружение не встало), строки через `\n`.
#[no_mangle]
pub extern "C" fn pcdock_farm_warnings(p: *mut PcdockFarm, out_buf: *mut u8, inout_len: *mut usize) -> i32 {
    guard(|| {
        let h = check(p)?;
        Ok(copy_out(h.warnings.as_bytes(), out_buf, inout_len))
    })
}

/// Останавливает потоки телефонов, возвращает телефоны в обычный режим и освобождает ферму.
#[no_mangle]
pub extern "C" fn pcdock_farm_free(p: *mut PcdockFarm) {
    let _ = catch_unwind(|| {
        if !p.is_null() {
            // SAFETY: указатель выдан `pcdock_farm_new` и освобождается один раз здесь.
            unsafe { drop(Box::from_raw(p)) };
        }
    });
}

// ----- события -----

fn kind_of(ev: &FarmEvent) -> (u32, u32) {
    match ev {
        FarmEvent::Appeared { phone, .. } => (PCDOCK_FARM_EV_APPEARED, *phone),
        FarmEvent::State { phone, .. } => (PCDOCK_FARM_EV_STATE, *phone),
        FarmEvent::Lost { phone } => (PCDOCK_FARM_EV_LOST, *phone),
        FarmEvent::Connected { phone, .. } => (PCDOCK_FARM_EV_CONNECTED, *phone),
        FarmEvent::Display { phone, .. } => (PCDOCK_FARM_EV_DISPLAY, *phone),
        FarmEvent::Video { phone, .. } => (PCDOCK_FARM_EV_VIDEO, *phone),
        FarmEvent::AudioConfig { phone, .. } => (PCDOCK_FARM_EV_AUDIO_CONFIG, *phone),
        FarmEvent::Audio { phone, .. } => (PCDOCK_FARM_EV_AUDIO, *phone),
        FarmEvent::Status { phone, .. } => (PCDOCK_FARM_EV_STATUS, *phone),
        FarmEvent::PeerSilent { phone } => (PCDOCK_FARM_EV_PEER_SILENT, *phone),
        FarmEvent::Malformed { phone, .. } => (PCDOCK_FARM_EV_MALFORMED, *phone),
        FarmEvent::Disconnected { phone, .. } => (PCDOCK_FARM_EV_DISCONNECTED, *phone),
        FarmEvent::LinkDown { phone, .. } => (PCDOCK_FARM_EV_LINK_DOWN, *phone),
        FarmEvent::Warning { phone, .. } => (PCDOCK_FARM_EV_WARNING, *phone),
    }
}

fn state_name(s: PhoneState) -> &'static str {
    match s {
        PhoneState::Appeared => "appeared",
        PhoneState::Connecting => "connecting",
        PhoneState::Ready => "ready",
        PhoneState::NeedsPermission => "needs_permission",
        PhoneState::Lost => "lost",
    }
}

fn event_json(ev: &FarmEvent) -> serde_json::Value {
    match ev {
        FarmEvent::Appeared { phone, transport, address, serial } => {
            json!({"phone": phone, "transport": transport.to_string(), "address": address, "serial": serial})
        }
        FarmEvent::State { phone, state } => json!({"phone": phone, "state": state_name(*state)}),
        FarmEvent::Lost { phone } => json!({"phone": phone}),
        FarmEvent::Connected { phone, version, capabilities, name } => {
            json!({"phone": phone, "version": version, "capabilities": capabilities, "name": name})
        }
        FarmEvent::Display { phone, stream_w, stream_h, screen_w, screen_h, dpi, rotation } => json!({
            "phone": phone, "stream_w": stream_w, "stream_h": stream_h,
            "screen_w": screen_w, "screen_h": screen_h, "dpi": dpi, "rotation": rotation,
        }),
        FarmEvent::Video { phone, frame } => json!({
            "phone": phone, "seq": frame.seq, "codec": frame.codec, "key": frame.key_frame,
            "pts_us": frame.pts_us, "size": frame.data.len(),
        }),
        FarmEvent::AudioConfig { phone, codec, rate, channels } => {
            json!({"phone": phone, "codec": codec, "rate": rate, "channels": channels})
        }
        FarmEvent::Audio { phone, pts_us, data } => {
            json!({"phone": phone, "pts_us": pts_us, "size": data.len()})
        }
        FarmEvent::Status { phone, code, detail } => json!({"phone": phone, "code": code, "detail": detail}),
        FarmEvent::PeerSilent { phone } => json!({"phone": phone}),
        FarmEvent::Malformed { phone, detail } => json!({"phone": phone, "detail": detail}),
        FarmEvent::Disconnected { phone, reason } => json!({"phone": phone, "reason": reason}),
        FarmEvent::LinkDown { phone, detail } => json!({"phone": phone, "detail": detail}),
        FarmEvent::Warning { phone, detail } => json!({"phone": phone, "detail": detail}),
    }
}

/// Берёт следующее событие и делает его текущим. Возвращает 1 (событие есть: вид и номер телефона записаны)
/// или 0 (событий нет). Видео приходит только от телефонов, за которыми «следят» (`pcdock_farm_set_watching`).
#[no_mangle]
pub extern "C" fn pcdock_farm_poll(p: *mut PcdockFarm, out_kind: *mut u32, out_phone: *mut u32) -> i32 {
    guard(|| {
        let h = check(p)?;
        if out_kind.is_null() || out_phone.is_null() {
            return Err(PCDOCK_ERR_NULL);
        }
        if h.queue.is_empty() {
            h.queue.extend(h.farm.drain());
        }
        match h.queue.pop_front() {
            Some(ev) => {
                let (kind, phone) = kind_of(&ev);
                // SAFETY: указатели проверены выше.
                unsafe {
                    *out_kind = kind;
                    *out_phone = phone;
                }
                h.current = Some(ev);
                Ok(PCDOCK_ITEM)
            }
            None => {
                h.current = None;
                Ok(PCDOCK_OK)
            }
        }
    })
}

/// Подробности текущего события в виде JSON-объекта (UTF-8). Если буфер мал — `-4` и нужный размер.
#[no_mangle]
pub extern "C" fn pcdock_farm_event_json(p: *mut PcdockFarm, out_buf: *mut u8, inout_len: *mut usize) -> i32 {
    guard(|| {
        let h = check(p)?;
        let ev = h.current.as_ref().ok_or(PCDOCK_ERR_ARG)?;
        Ok(copy_out(event_json(ev).to_string().as_bytes(), out_buf, inout_len))
    })
}

/// Двоичные данные текущего события: JPEG для видео, PCM для звука. Для остальных событий `-2`.
#[no_mangle]
pub extern "C" fn pcdock_farm_event_data(p: *mut PcdockFarm, out_buf: *mut u8, inout_len: *mut usize) -> i32 {
    guard(|| {
        let h = check(p)?;
        match h.current.as_ref().ok_or(PCDOCK_ERR_ARG)? {
            FarmEvent::Video { frame, .. } => Ok(copy_out(&frame.data, out_buf, inout_len)),
            FarmEvent::Audio { data, .. } => Ok(copy_out(data, out_buf, inout_len)),
            _ => Err(PCDOCK_ERR_ARG),
        }
    })
}

// ----- телефоны -----

/// Число телефонов в ферме.
#[no_mangle]
pub extern "C" fn pcdock_farm_phone_count(p: *mut PcdockFarm) -> i32 {
    guard(|| Ok(check(p)?.farm.phones().len() as i32))
}

/// Номер телефона по порядку 0..count. Возвращает 1 и пишет номер, либо 0, если такого нет.
#[no_mangle]
pub extern "C" fn pcdock_farm_phone_at(p: *mut PcdockFarm, index: u32, out_id: *mut u32) -> i32 {
    guard(|| {
        let h = check(p)?;
        if out_id.is_null() {
            return Err(PCDOCK_ERR_NULL);
        }
        match h.farm.phones().get(index as usize) {
            Some(id) => {
                // SAFETY: указатель проверен выше.
                unsafe { *out_id = *id };
                Ok(PCDOCK_ITEM)
            }
            None => Ok(PCDOCK_OK),
        }
    })
}

/// Состояние телефона: 0 appeared, 1 connecting, 2 ready, 3 needs_permission, 4 lost.
#[no_mangle]
pub extern "C" fn pcdock_farm_state(p: *mut PcdockFarm, id: u32) -> i32 {
    guard(|| {
        let h = check(p)?;
        Ok(match h.farm.state(id).map_err(err)? {
            PhoneState::Appeared => PCDOCK_PHONE_APPEARED,
            PhoneState::Connecting => PCDOCK_PHONE_CONNECTING,
            PhoneState::Ready => PCDOCK_PHONE_READY,
            PhoneState::NeedsPermission => PCDOCK_PHONE_NEEDS_PERMISSION,
            PhoneState::Lost => PCDOCK_PHONE_LOST,
        })
    })
}

/// Следить за телефоном: видео идёт в события только от тех, за кем следят. Кэш кадра хранится всегда.
#[no_mangle]
pub extern "C" fn pcdock_farm_set_watching(p: *mut PcdockFarm, id: u32, watching: bool) -> i32 {
    guard(|| {
        check(p)?.farm.set_watching(id, watching).map_err(err)?;
        Ok(PCDOCK_OK)
    })
}

#[no_mangle]
pub extern "C" fn pcdock_farm_is_watching(p: *mut PcdockFarm, id: u32) -> i32 {
    guard(|| Ok(i32::from(check(p)?.farm.is_watching(id).map_err(err)?)))
}

/// Последний кадр телефона без потребления событий. Возвращает 1 (есть), 0 (нет) или ошибку.
#[no_mangle]
#[allow(clippy::too_many_arguments)]
pub extern "C" fn pcdock_farm_last_frame(
    p: *mut PcdockFarm,
    id: u32,
    out_seq: *mut u64,
    out_codec: *mut u8,
    out_key: *mut bool,
    out_pts_us: *mut u64,
    out_buf: *mut u8,
    inout_len: *mut usize,
) -> i32 {
    guard(|| {
        let h = check(p)?;
        if out_seq.is_null() || out_codec.is_null() || out_key.is_null() || out_pts_us.is_null() {
            return Err(PCDOCK_ERR_NULL);
        }
        match h.farm.last_frame(id).map_err(err)? {
            // SAFETY: указатели проверены выше.
            Some(f) => unsafe {
                *out_seq = f.seq;
                *out_codec = f.codec;
                *out_key = f.key_frame;
                *out_pts_us = f.pts_us;
                Ok(copy_out(&f.data, out_buf, inout_len))
            },
            None => Ok(PCDOCK_OK),
        }
    })
}

// ----- ввод -----

fn command(p: *mut PcdockFarm, id: u32, cmd: InputCmd) -> i32 {
    guard(|| {
        check(p)?.farm.command(id, cmd).map_err(err)?;
        Ok(PCDOCK_OK)
    })
}

#[no_mangle]
pub extern "C" fn pcdock_farm_device_enable(p: *mut PcdockFarm, id: u32, kind: u32, enabled: bool) -> i32 {
    match device_kind(kind) {
        Ok(kind) => command(p, id, InputCmd::DeviceEnable { kind, on: enabled }),
        Err(e) => e,
    }
}

/// Касание. Отрицательные ширина, высота или сила — значения по умолчанию.
#[no_mangle]
#[allow(clippy::too_many_arguments)]
pub extern "C" fn pcdock_farm_touch_ex(
    p: *mut PcdockFarm,
    id: u32,
    cid: u8,
    phase: u32,
    x: f32,
    y: f32,
    width: f32,
    height: f32,
    pressure: f32,
) -> i32 {
    let phase = match touch_phase(phase) {
        Ok(v) => v,
        Err(e) => return e,
    };
    let opt = |v: f32| if v >= 0.0 { Some(v) } else { None };
    command(p, id, InputCmd::touch_ex(cid, phase, x, y, opt(width), opt(height), opt(pressure)))
}

#[no_mangle]
pub extern "C" fn pcdock_farm_cursor_move_abs(p: *mut PcdockFarm, id: u32, x: f32, y: f32) -> i32 {
    command(p, id, InputCmd::CursorMoveAbs { x, y })
}

#[no_mangle]
pub extern "C" fn pcdock_farm_cursor_move_rel(p: *mut PcdockFarm, id: u32, dx: i32, dy: i32) -> i32 {
    command(p, id, InputCmd::CursorMoveRel { dx, dy })
}

#[no_mangle]
pub extern "C" fn pcdock_farm_cursor_resync(p: *mut PcdockFarm, id: u32) -> i32 {
    command(p, id, InputCmd::CursorResync)
}

#[no_mangle]
pub extern "C" fn pcdock_farm_mouse_button(p: *mut PcdockFarm, id: u32, button: u32, down: bool) -> i32 {
    match mouse_button(button) {
        Ok(button) => command(p, id, InputCmd::MouseButton { button, down }),
        Err(e) => e,
    }
}

#[no_mangle]
pub extern "C" fn pcdock_farm_scroll(p: *mut PcdockFarm, id: u32, ticks: f32) -> i32 {
    command(p, id, InputCmd::Scroll { ticks })
}

#[no_mangle]
pub extern "C" fn pcdock_farm_scroll_h(p: *mut PcdockFarm, id: u32, ticks: f32) -> i32 {
    command(p, id, InputCmd::ScrollH { ticks })
}

#[no_mangle]
pub extern "C" fn pcdock_farm_key(p: *mut PcdockFarm, id: u32, usage: u16, down: bool) -> i32 {
    command(p, id, InputCmd::Key { usage, down })
}

#[no_mangle]
pub extern "C" fn pcdock_farm_release_all(p: *mut PcdockFarm, id: u32) -> i32 {
    command(p, id, InputCmd::ReleaseAll)
}

#[no_mangle]
pub extern "C" fn pcdock_farm_request_frame(p: *mut PcdockFarm, id: u32) -> i32 {
    command(p, id, InputCmd::RequestFrame)
}

#[no_mangle]
pub extern "C" fn pcdock_farm_wake(p: *mut PcdockFarm, id: u32) -> i32 {
    command(p, id, InputCmd::Wake)
}

#[no_mangle]
pub extern "C" fn pcdock_farm_set_stream(p: *mut PcdockFarm, id: u32, video: bool, audio: bool) -> i32 {
    command(p, id, InputCmd::SetStream { video, audio })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn new(cfg: &str) -> *mut PcdockFarm {
        pcdock_farm_new(cfg.as_ptr(), cfg.len())
    }

    /// Ферма без транспортов: ничего не ищет, проверяем только границу C ABI.
    const QUIET: &str = r#"{"usb": false, "adb": false, "serial": false}"#;

    #[test]
    fn farm_starts_empty_and_polls_nothing() {
        let p = new(QUIET);
        assert!(!p.is_null());
        assert_eq!(pcdock_farm_phone_count(p), 0);
        let (mut kind, mut phone) = (0u32, 0u32);
        assert_eq!(pcdock_farm_poll(p, &mut kind, &mut phone), PCDOCK_OK);
        let mut id = 0u32;
        assert_eq!(pcdock_farm_phone_at(p, 0, &mut id), PCDOCK_OK);
        let mut cap = 0usize;
        assert_eq!(
            pcdock_farm_event_json(p, std::ptr::null_mut(), &mut cap),
            PCDOCK_ERR_ARG,
            "нет текущего события"
        );
        pcdock_farm_free(p);
    }

    #[test]
    fn bad_config_returns_null_with_message() {
        let p = new(r#"{"autto": true}"#);
        assert!(p.is_null());
        let mut buf = [0u8; 256];
        let mut len = buf.len();
        assert_eq!(pcdock_farm_last_error(buf.as_mut_ptr(), &mut len), PCDOCK_OK);
        let msg = std::str::from_utf8(&buf[..len]).unwrap();
        assert!(msg.contains("autto"), "{msg}");

        // Без автообнаружения и закреплённых телефонов ферме нечего делать.
        assert!(new(r#"{"auto": false}"#).is_null());
        assert!(pcdock_farm_new(std::ptr::null(), 5).is_null());
    }

    #[test]
    fn commands_to_unknown_phone_are_errors_not_crashes() {
        let p = new(QUIET);
        assert_eq!(pcdock_farm_wake(p, 7), PCDOCK_ERR_ARG);
        assert_eq!(pcdock_farm_touch_ex(p, 7, 0, 99, 0.5, 0.5, -1.0, -1.0, -1.0), PCDOCK_ERR_ARG);
        assert_eq!(pcdock_farm_state(p, 7), PCDOCK_ERR_ARG);
        assert_eq!(pcdock_farm_wake(std::ptr::null_mut(), 1), PCDOCK_ERR_NULL);
        pcdock_farm_free(p);
        pcdock_farm_free(std::ptr::null_mut());
    }

    #[test]
    fn event_json_describes_events() {
        let ev = FarmEvent::State { phone: 3, state: PhoneState::NeedsPermission };
        assert_eq!(kind_of(&ev), (PCDOCK_FARM_EV_STATE, 3));
        assert_eq!(event_json(&ev), json!({"phone": 3, "state": "needs_permission"}));
        let ev = FarmEvent::Appeared {
            phone: 1,
            transport: pcdock_core::link::Transport::Usb,
            address: "1-2".into(),
            serial: None,
        };
        assert_eq!(event_json(&ev)["transport"], "usb");
    }
}
