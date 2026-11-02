//! AOA (Android Open Accessory) и HID: запросы управления, дескрипторы и отчёты.
//!
//! Ядро не выполняет USB-запросы само: оно формирует [`ControlRequest`], а хост-адаптер
//! выполняет их как vendor control transfer на устройстве (получатель: устройство).

pub const AOA_GET_PROTOCOL: u8 = 51;
pub const AOA_SEND_STRING: u8 = 52;
pub const AOA_START: u8 = 53;
pub const AOA_REGISTER_HID: u8 = 54;
pub const AOA_UNREGISTER_HID: u8 = 55;
pub const AOA_SET_HID_REPORT_DESC: u8 = 56;
pub const AOA_SEND_HID_EVENT: u8 = 57;

/// Максимальный размер данных в одном запросе `SET_HID_REPORT_DESC`.
pub const HID_DESC_CHUNK: usize = 4096;

pub const COORD_MAX: u16 = 32767;
pub const PRESSURE_MAX: u16 = 1023;
pub const REL_MAX: i32 = 32767;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    /// Из устройства в хост (ответ читает адаптер).
    In,
    /// Из хоста в устройство.
    Out,
}

/// USB control transfer: тип vendor, получатель device.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ControlRequest {
    pub direction: Direction,
    pub request: u8,
    pub value: u16,
    pub index: u16,
    /// Данные для `Direction::Out`.
    pub data: Vec<u8>,
    /// Сколько байт ждать для `Direction::In`.
    pub length: u16,
}

impl ControlRequest {
    pub fn out(request: u8, value: u16, index: u16, data: Vec<u8>) -> Self {
        Self { direction: Direction::Out, request, value, index, data, length: 0 }
    }

    pub fn input(request: u8, value: u16, index: u16, length: u16) -> Self {
        Self { direction: Direction::In, request, value, index, data: Vec::new(), length }
    }
}

/// Строки, которыми ПК представляется телефону. Должны совпадать с фильтром приложения-приёмника.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AoaStrings {
    pub manufacturer: String,
    pub model: String,
    pub description: String,
    pub version: String,
    pub uri: String,
    pub serial: String,
}

impl Default for AoaStrings {
    fn default() -> Self {
        Self {
            manufacturer: "PCDock".into(),
            model: "DeviceMedia".into(),
            description: "Screen Stream".into(),
            version: "1.0".into(),
            uri: "https://pc-dock.local".into(),
            serial: "0001".into(),
        }
    }
}

/// Запрос версии протокола AOA на телефоне (ответ: 2 байта little-endian).
pub fn get_protocol_request() -> ControlRequest {
    ControlRequest::input(AOA_GET_PROTOCOL, 0, 0, 2)
}

pub fn parse_protocol(response: &[u8]) -> Option<u16> {
    match response {
        [lo, hi, ..] => Some(u16::from_le_bytes([*lo, *hi])),
        _ => None,
    }
}

/// Строки и команда START: после них телефон переподключается в режиме аксессуара.
pub fn start_requests(strings: &AoaStrings) -> Vec<ControlRequest> {
    let all = [
        &strings.manufacturer,
        &strings.model,
        &strings.description,
        &strings.version,
        &strings.uri,
        &strings.serial,
    ];
    let mut out: Vec<ControlRequest> = all
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let mut data = s.as_bytes().to_vec();
            data.push(0);
            ControlRequest::out(AOA_SEND_STRING, 0, i as u16, data)
        })
        .collect();
    out.push(ControlRequest::out(AOA_START, 0, 0, Vec::new()));
    out
}

/// Дескриптор многоточечного тачскрина в стиле Windows 8 (один контакт на отчёт).
///
/// Именно такой дескриптор заставляет ядро Linux подключить `hid-multitouch`: устройство получает
/// признак «прямое касание», и Android ведёт его как сенсорный экран, а не как тачпад.
pub fn touch_descriptor(max_contacts: u8) -> Vec<u8> {
    vec![
        0x05,
        0x0D,
        0x09,
        0x04,
        0xA1,
        0x01, // Digitizer, Touch Screen, Application
        0x85,
        0x01, // Report ID 1
        0x09,
        0x22,
        0xA1,
        0x02, // Finger, Logical collection
        0x09,
        0x42,
        0x15,
        0x00,
        0x25,
        0x01,
        0x75,
        0x01,
        0x95,
        0x01,
        0x81,
        0x02, // Tip Switch
        0x75,
        0x01,
        0x95,
        0x07,
        0x81,
        0x03, // выравнивание
        0x09,
        0x51,
        0x15,
        0x00,
        0x26,
        0xFF,
        0x00,
        0x75,
        0x08,
        0x95,
        0x01,
        0x81,
        0x02, // Contact ID
        0x05,
        0x01,
        0x09,
        0x30,
        0x15,
        0x00,
        0x26,
        0xFF,
        0x7F,
        0x75,
        0x10,
        0x95,
        0x01,
        0x81,
        0x02, // X
        0x09,
        0x31,
        0x81,
        0x02, // Y
        0x05,
        0x0D,
        0x09,
        0x48,
        0x81,
        0x02, // Width
        0x09,
        0x49,
        0x81,
        0x02, // Height
        0x09,
        0x30,
        0x26,
        0xFF,
        0x03,
        0x81,
        0x02, // Tip Pressure 0..1023
        0xC0, //
        0x05,
        0x0D,
        0x09,
        0x54,
        0x15,
        0x00,
        0x25,
        0x7F,
        0x75,
        0x08,
        0x95,
        0x01,
        0x81,
        0x02, // Contact Count
        0x85,
        0x02,
        0x09,
        0x55,
        0x25,
        max_contacts,
        0x75,
        0x08,
        0x95,
        0x01,
        0xB1,
        0x02, // Contact Count Maximum
        0x06,
        0x00,
        0xFF,
        0x09,
        0xC5,
        0x85,
        0x03,
        0x15,
        0x00,
        0x26,
        0xFF,
        0x00,
        0x75,
        0x08, //
        0x96,
        0x00,
        0x01,
        0xB1,
        0x02, // сертификационный блок Windows 8 (feature, 256 байт)
        0xC0,
    ]
}

