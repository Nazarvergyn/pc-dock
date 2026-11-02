//! USB-адаптер PC Dock: соединяет ядро (`pcdock-core`) с телефоном через libusb.
//!
//! Адаптер знает про USB и время, ядро нет. Он:
//! - находит телефон и выводит его в режим аксессуара (AOA handshake);
//! - открывает bulk-канал и запускает сессию ядра;
//! - читает поток, отдаёт его ядру, исполняет то, что ядро просит отправить (байты и control transfer);
//! - при выходе возвращает телефон в обычный режим и сбрасывает USB, если телефон молчит.

use pcdock_core::hid::{self, AoaStrings, ControlRequest};
use pcdock_core::session::{Config, Event, Session, Transmit};
use rusb::{Context, Device, DeviceHandle, Recipient, RequestType, TransferType, UsbContext};
use std::fmt;
use std::thread::sleep;
use std::time::{Duration, Instant};

/// VID производителей, телефоны которых мы пробуем (Google, Samsung, HTC, Huawei, Xiaomi, ...).
pub const ANDROID_VIDS: [u16; 6] = [0x18D1, 0x04E8, 0x0BB4, 0x12D1, 0x2717, 0x22B8];
/// PID телефона в режиме аксессуара.
pub const ACCESSORY_PIDS: std::ops::RangeInclusive<u16> = 0x2D00..=0x2D05;

#[derive(Debug)]
pub enum Error {
    Usb(rusb::Error),
    /// Телефон не найден за отведённое время.
    NoDevice,
    /// Телефон не перешёл в режим аксессуара за отведённое время.
    HandshakeTimeout,
    /// Версия AOA на телефоне меньше 2: HID недоступен.
    AoaTooOld(u16),
    /// В интерфейсах телефона нет пары bulk IN/OUT.
    NoBulkEndpoints,
    /// Адаптер не подключён к телефону (вызовите `connect`).
    NotConnected,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Usb(e) => write!(f, "ошибка USB: {e}"),
            Self::NoDevice => write!(f, "телефон не найден"),
            Self::HandshakeTimeout => write!(f, "телефон не перешёл в режим аксессуара"),
            Self::AoaTooOld(v) => write!(f, "версия AOA {v} не поддерживает HID (нужна 2)"),
            Self::NoBulkEndpoints => write!(f, "не найдены bulk-эндпоинты аксессуара"),
            Self::NotConnected => write!(f, "нет подключения к телефону"),
        }
    }
}

impl std::error::Error for Error {}

impl From<rusb::Error> for Error {
    fn from(e: rusb::Error) -> Self {
        Self::Usb(e)
    }
}

/// Настройки адаптера (не протокола: это про USB и время).
#[derive(Clone, Debug)]
pub struct Settings {
    /// Какие VID считать телефонами.
    pub vendor_ids: Vec<u16>,
    /// Сколько ждать телефон при `connect`.
    pub connect_timeout: Duration,
    /// Сколько ждать перехода в режим аксессуара после handshake, прежде чем повторить его.
    pub handshake_grace: Duration,
    /// Таймаут одного чтения в `poll`: определяет, как часто фронтенд может обрабатывать свои события.
    pub read_timeout: Duration,
    /// Таймаут записи в bulk.
    pub write_timeout: Duration,
    /// Таймаут одного control transfer.
    pub control_timeout: Duration,
    /// Сбрасывать USB, когда телефон молчит (`Event::PeerSilent`): приложение-приёмник запускается
    /// системой только при переходе в режим аксессуара.
    pub reset_on_silence: bool,
    pub reset_min_interval: Duration,
    pub reset_max_tries: u32,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            vendor_ids: ANDROID_VIDS.to_vec(),
            connect_timeout: Duration::from_secs(40),
            handshake_grace: Duration::from_secs(5),
            read_timeout: Duration::from_millis(15),
            write_timeout: Duration::from_millis(1000),
            control_timeout: Duration::from_millis(2000),
            reset_on_silence: true,
            reset_min_interval: Duration::from_secs(60),
            reset_max_tries: 3,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Обычный режим: нужен AOA handshake.
    Normal,
    /// Режим аксессуара: можно открывать канал.
    Accessory,
}

/// Определяет, телефон ли это и в каком он режиме.
pub fn classify(vendor_ids: &[u16], vid: u16, pid: u16) -> Option<Mode> {
    if !vendor_ids.contains(&vid) {
        return None;
    }
    // Режим аксессуара у Google; у других производителей после handshake VID тоже становится Google.
    if vid == 0x18D1 && ACCESSORY_PIDS.contains(&pid) {
        Some(Mode::Accessory)
    } else {
        Some(Mode::Normal)
    }
}

