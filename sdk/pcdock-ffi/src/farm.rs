//! C ABI поверх `pcdock-farm`: реестр трубок для демона и обычных приложений.
//!
//! Те же правила, что у одиночной сессии: коды `0/1/отрицательные`,
//! буферы выделяет вызывающий, дескриптор однопоточный.

use std::collections::HashMap;
use std::panic::catch_unwind;

use pcdock_core::hid::Direction;
use pcdock_core::input::{DeviceKind, MouseButton, TouchPhase};
use pcdock_core::session::{Event, State, Transmit};
use pcdock_farm::{Farm, FarmError, PhoneId};

use super::{check_slice, copy_out, guard, transmit_rc};
use super::{
    PCDOCK_BTN_LEFT, PCDOCK_BTN_MIDDLE, PCDOCK_BTN_RIGHT, PCDOCK_DC_CLOSED, PCDOCK_DC_PEER_ERROR,
    PCDOCK_DC_TIMEOUT, PCDOCK_DC_VERSION, PCDOCK_DEV_KEYBOARD, PCDOCK_DEV_MOUSE, PCDOCK_DEV_TOUCH,
    PCDOCK_DIR_IN, PCDOCK_DIR_OUT, PCDOCK_ERR_ARG, PCDOCK_ERR_INPUT, PCDOCK_ERR_NULL, PCDOCK_EV_AUDIO,
    PCDOCK_EV_AUDIO_CONFIG, PCDOCK_EV_CONNECTED, PCDOCK_EV_DISCONNECTED, PCDOCK_EV_DISPLAY,
    PCDOCK_EV_MALFORMED, PCDOCK_EV_PEER_SILENT, PCDOCK_EV_STATUS, PCDOCK_EV_VIDEO, PCDOCK_ITEM, PCDOCK_OK,
    PCDOCK_STATE_ACTIVE, PCDOCK_STATE_CONNECTING, PCDOCK_STATE_IDLE, PCDOCK_TOUCH_CANCEL, PCDOCK_TOUCH_DOWN,
    PCDOCK_TOUCH_MOVE, PCDOCK_TOUCH_UP, PCDOCK_TX_CONTROL, PCDOCK_TX_STREAM,
};

/// Непрозрачный дескриптор фермы.
pub struct PcdockFarm {
    farm: Farm,
    last_tx: HashMap<PhoneId, Transmit>,
    last_ev: HashMap<PhoneId, Event>,
}

fn check_farm(p: *mut PcdockFarm) -> Result<&'static mut PcdockFarm, i32> {
    if p.is_null() {
        return Err(PCDOCK_ERR_NULL);
    }
    // SAFETY: указатель выдан `pcdock_farm_new` и жив до `pcdock_farm_free`.
    Ok(unsafe { &mut *p })
}

fn farm_err(e: FarmError) -> i32 {
    match e {
        FarmError::UnknownPhone(_) | FarmError::AlreadyExists(_) => PCDOCK_ERR_ARG,
        FarmError::Input(_) => PCDOCK_ERR_INPUT,
    }
}

fn device_kind(kind: u32) -> Result<DeviceKind, i32> {
    match kind {
        PCDOCK_DEV_TOUCH => Ok(DeviceKind::Touch),
        PCDOCK_DEV_MOUSE => Ok(DeviceKind::Mouse),
        PCDOCK_DEV_KEYBOARD => Ok(DeviceKind::Keyboard),
        _ => Err(PCDOCK_ERR_ARG),
    }
}

fn touch_phase(phase: u32) -> Result<TouchPhase, i32> {
    match phase {
        PCDOCK_TOUCH_DOWN => Ok(TouchPhase::Down),
        PCDOCK_TOUCH_MOVE => Ok(TouchPhase::Move),
        PCDOCK_TOUCH_UP => Ok(TouchPhase::Up),
        PCDOCK_TOUCH_CANCEL => Ok(TouchPhase::Cancel),
        _ => Err(PCDOCK_ERR_ARG),
    }
}

