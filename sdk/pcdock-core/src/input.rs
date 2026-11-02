//! Ввод: тачскрин, клавиатура и мышь. Состояние и преобразование команд в HID-запросы.
//!
//! Команды фронтенда (`touch`, `cursor_move_abs`, `key`, ...) превращаются в [`ControlRequest`],
//! которые хост-адаптер выполняет на USB-устройстве. Ядро ничего не отправляет само.

use crate::hid::*;
use std::collections::BTreeMap;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DeviceKind {
    Touch,
    Mouse,
    Keyboard,
}

impl DeviceKind {
    pub const ALL: [DeviceKind; 3] = [DeviceKind::Touch, DeviceKind::Mouse, DeviceKind::Keyboard];

    fn idx(self) -> usize {
        match self {
            Self::Touch => 0,
            Self::Mouse => 1,
            Self::Keyboard => 2,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TouchPhase {
    Down,
    Move,
    Up,
    Cancel,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
}

impl MouseButton {
    fn mask(self) -> u8 {
        match self {
            Self::Left => 1,
            Self::Right => 2,
            Self::Middle => 4,
        }
    }
}

/// Касание. Все числа в долях 0.0..1.0 отображаемой картинки (с учётом ориентации).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TouchPoint {
    pub id: u8,
    pub phase: TouchPhase,
    pub x: f32,
    pub y: f32,
    /// Размер пятна касания (доли ширины экрана). `None` = значение по умолчанию из конфигурации.
    pub width: Option<f32>,
    pub height: Option<f32>,
    /// Сила 0.0..1.0. `None` = значение по умолчанию.
    pub pressure: Option<f32>,
}

impl TouchPoint {
    pub fn new(id: u8, phase: TouchPhase, x: f32, y: f32) -> Self {
        Self { id, phase, x, y, width: None, height: None, pressure: None }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InputError {
    /// Устройство отключено командой `set_enabled` или не включено в конфигурации.
    DeviceDisabled(DeviceKind),
    /// Номер пальца вне диапазона `0..max_contacts`.
    InvalidContact(u8),
    /// Неизвестный код клавиши (допустимы USB HID usage 0x01..=0x65 и модификаторы 0xE0..=0xE7).
    InvalidKey(u16),
    /// Для абсолютного положения курсора нужен размер экрана (его присылает телефон в `DisplayInfo`).
    NoScreenSize,
    /// Поворот должен быть 0..3.
    InvalidRotation(u8),
}

impl std::fmt::Display for InputError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DeviceDisabled(k) => write!(f, "устройство ввода {k:?} отключено"),
            Self::InvalidContact(id) => write!(f, "недопустимый номер пальца {id}"),
            Self::InvalidKey(k) => write!(f, "недопустимый код клавиши {k:#x}"),
            Self::NoScreenSize => write!(f, "размер экрана телефона ещё неизвестен"),
            Self::InvalidRotation(r) => write!(f, "недопустимый поворот {r}"),
        }
    }
}

impl std::error::Error for InputError {}

/// Настройки HID-устройств. Идентификаторы и дескрипторы задаёт пользователь библиотеки.
#[derive(Clone, Debug, PartialEq)]
pub struct HidConfig {
    pub touch_id: u8,
    pub keyboard_id: u8,
    pub mouse_id: u8,
    /// Сколько пальцев поддерживает тачскрин (1..=10).
    pub max_contacts: u8,
    /// Размер пятна касания по умолчанию (доли ширины экрана).
    pub default_touch_size: f32,
    /// Сила касания по умолчанию.
    pub default_pressure: f32,
    /// Во сколько раз Android смещает курсор больше, чем мы просим (поправка на ускорение указателя).
    pub mouse_gain: f32,
    /// Какие устройства регистрировать при подключении.
    pub enabled: [bool; 3],
}

impl Default for HidConfig {
    fn default() -> Self {
        Self {
            touch_id: 1,
            keyboard_id: 2,
            mouse_id: 3,
            max_contacts: 10,
            default_touch_size: 0.04,
            default_pressure: 0.6,
            mouse_gain: 1.0,
            enabled: [true, true, true],
        }
    }
}

#[derive(Debug)]
pub struct Input {
    cfg: HidConfig,
    enabled: [bool; 3],
    registered: [bool; 3],
    connected: bool,
    rotation: u8,
    screen: (u32, u32),
    contacts: BTreeMap<u8, Contact>,
    kb_modifiers: u8,
    kb_keys: Vec<u8>,
    mouse_buttons: u8,
    mouse_pos: Option<(i32, i32)>,
    wheel_remainder: f32,
    wheel_remainder_h: f32,
}

fn scale(v: f32, max: u16) -> u16 {
    (v.clamp(0.0, 1.0) * f32::from(max)) as u16
}

impl Input {
    pub fn new(cfg: HidConfig) -> Self {
        let enabled = cfg.enabled;
        Self {
            cfg,
            enabled,
            registered: [false; 3],
            connected: false,
            rotation: 0,
            screen: (0, 0),
            contacts: BTreeMap::new(),
            kb_modifiers: 0,
            kb_keys: Vec::new(),
            mouse_buttons: 0,
            mouse_pos: None,
            wheel_remainder: 0.0,
            wheel_remainder_h: 0.0,
        }
    }