fn is_gone(e: &rusb::Error) -> bool {
    matches!(e, rusb::Error::NoDevice | rusb::Error::Io | rusb::Error::Pipe | rusb::Error::NotFound)
}

fn find_phone(ctx: &Context, vendor_ids: &[u16]) -> Result<Option<(Device<Context>, Mode)>, Error> {
    let mut normal = None;
    for device in ctx.devices()?.iter() {
        let Ok(desc) = device.device_descriptor() else { continue };
        match classify(vendor_ids, desc.vendor_id(), desc.product_id()) {
            Some(Mode::Accessory) => return Ok(Some((device, Mode::Accessory))),
            Some(Mode::Normal) if normal.is_none() => normal = Some(device),
            _ => {}
        }
    }
    Ok(normal.map(|d| (d, Mode::Normal)))
}

fn control_in(
    handle: &DeviceHandle<Context>,
    req: &ControlRequest,
    timeout: Duration,
) -> rusb::Result<Vec<u8>> {
    let rt = rusb::request_type(rusb::Direction::In, RequestType::Vendor, Recipient::Device);
    let mut buf = vec![0u8; req.length as usize];
    let n = handle.read_control(rt, req.request, req.value, req.index, &mut buf, timeout)?;
    buf.truncate(n);
    Ok(buf)
}

fn control_out(handle: &DeviceHandle<Context>, req: &ControlRequest, timeout: Duration) -> rusb::Result<()> {
    let rt = rusb::request_type(rusb::Direction::Out, RequestType::Vendor, Recipient::Device);
    handle.write_control(rt, req.request, req.value, req.index, &req.data, timeout)?;
    Ok(())
}

/// Выполняет AOA handshake: проверяет версию протокола, отправляет строки и START.
fn aoa_handshake(device: &Device<Context>, strings: &AoaStrings, timeout: Duration) -> Result<(), Error> {
    let handle = device.open()?;
    let reply = control_in(&handle, &hid::get_protocol_request(), timeout)?;
    let version = hid::parse_protocol(&reply).unwrap_or(0);
    if version < 2 {
        return Err(Error::AoaTooOld(version));
    }
    for req in hid::start_requests(strings) {
        control_out(&handle, &req, timeout)?;
    }
    Ok(())
}

/// Открытый канал к телефону в режиме аксессуара.
struct Phone {
    handle: DeviceHandle<Context>,
    iface: u8,
    ep_in: u8,
    ep_out: u8,
}

fn open_accessory(device: &Device<Context>) -> Result<Phone, Error> {
    let config = device.active_config_descriptor()?;
    for interface in config.interfaces() {
        for desc in interface.descriptors() {
            let (mut ep_in, mut ep_out) = (None, None);
            for ep in desc.endpoint_descriptors() {
                if ep.transfer_type() != TransferType::Bulk {
                    continue;
                }
                match ep.direction() {
                    rusb::Direction::In => ep_in = ep_in.or(Some(ep.address())),
                    rusb::Direction::Out => ep_out = ep_out.or(Some(ep.address())),
                }
            }
            if let (Some(ep_in), Some(ep_out)) = (ep_in, ep_out) {
                let handle = device.open()?;
                let _ = handle.set_auto_detach_kernel_driver(true);
                let iface = desc.interface_number();
                handle.claim_interface(iface)?;
                return Ok(Phone { handle, iface, ep_in, ep_out });
            }
        }
    }
    Err(Error::NoBulkEndpoints)
}

/// Хост-адаптер: владеет USB-каналом и сессией ядра.
pub struct Dock {
    ctx: Context,
    settings: Settings,
    session: Session,
    phone: Option<Phone>,
    t0: Instant,
    last_handshake: Option<Instant>,
    last_reset: Option<Instant>,
    resets: u32,
    warnings: Vec<String>,
    shut_down: bool,
}

impl Dock {
    pub fn new(config: Config, settings: Settings) -> Result<Self, Error> {
        Ok(Self {
            ctx: Context::new()?,
            settings,
            session: Session::new(config),
            phone: None,
            t0: Instant::now(),
            last_handshake: None,
            last_reset: None,
            resets: 0,
            warnings: Vec::new(),
            shut_down: false,
        })
    }

    fn now_ms(&self) -> u64 {
        self.t0.elapsed().as_millis() as u64
    }

    pub fn is_connected(&self) -> bool {
        self.phone.is_some()
    }

    /// Сессия ядра: через неё отправляются команды ввода (`touch`, `key`, `cursor_move_abs`, ...).
    pub fn session(&mut self) -> &mut Session {
        &mut self.session
    }

    pub fn session_ref(&self) -> &Session {
        &self.session
    }