fn mouse_button(button: u32) -> Result<MouseButton, i32> {
    match button {
        PCDOCK_BTN_LEFT => Ok(MouseButton::Left),
        PCDOCK_BTN_RIGHT => Ok(MouseButton::Right),
        PCDOCK_BTN_MIDDLE => Ok(MouseButton::Middle),
        _ => Err(PCDOCK_ERR_ARG),
    }
}

fn last_event(h: &PcdockFarm, id: PhoneId) -> Result<Event, i32> {
    h.last_ev.get(&id).cloned().ok_or(PCDOCK_ERR_ARG)
}

// ----- жизненный цикл -----

/// Создаёт пустую ферму. Освободить через `pcdock_farm_free`.
#[no_mangle]
pub extern "C" fn pcdock_farm_new() -> *mut PcdockFarm {
    match catch_unwind(|| {
        Box::into_raw(Box::new(PcdockFarm {
            farm: Farm::new(),
            last_tx: HashMap::new(),
            last_ev: HashMap::new(),
        }))
    }) {
        Ok(p) => p,
        Err(_) => std::ptr::null_mut(),
    }
}

/// Освобождает ферму вместе со всеми сессиями.
#[no_mangle]
pub extern "C" fn pcdock_farm_free(p: *mut PcdockFarm) {
    let _ = catch_unwind(|| {
        if !p.is_null() {
            // SAFETY: указатель выдан `pcdock_farm_new` и освобождается один раз здесь.
            unsafe { drop(Box::from_raw(p)) };
        }
    });
}

/// Добавляет трубку. Имя/поток задаются отдельно до `start`.
#[no_mangle]
pub extern "C" fn pcdock_farm_add(p: *mut PcdockFarm, id: u32) -> i32 {
    guard(|| {
        let h = check_farm(p)?;
        h.farm.add_phone(id).map_err(farm_err)?;
        Ok(PCDOCK_OK)
    })
}

/// Убирает трубку (сессия закрывается).
#[no_mangle]
pub extern "C" fn pcdock_farm_remove(p: *mut PcdockFarm, id: u32) -> i32 {
    guard(|| {
        let h = check_farm(p)?;
        h.farm.remove_phone(id).map_err(farm_err)?;
        h.last_tx.remove(&id);
        h.last_ev.remove(&id);
        Ok(PCDOCK_OK)
    })
}

/// Число трубок в ферме.
#[no_mangle]
pub extern "C" fn pcdock_farm_count(p: *mut PcdockFarm) -> i32 {
    match check_farm(p) {
        Ok(h) => h.farm.len() as i32,
        Err(_) => PCDOCK_ERR_NULL,
    }
}

/// Идентификатор трубки по порядковому номеру 0..count (порядок сортированный).
/// Возвращает 1 и пишет id, либо 0 (нет такой) или ошибку.
#[no_mangle]
pub extern "C" fn pcdock_farm_id_at(p: *mut PcdockFarm, index: u32, out_id: *mut u32) -> i32 {
    guard(|| {
        let h = check_farm(p)?;
        if out_id.is_null() {
            return Err(PCDOCK_ERR_NULL);
        }
        match h.farm.phone_ids().get(index as usize).copied() {
            // SAFETY: указатель проверен выше.
            Some(id) => unsafe {
                *out_id = id;
                Ok(PCDOCK_ITEM)
            },
            None => Ok(PCDOCK_OK),
        }
    })
}

/// Имя реализации для `HELLO` (вызывать до `start`).
#[no_mangle]
pub extern "C" fn pcdock_farm_set_name(p: *mut PcdockFarm, id: u32, name: *const u8, len: usize) -> i32 {
    guard(|| {
        let h = check_farm(p)?;
        let bytes = check_slice(name, len)?;
        h.farm.set_name(id, &String::from_utf8_lossy(bytes)).map_err(farm_err)?;
        Ok(PCDOCK_OK)
    })
}

/// Начальные настройки потока трубки (вызывать до `start`).
#[no_mangle]
pub extern "C" fn pcdock_farm_set_initial_stream(
    p: *mut PcdockFarm,
    id: u32,
    video: bool,
    audio: bool,
) -> i32 {
    guard(|| {
        let h = check_farm(p)?;
        h.farm.set_initial_stream(id, video, audio).map_err(farm_err)?;
        Ok(PCDOCK_OK)
    })
}

