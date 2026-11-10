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
    /// Работать нельзя, пока человек что-то не сделает (см. [`PhoneState::hint`]).
    Blocked(PhoneState, String),
}

impl std::fmt::Display for LinkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Gone(e) => write!(f, "устройство пропало: {e}"),
            Self::Fatal(e) => write!(f, "{e}"),
            Self::Blocked(_, e) => write!(f, "{e}"),
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

/// Каким путём телефон подключён.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Transport {
    /// USB-аксессуар (AOA).
    Usb,
    /// ADB-сервер на телефоне.
    Adb,
    /// Последовательная труба от микроконтроллера.
    Serial,
}

impl std::fmt::Display for Transport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Usb => "usb",
            Self::Adb => "adb",
            Self::Serial => "serial",
        })
    }
}

/// Факт «этот телефон сейчас виден таким путём». Обнаружение выдаёт факты, решения принимает ферма.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Sighting {
    pub transport: Transport,
    /// Адрес в пределах транспорта: порт USB (`1-2.3`), серийник ADB, путь serial.
    pub address: String,
    /// Серийник телефона, если известен: по нему ферма узнаёт один и тот же телефон на разных транспортах.
    pub serial: Option<String>,
    /// Что мешает работать, если телефон виден, но пользоваться им нельзя.
    pub issue: Option<Issue>,
}

/// Почему виден телефон, но подключиться к нему пока нельзя.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Issue {
    /// Нет прав на USB-устройство (Linux: правило udev; Windows: драйвер WinUSB).
    NoAccess,
    /// ADB: компьютер не подтверждён на телефоне («Разрешить отладку»).
    Unauthorized,
}

impl Issue {
    pub fn state(self) -> PhoneState {
        match self {
            Self::NoAccess => PhoneState::NeedsAccess,
            Self::Unauthorized => PhoneState::NeedsAuth,
        }
    }
}

impl Sighting {
    /// Ключ телефона: серийник, а без него — путь транспорта.
    pub fn key(&self) -> String {
        match &self.serial {
            Some(s) => format!("serial:{s}"),
            None => format!("{}:{}", self.transport, self.address),
        }
    }
}

/// Источник фактов о телефонах (скан USB, опрос ADB…). Вызывается из потока владельца фермы.
pub trait Discovery: Send {
    fn scan(&mut self) -> Vec<Sighting>;
}

/// Создаёт канал к телефону по факту обнаружения.
pub trait LinkFactory: Send {
    fn make(&mut self, sighting: &Sighting) -> Result<Box<dyn PhoneLink>, String>;
}

/// Состояние телефона для потребителя.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PhoneState {
    /// Только что замечен.
    Appeared,
    /// Идёт подключение или переподключение.
    Connecting,
    /// Рукопожатие прошло.
    Ready,
    /// Приложение запущено, но захвата экрана нет (нужно разрешение на телефоне).
    NeedsPermission,
    /// Телефон перешёл в режим аксессуара, но приложение PC Dock не отвечает (не установлено или не запущено).
    NeedsApp,
    /// Нет прав на USB-устройство.
    NeedsAccess,
    /// ADB ждёт подтверждения компьютера на телефоне.
    NeedsAuth,
    /// Пропал с обнаружения.
    Lost,
}

impl PhoneState {
    /// Что сказать пользователю в этом состоянии (пусто, если ничего делать не нужно).
    pub fn hint(self) -> &'static str {
        match self {
            Self::NeedsPermission => "Разрешите запись экрана на телефоне (диалог «Начать запись»).",
            Self::NeedsApp => {
                "Телефон в режиме аксессуара, но приложение PC Dock не отвечает: оно не установлено, не запущено или \
                 ждёт ответа на экране (подтвердить запуск для аксессуара, разрешить запись экрана). Посмотрите на телефон."
            }
            Self::NeedsAccess => {
                "Нет доступа к USB-устройству: на Linux добавьте правило udev, на Windows поставьте драйвер WinUSB \
                 (см. sdk/README.md, «Первое подключение»)."
            }
            Self::NeedsAuth => {
                "Подтвердите на телефоне «Разрешить отладку по USB с этого компьютера» (отметьте «Всегда разрешать»)."
            }
            _ => "",
        }
    }
}