    pub fn is_enabled(&self, kind: DeviceKind) -> bool {
        self.enabled[kind.idx()]
    }

    pub fn is_registered(&self, kind: DeviceKind) -> bool {
        self.registered[kind.idx()]
    }

    pub fn rotation(&self) -> u8 {
        self.rotation
    }

    fn id_of(&self, kind: DeviceKind) -> u8 {
        match kind {
            DeviceKind::Touch => self.cfg.touch_id,
            DeviceKind::Mouse => self.cfg.mouse_id,
            DeviceKind::Keyboard => self.cfg.keyboard_id,
        }
    }

    fn descriptor_of(&self, kind: DeviceKind) -> Vec<u8> {
        match kind {
            DeviceKind::Touch => touch_descriptor(self.cfg.max_contacts),
            DeviceKind::Mouse => mouse_descriptor(),
            DeviceKind::Keyboard => keyboard_descriptor(),
        }
    }

    fn require(&self, kind: DeviceKind) -> Result<(), InputError> {
        if self.enabled[kind.idx()] && self.registered[kind.idx()] {
            Ok(())
        } else {
            Err(InputError::DeviceDisabled(kind))
        }
    }

    fn send(&self, kind: DeviceKind, report: &[u8], out: &mut Vec<ControlRequest>) {
        out.push(ControlRequest::out(AOA_SEND_HID_EVENT, u16::from(self.id_of(kind)), 0, report.to_vec()));
    }

    fn register(&mut self, kind: DeviceKind, out: &mut Vec<ControlRequest>) {
        let id = u16::from(self.id_of(kind));
        let desc = self.descriptor_of(kind);
        out.push(ControlRequest::out(AOA_REGISTER_HID, id, desc.len() as u16, Vec::new()));
        for (i, chunk) in desc.chunks(HID_DESC_CHUNK).enumerate() {
            out.push(ControlRequest::out(
                AOA_SET_HID_REPORT_DESC,
                id,
                (i * HID_DESC_CHUNK) as u16,
                chunk.to_vec(),
            ));
        }
        self.registered[kind.idx()] = true;
    }

    fn unregister(&mut self, kind: DeviceKind, out: &mut Vec<ControlRequest>) {
        out.push(ControlRequest::out(AOA_UNREGISTER_HID, u16::from(self.id_of(kind)), 0, Vec::new()));
        self.registered[kind.idx()] = false;
    }

    /// Телефон перешёл в режим аксессуара (или переподключился): состояние сбрасывается,
    /// включённые устройства регистрируются заново.
    pub fn start(&mut self, out: &mut Vec<ControlRequest>) {
        self.connected = true;
        self.registered = [false; 3];
        self.contacts.clear();
        self.kb_modifiers = 0;
        self.kb_keys.clear();
        self.mouse_buttons = 0;
        self.mouse_pos = None;
        self.wheel_remainder = 0.0;
        self.wheel_remainder_h = 0.0;
        for kind in DeviceKind::ALL {
            if self.enabled[kind.idx()] {
                self.register(kind, out);
            }
        }
    }

