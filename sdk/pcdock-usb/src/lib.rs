//! USB-адаптер PC Dock: соединяет ядро (`pcdock-core`) с телефоном через libusb.
//!
//! Адаптер знает про USB и время, ядро нет. Он:
//! - находит телефон и выводит его в режим аксессуара (AOA handshake);
//! - открывает bulk-канал и запускает сессию ядра;
//! - читает поток, отдаёт его ядру, исполняет то, что ядро просит отправить (байты и control transfer);
//! - при выходе возвращает телефон в обычный режим и сбрасывает USB, если телефон молчит.
//!
//! Телефонов на ПК может быть несколько: `Settings::target` выбирает, какой из них трогать
//! (по порту или серийнику), а `shutdown` сбрасывает только тот телефон, с которым работал этот `Dock`.

use pcdock_core::hid::{self, AoaStrings, ControlRequest};
use pcdock_core::session::{Config, Event, Session, Transmit};
use rusb::{Context, Device, DeviceHandle, Recipient, RequestType, TransferType, UsbContext};
use std::collections::HashMap;
use std::fmt;
use std::str::FromStr;
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
    /// Какой телефон брать, если их несколько. `None` — как раньше: первый телефон в режиме
    /// аксессуара, иначе первый в обычном режиме.
    pub target: Option<PhoneSelector>,
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
            target: None,
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

/// Физический адрес устройства: номер шины и цепочка портов от корневого хаба
/// (`Device::bus_number()` + `Device::port_numbers()`). Пишется как в Linux sysfs: `1-2.3`.
///
/// Адрес принадлежит гнезду, а не телефону. После AOA handshake телефон отключается и
/// появляется заново уже с PID аксессуара (0x2D00/0x2D01), но в том же гнезде, поэтому порт
/// обычно сохраняется: это стабильнее адреса устройства, который libusb выдаёт заново при каждом
/// подключении. Исключение — USB, проброшенный по сети (usbip, `usbipd` в WSL2): после
/// переподключения виртуальный порт может смениться. На этот случай `Dock` запоминает ещё и
/// серийник телефона (см. [`PhoneSelector`]).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct UsbPort {
    pub bus: u8,
    pub ports: Vec<u8>,
}

impl UsbPort {
    /// Адрес устройства. Открывать устройство для этого не нужно.
    pub fn of<T: UsbContext>(device: &Device<T>) -> Self {
        Self { bus: device.bus_number(), ports: device.port_numbers().unwrap_or_default() }
    }
}

impl fmt::Display for UsbPort {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.bus)?;
        for (i, p) in self.ports.iter().enumerate() {
            write!(f, "{}{p}", if i == 0 { '-' } else { '.' })?;
        }
        Ok(())
    }
}

impl FromStr for UsbPort {
    type Err = String;

    /// Разбирает `1-2.3` (шина 1, порт 2 корневого хаба, порт 3 следующего хаба).
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let bad = || format!("ожидался порт вида 1-2.3, а не {s:?}");
        let (bus, path) = s.split_once('-').ok_or_else(bad)?;
        let bus = bus.parse().map_err(|_| bad())?;
        let ports = path.split('.').map(|p| p.parse().map_err(|_| bad())).collect::<Result<Vec<u8>, _>>()?;
        Ok(Self { bus, ports })
    }
}

/// Какой телефон брать. Порт сверяется без открытия устройства (на macOS открытие телефона через
/// libusb может мешать adb), серийник — запасной ключ: чтобы его прочитать, устройство приходится
/// открыть, поэтому он читается, только если на заданном порту никого нет (а сразу после handshake —
/// не раньше чем через пару секунд и только у телефонов в режиме аксессуара). Поля работают как «или»:
/// телефон подходит, если он на этом порту **или** с этим серийником. Пустой селектор подходит любому.
///
/// Серийник телефон обычно отдаёт один и тот же в обычном режиме и в режиме аксессуара: это
/// `ro.serialno`, он же серийник для adb.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PhoneSelector {
    pub port: Option<UsbPort>,
    pub serial: Option<String>,
}

impl PhoneSelector {
    pub fn port(port: UsbPort) -> Self {
        Self { port: Some(port), serial: None }
    }

    pub fn serial(serial: impl Into<String>) -> Self {
        Self { port: None, serial: Some(serial.into()) }
    }

