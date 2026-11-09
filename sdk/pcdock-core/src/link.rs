//! Общий интерфейс транспорта одного телефона и команды ввода к нему.
//!
//! Транспорты (USB, ADB, serial) реализуют [`PhoneLink`]; ферма владеет ими и про тип
//! подключения не знает. Здесь только описание, ввода-вывода в ядре нет.

use crate::input::{DeviceKind, MouseButton, TouchPhase};
use crate::session::Event;

/// Идентификатор трубки. Назначает вызывающий.
pub type PhoneId = u32;

/// Команда ввода одной трубке. Человеческий темп (паузы между DOWN/MOVE/UP,
/// мелкие порции скролла) делает вызывающий; либа шлёт как есть.
#[derive(Clone, Debug)]
pub enum InputCmd {
    DeviceEnable {
        kind: DeviceKind,
        on: bool,
    },
    Touch {
        cid: u8,
        phase: TouchPhase,
        x: f32,
        y: f32,
        width: Option<f32>,
        height: Option<f32>,
        pressure: Option<f32>,
    },
    CursorMoveAbs {
        x: f32,
        y: f32,
    },
    CursorMoveRel {
        dx: i32,
        dy: i32,
    },
    CursorResync,
    MouseButton {
        button: MouseButton,
        down: bool,
    },
    Scroll {
        ticks: f32,
    },
    ScrollH {
        ticks: f32,
    },
    Key {
        usage: u16,
        down: bool,
    },
    ReleaseAll,
    RequestFrame,
    Wake,
    SetStream {
        video: bool,
        audio: bool,
    },
}

impl InputCmd {
    /// Касание с явными пятном/силой. `None` = default ядра.
    #[allow(clippy::too_many_arguments)]
    pub fn touch_ex(
        cid: u8,
        phase: TouchPhase,
        x: f32,
        y: f32,
        width: Option<f32>,
        height: Option<f32>,
        pressure: Option<f32>,
    ) -> Self {
        Self::Touch { cid, phase, x, y, width, height, pressure }
    }
}

/// Ошибка связи. `Gone` = устройство пропало, надо переподключать.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LinkError {
    Gone(String),
    Fatal(String),
}

impl std::fmt::Display for LinkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Gone(e) => write!(f, "устройство пропало: {e}"),
            Self::Fatal(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for LinkError {}

/// Связь с одной трубкой. Реализации: USB сегодня, ADB и serial-труба завтра.
/// Сессия ядра живёт внутри реализации; наружу идут готовые события.
pub trait PhoneLink: Send {
    fn is_connected(&self) -> bool;
    /// Найти устройство и поднять сессию. `Gone` = повторить позже.
    fn connect(&mut self) -> Result<(), LinkError>;
    /// Один шаг: отправить накопленное, прочитать входящее, вернуть события.
    fn poll(&mut self) -> Result<Vec<Event>, LinkError>;
    /// Применить команду ввода (уйдёт при следующем `poll`).
    fn input(&mut self, cmd: &InputCmd) -> Result<(), LinkError>;
    /// Аккуратно отпустить устройство.
    fn shutdown(&mut self);
}