    /// Связь с телефоном потеряна. Ничего не отправляется: устройство уже недоступно.
    pub fn stop(&mut self) {
        self.connected = false;
        self.registered = [false; 3];
        self.contacts.clear();
        self.kb_modifiers = 0;
        self.kb_keys.clear();
        self.mouse_buttons = 0;
        self.mouse_pos = None;
    }

    /// Включает или отключает устройство. Отключение отпускает всё нажатое и снимает регистрацию на телефоне.
    pub fn set_enabled(&mut self, kind: DeviceKind, on: bool, out: &mut Vec<ControlRequest>) {
        let i = kind.idx();
        if self.enabled[i] == on {
            return;
        }
        if on {
            self.enabled[i] = true;
            if self.connected && !self.registered[i] {
                self.register(kind, out);
            }
        } else {
            if self.registered[i] {
                self.release_kind(kind, out);
                self.unregister(kind, out);
            }
            self.enabled[i] = false;
        }
    }

    fn release_kind(&mut self, kind: DeviceKind, out: &mut Vec<ControlRequest>) {
        match kind {
            DeviceKind::Touch => self.lift_all_contacts(out),
            DeviceKind::Keyboard => {
                if self.kb_modifiers != 0 || !self.kb_keys.is_empty() {
                    self.kb_modifiers = 0;
                    self.kb_keys.clear();
                    self.flush_keyboard(out);
                }
            }
            DeviceKind::Mouse => {
                if self.mouse_buttons != 0 {
                    self.mouse_buttons = 0;
                    self.send(DeviceKind::Mouse, &mouse_report(0, 0, 0, 0, 0), out);
                }
            }
        }
    }

    /// Отпускает всё нажатое: пальцы, клавиши, кнопки мыши (например, при потере фокуса окна).
    pub fn release_all(&mut self, out: &mut Vec<ControlRequest>) {
        for kind in DeviceKind::ALL {
            if self.registered[kind.idx()] {
                self.release_kind(kind, out);
            }
        }
    }

    /// Размер экрана телефона в пикселях в текущей ориентации (из `DisplayInfo`).
    pub fn set_screen(&mut self, width: u32, height: u32) {
        self.screen = (width, height);
        if let Some((x, y)) = self.mouse_pos {
            self.mouse_pos = Some((x.min(width as i32 - 1).max(0), y.min(height as i32 - 1).max(0)));
        }
    }

    /// Экран повернулся: активные касания отпускаются, дальше координаты пересчитываются по новому повороту.
    pub fn set_rotation(&mut self, rotation: u8, out: &mut Vec<ControlRequest>) -> Result<(), InputError> {
        if rotation > 3 {
            return Err(InputError::InvalidRotation(rotation));
        }
        if rotation != self.rotation {
            if self.registered[DeviceKind::Touch.idx()] {
                self.lift_all_contacts(out);
            }
            self.rotation = rotation;
        }
        Ok(())
    }

    // ----- тачскрин -----

    fn flush_touch(&self, lifted: Option<(u8, Contact)>, out: &mut Vec<ControlRequest>) {
        let mut frame: Vec<(u8, bool, Contact)> =
            self.contacts.iter().map(|(id, c)| (*id, true, *c)).collect();
        if let Some((id, c)) = lifted {
            frame.push((id, false, c));
        }
        let count = frame.len() as u8;
        for (i, (id, tip, c)) in frame.iter().enumerate() {
            let report = touch_report(*tip, *id, c, if i == 0 { count } else { 0 });
            self.send(DeviceKind::Touch, &report, out);
        }
    }

    fn lift_all_contacts(&mut self, out: &mut Vec<ControlRequest>) {
        while let Some((id, c)) = self.contacts.pop_first() {
            self.flush_touch(Some((id, c)), out);
        }
    }

