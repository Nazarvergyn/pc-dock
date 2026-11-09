//! `pcdockd` — демон док-станции.
//!
//! Режимы: `--auto` находит всё сам (USB, ADB, serial-трубы),
//! без него — явный список `--phone`. С `--auto` работают `--except`
//! и `--no-usb/--no-adb/--no-serial`; без `--auto` `--phone` это «только эти».

use clap::Parser;
use pcdock_core::link::{Discovery, LinkFactory, PhoneLink, Sighting, Transport};
use pcdock_core::session::Config;
use pcdock_farm::{DiscoveryConfig, LiveFarm};
use pcdock_serve::Serve;
use pcdock_usb::UsbLink;
use pcdock_usb::{PhoneSelector, Settings, UsbPort, ANDROID_VIDS};

/// Демон PC Dock: трубки на любых транспортах, один протокол наружу.
#[derive(Debug, Parser)]
#[command(name = "pcdockd", version)]
struct Args {
    /// Найти всё сам: USB, ADB, serial-трубы.
    #[arg(long, short = 'a')]
    auto: bool,

    /// Явная трубка (без --auto: «только эти»). Формат: `usb` (первый телефон),
    /// `usb:1-2.3` (телефон на этом USB-порту), `usb:СЕРИЙНИК`, `adb:СЕРИЙНИК`,
    /// `serial:ПУТЬ`. Можно несколько раз.
    #[arg(long = "phone")]
    phones: Vec<String>,

    /// Пропустить при --auto. Формат тот же, что у --phone (USB — по порту: `usb:1-2.3`).
    #[arg(long = "except")]
    except: Vec<String>,

    /// Выключить семейство при --auto.
    #[arg(long = "no-usb")]
    no_usb: bool,
    #[arg(long = "no-adb")]
    no_adb: bool,
    #[arg(long = "no-serial")]
    no_serial: bool,

    /// Слушать TCP (протокол с конвертом). Пусто = не слушать.
    #[arg(long, default_value = "127.0.0.1:9001")]
    tcp: Option<String>,
    #[arg(long = "no-tcp")]
    no_tcp: bool,

    /// Слушать веб-сокеты (браузеры). Пусто = не слушать.
    #[arg(long, default_value = "127.0.0.1:9002")]
    ws: Option<String>,
    #[arg(long = "no-ws")]
    no_ws: bool,

    /// Локальная труба (unix-сокет / именованный пайп). Пусто = не слушать.
    #[arg(long, default_value = "pcdock")]
    local: Option<String>,
    #[arg(long = "no-local")]
    no_local: bool,

    /// Имя хоста в HELLO.
    #[arg(long, default_value = "pcdockd/1.0")]
    name: String,

    /// Подробные логи (-v debug, -vv trace). Без флага — info.
    #[arg(short = 'v', action = clap::ArgAction::Count)]
    verbose: u8,

    /// Видео/звук по умолчанию для новых трубок.
    #[arg(long, default_value_t = true)]
    video: bool,
    #[arg(long, default_value_t = false)]
    audio: bool,
}