    pub fn is_empty(&self) -> bool {
        self.port.is_none() && self.serial.is_none()
    }

    /// Подходит ли устройство на порту `port`. `serial` читает серийник (открывает устройство) и
    /// вызывается, только если без него не решить.
    pub fn matches(&self, port: &UsbPort, serial: impl FnOnce() -> Option<String>) -> bool {
        if self.is_empty() || self.port.as_ref() == Some(port) {
            return true;
        }
        match &self.serial {
            Some(want) => serial().as_deref() == Some(want.as_str()),
            None => false,
        }
    }
}

/// Телефон, видимый на USB (см. [`list_phones`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PhoneInfo {
    pub vid: u16,
    pub pid: u16,
    pub mode: Mode,
    pub port: UsbPort,
    /// Серийник, если его просили прочитать и телефон его отдал.
    pub serial: Option<String>,
}

/// Таймаут чтения серийника в [`list_phones`].
const SERIAL_TIMEOUT: Duration = Duration::from_millis(500);

/// Телефоны на USB — чтобы выбрать `Settings::target`. Без `read_serials` устройства не открываются;
/// с ним каждый телефон открывается ради серийника (на macOS это может мешать adb).
pub fn list_phones(vendor_ids: &[u16], read_serials: bool) -> Result<Vec<PhoneInfo>, Error> {
    let ctx = Context::new()?;
    let mut phones = scan(&ctx, vendor_ids)?;
    if read_serials {
        for (device, info) in &mut phones {
            info.serial = read_serial(device, SERIAL_TIMEOUT);
        }
    }
    Ok(phones.into_iter().map(|(_, info)| info).collect())
}

/// Все телефоны на шине. Устройства не открываются.
fn scan(ctx: &Context, vendor_ids: &[u16]) -> Result<Vec<(Device<Context>, PhoneInfo)>, Error> {
    let mut out = Vec::new();
    for device in ctx.devices()?.iter() {
        let Ok(desc) = device.device_descriptor() else { continue };
        let (vid, pid) = (desc.vendor_id(), desc.product_id());
        let Some(mode) = classify(vendor_ids, vid, pid) else { continue };
        let port = UsbPort::of(&device);
        out.push((device, PhoneInfo { vid, pid, mode, port, serial: None }));
    }
    Ok(out)
}

/// Серийник уже открытого устройства; `None`, если его нет или телефон не ответил.
fn serial_of_handle(handle: &DeviceHandle<Context>, timeout: Duration) -> Option<String> {
    let desc = handle.device().device_descriptor().ok()?;
    desc.serial_number_string_index()?;
    let lang = *handle.read_languages(timeout).ok()?.first()?;
    handle.read_serial_number_string(lang, &desc, timeout).ok().filter(|s| !s.is_empty())
}

fn read_serial(device: &Device<Context>, timeout: Duration) -> Option<String> {
    serial_of_handle(&device.open().ok()?, timeout)
}

/// Через сколько после handshake `connect` и `shutdown` начинают искать свой телефон по серийнику
/// на других портах. Раньше нет смысла: телефон переподключается 1–3 с, обычно в то же гнездо, а
/// чтение серийника открывает соседние телефоны (на macOS это может мешать их adb).
const SERIAL_FALLBACK_DELAY: Duration = Duration::from_secs(2);

/// Кого можно открыть ради серийника, если на порту из селектора никого нет.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SerialLookup {
    /// Никого: handshake был только что, свой телефон вот-вот появится на своём порту.
    Off,
    /// Только телефоны в режиме аксессуара: после handshake свой телефон вернётся именно в нём.
    AccessoryOnly,
    /// Любые телефоны: handshake не было (или ожидание перехода уже кончилось).
    Any,
}

impl SerialLookup {
    /// `since_handshake` — сколько прошло с handshake этого `Dock` (`None`, если его не было);
    /// `grace` — сколько ждать перехода в режим аксессуара (`Settings::handshake_grace`).
    fn after_handshake(since_handshake: Option<Duration>, grace: Duration) -> Self {
        match since_handshake {
            Some(t) if t < SERIAL_FALLBACK_DELAY => Self::Off,
            Some(t) if t < grace => Self::AccessoryOnly,
            _ => Self::Any,
        }
    }