    pub fn touch(&mut self, p: TouchPoint, out: &mut Vec<ControlRequest>) -> Result<(), InputError> {
        self.require(DeviceKind::Touch)?;
        if p.id >= self.cfg.max_contacts {
            return Err(InputError::InvalidContact(p.id));
        }
        let previous = self.contacts.get(&p.id).copied();
        let (x, y) = to_device_coords(self.rotation, p.x, p.y);
        let (w, h) =
            (p.width.unwrap_or(self.cfg.default_touch_size), p.height.unwrap_or(self.cfg.default_touch_size));
        // Пятно касания поворачивается вместе с телефоном: на 90° и 270° ширина и высота меняются местами.
        let (w, h) = if self.rotation % 2 == 1 { (h, w) } else { (w, h) };
        let contact = Contact {
            x: scale(x, COORD_MAX),
            y: scale(y, COORD_MAX),
            width: scale(w, COORD_MAX),
            height: scale(h, COORD_MAX),
            pressure: scale(p.pressure.unwrap_or(self.cfg.default_pressure), PRESSURE_MAX),
        };
        match p.phase {
            TouchPhase::Down => {
                self.contacts.insert(p.id, contact);
                self.flush_touch(None, out);
            }
            TouchPhase::Move => {
                if previous.is_some() {
                    self.contacts.insert(p.id, contact);
                    self.flush_touch(None, out);
                }
            }
            TouchPhase::Up | TouchPhase::Cancel => {
                if let Some(prev) = previous {
                    self.contacts.remove(&p.id);
                    // Палец поднимается там, где он был, но с актуальными размерами и силой.
                    let lifted = Contact { x: prev.x, y: prev.y, ..contact };
                    self.flush_touch(Some((p.id, lifted)), out);
                }
            }
        }
        Ok(())
    }

    // ----- клавиатура -----

    fn flush_keyboard(&self, out: &mut Vec<ControlRequest>) {
        self.send(DeviceKind::Keyboard, &keyboard_report(self.kb_modifiers, &self.kb_keys), out);
    }

    /// `usage` — код клавиши USB HID (страница Keyboard, совпадает со scancode SDL).
    pub fn key(&mut self, usage: u16, down: bool, out: &mut Vec<ControlRequest>) -> Result<(), InputError> {
        self.require(DeviceKind::Keyboard)?;
        match usage {
            0xE0..=0xE7 => {
                let bit = 1u8 << (usage - 0xE0);
                if down {
                    self.kb_modifiers |= bit;
                } else {
                    self.kb_modifiers &= !bit;
                }
            }
            0x01..=0x65 => {
                let code = usage as u8;
                if down {
                    if !self.kb_keys.contains(&code) && self.kb_keys.len() < 6 {
                        self.kb_keys.push(code);
                    }
                } else {
                    self.kb_keys.retain(|k| *k != code);
                }
            }
            _ => return Err(InputError::InvalidKey(usage)),
        }
        self.flush_keyboard(out);
        Ok(())
    }

    // ----- мышь -----

    fn move_pixels(&mut self, dx: i32, dy: i32, out: &mut Vec<ControlRequest>) {
        let gain = if self.cfg.mouse_gain > 0.0 { self.cfg.mouse_gain } else { 1.0 };
        let (mut rx, mut ry) = ((dx as f32 / gain).round() as i32, (dy as f32 / gain).round() as i32);
        while rx != 0 || ry != 0 {
            let (sx, sy) = (rx.clamp(-REL_MAX, REL_MAX), ry.clamp(-REL_MAX, REL_MAX));
            self.send(DeviceKind::Mouse, &mouse_report(self.mouse_buttons, sx, sy, 0, 0), out);
            rx -= sx;
            ry -= sy;
        }
    }