/// Флаг «трубку сейчас смотрят»: сокет-мост шлёт её кадры, анализ берёт по запросу.
#[no_mangle]
pub extern "C" fn pcdock_farm_set_watching(p: *mut PcdockFarm, id: u32, watching: bool) -> i32 {
    guard(|| {
        let h = check_farm(p)?;
        h.farm.set_watching(id, watching).map_err(farm_err)?;
        Ok(PCDOCK_OK)
    })
}

#[no_mangle]
pub extern "C" fn pcdock_farm_is_watching(p: *mut PcdockFarm, id: u32) -> i32 {
    guard(|| {
        let h = check_farm(p)?;
        Ok(if h.farm.is_watching(id).map_err(farm_err)? { 1 } else { 0 })
    })
}

/// Последний кадр трубки без потребления очереди событий.
/// Пишет seq/кодек/ключ/pts и данные в буфер. Возвращает 1 (есть кадр), 0 (нет) или ошибку.
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
        let h = check_farm(p)?;
        if out_seq.is_null() || out_codec.is_null() || out_key.is_null() || out_pts_us.is_null() {
            return Err(PCDOCK_ERR_NULL);
        }
        match h.farm.last_frame(id).map_err(farm_err)? {
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

/// Состояние сессии трубки: 0 idle, 1 connecting, 2 active.
#[no_mangle]
pub extern "C" fn pcdock_farm_state(p: *mut PcdockFarm, id: u32) -> i32 {
    guard(|| {
        let h = check_farm(p)?;
        Ok(match h.farm.state(id).map_err(farm_err)? {
            State::Idle => PCDOCK_STATE_IDLE as i32,
            State::Connecting => PCDOCK_STATE_CONNECTING as i32,
            State::Active => PCDOCK_STATE_ACTIVE as i32,
        })
    })
}

// ----- транспорт -----

#[no_mangle]
pub extern "C" fn pcdock_farm_start(p: *mut PcdockFarm, id: u32, now_ms: u64) -> i32 {
    guard(|| {
        let h = check_farm(p)?;
        h.farm.start(id, now_ms).map_err(farm_err)?;
        Ok(PCDOCK_OK)
    })
}

#[no_mangle]
pub extern "C" fn pcdock_farm_close(p: *mut PcdockFarm, id: u32) -> i32 {
    guard(|| {
        let h = check_farm(p)?;
        h.farm.close(id).map_err(farm_err)?;
        Ok(PCDOCK_OK)
    })
}

#[no_mangle]
pub extern "C" fn pcdock_farm_handle_input(
    p: *mut PcdockFarm,
    id: u32,
    now_ms: u64,
    data: *const u8,
    len: usize,
) -> i32 {
    guard(|| {
        let h = check_farm(p)?;
        let bytes = check_slice(data, len)?;
        h.farm.handle_input(id, now_ms, bytes).map_err(farm_err)?;
        Ok(PCDOCK_OK)
    })
}

#[no_mangle]
pub extern "C" fn pcdock_farm_handle_timeout(p: *mut PcdockFarm, id: u32, now_ms: u64) -> i32 {
    guard(|| {
        let h = check_farm(p)?;
        h.farm.handle_timeout(id, now_ms).map_err(farm_err)?;
        Ok(PCDOCK_OK)
    })
}

#[no_mangle]
pub extern "C" fn pcdock_farm_next_timeout(p: *mut PcdockFarm, id: u32, out_ms: *mut u64) -> i32 {
    guard(|| {
        let h = check_farm(p)?;
        if out_ms.is_null() {
            return Err(PCDOCK_ERR_NULL);
        }
        match h.farm.next_timeout(id).map_err(farm_err)? {
            // SAFETY: указатель проверен выше.
            Some(ms) => unsafe {
                *out_ms = ms;
                Ok(PCDOCK_ITEM)
            },
            None => Ok(PCDOCK_OK),
        }
    })
}

#[no_mangle]
#[allow(clippy::too_many_arguments)]
pub extern "C" fn pcdock_farm_poll_transmit(
    p: *mut PcdockFarm,
    id: u32,
    out_kind: *mut u32,
    out_dir: *mut u32,
    out_request: *mut u8,
    out_value: *mut u16,
    out_index: *mut u16,
    out_in_len: *mut u16,
    out_buf: *mut u8,
    inout_len: *mut usize,
) -> i32 {
    guard(|| {
        let h = check_farm(p)?;
        if out_kind.is_null()
            || out_dir.is_null()
            || out_request.is_null()
            || out_value.is_null()
            || out_index.is_null()
            || out_in_len.is_null()
        {
            return Err(PCDOCK_ERR_NULL);
        }
        let tx = match h.farm.poll_transmit(id).map_err(farm_err)? {
            Some(t) => t,
            None => return Ok(PCDOCK_OK),
        };
        // SAFETY: все out-указатели проверены выше.
        unsafe {
            match tx {
                Transmit::Stream(bytes) => {
                    *out_kind = PCDOCK_TX_STREAM;
                    *out_dir = PCDOCK_DIR_OUT;
                    *out_request = 0;
                    *out_value = 0;
                    *out_index = 0;
                    *out_in_len = 0;
                    let rc = copy_out(&bytes, out_buf, inout_len);
                    h.last_tx.insert(id, Transmit::Stream(bytes));
                    Ok(transmit_rc(rc))
                }
                Transmit::Control(req) => {
                    *out_kind = PCDOCK_TX_CONTROL;
                    *out_dir = match req.direction {
                        Direction::Out => PCDOCK_DIR_OUT,
                        Direction::In => PCDOCK_DIR_IN,
                    };
                    *out_request = req.request;
                    *out_value = req.value;
                    *out_index = req.index;
                    *out_in_len = req.length;
                    let rc = copy_out(&req.data, out_buf, inout_len);
                    h.last_tx.insert(id, Transmit::Control(req));
                    Ok(transmit_rc(rc))
                }
            }
        }
    })
}

#[no_mangle]
pub extern "C" fn pcdock_farm_poll_event(p: *mut PcdockFarm, id: u32, out_kind: *mut u32) -> i32 {
    guard(|| {
        let h = check_farm(p)?;
        if out_kind.is_null() {
            return Err(PCDOCK_ERR_NULL);
        }
        let ev = match h.farm.poll_event(id).map_err(farm_err)? {
            Some(e) => e,
            None => return Ok(PCDOCK_OK),
        };
        let kind = match &ev {
            Event::Connected { .. } => PCDOCK_EV_CONNECTED,
            Event::Display(_) => PCDOCK_EV_DISPLAY,
            Event::Video(_) => PCDOCK_EV_VIDEO,
            Event::AudioConfig(_) => PCDOCK_EV_AUDIO_CONFIG,
            Event::Audio(_) => PCDOCK_EV_AUDIO,
            Event::Status(_) => PCDOCK_EV_STATUS,
            Event::PeerSilent => PCDOCK_EV_PEER_SILENT,
            Event::Malformed(_) => PCDOCK_EV_MALFORMED,
            Event::Disconnected(_) => PCDOCK_EV_DISCONNECTED,
        };
        // SAFETY: указатель проверен выше.
        unsafe { *out_kind = kind };
        h.last_ev.insert(id, ev);
        Ok(PCDOCK_ITEM)
    })
}

// ----- детали событий (читаются после poll_event той же трубки) -----

#[no_mangle]
pub extern "C" fn pcdock_farm_event_connected(
    p: *mut PcdockFarm,
    id: u32,
    out_version: *mut u16,
    out_caps: *mut u32,
    out_buf: *mut u8,
    inout_len: *mut usize,
) -> i32 {
    guard(|| {
        let h = check_farm(p)?;
        if out_version.is_null() || out_caps.is_null() {
            return Err(PCDOCK_ERR_NULL);
        }
        match last_event(h, id)? {
            Event::Connected { version, capabilities, peer_name } => unsafe {
                *out_version = version;
                *out_caps = capabilities;
                Ok(copy_out(peer_name.as_bytes(), out_buf, inout_len))
            },
            _ => Err(PCDOCK_ERR_ARG),
        }
    })
}

#[no_mangle]
#[allow(clippy::too_many_arguments)]
pub extern "C" fn pcdock_farm_event_display(
    p: *mut PcdockFarm,
    id: u32,
    out_stream_w: *mut u16,
    out_stream_h: *mut u16,
    out_screen_w: *mut u16,
    out_screen_h: *mut u16,
    out_dpi: *mut u16,
    out_rotation: *mut u8,
) -> i32 {
    guard(|| {
        let h = check_farm(p)?;
        if out_stream_w.is_null()
            || out_stream_h.is_null()
            || out_screen_w.is_null()
            || out_screen_h.is_null()
            || out_dpi.is_null()
            || out_rotation.is_null()
        {
            return Err(PCDOCK_ERR_NULL);
        }
        match last_event(h, id)? {
            Event::Display(d) => unsafe {
                *out_stream_w = d.stream_width;
                *out_stream_h = d.stream_height;
                *out_screen_w = d.screen_width;
                *out_screen_h = d.screen_height;
                *out_dpi = d.density_dpi;
                *out_rotation = d.rotation;
                Ok(PCDOCK_OK)
            },
            _ => Err(PCDOCK_ERR_ARG),
        }
    })
}

#[no_mangle]
pub extern "C" fn pcdock_farm_event_video(
    p: *mut PcdockFarm,
    id: u32,
    out_codec: *mut u8,
    out_key: *mut bool,
    out_pts_us: *mut u64,
    out_buf: *mut u8,
    inout_len: *mut usize,
) -> i32 {
    guard(|| {
        let h = check_farm(p)?;
        if out_codec.is_null() || out_key.is_null() || out_pts_us.is_null() {
            return Err(PCDOCK_ERR_NULL);
        }
        match last_event(h, id)? {
            Event::Video(f) => unsafe {
                *out_codec = f.codec;
                *out_key = f.key_frame;
                *out_pts_us = f.pts_us;
                Ok(copy_out(&f.data, out_buf, inout_len))
            },
            _ => Err(PCDOCK_ERR_ARG),
        }
    })
}

#[no_mangle]
pub extern "C" fn pcdock_farm_event_audio_config(
    p: *mut PcdockFarm,
    id: u32,
    out_codec: *mut u8,
    out_rate: *mut u32,
    out_channels: *mut u8,
) -> i32 {
    guard(|| {
        let h = check_farm(p)?;
        if out_codec.is_null() || out_rate.is_null() || out_channels.is_null() {
            return Err(PCDOCK_ERR_NULL);
        }
        match last_event(h, id)? {
            Event::AudioConfig(c) => unsafe {
                *out_codec = c.codec;
                *out_rate = c.sample_rate;
                *out_channels = c.channels;
                Ok(PCDOCK_OK)
            },
            _ => Err(PCDOCK_ERR_ARG),
        }
    })
}

#[no_mangle]
pub extern "C" fn pcdock_farm_event_audio(
    p: *mut PcdockFarm,
    id: u32,
    out_pts_us: *mut u64,
    out_buf: *mut u8,
    inout_len: *mut usize,
) -> i32 {
    guard(|| {
        let h = check_farm(p)?;
        if out_pts_us.is_null() {
            return Err(PCDOCK_ERR_NULL);
        }
        match last_event(h, id)? {
            Event::Audio(f) => unsafe {
                *out_pts_us = f.pts_us;
                Ok(copy_out(&f.data, out_buf, inout_len))
            },
            _ => Err(PCDOCK_ERR_ARG),
        }
    })
}

#[no_mangle]
pub extern "C" fn pcdock_farm_event_status(
    p: *mut PcdockFarm,
    id: u32,
    out_code: *mut u16,
    out_buf: *mut u8,
    inout_len: *mut usize,
) -> i32 {
    guard(|| {
        let h = check_farm(p)?;
        if out_code.is_null() {
            return Err(PCDOCK_ERR_NULL);
        }
        match last_event(h, id)? {
            Event::Status(s) => unsafe {
                *out_code = s.code;
                Ok(copy_out(s.detail.as_bytes(), out_buf, inout_len))
            },
            Event::Malformed(detail) => unsafe {
                *out_code = 0;
                Ok(copy_out(detail.as_bytes(), out_buf, inout_len))
            },
            _ => Err(PCDOCK_ERR_ARG),
        }
    })
}

#[no_mangle]
pub extern "C" fn pcdock_farm_event_disconnected(
    p: *mut PcdockFarm,
    id: u32,
    out_reason: *mut u32,
    out_buf: *mut u8,
    inout_len: *mut usize,
) -> i32 {
    guard(|| {
        let h = check_farm(p)?;
        if out_reason.is_null() {
            return Err(PCDOCK_ERR_NULL);
        }
        match last_event(h, id)? {
            Event::Disconnected(reason) => unsafe {
                use pcdock_core::session::DisconnectReason;
                let (code, detail) = match &reason {
                    DisconnectReason::Timeout => (PCDOCK_DC_TIMEOUT, ""),
                    DisconnectReason::VersionMismatch => (PCDOCK_DC_VERSION, ""),
                    DisconnectReason::PeerError(e) => (PCDOCK_DC_PEER_ERROR, e.message.as_str()),
                    DisconnectReason::Closed => (PCDOCK_DC_CLOSED, ""),
                };
                *out_reason = code;
                Ok(copy_out(detail.as_bytes(), out_buf, inout_len))
            },
            _ => Err(PCDOCK_ERR_ARG),
        }
    })
}

// ----- ввод -----

#[no_mangle]
pub extern "C" fn pcdock_farm_device_enable(p: *mut PcdockFarm, id: u32, kind: u32, enabled: bool) -> i32 {
    guard(|| {
        let h = check_farm(p)?;
        h.farm.device_enable(id, device_kind(kind)?, enabled).map_err(farm_err)?;
        Ok(PCDOCK_OK)
    })
}

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
    guard(|| {
        let h = check_farm(p)?;
        let w = if width >= 0.0 { Some(width) } else { None };
        let hh = if height >= 0.0 { Some(height) } else { None };
        let pr = if pressure >= 0.0 { Some(pressure) } else { None };
        h.farm.touch_ex(id, cid, touch_phase(phase)?, x, y, w, hh, pr).map_err(farm_err)?;
        Ok(PCDOCK_OK)
    })
}