    fn allows(self, mode: Mode) -> bool {
        match self {
            Self::Off => false,
            Self::AccessoryOnly => mode == Mode::Accessory,
            Self::Any => true,
        }
    }
}

/// Первый подходящий телефон в режиме аксессуара, иначе первый подходящий в обычном. Ничего не открывает.
fn prefer_accessory<D>(cands: &[(D, PhoneInfo)], mut ok: impl FnMut(&PhoneInfo) -> bool) -> Option<usize> {
    let mut normal = None;
    for (i, (_, info)) in cands.iter().enumerate() {
        if !ok(info) {
            continue;
        }
        match info.mode {
            Mode::Accessory => return Some(i),
            Mode::Normal => {
                normal.get_or_insert(i);
            }
        }
    }
    normal
}

/// Выбор телефона для `connect`: сначала режим аксессуара, потом обычный (без селектора — как раньше:
/// первый аксессуар, иначе первый обычный). Возвращает индекс в `cands`.
///
/// С селектором сначала смотрим порт: устройство на нём берём, никого не открывая. Серийник —
/// только если на порту пусто, и только у тех, кого разрешает `lookup` (сначала аксессуары, чтобы
/// при совпадении не открывать телефоны в обычном режиме).
fn pick<D>(
    cands: &[(D, PhoneInfo)],
    sel: Option<&PhoneSelector>,
    lookup: SerialLookup,
    mut serial_of: impl FnMut(&D, &PhoneInfo) -> Option<String>,
) -> Option<usize> {
    let Some(sel) = sel.filter(|s| !s.is_empty()) else { return prefer_accessory(cands, |_| true) };
    if let Some(port) = &sel.port {
        let on_port = prefer_accessory(cands, |info| &info.port == port);
        if on_port.is_some() {
            return on_port;
        }
    }
    let want = sel.serial.as_deref()?;
    for mode in [Mode::Accessory, Mode::Normal] {
        if !lookup.allows(mode) {
            continue;
        }
        let found = cands
            .iter()
            .position(|(device, info)| info.mode == mode && serial_of(device, info).as_deref() == Some(want));
        if found.is_some() {
            return found;
        }
    }
    None
}