    /// Двигает курсор в точку (доли отображаемой картинки). При первом вызове «прижимает» его к
    /// углу, чтобы знать, откуда считать.
    pub fn cursor_move_abs(
        &mut self,
        x: f32,
        y: f32,
        out: &mut Vec<ControlRequest>,
    ) -> Result<(), InputError> {
        self.require(DeviceKind::Mouse)?;
        let (w, h) = self.screen;
        if w == 0 || h == 0 {
            return Err(InputError::NoScreenSize);
        }
        let tx = (x.clamp(0.0, 1.0) * (w - 1) as f32).round() as i32;
        let ty = (y.clamp(0.0, 1.0) * (h - 1) as f32).round() as i32;
        if self.mouse_pos.is_none() {
            self.send(DeviceKind::Mouse, &mouse_report(self.mouse_buttons, -REL_MAX, -REL_MAX, 0, 0), out);
            self.mouse_pos = Some((0, 0));
        }
        let (px, py) = self.mouse_pos.unwrap_or((0, 0));
        self.move_pixels(tx - px, ty - py, out);
        self.mouse_pos = Some((tx, ty));
        Ok(())
    }

    /// Относительное движение курсора в пикселях телефона.
    pub fn cursor_move_rel(
        &mut self,
        dx: i32,
        dy: i32,
        out: &mut Vec<ControlRequest>,
    ) -> Result<(), InputError> {
        self.require(DeviceKind::Mouse)?;
        self.move_pixels(dx, dy, out);
        if let Some((x, y)) = self.mouse_pos {
            let (w, h) = (self.screen.0 as i32, self.screen.1 as i32);
            let (nx, ny) = (x + dx, y + dy);
            self.mouse_pos =
                Some(if w > 0 && h > 0 { (nx.clamp(0, w - 1), ny.clamp(0, h - 1)) } else { (nx, ny) });
        }
        Ok(())
    }

    /// Забыть оценку положения курсора: при следующем абсолютном движении он снова прижмётся к углу.
    pub fn cursor_resync(&mut self) {
        self.mouse_pos = None;
    }

    pub fn mouse_button(
        &mut self,
        button: MouseButton,
        down: bool,
        out: &mut Vec<ControlRequest>,
    ) -> Result<(), InputError> {
        self.require(DeviceKind::Mouse)?;
        if down {
            self.mouse_buttons |= button.mask();
        } else {
            self.mouse_buttons &= !button.mask();
        }
        self.send(DeviceKind::Mouse, &mouse_report(self.mouse_buttons, 0, 0, 0, 0), out);
        Ok(())
    }

    /// Прокрутка в «щелчках» колеса (положительное значение = вверх). Дробные части накапливаются.
    pub fn scroll(&mut self, ticks: f32, out: &mut Vec<ControlRequest>) -> Result<(), InputError> {
        self.require(DeviceKind::Mouse)?;
        self.wheel_remainder += ticks;
        let whole = self.wheel_remainder.trunc();
        self.wheel_remainder -= whole;
        let mut left = whole as i32;
        while left != 0 {
            let step = left.clamp(-127, 127);
            self.send(DeviceKind::Mouse, &mouse_report(self.mouse_buttons, 0, 0, step, 0), out);
            left -= step;
        }
        Ok(())
    }