#[no_mangle]
pub extern "C" fn pcdock_farm_cursor_move_abs(p: *mut PcdockFarm, id: u32, x: f32, y: f32) -> i32 {
    guard(|| {
        let h = check_farm(p)?;
        h.farm.cursor_move_abs(id, x, y).map_err(farm_err)?;
        Ok(PCDOCK_OK)
    })
}

#[no_mangle]
pub extern "C" fn pcdock_farm_cursor_move_rel(p: *mut PcdockFarm, id: u32, dx: i32, dy: i32) -> i32 {
    guard(|| {
        let h = check_farm(p)?;
        h.farm.cursor_move_rel(id, dx, dy).map_err(farm_err)?;
        Ok(PCDOCK_OK)
    })
}

#[no_mangle]
pub extern "C" fn pcdock_farm_cursor_resync(p: *mut PcdockFarm, id: u32) -> i32 {
    guard(|| {
        let h = check_farm(p)?;
        h.farm.cursor_resync(id).map_err(farm_err)?;
        Ok(PCDOCK_OK)
    })
}

#[no_mangle]
pub extern "C" fn pcdock_farm_mouse_button(p: *mut PcdockFarm, id: u32, button: u32, down: bool) -> i32 {
    guard(|| {
        let h = check_farm(p)?;
        h.farm.mouse_button(id, mouse_button(button)?, down).map_err(farm_err)?;
        Ok(PCDOCK_OK)
    })
}

