//! C ABI поверх [`pcdock_core`]: владеет [`Session`], отдаёт события и запросы наружу.
//!
//! Правила:
//! - все функции возвращают `i32`: `0` = ок, `1` = есть элемент (для `poll_*`),
//!   отрицательные = ошибка (`-1` нулевой указатель, `-2` неверный аргумент,
//!   `-3` ошибка ввода ядра, `-4` мал буфер, `-99` паника внутри);
//! - вызывающий выделяет буферы сам; если буфер мал, функция возвращает `-4`
//!   и записывает в `*inout_len` нужный размер;
//! - `Session` однопоточная: вызывать из одного потока (как в Python-viewer);
//! - время (`now_ms`) — монотонные миллисекунды, их даёт вызывающий.
//!
//! `unsafe` здесь только на границе FFI (проверенные копирования через сырые указатели);
//! паники ловятся и превращаются в `PCDOCK_ERR_PANIC`, нулевые указатели — в `PCDOCK_ERR_NULL`.
#![allow(clippy::not_unsafe_ptr_arg_deref)] // C ABI: сырые указатели — часть стабильного интерфейса

use pcdock_core::hid::AoaStrings;
use pcdock_core::input::{DeviceKind, MouseButton, TouchPhase, TouchPoint};
use pcdock_core::packet::SetStream;
use pcdock_core::session::{DisconnectReason, Event, Session, State, Transmit};
use std::panic::{catch_unwind, UnwindSafe};

/// Коды возврата.
pub const PCDOCK_OK: i32 = 0;
pub const PCDOCK_ITEM: i32 = 1;
pub const PCDOCK_ERR_NULL: i32 = -1;
pub const PCDOCK_ERR_ARG: i32 = -2;
pub const PCDOCK_ERR_INPUT: i32 = -3;
pub const PCDOCK_ERR_SMALL_BUF: i32 = -4;
pub const PCDOCK_ERR_PANIC: i32 = -99;

/// `Transmit::Stream` — байты в транспорт; `Transmit::Control` — USB control transfer.
pub const PCDOCK_TX_STREAM: u32 = 0;
pub const PCDOCK_TX_CONTROL: u32 = 1;
/// Направление control transfer: хост→устройство / устройство→хост.
pub const PCDOCK_DIR_OUT: u32 = 0;
pub const PCDOCK_DIR_IN: u32 = 1;

/// Виды событий (`poll_event`).
pub const PCDOCK_EV_CONNECTED: u32 = 1;
pub const PCDOCK_EV_DISPLAY: u32 = 2;
pub const PCDOCK_EV_VIDEO: u32 = 3;
pub const PCDOCK_EV_AUDIO_CONFIG: u32 = 4;
pub const PCDOCK_EV_AUDIO: u32 = 5;
pub const PCDOCK_EV_STATUS: u32 = 6;
pub const PCDOCK_EV_PEER_SILENT: u32 = 7;
pub const PCDOCK_EV_MALFORMED: u32 = 8;
pub const PCDOCK_EV_DISCONNECTED: u32 = 9;

/// Состояние сессии.
pub const PCDOCK_STATE_IDLE: u32 = 0;
pub const PCDOCK_STATE_CONNECTING: u32 = 1;
pub const PCDOCK_STATE_ACTIVE: u32 = 2;

/// Устройства ввода: тачскрин, мышь, клавиатура.
pub const PCDOCK_DEV_TOUCH: u32 = 0;
pub const PCDOCK_DEV_MOUSE: u32 = 1;
pub const PCDOCK_DEV_KEYBOARD: u32 = 2;

/// Фазы касания: опущен, движется, поднят, отменён.
pub const PCDOCK_TOUCH_DOWN: u32 = 0;
pub const PCDOCK_TOUCH_MOVE: u32 = 1;
pub const PCDOCK_TOUCH_UP: u32 = 2;
pub const PCDOCK_TOUCH_CANCEL: u32 = 3;

/// Кнопки мыши: левая, правая, средняя.
pub const PCDOCK_BTN_LEFT: u32 = 0;
pub const PCDOCK_BTN_RIGHT: u32 = 1;
pub const PCDOCK_BTN_MIDDLE: u32 = 2;

/// Причины отключения.
pub const PCDOCK_DC_TIMEOUT: u32 = 0;
pub const PCDOCK_DC_VERSION: u32 = 1;
pub const PCDOCK_DC_PEER_ERROR: u32 = 2;
pub const PCDOCK_DC_CLOSED: u32 = 3;