/// Выбор телефона в режиме аксессуара, который `shutdown` вернёт в обычный режим. С селектором — то же
/// правило, что в [`pick`]: если на своём порту кто-то есть, решаем по нему (ещё не аксессуар —
/// сбрасывать некого), соседей ради серийника открываем, только если порт пуст.
fn pick_for_reset<D>(
    cands: &[(D, PhoneInfo)],
    sel: Option<&PhoneSelector>,
    lookup: SerialLookup,
    mut serial_of: impl FnMut(&D, &PhoneInfo) -> Option<String>,
) -> Option<usize> {
    let accessory = |info: &PhoneInfo| info.mode == Mode::Accessory;
    let Some(sel) = sel.filter(|s| !s.is_empty()) else {
        return cands.iter().position(|(_, info)| accessory(info));
    };
    if let Some(port) = &sel.port {
        if let Some(i) = cands.iter().position(|(_, info)| &info.port == port) {
            return accessory(&cands[i].1).then_some(i);
        }
    }
    let want = sel.serial.as_deref()?;
    if !lookup.allows(Mode::Accessory) {
        return None;
    }
    cands
        .iter()
        .position(|(device, info)| accessory(info) && serial_of(device, info).as_deref() == Some(want))
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
/// Возвращает серийник телефона (если он есть): после START телефон переподключится, и по серийнику
/// его можно узнать, даже если порт сменился (см. [`UsbPort`]). Устройство всё равно открыто, так что
/// это два лишних control transfer.
fn aoa_handshake(
    device: &Device<Context>,
    strings: &AoaStrings,
    timeout: Duration,
) -> Result<Option<String>, Error> {
    let handle = device.open()?;
    let reply = control_in(&handle, &hid::get_protocol_request(), timeout)?;
    let version = hid::parse_protocol(&reply).unwrap_or(0);
    if version < 2 {
        return Err(Error::AoaTooOld(version));
    }
    let serial = serial_of_handle(&handle, timeout);
    for req in hid::start_requests(strings) {
        control_out(&handle, &req, timeout)?;
    }
    Ok(serial)
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
    /// Телефон, с которым работал этот `Dock` (handshake или открытый канал): порт и, если удалось
    /// прочитать, серийник. Его и только его сбрасывает `shutdown`.
    identity: Option<PhoneSelector>,
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
            identity: None,
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
        let timeout = self.settings.control_timeout;
        // Серийники чужих телефонов читаем один раз за `connect`, а не каждые 300 мс: чтение открывает
        // устройство. Ключ — порт и VID/PID: пока телефон не переподключился, это то же устройство.
        let mut serials: HashMap<(UsbPort, u16, u16), Option<String>> = HashMap::new();
        loop {
            // Селектор и правило чтения серийников зависят от того, ждём ли мы телефон после handshake.
            let target = self.connect_selector();
            let lookup = SerialLookup::after_handshake(
                self.last_handshake.map(|t| t.elapsed()),
                self.settings.handshake_grace,
            );
            let mut cands = scan(&self.ctx, &self.settings.vendor_ids)?;
            let found = pick(&cands, target.as_ref(), lookup, |d, info| {
                let key = (info.port.clone(), info.vid, info.pid);
                serials.entry(key).or_insert_with(|| read_serial(d, timeout)).clone()
            });
            match found.map(|i| cands.swap_remove(i)) {
                Some((device, info)) if info.mode == Mode::Accessory => match open_accessory(&device) {
                    Ok(phone) => {
                        let serial = serial_of_handle(&phone.handle, timeout);
                        self.identity = Some(PhoneSelector { port: Some(info.port), serial });
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
                Some((device, info)) => {
                    saw_phone = true;
                    let transitioning =
                        self.last_handshake.is_some_and(|t| t.elapsed() < self.settings.handshake_grace);
                    if !transitioning {
                        match aoa_handshake(&device, &self.session.config().aoa, timeout) {
                            Ok(serial) => {
                                self.last_handshake = Some(Instant::now());
                                // Узнали серийник: дальше ищем телефон и по нему (см. `connect_selector`).
                                self.identity = Some(PhoneSelector { port: Some(info.port), serial });
                            }
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

    /// Селектор для `connect`: `Settings::target`, дополненный серийником телефона, которого мы уже
    /// касались (если в `target` серийника нет): после handshake телефон может вернуться на другом
    /// порту (usbip). Пока ждём переход в режим аксессуара, а в `target` нет порта (только серийник),
    /// подставляем порт, на котором был handshake: телефон там и появится, и соседей открывать не
    /// придётся. Без `target` — как раньше, первый попавшийся телефон.
    fn connect_selector(&self) -> Option<PhoneSelector> {
        let mut sel = self.settings.target.clone().filter(|s| !s.is_empty())?;
        let Some(id) = &self.identity else { return Some(sel) };
        if sel.serial.is_none() {
            sel.serial = id.serial.clone();
        }
        let waiting = self.last_handshake.is_some_and(|t| t.elapsed() < self.settings.handshake_grace);
        if sel.port.is_none() && waiting {
            sel.port = id.port.clone();
        }
        Some(sel)
    }

    /// Какой телефон в режиме аксессуара сбрасывать при выходе: тот, с которым работали; если ни с
    /// каким — подходящий под `target`; без `target` — первый (как раньше: телефон, оставшийся в
    /// режиме аксессуара, при следующем подключении не запустит приложение сам).
    fn reset_selector(&self) -> Option<PhoneSelector> {
        self.identity.clone().or_else(|| self.settings.target.clone())
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
        // Сбрасываем только свой телефон: остальные могут принадлежать другим `Dock` (другим трубкам).
        let wait_until = self.last_handshake.map(|t| t + Duration::from_secs(8));
        let sel = self.reset_selector();
        let timeout = self.settings.control_timeout;
        // Как в `connect`: серийник соседа читаем не чаще раза на устройство.
        let mut serials: HashMap<(UsbPort, u16, u16), Option<String>> = HashMap::new();
        loop {
            let lookup = SerialLookup::after_handshake(
                self.last_handshake.map(|t| t.elapsed()),
                self.settings.handshake_grace,
            );
            if let Ok(cands) = scan(&self.ctx, &self.settings.vendor_ids) {
                let found = pick_for_reset(&cands, sel.as_ref(), lookup, |d, info| {
                    let key = (info.port.clone(), info.vid, info.pid);
                    serials.entry(key).or_insert_with(|| read_serial(d, timeout)).clone()
                });
                if let Some(i) = found {
                    if let Ok(handle) = cands[i].0.open() {
                        let _ = handle.reset();
                    }
                    return;
                }
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

    fn port(s: &str) -> UsbPort {
        s.parse().unwrap()
    }

    /// Фейковое устройство: имя для проверки и серийник, который «прочитается» при открытии.
    struct Fake {
        serial: Option<&'static str>,
    }

    fn phone(pid: u16, at: &str, serial: Option<&'static str>) -> (Fake, PhoneInfo) {
        let vid = if ACCESSORY_PIDS.contains(&pid) { 0x18D1 } else { 0x2717 };
        let mode = classify(&ANDROID_VIDS, vid, pid).unwrap();
        (Fake { serial }, PhoneInfo { vid, pid, mode, port: port(at), serial: None })
    }

    fn serial_of(d: &Fake, _: &PhoneInfo) -> Option<String> {
        d.serial.map(String::from)
    }

    #[test]
    fn usb_port_round_trips() {
        let p = port("1-2.3");
        assert_eq!(p, UsbPort { bus: 1, ports: vec![2, 3] });
        assert_eq!(p.to_string(), "1-2.3");
        assert_eq!(port("20-1").to_string(), "20-1");
        assert!("1".parse::<UsbPort>().is_err());
        assert!("1-x".parse::<UsbPort>().is_err());
        assert!("abc-1".parse::<UsbPort>().is_err());
    }

    #[test]
    fn without_selector_picks_like_before() {
        // Первый аксессуар важнее первого обычного, даже если он дальше в списке.
        let cands = [phone(0xFF48, "1-1", None), phone(0x2D01, "1-2", None), phone(0x2D00, "1-3", None)];
        assert_eq!(pick(&cands, None, SerialLookup::Any, serial_of), Some(1));
        let normal = [phone(0xFF48, "1-1", None), phone(0xFF40, "1-2", None)];
        assert_eq!(pick(&normal, None, SerialLookup::Any, serial_of), Some(0));
        assert_eq!(pick::<Fake>(&[], None, SerialLookup::Any, serial_of), None);
        // Пустой селектор = без селектора.
        assert_eq!(pick(&cands, Some(&PhoneSelector::default()), SerialLookup::Any, serial_of), Some(1));
    }

    #[test]
    fn selector_by_port_ignores_other_phones() {
        // Второй телефон уже в режиме аксессуара (его держит другой Dock): нас он не интересует.
        let cands = [phone(0x2D01, "1-2", None), phone(0xFF48, "1-4", None)];
        let sel = PhoneSelector::port(port("1-4"));
        let mut opened = 0;
        let found = pick(&cands, Some(&sel), SerialLookup::Any, |d, i| {
            opened += 1;
            serial_of(d, i)
        });
        assert_eq!(found, Some(1));
        assert_eq!(opened, 0, "без серийника в селекторе устройства не открываются");
        assert_eq!(pick(&cands, Some(&PhoneSelector::port(port("2-1"))), SerialLookup::Any, serial_of), None);
    }

    #[test]
    fn selector_survives_switch_to_accessory_on_same_port() {
        let sel = PhoneSelector::port(port("1-4"));
        // До handshake: телефон в обычном режиме.
        let before = [phone(0x2D00, "1-2", None), phone(0xFF48, "1-4", None)];
        assert_eq!(pick(&before, Some(&sel), SerialLookup::Any, serial_of), Some(1));
        // После: тот же порт, PID аксессуара; соседний аксессуар не берём.
        let after = [phone(0x2D00, "1-2", None), phone(0x2D01, "1-4", None)];
        assert_eq!(pick(&after, Some(&sel), SerialLookup::Any, serial_of), Some(1));
    }

    #[test]
    fn serial_finds_phone_that_moved_to_another_port() {
        // usbip: после переподключения телефон получил другой виртуальный порт.
        let sel = PhoneSelector { port: Some(port("3-1")), serial: Some("ABC".into()) };
        let cands = [phone(0x2D01, "3-1", Some("OTHER")), phone(0x2D01, "3-2", Some("ABC"))];
        assert_eq!(
            pick(&cands, Some(&sel), SerialLookup::Any, serial_of),
            Some(0),
            "порт совпал — серийник не нужен"
        );
        let moved = [phone(0x2D01, "3-5", Some("OTHER")), phone(0x2D01, "3-2", Some("ABC"))];
        assert_eq!(pick(&moved, Some(&sel), SerialLookup::Any, serial_of), Some(1));
        let only_serial = PhoneSelector::serial("ABC");
        assert_eq!(pick(&moved, Some(&only_serial), SerialLookup::Any, serial_of), Some(1));
        let unknown = PhoneSelector::serial("NOPE");
        assert_eq!(pick(&moved, Some(&unknown), SerialLookup::Any, serial_of), None);
    }

    #[test]
    fn reset_targets_only_own_phone() {
        // Свой телефон ещё в обычном режиме (сброс прошёл), чужой — в режиме аксессуара.
        let own = PhoneSelector { port: Some(port("1-4")), serial: Some("MINE".into()) };
        let cands = [phone(0x2D01, "1-2", Some("THEIRS")), phone(0xFF48, "1-4", Some("MINE"))];
        assert_eq!(
            pick_for_reset(&cands, Some(&own), SerialLookup::Any, serial_of),
            None,
            "чужой не трогаем"
        );
        // Свой дошёл до режима аксессуара.
        let cands = [phone(0x2D01, "1-2", Some("THEIRS")), phone(0x2D01, "1-4", Some("MINE"))];
        assert_eq!(pick_for_reset(&cands, Some(&own), SerialLookup::Any, serial_of), Some(1));
        // Без селектора — как раньше: первый аксессуар.
        assert_eq!(pick_for_reset(&cands, None, SerialLookup::Any, serial_of), Some(0));
    }

    /// `serial_of`, который считает «открытия» устройств и запоминает, какие порты открывались.
    fn counting(opened: &mut Vec<String>) -> impl FnMut(&Fake, &PhoneInfo) -> Option<String> + '_ {
        |d, info| {
            opened.push(info.port.to_string());
            serial_of(d, info)
        }
    }

    #[test]
    fn serial_lookup_waits_after_handshake() {
        let grace = Duration::from_secs(5);
        assert_eq!(SerialLookup::after_handshake(None, grace), SerialLookup::Any);
        assert_eq!(SerialLookup::after_handshake(Some(Duration::ZERO), grace), SerialLookup::Off);
        assert_eq!(
            SerialLookup::after_handshake(Some(Duration::from_millis(1900)), grace),
            SerialLookup::Off
        );
        assert_eq!(
            SerialLookup::after_handshake(Some(Duration::from_secs(2)), grace),
            SerialLookup::AccessoryOnly
        );
        assert_eq!(SerialLookup::after_handshake(Some(Duration::from_secs(5)), grace), SerialLookup::Any);
    }

    #[test]
    fn target_port_present_opens_no_neighbours() {
        // Соседи стоят раньше в списке и в обоих режимах; у селектора есть серийник.
        let sel = PhoneSelector { port: Some(port("1-4")), serial: Some("MINE".into()) };
        let cands = [
            phone(0x2D01, "1-1", Some("THEIRS")),
            phone(0xFF48, "1-2", Some("OTHER")),
            phone(0x2D01, "1-4", Some("MINE")),
        ];
        for lookup in [SerialLookup::Off, SerialLookup::AccessoryOnly, SerialLookup::Any] {
            let mut opened = Vec::new();
            assert_eq!(pick(&cands, Some(&sel), lookup, counting(&mut opened)), Some(2));
            assert!(opened.is_empty(), "{lookup:?}: открыты {opened:?}");
            assert_eq!(pick_for_reset(&cands, Some(&sel), lookup, counting(&mut opened)), Some(2));
            assert!(opened.is_empty(), "{lookup:?}: сброс открыл {opened:?}");
        }
        // Свой телефон ещё в обычном режиме (не успел переключиться): соседний аксессуар не открываем.
        let switching = [phone(0x2D01, "1-1", Some("THEIRS")), phone(0xFF48, "1-4", Some("MINE"))];
        let mut opened = Vec::new();
        assert_eq!(pick(&switching, Some(&sel), SerialLookup::Any, counting(&mut opened)), Some(1));
        assert_eq!(pick_for_reset(&switching, Some(&sel), SerialLookup::Any, counting(&mut opened)), None);
        assert!(opened.is_empty(), "открыты {opened:?}");
    }

    #[test]
    fn serial_fallback_only_after_grace_and_only_accessories() {
        // Порт пуст: телефон переподключается или переехал (usbip).
        let sel = PhoneSelector { port: Some(port("1-4")), serial: Some("MINE".into()) };
        let cands = [
            phone(0xFF48, "1-1", Some("OTHER")),
            phone(0x2D01, "1-2", Some("THEIRS")),
            phone(0x2D01, "1-7", Some("MINE")),
        ];
        // Первые секунды после handshake никого не открываем.
        let mut opened = Vec::new();
        assert_eq!(pick(&cands, Some(&sel), SerialLookup::Off, counting(&mut opened)), None);
        assert_eq!(pick_for_reset(&cands, Some(&sel), SerialLookup::Off, counting(&mut opened)), None);
        assert!(opened.is_empty());
        // Потом — только аксессуары; телефон в обычном режиме не открывается.
        assert_eq!(pick(&cands, Some(&sel), SerialLookup::AccessoryOnly, counting(&mut opened)), Some(2));
        assert_eq!(opened, ["1-2", "1-7"]);
        opened.clear();
        assert_eq!(
            pick_for_reset(&cands, Some(&sel), SerialLookup::AccessoryOnly, counting(&mut opened)),
            Some(2)
        );
        assert_eq!(opened, ["1-2", "1-7"]);
        // Свой телефон в обычном режиме на другом порту: находится только без ожидания handshake.
        let normal = [phone(0x2D01, "1-2", Some("THEIRS")), phone(0xFF48, "1-7", Some("MINE"))];
        assert_eq!(pick(&normal, Some(&sel), SerialLookup::AccessoryOnly, serial_of), None);
        assert_eq!(pick(&normal, Some(&sel), SerialLookup::Any, serial_of), Some(1));
    }

    #[test]
    fn dock_remembers_identity_for_connect_and_reset() {
        let settings = Settings { target: Some(PhoneSelector::port(port("1-4"))), ..Settings::default() };
        let mut dock = Dock::new(Config::default(), settings).unwrap();
        assert_eq!(dock.connect_selector(), Some(PhoneSelector::port(port("1-4"))));
        assert_eq!(dock.reset_selector(), Some(PhoneSelector::port(port("1-4"))));
        // Handshake прочитал серийник: им можно найти телефон, если порт сменится.
        dock.identity = Some(PhoneSelector { port: Some(port("1-4")), serial: Some("ABC".into()) });
        let both = PhoneSelector { port: Some(port("1-4")), serial: Some("ABC".into()) };
        assert_eq!(dock.connect_selector(), Some(both.clone()));
        assert_eq!(dock.reset_selector(), Some(both));
        dock.shut_down = true; // Drop не должен трогать настоящий USB

        // Target только по серийнику: пока ждём переход, ищем и на порту handshake; потом — нет.
        let settings = Settings { target: Some(PhoneSelector::serial("ABC")), ..Settings::default() };
        let mut by_serial = Dock::new(Config::default(), settings).unwrap();
        by_serial.identity = Some(PhoneSelector { port: Some(port("1-4")), serial: Some("ABC".into()) });
        assert_eq!(by_serial.connect_selector(), Some(PhoneSelector::serial("ABC")));
        by_serial.last_handshake = Some(Instant::now());
        let waiting = PhoneSelector { port: Some(port("1-4")), serial: Some("ABC".into()) };
        assert_eq!(by_serial.connect_selector(), Some(waiting));
        by_serial.shut_down = true;

        // Без target: connect как раньше, а сброс — только своего телефона, если такой был.
        let mut legacy = Dock::new(Config::default(), Settings::default()).unwrap();
        assert_eq!(legacy.connect_selector(), None);
        assert_eq!(legacy.reset_selector(), None);
        legacy.identity = Some(PhoneSelector::port(port("1-2")));
        assert_eq!(legacy.connect_selector(), None);
        assert_eq!(legacy.reset_selector(), Some(PhoneSelector::port(port("1-2"))));
        legacy.shut_down = true;
    }
}