#[no_mangle]
pub extern "C" fn pcdock_farm_scroll(p: *mut PcdockFarm, id: u32, ticks: f32) -> i32 {
    guard(|| {
        let h = check_farm(p)?;
        h.farm.scroll(id, ticks).map_err(farm_err)?;
        Ok(PCDOCK_OK)
    })
}

#[no_mangle]
pub extern "C" fn pcdock_farm_scroll_h(p: *mut PcdockFarm, id: u32, ticks: f32) -> i32 {
    guard(|| {
        let h = check_farm(p)?;
        h.farm.scroll_h(id, ticks).map_err(farm_err)?;
        Ok(PCDOCK_OK)
    })
}

#[no_mangle]
pub extern "C" fn pcdock_farm_key(p: *mut PcdockFarm, id: u32, usage: u16, down: bool) -> i32 {
    guard(|| {
        let h = check_farm(p)?;
        h.farm.key(id, usage, down).map_err(farm_err)?;
        Ok(PCDOCK_OK)
    })
}

#[no_mangle]
pub extern "C" fn pcdock_farm_release_all(p: *mut PcdockFarm, id: u32) -> i32 {
    guard(|| {
        let h = check_farm(p)?;
        h.farm.release_all(id).map_err(farm_err)?;
        Ok(PCDOCK_OK)
    })
}