fn main() {
    let args = Args::parse();
    if std::env::var("RUST_LOG").is_err() {
        let level = match args.verbose {
            0 => "info",
            1 => "debug",
            _ => "trace",
        };
        // SAFETY: однопоточный старт, гонок нет.
        unsafe { std::env::set_var("RUST_LOG", level) };
    }
    env_logger::init();
    if !args.auto && args.phones.is_empty() {
        log::error!("нужен --auto или хотя бы один --phone (см. --help)");
        std::process::exit(2);
    }
    if args.auto && !args.phones.is_empty() {
        log::error!("--phone без --auto это «только эти»; с --auto используй --except");
        std::process::exit(2);
    }

    let mut daemon = LiveFarm::new();
    let mut next_id: u32 = 1;

    if args.auto {
        // Живая ферма стартует пустой: телефоны подхватываются по мере появления (plug&play).
        enable_auto(&mut daemon, &args);
    } else {
        for spec in &args.phones {
            next_id = add_spec(&mut daemon, &args, spec, next_id);
        }
    };
    let _ = next_id; // id выдан, дальше трубки живут в LiveFarm

    if !args.auto && daemon.phones().is_empty() {
        log::error!("нет трубок: демон слушать нечего");
        std::process::exit(1);
    }

    let serve = Serve::new(daemon);
    if !args.no_tcp {
        match args.tcp.as_deref().unwrap_or("") {
            "" => {}
            addr => match serve.run_tcp(addr) {
                Ok(bound) => log::info!("TCP: {bound}"),
                Err(e) => log::error!("TCP {addr} не встал: {e}"),
            },
        }
    }
    if !args.no_ws {
        match args.ws.as_deref().unwrap_or("") {
            "" => {}
            addr => match serve.run_ws(addr) {
                Ok(bound) => log::info!("WS: {bound}"),
                Err(e) => log::error!("WS {addr} не встал: {e}"),
            },
        }
    }
    if !args.no_local {
        match args.local.as_deref().unwrap_or("") {
            "" => {}
            name => match serve.run_local(name) {
                Ok(()) => log::info!("local: {name}"),
                Err(e) => log::error!("local {name} не встал: {e}"),
            },
        }
    }

    log::info!("pcdockd работает, трубки: {:?}", serve.daemon().lock().unwrap().phones());
    // Воркеры и слушатели живут в своих потоках; главный ждёт вечно.
    loop {
        std::thread::sleep(std::time::Duration::from_secs(3600));
    }
}

/// Автообнаружение: источники по семействам, исключения, фабрика каналов.
fn enable_auto(daemon: &mut LiveFarm, args: &Args) {
    let mut providers: Vec<Box<dyn Discovery>> = Vec::new();
    if !args.no_usb {
        match pcdock_usb::UsbDiscovery::new(ANDROID_VIDS.to_vec()) {
            Ok(d) => providers.push(Box::new(Filtered { inner: Box::new(d), except: args.except.clone() })),
            Err(e) => log::error!("USB-обнаружение не встало: {e}"),
        }
    }
    if !args.no_adb {
        let d = pcdock_adb::AdbDiscovery::new(pcdock_adb::Adb::local());
        providers.push(Box::new(Filtered { inner: Box::new(d), except: args.except.clone() }));
    }
    if !args.no_serial {
        log::warn!("serial-трубы: транспорт не готов, пропускаю");
    }
    let factory = Factory { args: FactoryArgs::from(args), next_adb_port: 47000 };
    daemon.enable_discovery(providers, Box::new(factory), DiscoveryConfig::default());
    log::info!("автообнаружение включено");
}

/// Отбрасывает факты, подпавшие под `--except`: семейство (`usb`), `usb:1-2.3`, `adb:СЕРИЙНИК`, серийник.
struct Filtered {
    inner: Box<dyn Discovery>,
    except: Vec<String>,
}

impl Discovery for Filtered {
    fn scan(&mut self) -> Vec<Sighting> {
        let except = &self.except;
        self.inner
            .scan()
            .into_iter()
            .filter(|s| {
                let by_address = format!("{}:{}", s.transport, s.address);
                !except.iter().any(|e| {
                    *e == s.transport.to_string()
                        || *e == by_address
                        || s.serial.as_deref() == Some(e.as_str())
                })
            })
            .collect()
    }
}

struct FactoryArgs {
    name: String,
    video: bool,
    audio: bool,
}

impl FactoryArgs {
    fn from(a: &Args) -> Self {
        Self { name: a.name.clone(), video: a.video, audio: a.audio }
    }

    fn config(&self) -> Config {
        Config {
            name: self.name.clone(),
            stream: Some(pcdock_core::packet::SetStream {
                video: self.video,
                audio: self.audio,
                ..Default::default()
            }),
            ..Default::default()
        }
    }
}

/// Создаёт канал по факту обнаружения.
struct Factory {
    args: FactoryArgs,
    next_adb_port: u16,
}