/// Непрозрачный дескриптор сессии. Создаётся в `pcdock_session_new`, умирает в `pcdock_session_free`.
pub struct PcdockSession {
    session: Session,
    last_tx: Option<Transmit>,
    last_ev: Option<Event>,
}

fn guard(body: impl FnOnce() -> Result<i32, i32> + UnwindSafe) -> i32 {
    match catch_unwind(body) {
        Ok(Ok(v)) => v,
        Ok(Err(e)) => e,
        Err(_) => PCDOCK_ERR_PANIC,
    }
}

fn check_handle(p: *mut PcdockSession) -> Result<&'static mut PcdockSession, i32> {
    if p.is_null() {
        return Err(PCDOCK_ERR_NULL);
    }
    // SAFETY: указатель выдан `pcdock_session_new` (Box::into_raw) и жив до `pcdock_session_free`.
    // Сессия однопоточная: вызывающий не передаёт дескриптор между потоками.
    Ok(unsafe { &mut *p })
}

fn check_slice(data: *const u8, len: usize) -> Result<&'static [u8], i32> {
    if len > 0 && data.is_null() {
        return Err(PCDOCK_ERR_NULL);
    }
    if len == 0 {
        return Ok(&[]);
    }
    // SAFETY: вызывающий гарантирует живой буфер длиной `len` на время вызова.
    Ok(unsafe { std::slice::from_raw_parts(data, len) })
}

fn check_out(buf: *mut u8, inout_len: *mut usize) -> Result<(*mut u8, usize), i32> {
    if inout_len.is_null() {
        return Err(PCDOCK_ERR_NULL);
    }
    // SAFETY: вызывающий гарантирует живой `*inout_len` на запись.
    let cap = unsafe { *inout_len };
    if cap > 0 && buf.is_null() {
        return Err(PCDOCK_ERR_NULL);
    }
    Ok((buf, cap))
}

/// Копирует `src` в буфер вызывающего. Если мал — возвращает нужный размер и `-4`.
fn copy_out(src: &[u8], buf: *mut u8, inout_len: *mut usize) -> i32 {
    let (_, cap) = match check_out(buf, inout_len) {
        Ok(v) => v,
        Err(e) => return e,
    };
    if cap < src.len() {
        // SAFETY: `inout_len` проверен выше.
        unsafe { *inout_len = src.len() };
        return PCDOCK_ERR_SMALL_BUF;
    }
    if !src.is_empty() {
        // SAFETY: буфер живой и вмещает `src.len()` (проверено выше).
        unsafe { std::ptr::copy_nonoverlapping(src.as_ptr(), buf, src.len()) };
    }
    // SAFETY: `inout_len` проверен выше.
    unsafe { *inout_len = src.len() };
    PCDOCK_OK
}