/// Дескриптор клавиатуры: стандартный отчёт на 8 байт.
pub fn keyboard_descriptor() -> Vec<u8> {
    vec![
        0x05, 0x01, 0x09, 0x06, 0xA1, 0x01, // Generic Desktop, Keyboard
        0x05, 0x07, 0x19, 0xE0, 0x29, 0xE7, // модификаторы E0..E7
        0x15, 0x00, 0x25, 0x01, 0x75, 0x01, 0x95, 0x08, 0x81, 0x02, //
        0x95, 0x01, 0x75, 0x08, 0x81, 0x03, // резервный байт
        0x95, 0x05, 0x75, 0x01, 0x05, 0x08, 0x19, 0x01, 0x29, 0x05, 0x91, 0x02, // LED (выход)
        0x95, 0x01, 0x75, 0x03, 0x91, 0x03, //
        0x95, 0x06, 0x75, 0x08, 0x15, 0x00, 0x25, 0x65, //
        0x05, 0x07, 0x19, 0x00, 0x29, 0x65, 0x81, 0x00, // до 6 клавиш
        0xC0,
    ]
}

/// Дескриптор мыши: 3 кнопки, относительные X/Y (16 бит со знаком), колесо В (8 бит),
/// колесо Г через Consumer AC Pan (8 бит; ядро Linux маппит в REL_HWHEEL).
pub fn mouse_descriptor() -> Vec<u8> {
    vec![
        0x05, 0x01, 0x09, 0x02, 0xA1, 0x01, 0x09, 0x01, 0xA1, 0x00, //
        0x05, 0x09, 0x19, 0x01, 0x29, 0x03, 0x15, 0x00, 0x25, 0x01, //
        0x95, 0x03, 0x75, 0x01, 0x81, 0x02, // 3 кнопки
        0x95, 0x01, 0x75, 0x05, 0x81, 0x03, // выравнивание
        0x05, 0x01, 0x09, 0x30, 0x09, 0x31, //
        0x16, 0x01, 0x80, 0x26, 0xFF, 0x7F, // -32767..32767
        0x75, 0x10, 0x95, 0x02, 0x81, 0x06, // X, Y относительные
        0x09, 0x38, 0x15, 0x81, 0x25, 0x7F, //
        0x75, 0x08, 0x95, 0x01, 0x81, 0x06, // колесо (вертикаль)
        0x05, 0x0C, // Usage Page (Consumer)
        0x0A, 0x38, 0x02, // Usage (AC Pan: горизонталь)
        0x15, 0x81, 0x25, 0x7F, // -127..127
        0x75, 0x08, 0x95, 0x01, 0x81, 0x06, // относительное
        0xC0, 0xC0,
    ]
}

/// Один контакт в единицах HID.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Contact {
    pub x: u16,
    pub y: u16,
    pub width: u16,
    pub height: u16,
    pub pressure: u16,
}

/// Отчёт тачскрина (14 байт, Report ID 1; числа внутри HID-отчёта little-endian).
pub fn touch_report(tip: bool, contact_id: u8, c: &Contact, count: u8) -> [u8; 14] {
    let mut r = [0u8; 14];
    r[0] = 1;
    r[1] = u8::from(tip);
    r[2] = contact_id;
    for (i, v) in [c.x, c.y, c.width, c.height, c.pressure].into_iter().enumerate() {
        r[3 + i * 2..5 + i * 2].copy_from_slice(&v.to_le_bytes());
    }
    r[13] = count;
    r
}