#[no_mangle]
pub extern "C" fn pcdock_farm_request_frame(p: *mut PcdockFarm, id: u32) -> i32 {
    guard(|| {
        let h = check_farm(p)?;
        h.farm.request_frame(id).map_err(farm_err)?;
        Ok(PCDOCK_OK)
    })
}

#[no_mangle]
pub extern "C" fn pcdock_farm_wake(p: *mut PcdockFarm, id: u32) -> i32 {
    guard(|| {
        let h = check_farm(p)?;
        h.farm.wake(id).map_err(farm_err)?;
        Ok(PCDOCK_OK)
    })
}

#[no_mangle]
pub extern "C" fn pcdock_farm_set_stream(p: *mut PcdockFarm, id: u32, video: bool, audio: bool) -> i32 {
    guard(|| {
        let h = check_farm(p)?;
        h.farm.set_stream(id, video, audio).map_err(farm_err)?;
        Ok(PCDOCK_OK)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use pcdock_core::packet::{Hello, Role};

    fn hello_bytes() -> Vec<u8> {
        Hello { version_max: 2, version_min: 2, capabilities: 0, role: Role::Device, name: "phone".into() }
            .to_packet()
            .encode()
    }

    #[test]
    fn farm_over_ffi_routes_by_phone() {
        let f = pcdock_farm_new();
        assert!(!f.is_null());
        assert_eq!(pcdock_farm_add(f, 1), PCDOCK_OK);
        assert_eq!(pcdock_farm_add(f, 2), PCDOCK_OK);
        assert_eq!(pcdock_farm_add(f, 1), PCDOCK_ERR_ARG);
        assert_eq!(pcdock_farm_count(f), 2);
        let mut id = 0u32;
        assert_eq!(pcdock_farm_id_at(f, 0, &mut id), PCDOCK_ITEM);
        assert_eq!(id, 1);
        assert_eq!(pcdock_farm_start(f, 1, 1000), PCDOCK_OK);
        assert_eq!(pcdock_farm_start(f, 2, 1000), PCDOCK_OK);
        let hb = hello_bytes();
        assert_eq!(pcdock_farm_handle_input(f, 1, 1001, hb.as_ptr(), hb.len()), PCDOCK_OK);
        assert_eq!(pcdock_farm_state(f, 1), PCDOCK_STATE_ACTIVE as i32);
        assert_eq!(pcdock_farm_state(f, 2), PCDOCK_STATE_CONNECTING as i32);
        // touch_ex второй трубке: первая не затронута.
        assert_eq!(pcdock_farm_touch_ex(f, 2, 0, PCDOCK_TOUCH_DOWN, 0.5, 0.5, -1.0, -1.0, -1.0), PCDOCK_OK);
        assert_eq!(pcdock_farm_set_watching(f, 1, true), PCDOCK_OK);
        assert_eq!(pcdock_farm_is_watching(f, 1), 1);
        assert_eq!(pcdock_farm_remove(f, 2), PCDOCK_OK);
        assert_eq!(pcdock_farm_count(f), 1);
        pcdock_farm_free(f);
    }
}