    /// Нефатальные проблемы (например, отказ телефона выполнить запрос), накопленные с прошлого вызова.
    pub fn take_warnings(&mut self) -> Vec<String> {
        std::mem::take(&mut self.warnings)
    }

    /// Находит телефон, при необходимости выводит его в режим аксессуара, открывает канал и запускает сессию.
    pub fn connect(&mut self) -> Result<(), Error> {
        if self.phone.is_some() {
            return Ok(());
        }
        self.shut_down = false;
        // После сброса USB старое устройство исчезает не мгновенно: даём ему пропасть, чтобы не открыть его.
        if let Some(t) = self.last_reset {
            if let Some(left) = Duration::from_millis(1500).checked_sub(t.elapsed()) {
                sleep(left);
            }
        }
        let deadline = Instant::now() + self.settings.connect_timeout;
        let mut saw_phone = false;
        loop {
            match find_phone(&self.ctx, &self.settings.vendor_ids)? {
                Some((device, Mode::Accessory)) => match open_accessory(&device) {
                    Ok(phone) => {
                        self.phone = Some(phone);
                        self.session.start(self.now_ms());
                        match self.flush() {
                            Ok(()) => return Ok(()),
                            // Это был ещё не исчезнувший узел старого соединения: откатываемся и ждём дальше.
                            Err(Error::Usb(e)) if is_gone(&e) => self.abandon_phone(),
                            Err(e) => {
                                self.abandon_phone();
                                return Err(e);
                            }
                        }
                        saw_phone = true;
                    }
                    // Телефон ещё переподключается: подождём и попробуем снова.
                    Err(Error::Usb(e)) if is_gone(&e) || matches!(e, rusb::Error::Busy) => saw_phone = true,
                    Err(e) => return Err(e),
                },
                Some((device, Mode::Normal)) => {
                    saw_phone = true;
                    let transitioning =
                        self.last_handshake.is_some_and(|t| t.elapsed() < self.settings.handshake_grace);
                    if !transitioning {
                        match aoa_handshake(
                            &device,
                            &self.session.config().aoa,
                            self.settings.control_timeout,
                        ) {
                            Ok(()) => self.last_handshake = Some(Instant::now()),
                            Err(Error::Usb(e)) if is_gone(&e) => {}
                            Err(e) => return Err(e),
                        }
                    }
                }
                None => {}
            }
            if Instant::now() >= deadline {
                return Err(if saw_phone { Error::HandshakeTimeout } else { Error::NoDevice });
            }
            sleep(Duration::from_millis(300));
        }
    }

    /// Один шаг цикла: отправляет накопленное, читает поток (не дольше `read_timeout`), обрабатывает таймеры.
    /// Возвращает события для фронтенда. При потере USB возвращается `Event::Disconnected`, а `is_connected()` = false.
    pub fn poll(&mut self) -> Result<Vec<Event>, Error> {
        if self.phone.is_none() {
            return Err(Error::NotConnected);
        }
        let mut events = Vec::new();
        if let Err(e) = self.step(&mut events) {
            match e {
                Error::Usb(ref u) if is_gone(u) => self.lost(&mut events),
                other => return Err(other),
            }
        }
        Ok(events)
    }

    fn step(&mut self, events: &mut Vec<Event>) -> Result<(), Error> {
        self.flush()?;
        let mut buf = [0u8; 65536];
        let read = match &self.phone {
            Some(p) => p.handle.read_bulk(p.ep_in, &mut buf, self.settings.read_timeout),
            None => return Ok(()),
        };
        let now = self.now_ms();
        match read {
            Ok(n) => self.session.handle_input(now, &buf[..n]),
            Err(rusb::Error::Timeout) => {}
            Err(e) => return Err(e.into()),
        }
        if self.session.next_timeout().is_some_and(|t| now >= t) {
            self.session.handle_timeout(now);
        }
        self.flush()?;
        while let Some(event) = self.session.poll_event() {
            match &event {
                Event::Connected { .. } => self.resets = 0,
                Event::PeerSilent => self.on_silent(),
                _ => {}
            }
            events.push(event);
        }
        Ok(())
    }

    /// Телефон молчит: возможно, приложение не запущено. Сбрасываем USB, чтобы телефон заново прошёл
    /// через режим аксессуара (в этот момент система запускает приложение).
    fn on_silent(&mut self) {
        if !self.settings.reset_on_silence || self.resets >= self.settings.reset_max_tries {
            return;
        }
        if self.last_reset.is_some_and(|t| t.elapsed() < self.settings.reset_min_interval) {
            return;
        }
        self.resets += 1;
        self.last_reset = Some(Instant::now());
        self.warnings.push(format!(
            "телефон не ответил на HELLO: сбрасываю USB (попытка {}/{})",
            self.resets, self.settings.reset_max_tries
        ));
        if let Some(p) = &self.phone {
            let _ = p.handle.reset();
        }
        // Дальнейшее чтение вернёт «нет устройства», и poll() переведёт адаптер в отключённое состояние.
    }