/// Код возврата для `poll_transmit`: данные скопированы — есть элемент, иначе ошибка копирования.
fn transmit_rc(copy_rc: i32) -> i32 {
    if copy_rc == PCDOCK_OK {
        PCDOCK_ITEM
    } else {
        copy_rc
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

// ----- жизненный цикл -----

/// Создаёт сессию с настройками по умолчанию. Освободить через `pcdock_session_free`.
#[no_mangle]
pub extern "C" fn pcdock_session_new() -> *mut PcdockSession {
    match catch_unwind(|| {
        let h = Box::new(PcdockSession {
            session: Session::new(Default::default()),
            last_tx: None,
            last_ev: None,
        });
        Box::into_raw(h)
    }) {
        Ok(p) => p,
        Err(_) => std::ptr::null_mut(),
    }
}

/// Освобождает сессию. После вызова указатель недействителен.
#[no_mangle]
pub extern "C" fn pcdock_session_free(p: *mut PcdockSession) {
    let _ = catch_unwind(|| {
        if !p.is_null() {
            // SAFETY: указатель выдан `pcdock_session_new` и освобождается один раз здесь.
            unsafe { drop(Box::from_raw(p)) };
        }
    });
}

/// Имя реализации для `HELLO` (UTF-8, например "pcdock-viewer/1.0").
#[no_mangle]
pub extern "C" fn pcdock_session_set_name(p: *mut PcdockSession, name: *const u8, len: usize) -> i32 {
    guard(|| {
        let h = check_handle(p)?;
        let bytes = check_slice(name, len)?;
        // Имя хранится в конфиге ядра; прямого сеттера нет, поэтому пересоздаём сессию
        // с тем же конфигом, но новым именем (вызывать до `start`).
        let mut cfg = h.session.config().clone();
        cfg.name = String::from_utf8_lossy(bytes).into_owned();
        h.session = Session::new(cfg);
        h.last_tx = None;
        h.last_ev = None;
        Ok(PCDOCK_OK)
    })
}

/// Настройки потока: отправятся телефону сразу после согласования (вызывать до `start`;
/// вживую меняется через `pcdock_session_set_stream`).
#[no_mangle]
pub extern "C" fn pcdock_session_set_initial_stream(p: *mut PcdockSession, video: bool, audio: bool) -> i32 {
    guard(|| {
        let h = check_handle(p)?;
        h.session.set_initial_stream(SetStream { video, audio, ..SetStream::default() });
        Ok(PCDOCK_OK)
    })
}

/// Транспорт открыт (режим аксессуара): шлёт `HELLO` и регистрирует устройства ввода.
#[no_mangle]
pub extern "C" fn pcdock_session_start(p: *mut PcdockSession, now_ms: u64) -> i32 {
    guard(|| {
        let h = check_handle(p)?;
        h.session.start(now_ms);
        Ok(PCDOCK_OK)
    })
}

/// Транспорт потерян (кабель выдернули): сессия закрывается, ввод сбрасывается.
#[no_mangle]
pub extern "C" fn pcdock_session_close(p: *mut PcdockSession) -> i32 {
    guard(|| {
        let h = check_handle(p)?;
        h.session.close();
        Ok(PCDOCK_OK)
    })
}

/// Состояние: 0 idle, 1 connecting, 2 active.
#[no_mangle]
pub extern "C" fn pcdock_session_state(p: *mut PcdockSession) -> i32 {
    guard(|| {
        let h = check_handle(p)?;
        Ok(match h.session.state() {
            State::Idle => PCDOCK_STATE_IDLE as i32,
            State::Connecting => PCDOCK_STATE_CONNECTING as i32,
            State::Active => PCDOCK_STATE_ACTIVE as i32,
        })
    })
}

// ----- ввод-вывод -----

/// Байты, прочитанные из транспорта.
#[no_mangle]
pub extern "C" fn pcdock_session_handle_input(
    p: *mut PcdockSession,
    now_ms: u64,
    data: *const u8,
    len: usize,
) -> i32 {
    guard(|| {
        let h = check_handle(p)?;
        let bytes = check_slice(data, len)?;
        h.session.handle_input(now_ms, bytes);
        Ok(PCDOCK_OK)
    })
}

/// Вызвать, когда наступило время из `pcdock_session_next_timeout`.
#[no_mangle]
pub extern "C" fn pcdock_session_handle_timeout(p: *mut PcdockSession, now_ms: u64) -> i32 {
    guard(|| {
        let h = check_handle(p)?;
        h.session.handle_timeout(now_ms);
        Ok(PCDOCK_OK)
    })
}

/// Когда вызвать `handle_timeout` в следующий раз. Возвращает 1 и пишет метку, либо 0 (нет).
#[no_mangle]
pub extern "C" fn pcdock_session_next_timeout(p: *mut PcdockSession, out_ms: *mut u64) -> i32 {
    guard(|| {
        let h = check_handle(p)?;
        if out_ms.is_null() {
            return Err(PCDOCK_ERR_NULL);
        }
        match h.session.next_timeout() {
            Some(ms) => {
                // SAFETY: указатель проверен выше, вызывающий держит его живым.
                unsafe { *out_ms = ms };
                Ok(PCDOCK_ITEM)
            }
            None => Ok(PCDOCK_OK),
        }
    })
}

/// Забирает одну единицу на отправку. Возвращает 1 (заполнены `out_*`), 0 (пусто) или ошибку.
///
/// `kind`: 0 = байты потока (bulk OUT), 1 = USB control transfer.
/// Для control: `dir` 0 = OUT (данные в `out_buf`), 1 = IN (`in_len` = сколько байт ждать).
#[no_mangle]
#[allow(clippy::too_many_arguments)]
pub extern "C" fn pcdock_session_poll_transmit(
    p: *mut PcdockSession,
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
        let h = check_handle(p)?;
        if out_kind.is_null()
            || out_dir.is_null()
            || out_request.is_null()
            || out_value.is_null()
            || out_index.is_null()
            || out_in_len.is_null()
        {
            return Err(PCDOCK_ERR_NULL);
        }
        let tx = match h.session.poll_transmit() {
            Some(t) => t,
            None => return Ok(PCDOCK_OK),
        };
        // SAFETY: все out-указатели проверены выше, вызывающий держит их живыми.
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
                    h.last_tx = Some(Transmit::Stream(bytes));
                    Ok(transmit_rc(rc))
                }
                Transmit::Control(req) => {
                    use pcdock_core::hid::Direction;
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
                    h.last_tx = Some(Transmit::Control(req));
                    Ok(transmit_rc(rc))
                }
            }
        }
    })
}