/// Отчёт клавиатуры (8 байт): модификаторы, резерв, до 6 клавиш.
pub fn keyboard_report(modifiers: u8, keys: &[u8]) -> [u8; 8] {
    let mut r = [0u8; 8];
    r[0] = modifiers;
    for (i, k) in keys.iter().take(6).enumerate() {
        r[2 + i] = *k;
    }
    r
}

/// Отчёт мыши (7 байт): кнопки, dx, dy (16 бит со знаком), колесо В, колесо Г (8 бит).
/// Горизонталь — Consumer AC Pan: ядро Linux маппит его в REL_HWHEEL.
#[allow(clippy::too_many_arguments)]
pub fn mouse_report(buttons: u8, dx: i32, dy: i32, wheel_v: i32, wheel_h: i32) -> [u8; 7] {
    let dx = dx.clamp(-REL_MAX, REL_MAX) as i16;
    let dy = dy.clamp(-REL_MAX, REL_MAX) as i16;
    let wv = wheel_v.clamp(-127, 127) as i8;
    let wh = wheel_h.clamp(-127, 127) as i8;
    let (x, y) = (dx.to_le_bytes(), dy.to_le_bytes());
    [buttons & 0x07, x[0], x[1], y[0], y[1], wv as u8, wh as u8]
}

/// Переводит точку отображаемой картинки (доли 0..1) в координаты тачскрина «по корпусу телефона».
///
/// Android привязывает виртуальный тачскрин к корпусу и сам поворачивает его координаты вместе
/// с экраном (измерено на Redmi A5, Android 15), поэтому нужно обратное преобразование.
/// `rotation`: 0..3 = поворот экрана на 0°, 90°, 180°, 270°.
pub fn to_device_coords(rotation: u8, x: f32, y: f32) -> (f32, f32) {
    match rotation {
        1 => (1.0 - y, x),
        2 => (1.0 - x, 1.0 - y),
        3 => (y, 1.0 - x),
        _ => (x, y),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descriptor_sizes_match_the_working_python_prototype() {
        assert_eq!(keyboard_descriptor().len(), 63);
        assert_eq!(mouse_descriptor().len(), 79); // +15 байт: Consumer AC Pan (горизонталь)
        let t = touch_descriptor(10);
        assert!(
            t.windows(3).any(|w| w == [0x25, 10, 0x75]),
            "максимум контактов должен попасть в дескриптор"
        );
        assert_ne!(touch_descriptor(5), t);
    }

    #[test]
    fn touch_report_layout() {
        // Те же значения, что в проверке на телефоне: X=3277, Y=9830, 400x400, сила 300.
        let c = Contact { x: 3277, y: 9830, width: 400, height: 400, pressure: 300 };
        assert_eq!(
            touch_report(true, 0, &c, 1),
            [1, 1, 0, 0xCD, 0x0C, 0x66, 0x26, 0x90, 0x01, 0x90, 0x01, 0x2C, 0x01, 1]
        );
    }

    #[test]
    fn keyboard_and_mouse_reports() {
        assert_eq!(keyboard_report(0x02, &[4, 5]), [0x02, 0, 4, 5, 0, 0, 0, 0]);
        assert_eq!(mouse_report(1, -1, 2, -3, 4), [1, 0xFF, 0xFF, 2, 0, 0xFD, 4]);
        assert_eq!(mouse_report(0, -32767, 32767, 0, 0), [0, 0x01, 0x80, 0xFF, 0x7F, 0, 0]);
    }

    #[test]
    fn aoa_handshake_requests() {
        let reqs = start_requests(&AoaStrings::default());
        assert_eq!(reqs.len(), 7);
        assert_eq!(reqs[0].request, AOA_SEND_STRING);
        assert_eq!(reqs[0].index, 0);
        assert_eq!(reqs[0].data, b"PCDock\0");
        assert_eq!(reqs[5].index, 5);
        assert_eq!(reqs[6].request, AOA_START);
        assert_eq!(parse_protocol(&[2, 0]), Some(2));
        assert_eq!(parse_protocol(&[2]), None);
    }

    /// Измерено на телефоне (Chrome, все четыре поворота): отправили (fx, fy), точка легла в (X, Y) картинки.
    #[test]
    fn rotation_mapping_matches_measurements_on_the_phone() {
        type Point = (f32, f32);
        let measured: [(u8, Point, Point); 5] = [
            (0, (0.1, 0.3), (0.099, 0.300)),
            (1, (0.1, 0.3), (0.300, 0.899)),
            (1, (0.4, 0.3), (0.300, 0.599)),
            (2, (0.1, 0.3), (0.899, 0.700)),
            (3, (0.4, 0.3), (0.700, 0.399)),
        ];
        for (rot, sent, shown) in measured {
            let got = to_device_coords(rot, shown.0, shown.1);
            assert!(
                (got.0 - sent.0).abs() < 0.005 && (got.1 - sent.1).abs() < 0.005,
                "rot={rot}: {got:?} != {sent:?}"
            );
        }
    }
}