    /// Откатывает состояние после неудачного открытия: сессия закрывается, накопленное отбрасывается.
    fn abandon_phone(&mut self) {
        self.phone = None;
        self.last_handshake = None;
        self.session.close();
        while self.session.poll_event().is_some() {}
        while self.session.poll_transmit().is_some() {}
    }

    fn lost(&mut self, events: &mut Vec<Event>) {
        self.phone = None;
        self.last_handshake = None;
        self.session.close();
        while let Some(e) = self.session.poll_event() {
            events.push(e);
        }
        while self.session.poll_transmit().is_some() {}
    }

    /// Отправляет всё, что накопило ядро. Нефатальные ошибки складываются в предупреждения.
    fn flush(&mut self) -> Result<(), Error> {
        while let Some(t) = self.session.poll_transmit() {
            let Some(p) = &self.phone else { return Ok(()) };
            match t {
                Transmit::Stream(bytes) => {
                    let mut sent = 0;
                    while sent < bytes.len() {
                        match p.handle.write_bulk(p.ep_out, &bytes[sent..], self.settings.write_timeout) {
                            Ok(n) => sent += n,
                            Err(rusb::Error::Timeout) => {
                                self.warnings
                                    .push("запись в USB не завершилась за таймаут, данные отброшены".into());
                                break;
                            }
                            Err(e) => return Err(e.into()),
                        }
                    }
                }
                Transmit::Control(req) => {
                    let result = match req.direction {
                        hid::Direction::Out => control_out(&p.handle, &req, self.settings.control_timeout),
                        hid::Direction::In => {
                            control_in(&p.handle, &req, self.settings.control_timeout).map(|_| ())
                        }
                    };
                    match result {
                        Ok(()) => {}
                        Err(e) if is_gone(&e) && !matches!(e, rusb::Error::Pipe) => return Err(e.into()),
                        Err(e) => self.warnings.push(format!("запрос AOA {} отклонён: {e}", req.request)),
                    }
                }
            }
        }
        Ok(())
    }

    /// Аккуратное завершение: отпускает ввод, снимает HID-устройства с телефона и сбрасывает USB,
    /// чтобы телефон вернулся в обычный режим (иначе при следующем запуске приложение не стартует само).
    pub fn shutdown(&mut self) {
        if self.shut_down {
            return;
        }
        self.shut_down = true;
        if self.phone.is_some() {
            self.session.release_all();
            for kind in pcdock_core::input::DeviceKind::ALL {
                self.session.device_enable(kind, false);
            }
            let _ = self.flush();
        }
        if let Some(p) = self.phone.take() {
            let _ = p.handle.release_interface(p.iface);
            let _ = p.handle.reset();
        }
        self.session.close();
        // Если выходим сразу после handshake, телефон ещё переходит в режим аксессуара: дождёмся его.
        let wait_until = self.last_handshake.map(|t| t + Duration::from_secs(8));
        loop {
            if let Ok(Some((device, Mode::Accessory))) = find_phone(&self.ctx, &self.settings.vendor_ids) {
                if let Ok(handle) = device.open() {
                    let _ = handle.reset();
                }
                return;
            }
            if wait_until.is_none_or(|t| Instant::now() >= t) {
                return;
            }
            sleep(Duration::from_millis(500));
        }
    }
}

impl Drop for Dock {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_phone_modes() {
        let vids = ANDROID_VIDS;
        assert_eq!(classify(&vids, 0x18D1, 0x4EE7), Some(Mode::Normal));
        assert_eq!(classify(&vids, 0x18D1, 0x2D01), Some(Mode::Accessory));
        assert_eq!(classify(&vids, 0x18D1, 0x2D00), Some(Mode::Accessory));
        assert_eq!(classify(&vids, 0x2717, 0xFF48), Some(Mode::Normal));
        assert_eq!(classify(&vids, 0x1D6B, 0x0002), None, "хаб не телефон");
        assert_eq!(classify(&[0x1234], 0x18D1, 0x4EE7), None, "VID можно ограничить");
    }

    #[test]
    fn gone_errors() {
        assert!(is_gone(&rusb::Error::NoDevice));
        assert!(is_gone(&rusb::Error::Io));
        assert!(!is_gone(&rusb::Error::Timeout));
    }

    #[test]
    fn dock_reports_not_connected_before_connect() {
        let mut dock = Dock::new(Config::default(), Settings::default()).unwrap();
        assert!(!dock.is_connected());
        assert!(matches!(dock.poll(), Err(Error::NotConnected)));
    }
}