impl LinkFactory for Factory {
    fn make(&mut self, s: &Sighting) -> Result<Box<dyn PhoneLink>, String> {
        match s.transport {
            Transport::Usb => {
                let port: UsbPort = s.address.parse()?;
                let target = PhoneSelector { port: Some(port), serial: s.serial.clone() };
                let link = UsbLink::new(
                    self.args.config(),
                    Settings { target: Some(target), ..Settings::default() },
                )?;
                Ok(Box::new(link))
            }
            Transport::Adb => {
                let local = self.next_adb_port;
                self.next_adb_port = self.next_adb_port.wrapping_add(1).max(47000);
                Ok(Box::new(pcdock_adb::AdbLink::new(self.args.config(), s.address.clone(), local)))
            }
            Transport::Serial => Err("serial-транспорт не готов".into()),
        }
    }
}

/// Явный список «только эти».
fn add_spec(daemon: &mut LiveFarm, args: &Args, spec: &str, next_id: u32) -> u32 {
    if spec == "usb" {
        return add_usb(daemon, args, next_id, None);
    }
    if let Some(what) = spec.strip_prefix("usb:") {
        if what.is_empty() {
            log::error!("пустой порт/серийник в --phone {spec}");
            return next_id;
        }
        let target = match what.parse::<UsbPort>() {
            Ok(port) => PhoneSelector::port(port),
            Err(_) => PhoneSelector::serial(what),
        };
        return add_usb(daemon, args, next_id, Some(target));
    }
    if let Some(serial) = spec.strip_prefix("adb:") {
        if serial.is_empty() {
            log::error!("пустой серийник в --phone {spec}");
            return next_id;
        }
        return add_adb(daemon, args, serial, next_id);
    }
    if let Some(path) = spec.strip_prefix("serial:") {
        if path.is_empty() {
            log::error!("пустой путь в --phone {spec}");
            return next_id;
        }
        log::warn!("serial {path}: транспорт не готов, пропускаю");
        return next_id;
    }
    log::error!("неизвестный формат --phone {spec}");
    next_id
}

fn add_usb(daemon: &mut LiveFarm, args: &Args, id: u32, target: Option<PhoneSelector>) -> u32 {
    let label = match &target {
        None => "первый телефон".to_string(),
        Some(PhoneSelector { port: Some(port), .. }) => format!("порт {port}"),
        Some(PhoneSelector { serial: Some(serial), .. }) => format!("серийник {serial}"),
        Some(_) => "первый телефон".to_string(),
    };
    let cfg = Config {
        name: args.name.clone(),
        stream: Some(pcdock_core::packet::SetStream {
            video: args.video,
            audio: args.audio,
            ..Default::default()
        }),
        ..Default::default()
    };
    let link = match UsbLink::new(cfg, Settings { target, ..Settings::default() }) {
        Ok(l) => l,
        Err(e) => {
            log::error!("USB-трубка не создалась: {e}");
            return id;
        }
    };
    match daemon.add_phone(id, Box::new(link)) {
        Ok(()) => {
            // Поток поднимет сессию сам; video/audio по умолчанию из Config.
            // Живое переключение — командой SetStream по сокету.
            log::info!("трубка {id}: USB ({label})");
            id + 1
        }
        Err(e) => {
            log::error!("трубка {id} не добавилась: {e}");
            id
        }
    }
}

/// ADB-трубка: локальный порт проброса выбираем рядом с телефонным.
fn add_adb(daemon: &mut LiveFarm, args: &Args, serial: &str, id: u32) -> u32 {
    let cfg = Config {
        name: args.name.clone(),
        stream: Some(pcdock_core::packet::SetStream {
            video: args.video,
            audio: args.audio,
            ..Default::default()
        }),
        ..Default::default()
    };
    let local_port = 47000 + (id as u16 % 1000);
    let link = pcdock_adb::AdbLink::new(cfg, serial, local_port);
    match daemon.add_phone(id, Box::new(link)) {
        Ok(()) => {
            log::info!("трубка {id}: ADB {serial} (проброс {local_port})");
            id + 1
        }
        Err(e) => {
            log::error!("трубка {id} не добавилась: {e}");
            id
        }
    }
}