    /// Горизонтальная прокрутка (наклон колеса / жест трекпада). Дробные части накапливаются.
    /// В HID это Consumer AC Pan — ядро Linux маппит его в REL_HWHEEL.
    pub fn scroll_h(&mut self, ticks: f32, out: &mut Vec<ControlRequest>) -> Result<(), InputError> {
        self.require(DeviceKind::Mouse)?;
        self.wheel_remainder_h += ticks;
        let whole = self.wheel_remainder_h.trunc();
        self.wheel_remainder_h -= whole;
        let mut left = whole as i32;
        while left != 0 {
            let step = left.clamp(-127, 127);
            self.send(DeviceKind::Mouse, &mouse_report(self.mouse_buttons, 0, 0, 0, step), out);
            left -= step;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn connected() -> (Input, Vec<ControlRequest>) {
        let mut input = Input::new(HidConfig::default());
        let mut out = Vec::new();
        input.start(&mut out);
        out.clear();
        input.set_screen(720, 1640);
        (input, out)
    }

    fn sent_reports(out: &[ControlRequest]) -> Vec<&Vec<u8>> {
        out.iter().filter(|r| r.request == AOA_SEND_HID_EVENT).map(|r| &r.data).collect()
    }

    #[test]
    fn start_registers_enabled_devices_in_order() {
        let mut input = Input::new(HidConfig { enabled: [true, false, true], ..HidConfig::default() });
        let mut out = Vec::new();
        input.start(&mut out);
        let kinds: Vec<(u8, u16)> = out.iter().map(|r| (r.request, r.value)).collect();
        assert_eq!(
            kinds,
            vec![
                (AOA_REGISTER_HID, 1),
                (AOA_SET_HID_REPORT_DESC, 1),
                (AOA_REGISTER_HID, 2),
                (AOA_SET_HID_REPORT_DESC, 2),
            ]
        );
        assert_eq!(out[0].index as usize, touch_descriptor(10).len());
        assert!(input.is_registered(DeviceKind::Touch));
        assert!(!input.is_registered(DeviceKind::Mouse));
    }

    #[test]
    fn disabling_releases_state_then_unregisters_and_enabling_registers_again() {
        let (mut input, mut out) = connected();
        input.key(4, true, &mut out).unwrap();
        out.clear();
        input.set_enabled(DeviceKind::Keyboard, false, &mut out);
        assert_eq!(out[0].request, AOA_SEND_HID_EVENT);
        assert_eq!(out[0].data, keyboard_report(0, &[]).to_vec());
        assert_eq!(out[1].request, AOA_UNREGISTER_HID);
        assert_eq!(out[1].value, 2);
        assert_eq!(input.key(4, true, &mut out), Err(InputError::DeviceDisabled(DeviceKind::Keyboard)));
        out.clear();
        input.set_enabled(DeviceKind::Keyboard, true, &mut out);
        assert_eq!(out[0].request, AOA_REGISTER_HID);
        assert!(input.key(4, true, &mut out).is_ok());
    }

    #[test]
    fn enabling_before_connection_registers_on_start() {
        let mut input = Input::new(HidConfig { enabled: [false; 3], ..HidConfig::default() });
        let mut out = Vec::new();
        input.set_enabled(DeviceKind::Mouse, true, &mut out);
        assert!(out.is_empty());
        input.start(&mut out);
        assert_eq!(out[0].value, 3);
    }

    #[test]
    fn single_touch_report_matches_prototype_values() {
        let (mut input, mut out) = connected();
        let p = TouchPoint {
            width: Some(0.04),
            height: Some(0.04),
            pressure: Some(0.7),
            ..TouchPoint::new(0, TouchPhase::Down, 0.5, 0.25)
        };
        input.touch(p, &mut out).unwrap();
        let reports = sent_reports(&out);
        assert_eq!(reports.len(), 1);
        let r = reports[0];
        assert_eq!(&r[..3], &[1, 1, 0]);
        assert_eq!(u16::from_le_bytes([r[3], r[4]]), 16383); // 0.5 * 32767
        assert_eq!(u16::from_le_bytes([r[5], r[6]]), 8191); // 0.25 * 32767
        assert_eq!(u16::from_le_bytes([r[11], r[12]]), 716); // 0.7 * 1023
        assert_eq!(r[13], 1);
    }

    #[test]
    fn two_fingers_report_all_active_contacts_with_count_in_first() {
        let (mut input, mut out) = connected();
        input.touch(TouchPoint::new(0, TouchPhase::Down, 0.3, 0.4), &mut out).unwrap();
        out.clear();
        input.touch(TouchPoint::new(1, TouchPhase::Down, 0.7, 0.4), &mut out).unwrap();
        let reports = sent_reports(&out);
        assert_eq!(reports.len(), 2);
        assert_eq!((reports[0][2], reports[0][13]), (0, 2));
        assert_eq!((reports[1][2], reports[1][13]), (1, 0));
        out.clear();
        input.touch(TouchPoint::new(0, TouchPhase::Up, 0.0, 0.0), &mut out).unwrap();
        let reports = sent_reports(&out);
        assert_eq!(reports.len(), 2, "оставшийся палец и поднятый");
        assert_eq!(reports[0][1], 1, "второй палец всё ещё касается");
        assert_eq!(reports[1][1], 0, "первый поднят");
    }

    #[test]
    fn lifted_finger_keeps_its_last_position() {
        let (mut input, mut out) = connected();
        input.touch(TouchPoint::new(0, TouchPhase::Down, 0.5, 0.5), &mut out).unwrap();
        out.clear();
        input.touch(TouchPoint::new(0, TouchPhase::Up, 0.0, 0.0), &mut out).unwrap();
        let r = sent_reports(&out)[0].clone();
        assert_eq!(r[1], 0);
        assert_eq!(u16::from_le_bytes([r[3], r[4]]), 16383);
    }

    #[test]
    fn move_of_unknown_finger_is_ignored_and_bad_id_rejected() {
        let (mut input, mut out) = connected();
        input.touch(TouchPoint::new(2, TouchPhase::Move, 0.5, 0.5), &mut out).unwrap();
        assert!(out.is_empty());
        assert_eq!(
            input.touch(TouchPoint::new(10, TouchPhase::Down, 0.5, 0.5), &mut out),
            Err(InputError::InvalidContact(10))
        );
    }

    #[test]
    fn rotation_releases_contacts_and_maps_coordinates() {
        let (mut input, mut out) = connected();
        input.touch(TouchPoint::new(0, TouchPhase::Down, 0.5, 0.5), &mut out).unwrap();
        out.clear();
        input.set_rotation(1, &mut out).unwrap();
        assert_eq!(sent_reports(&out).len(), 1, "палец отпущен при повороте");
        out.clear();
        // На 90° точка картинки (0.3, 0.9) соответствует точке корпуса (0.1, 0.3)
        input.touch(TouchPoint::new(0, TouchPhase::Down, 0.3, 0.9), &mut out).unwrap();
        let r = sent_reports(&out)[0].clone();
        let (x, y) = (u16::from_le_bytes([r[3], r[4]]), u16::from_le_bytes([r[5], r[6]]));
        assert!((i32::from(x) - 3277).abs() < 40 && (i32::from(y) - 9830).abs() < 40, "x={x} y={y}");
        assert_eq!(input.set_rotation(4, &mut out), Err(InputError::InvalidRotation(4)));
    }

    #[test]
    fn keyboard_reports() {
        let (mut input, mut out) = connected();
        input.key(0xE1, true, &mut out).unwrap(); // левый Shift
        input.key(4, true, &mut out).unwrap(); // A
        input.key(4, false, &mut out).unwrap();
        input.key(0xE1, false, &mut out).unwrap();
        let r: Vec<Vec<u8>> = sent_reports(&out).into_iter().cloned().collect();
        assert_eq!(r[0], vec![0x02, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(r[1], vec![0x02, 0, 4, 0, 0, 0, 0, 0]);
        assert_eq!(r[2], vec![0x02, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(r[3], vec![0, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(input.key(0x200, true, &mut out), Err(InputError::InvalidKey(0x200)));
    }

    #[test]
    fn keyboard_limits_to_six_keys() {
        let (mut input, mut out) = connected();
        for k in 4..=11u16 {
            input.key(k, true, &mut out).unwrap();
        }
        assert_eq!(sent_reports(&out).last().unwrap()[2..], [4, 5, 6, 7, 8, 9]);
    }

    #[test]
    fn mouse_first_absolute_move_homes_the_cursor_then_moves() {
        let (mut input, mut out) = connected();
        input.cursor_move_abs(0.5, 0.5, &mut out).unwrap();
        let r = sent_reports(&out);
        assert_eq!(r.len(), 2);
        assert_eq!(*r[0], mouse_report(0, -REL_MAX, -REL_MAX, 0, 0).to_vec());
        assert_eq!(*r[1], mouse_report(0, 360, 820, 0, 0).to_vec()); // (719 * 0.5).round(), (1639 * 0.5).round()
        out.clear();
        input.cursor_move_abs(0.5, 0.5, &mut out).unwrap();
        assert!(out.is_empty(), "тот же пиксель: ничего слать не нужно");
        input.cursor_move_abs(0.0, 0.5, &mut out).unwrap();
        assert_eq!(*sent_reports(&out)[0], mouse_report(0, -360, 0, 0, 0).to_vec());
    }

    #[test]
    fn mouse_needs_screen_size_and_resync_homes_again() {
        let mut input = Input::new(HidConfig::default());
        let mut out = Vec::new();
        input.start(&mut out);
        out.clear();
        assert_eq!(input.cursor_move_abs(0.5, 0.5, &mut out), Err(InputError::NoScreenSize));
        input.set_screen(100, 100);
        input.cursor_move_abs(1.0, 1.0, &mut out).unwrap();
        out.clear();
        input.cursor_resync();
        input.cursor_move_abs(1.0, 1.0, &mut out).unwrap();
        assert_eq!(sent_reports(&out).len(), 2);
    }

    #[test]
    fn mouse_gain_compensates_pointer_acceleration() {
        let mut input = Input::new(HidConfig { mouse_gain: 2.0, ..HidConfig::default() });
        let mut out = Vec::new();
        input.start(&mut out);
        input.set_screen(1000, 1000);
        out.clear();
        input.cursor_move_rel(100, -50, &mut out).unwrap();
        assert_eq!(*sent_reports(&out)[0], mouse_report(0, 50, -25, 0, 0).to_vec());
    }

    #[test]
    fn mouse_buttons_and_scroll() {
        let (mut input, mut out) = connected();
        input.mouse_button(MouseButton::Right, true, &mut out).unwrap();
        input.mouse_button(MouseButton::Right, false, &mut out).unwrap();
        let r = sent_reports(&out);
        assert_eq!(r[0][0], 2);
        assert_eq!(r[1][0], 0);
        out.clear();
        input.scroll(0.4, &mut out).unwrap();
        assert!(out.is_empty());
        input.scroll(0.7, &mut out).unwrap();
        assert_eq!(sent_reports(&out)[0][5], 1, "накопили целый щелчок");
        out.clear();
        // Остаток после предыдущих прокруток 0.1 (0.4 + 0.7 - 1), поэтому -3.0 дают -2.9 и один щелчок -2.
        input.scroll(-3.0, &mut out).unwrap();
        assert_eq!(sent_reports(&out)[0][5], 0xFE);
        out.clear();
        input.scroll(-0.9, &mut out).unwrap(); // остаток -0.9 + (-0.9) = -1.8 -> ещё щелчок -1
        assert_eq!(sent_reports(&out)[0][5], 0xFF);
    }

    #[test]
    fn mouse_horizontal_scroll_uses_ac_pan_byte() {
        let (mut input, mut out) = connected();
        input.scroll_h(0.4, &mut out).unwrap();
        assert!(out.is_empty());
        input.scroll_h(0.7, &mut out).unwrap();
        let r = sent_reports(&out);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0][5], 0, "вертикаль не тронута");
        assert_eq!(r[0][6], 1, "горизонталь ушла в 7-й байт");
        out.clear();
        input.scroll_h(-2.0, &mut out).unwrap(); // остаток 0.1 - 2.0 = -1.9 -> щелчок -1
        assert_eq!(sent_reports(&out)[0][6], 0xFF);
    }

    #[test]
    fn release_all_lifts_everything() {
        let (mut input, mut out) = connected();
        input.touch(TouchPoint::new(0, TouchPhase::Down, 0.5, 0.5), &mut out).unwrap();
        input.key(4, true, &mut out).unwrap();
        input.mouse_button(MouseButton::Left, true, &mut out).unwrap();
        out.clear();
        input.release_all(&mut out);
        assert_eq!(sent_reports(&out).len(), 3);
    }

    #[test]
    fn stop_clears_state_without_sending() {
        let (mut input, mut out) = connected();
        input.touch(TouchPoint::new(0, TouchPhase::Down, 0.5, 0.5), &mut out).unwrap();
        input.stop();
        assert!(!input.is_registered(DeviceKind::Touch));
        out.clear();
        input.start(&mut out);
        assert!(out.iter().all(|r| r.request != AOA_SEND_HID_EVENT));
    }
}