// ----- события -----

/// Забирает одно событие. Возвращает 1 (вид в `out_kind`, детали — через `pcdock_event_*`),
/// 0 (пусто) или ошибку.
#[no_mangle]
pub extern "C" fn pcdock_session_poll_event(p: *mut PcdockSession, out_kind: *mut u32) -> i32 {
    guard(|| {
        let h = check_handle(p)?;
        if out_kind.is_null() {
            return Err(PCDOCK_ERR_NULL);
        }
        let ev = match h.session.poll_event() {
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
        h.last_ev = Some(ev);
        Ok(PCDOCK_ITEM)
    })
}

fn last_event(p: *mut PcdockSession) -> Result<(*mut PcdockSession, Event), i32> {
    let h = check_handle(p)?;
    match h.last_ev.clone() {
        Some(ev) => Ok((p, ev)),
        None => Err(PCDOCK_ERR_ARG),
    }
}

/// Детали `Connected`: версия, общие возможности, имя телефона (UTF-8 в буфере).
#[no_mangle]
pub extern "C" fn pcdock_event_connected(
    p: *mut PcdockSession,
    out_version: *mut u16,
    out_caps: *mut u32,
    out_buf: *mut u8,
    inout_len: *mut usize,
) -> i32 {
    guard(|| {
        let (_, ev) = last_event(p)?;
        if out_version.is_null() || out_caps.is_null() {
            return Err(PCDOCK_ERR_NULL);
        }
        match ev {
            Event::Connected { version, capabilities, peer_name } => unsafe {
                *out_version = version;
                *out_caps = capabilities;
                Ok(copy_out(peer_name.as_bytes(), out_buf, inout_len))
            },
            _ => Err(PCDOCK_ERR_ARG),
        }
    })
}

/// Детали `Display`: размеры потока и экрана, плотность, поворот.
#[no_mangle]
#[allow(clippy::too_many_arguments)]
pub extern "C" fn pcdock_event_display(
    p: *mut PcdockSession,
    out_stream_w: *mut u16,
    out_stream_h: *mut u16,
    out_screen_w: *mut u16,
    out_screen_h: *mut u16,
    out_dpi: *mut u16,
    out_rotation: *mut u8,
) -> i32 {
    guard(|| {
        let (_, ev) = last_event(p)?;
        if out_stream_w.is_null()
            || out_stream_h.is_null()
            || out_screen_w.is_null()
            || out_screen_h.is_null()
            || out_dpi.is_null()
            || out_rotation.is_null()
        {
            return Err(PCDOCK_ERR_NULL);
        }
        match ev {
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

/// Детали `Video`: кодек, ключевой ли кадр, метка времени, данные кадра в буфере.
#[no_mangle]
pub extern "C" fn pcdock_event_video(
    p: *mut PcdockSession,
    out_codec: *mut u8,
    out_key: *mut bool,
    out_pts_us: *mut u64,
    out_buf: *mut u8,
    inout_len: *mut usize,
) -> i32 {
    guard(|| {
        let (_, ev) = last_event(p)?;
        if out_codec.is_null() || out_key.is_null() || out_pts_us.is_null() {
            return Err(PCDOCK_ERR_NULL);
        }
        match ev {
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

/// Детали `AudioConfig`: кодек, частота, число каналов.
#[no_mangle]
pub extern "C" fn pcdock_event_audio_config(
    p: *mut PcdockSession,
    out_codec: *mut u8,
    out_rate: *mut u32,
    out_channels: *mut u8,
) -> i32 {
    guard(|| {
        let (_, ev) = last_event(p)?;
        if out_codec.is_null() || out_rate.is_null() || out_channels.is_null() {
            return Err(PCDOCK_ERR_NULL);
        }
        match ev {
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

/// Детали `Audio`: метка времени, PCM-данные в буфере.
#[no_mangle]
pub extern "C" fn pcdock_event_audio(
    p: *mut PcdockSession,
    out_pts_us: *mut u64,
    out_buf: *mut u8,
    inout_len: *mut usize,
) -> i32 {
    guard(|| {
        let (_, ev) = last_event(p)?;
        if out_pts_us.is_null() {
            return Err(PCDOCK_ERR_NULL);
        }
        match ev {
            Event::Audio(f) => unsafe {
                *out_pts_us = f.pts_us;
                Ok(copy_out(&f.data, out_buf, inout_len))
            },
            _ => Err(PCDOCK_ERR_ARG),
        }
    })
}

/// Детали `Status`/`Malformed`: код и текст в буфере. Для `Malformed` код всегда 0.
#[no_mangle]
pub extern "C" fn pcdock_event_status(
    p: *mut PcdockSession,
    out_code: *mut u16,
    out_buf: *mut u8,
    inout_len: *mut usize,
) -> i32 {
    guard(|| {
        let (_, ev) = last_event(p)?;
        if out_code.is_null() {
            return Err(PCDOCK_ERR_NULL);
        }
        match ev {
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

/// Детали `Disconnected`: причина (0 таймаут, 1 версия, 2 ошибка телефона, 3 закрыто),
/// для ошибки телефона — её текст в буфере.
#[no_mangle]
pub extern "C" fn pcdock_event_disconnected(
    p: *mut PcdockSession,
    out_reason: *mut u32,
    out_buf: *mut u8,
    inout_len: *mut usize,
) -> i32 {
    guard(|| {
        let (_, ev) = last_event(p)?;
        if out_reason.is_null() {
            return Err(PCDOCK_ERR_NULL);
        }
        match ev {
            Event::Disconnected(reason) => unsafe {
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

// ----- команды ввода -----

/// Включает или отключает устройство ввода (отключение снимает его с телефона).
#[no_mangle]
pub extern "C" fn pcdock_session_device_enable(p: *mut PcdockSession, kind: u32, enabled: bool) -> i32 {
    guard(|| {
        let h = check_handle(p)?;
        h.session.device_enable(device_kind(kind)?, enabled);
        Ok(PCDOCK_OK)
    })
}

/// Касание: `id` — номер пальца, `phase` 0/1/2/3 = down/move/up/cancel,
/// координаты — доли отображаемой картинки 0.0..1.0.
#[no_mangle]
pub extern "C" fn pcdock_session_touch(p: *mut PcdockSession, id: u8, phase: u32, x: f32, y: f32) -> i32 {
    guard(|| {
        let h = check_handle(p)?;
        match h.session.touch(TouchPoint::new(id, touch_phase(phase)?, x, y)) {
            Ok(()) => Ok(PCDOCK_OK),
            Err(_) => Err(PCDOCK_ERR_INPUT),
        }
    })
}

/// Касание с явными пятном и силой: `width`/`height` — доли ширины экрана 0.0..1.0,
/// `pressure` — 0.0..1.0. Отрицательное значение = default из конфига.
/// Скрипт автотестов задаёт их сам на каждый вызов: клик и свайп собираются
/// из DOWN/MOVE/UP с нужными паузами и координатами на стороне вызывающего.
#[no_mangle]
pub extern "C" fn pcdock_session_touch_ex(
    p: *mut PcdockSession,
    id: u8,
    phase: u32,
    x: f32,
    y: f32,
    width: f32,
    height: f32,
    pressure: f32,
) -> i32 {
    guard(|| {
        let h = check_handle(p)?;
        let mut pt = TouchPoint::new(id, touch_phase(phase)?, x, y);
        if width >= 0.0 {
            pt.width = Some(width);
        }
        if height >= 0.0 {
            pt.height = Some(height);
        }
        if pressure >= 0.0 {
            pt.pressure = Some(pressure);
        }
        match h.session.touch(pt) {
            Ok(()) => Ok(PCDOCK_OK),
            Err(_) => Err(PCDOCK_ERR_INPUT),
        }
    })
}

/// Абсолютное положение курсора в долях картинки 0.0..1.0.
#[no_mangle]
pub extern "C" fn pcdock_session_cursor_move_abs(p: *mut PcdockSession, x: f32, y: f32) -> i32 {
    guard(|| {
        let h = check_handle(p)?;
        match h.session.cursor_move_abs(x, y) {
            Ok(()) => Ok(PCDOCK_OK),
            Err(_) => Err(PCDOCK_ERR_INPUT),
        }
    })
}

/// Относительное движение курсора в пикселях.
#[no_mangle]
pub extern "C" fn pcdock_session_cursor_move_rel(p: *mut PcdockSession, dx: i32, dy: i32) -> i32 {
    guard(|| {
        let h = check_handle(p)?;
        match h.session.cursor_move_rel(dx, dy) {
            Ok(()) => Ok(PCDOCK_OK),
            Err(_) => Err(PCDOCK_ERR_INPUT),
        }
    })
}

/// Забыть оценку положения курсора: следующее движение прижмёт его к углу.
#[no_mangle]
pub extern "C" fn pcdock_session_cursor_resync(p: *mut PcdockSession) -> i32 {
    guard(|| {
        let h = check_handle(p)?;
        h.session.cursor_resync();
        Ok(PCDOCK_OK)
    })
}

/// Кнопка мыши: 0 левая, 1 правая, 2 средняя.
#[no_mangle]
pub extern "C" fn pcdock_session_mouse_button(p: *mut PcdockSession, button: u32, down: bool) -> i32 {
    guard(|| {
        let h = check_handle(p)?;
        match h.session.mouse_button(mouse_button(button)?, down) {
            Ok(()) => Ok(PCDOCK_OK),
            Err(_) => Err(PCDOCK_ERR_INPUT),
        }
    })
}

/// Прокрутка в «щелчках» колеса (дробь копится внутри ядра).
#[no_mangle]
pub extern "C" fn pcdock_session_scroll(p: *mut PcdockSession, ticks: f32) -> i32 {
    guard(|| {
        let h = check_handle(p)?;
        match h.session.scroll(ticks) {
            Ok(()) => Ok(PCDOCK_OK),
            Err(_) => Err(PCDOCK_ERR_INPUT),
        }
    })
}

/// Горизонтальная прокрутка (наклон колеса / жест трекпада).
#[no_mangle]
pub extern "C" fn pcdock_session_scroll_h(p: *mut PcdockSession, ticks: f32) -> i32 {
    guard(|| {
        let h = check_handle(p)?;
        match h.session.scroll_h(ticks) {
            Ok(()) => Ok(PCDOCK_OK),
            Err(_) => Err(PCDOCK_ERR_INPUT),
        }
    })
}

/// Клавиша: USB HID usage (страница Keyboard), как SDL scancode.
#[no_mangle]
pub extern "C" fn pcdock_session_key(p: *mut PcdockSession, usage: u16, down: bool) -> i32 {
    guard(|| {
        let h = check_handle(p)?;
        match h.session.key(usage, down) {
            Ok(()) => Ok(PCDOCK_OK),
            Err(_) => Err(PCDOCK_ERR_INPUT),
        }
    })
}

/// Отпускает всё нажатое (потеря фокуса окна).
#[no_mangle]
pub extern "C" fn pcdock_session_release_all(p: *mut PcdockSession) -> i32 {
    guard(|| {
        let h = check_handle(p)?;
        h.session.release_all();
        Ok(PCDOCK_OK)
    })
}

/// Просит телефон изменить параметры потока (вживую; до старта — как начальные).
#[no_mangle]
pub extern "C" fn pcdock_session_set_stream(p: *mut PcdockSession, video: bool, audio: bool) -> i32 {
    guard(|| {
        let h = check_handle(p)?;
        let s = SetStream { video, audio, ..SetStream::default() };
        h.session.set_initial_stream(s);
        h.session.set_stream(s);
        Ok(PCDOCK_OK)
    })
}

/// Просит телефон прислать текущее содержимое экрана (вживую; до согласования — ничего).
#[no_mangle]
pub extern "C" fn pcdock_session_request_frame(p: *mut PcdockSession) -> i32 {
    guard(|| {
        let h = check_handle(p)?;
        h.session.request_frame();
        Ok(PCDOCK_OK)
    })
}

/// Просит телефон включить экран на минуту (разблокировка вручную).
#[no_mangle]
pub extern "C" fn pcdock_session_wake(p: *mut PcdockSession) -> i32 {
    guard(|| {
        let h = check_handle(p)?;
        h.session.wake();
        Ok(PCDOCK_OK)
    })
}

/// Сколько байт отброшено при перестройке потока. `u64::MAX` при нулевом дескрипторе.
#[no_mangle]
pub extern "C" fn pcdock_session_dropped_bytes(p: *mut PcdockSession) -> u64 {
    match check_handle(p) {
        Ok(h) => h.session.dropped_bytes(),
        Err(_) => u64::MAX,
    }
}

/// Строка AOA по индексу 0..5 (производитель, модель, описание, версия, URI, серийник).
/// Возвращает 1 (строка в буфере с завершающим нулём), 0 (нет такой) или ошибку.
#[no_mangle]
pub extern "C" fn pcdock_aoa_string(index: u32, out_buf: *mut u8, inout_len: *mut usize) -> i32 {
    guard(|| {
        let defaults = AoaStrings::default();
        let all = [
            defaults.manufacturer.as_str(),
            defaults.model.as_str(),
            defaults.description.as_str(),
            defaults.version.as_str(),
            defaults.uri.as_str(),
            defaults.serial.as_str(),
        ];
        let s = match all.get(index as usize) {
            Some(s) => s,
            None => return Ok(PCDOCK_OK),
        };
        let mut bytes = s.as_bytes().to_vec();
        bytes.push(0);
        match copy_out(&bytes, out_buf, inout_len) {
            PCDOCK_OK => Ok(PCDOCK_ITEM),
            e => Err(e),
        }
    })
}

static VERSION_CSTR: &[u8] = concat!(env!("CARGO_PKG_VERSION"), "\0").as_bytes();

/// Версия библиотеки (NUL-терминированная строка, живёт всегда).
#[no_mangle]
pub extern "C" fn pcdock_version() -> *const u8 {
    VERSION_CSTR.as_ptr()
}

#[cfg(test)]
mod tests {
    use super::*;
    use pcdock_core::packet::{AudioConfig, DisplayInfo, Hello, Role, VideoFrame};

    fn new() -> *mut PcdockSession {
        let p = pcdock_session_new();
        assert!(!p.is_null());
        p
    }

    /// Забирает всё из очереди: возвращает (пакеты потока, число control-запросов).
    fn drain_all(p: *mut PcdockSession) -> (Vec<Vec<u8>>, usize) {
        let mut streams = Vec::new();
        let mut controls = 0usize;
        let mut buf = vec![0u8; 8192];
        loop {
            let mut kind = 0u32;
            let (mut dir, mut req, mut value, mut index, mut ilen) = (0u32, 0u8, 0u16, 0u16, 0u16);
            let mut len = buf.len();
            let rc = pcdock_session_poll_transmit(
                p,
                &mut kind,
                &mut dir,
                &mut req,
                &mut value,
                &mut index,
                &mut ilen,
                buf.as_mut_ptr(),
                &mut len,
            );
            assert_eq!(rc, PCDOCK_ITEM);
            if kind == PCDOCK_TX_STREAM {
                streams.push(buf[..len].to_vec());
                if streams.len() >= 2 {
                    break; // HELLO + SET_STREAM есть, остальное — HID-контролы
                }
            } else {
                controls += 1;
                if controls > 64 {
                    break; // дальше только HID-дескрипторы, хватит
                }
            }
        }
        (streams, controls)
    }

    #[test]
    fn session_lifecycle_over_ffi() {
        let p = new();
        assert_eq!(pcdock_session_set_name(p, c"test".as_ptr() as *const u8, 4), PCDOCK_OK);
        assert_eq!(pcdock_session_set_initial_stream(p, true, false), PCDOCK_OK);
        assert_eq!(pcdock_session_device_enable(p, PCDOCK_DEV_MOUSE, false), PCDOCK_OK);
        assert_eq!(pcdock_session_start(p, 1000), PCDOCK_OK);
        assert_eq!(pcdock_session_state(p), PCDOCK_STATE_CONNECTING as i32);

        // Телефон отвечает HELLO: сессия активна, initial stream ушёл следом.
        let hello = Hello {
            version_max: 2,
            version_min: 2,
            capabilities: 0xFFFF_FFFF,
            role: Role::Device,
            name: "phone".into(),
        }
        .to_packet()
        .encode();
        assert_eq!(pcdock_session_handle_input(p, 1100, hello.as_ptr(), hello.len()), PCDOCK_OK);
        let mut kind = 0u32;
        assert_eq!(pcdock_session_poll_event(p, &mut kind), PCDOCK_ITEM);
        assert_eq!(kind, PCDOCK_EV_CONNECTED);
        // Очередь: HELLO, HID-контролы, затем SET_STREAM (initial).
        let (streams, controls) = drain_all(p);
        assert_eq!(streams.len(), 2, "HELLO + SET_STREAM, получили {streams:?}");
        assert!(controls > 0, "регистрация HID-устройств");

        // Кадр и звук доходят до фронтенда.
        let frame = VideoFrame { codec: 1, key_frame: true, pts_us: 42, data: vec![0xFF, 0xD8, 1, 2] }
            .to_packet()
            .encode();
        assert_eq!(pcdock_session_handle_input(p, 1200, frame.as_ptr(), frame.len()), PCDOCK_OK);
        assert_eq!(pcdock_session_poll_event(p, &mut kind), PCDOCK_ITEM);
        assert_eq!(kind, PCDOCK_EV_VIDEO);
        let (mut codec, mut key, mut pts) = (0u8, false, 0u64);
        let mut buf = vec![0u8; 64];
        let mut len = buf.len();
        assert_eq!(
            pcdock_event_video(p, &mut codec, &mut key, &mut pts, buf.as_mut_ptr(), &mut len),
            PCDOCK_OK
        );
        assert_eq!((codec, key, pts), (1, true, 42));
        assert_eq!(&buf[..len], &[0xFF, 0xD8, 1, 2]);

        // Ввод на отключённой мыши запрещён, на тачскрине — идёт в control.
        assert_eq!(pcdock_session_touch(p, 0, PCDOCK_TOUCH_DOWN, 0.5, 0.5), PCDOCK_OK);
        assert_eq!(pcdock_session_scroll(p, 1.0), PCDOCK_ERR_INPUT);
        // REQUEST_FRAME вживую уходит потоком с типом 0x40.
        assert_eq!(pcdock_session_request_frame(p), PCDOCK_OK);
        assert_eq!(pcdock_session_state(p), PCDOCK_STATE_ACTIVE as i32);
        pcdock_session_free(p);
    }

    #[test]
    fn display_and_audio_config_over_ffi() {
        let p = new();
        assert_eq!(pcdock_session_start(p, 0), PCDOCK_OK);
        let hello = Hello {
            version_max: 2,
            version_min: 2,
            capabilities: 0xFFFF_FFFF,
            role: Role::Device,
            name: "phone".into(),
        }
        .to_packet()
        .encode();
        assert_eq!(pcdock_session_handle_input(p, 10, hello.as_ptr(), hello.len()), PCDOCK_OK);
        let mut kind = 0u32;
        assert_eq!(pcdock_session_poll_event(p, &mut kind), PCDOCK_ITEM);
        assert_eq!(kind, PCDOCK_EV_CONNECTED);

        let disp = DisplayInfo {
            stream_width: 360,
            stream_height: 820,
            screen_width: 720,
            screen_height: 1640,
            density_dpi: 320,
            rotation: 1,
        }
        .to_packet()
        .encode();
        assert_eq!(pcdock_session_handle_input(p, 20, disp.as_ptr(), disp.len()), PCDOCK_OK);
        assert_eq!(pcdock_session_poll_event(p, &mut kind), PCDOCK_ITEM);
        assert_eq!(kind, PCDOCK_EV_DISPLAY);
        let (mut sw, mut sh, mut dw, mut dh, mut dpi, mut rot) = (0u16, 0u16, 0u16, 0u16, 0u16, 0u8);
        assert_eq!(
            pcdock_event_display(p, &mut sw, &mut sh, &mut dw, &mut dh, &mut dpi, &mut rot),
            PCDOCK_OK
        );
        assert_eq!((sw, sh, dw, dh, dpi, rot), (360, 820, 720, 1640, 320, 1));

        let cfg = AudioConfig { codec: 1, sample_rate: 48000, channels: 2 }.to_packet().encode();
        assert_eq!(pcdock_session_handle_input(p, 30, cfg.as_ptr(), cfg.len()), PCDOCK_OK);
        assert_eq!(pcdock_session_poll_event(p, &mut kind), PCDOCK_ITEM);
        assert_eq!(kind, PCDOCK_EV_AUDIO_CONFIG);
        let (mut codec, mut rate, mut ch) = (0u8, 0u32, 0u8);
        assert_eq!(pcdock_event_audio_config(p, &mut codec, &mut rate, &mut ch), PCDOCK_OK);
        assert_eq!((codec, rate, ch), (1, 48000, 2));
        pcdock_session_free(p);
    }

    #[test]
    fn null_and_small_buffers_are_errors_not_crashes() {
        assert_eq!(pcdock_session_start(std::ptr::null_mut(), 0), PCDOCK_ERR_NULL);
        assert_eq!(pcdock_session_poll_event(std::ptr::null_mut(), std::ptr::null_mut()), PCDOCK_ERR_NULL);
        let p = new();
        let mut kind = 0u32;
        assert_eq!(pcdock_session_poll_event(p, &mut kind), PCDOCK_OK); // пусто
        assert_eq!(pcdock_session_touch(p, 0, 99, 0.0, 0.0), PCDOCK_ERR_ARG);
        assert_eq!(pcdock_session_device_enable(p, 7, true), PCDOCK_ERR_ARG);
        let mut tiny = [0u8; 2];
        let mut len = tiny.len();
        assert_eq!(pcdock_aoa_string(0, tiny.as_mut_ptr(), &mut len), PCDOCK_ERR_SMALL_BUF);
        assert!(len > 2);
        assert_eq!(pcdock_aoa_string(6, tiny.as_mut_ptr(), &mut len), PCDOCK_OK); // нет такой
        pcdock_session_free(p);
    }
}
