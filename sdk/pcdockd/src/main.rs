//! `pcdockd` — демон док-станции.
//!
//! Режимы: `--auto` находит всё сам (USB, ADB, serial-трубы),
//! без него — явный список `--phone`. С `--auto` работают `--except`
//! и `--no-usb/--no-adb/--no-serial`; без `--auto` `--phone` это «только эти».

use clap::Parser;
use pcdock_core::session::Config;
use pcdock_daemon::{Daemon, UsbLink};
use pcdock_serve::Serve;
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

    let mut daemon = Daemon::new();
    let mut next_id: u32 = 1;

    if args.auto {
        next_id = auto_add(&mut daemon, &args, next_id);
    } else {
        for spec in &args.phones {
            next_id = add_spec(&mut daemon, &args, spec, next_id);
        }
    };
    let _ = next_id; // id выдан, дальше трубки живут в Daemon

    if daemon.phones().is_empty() {
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

/// Автообнаружение всех семейств. Возвращает следующий свободный id.
fn auto_add(daemon: &mut Daemon, args: &Args, mut next_id: u32) -> u32 {
    if !args.no_usb {
        // Каждому телефону своя трубка, привязанная к порту: `Dock` трогает только свой телефон.
        for port in usb_scan() {
            let spec = format!("usb:{port}");
            if args.except.iter().any(|e| e == "usb" || e == &spec) {
                continue;
            }
            next_id = add_usb(daemon, args, next_id, Some(PhoneSelector::port(port)));
        }
    }
    if !args.no_adb {
        for serial in adb_scan() {
            let spec = format!("adb:{serial}");
            if args.except.iter().any(|e| e == &spec || e == "adb") {
                continue;
            }
            next_id = add_adb(daemon, args, &serial, next_id);
        }
    }
    if !args.no_serial {
        for port in serial_scan() {
            let spec = format!("serial:{port}");
            if args.except.iter().any(|e| e == &spec || e == "serial") {
                continue;
            }
            log::warn!("serial {port}: транспорт не готов, пропускаю");
        }
    }
    next_id
}

/// Явный список «только эти».
fn add_spec(daemon: &mut Daemon, args: &Args, spec: &str, next_id: u32) -> u32 {
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

fn add_usb(daemon: &mut Daemon, args: &Args, id: u32, target: Option<PhoneSelector>) -> u32 {
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
fn add_adb(daemon: &mut Daemon, args: &Args, serial: &str, id: u32) -> u32 {
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

/// Порты, на которых видны телефоны (в любом режиме). Устройства не открываются:
/// на macOS это мешало бы adb.
fn usb_scan() -> Vec<UsbPort> {
    match pcdock_usb::list_phones(&ANDROID_VIDS, false) {
        Ok(phones) => phones.into_iter().map(|p| p.port).collect(),
        Err(e) => {
            log::error!("USB-скан не удался: {e}");
            Vec::new()
        }
    }
}

/// Серийники через ADB-сервер (без шелл-аутов). Нет сервера — пусто.
fn adb_scan() -> Vec<String> {
    match pcdock_adb::Adb::local().devices() {
        Ok(devs) => devs,
        Err(e) => {
            log::debug!("ADB-скан: {e}");
            Vec::new()
        }
    }
}

/// Кандидаты serial-труб: USB-COM порты.
fn serial_scan() -> Vec<String> {
    let mut out = Vec::new();
    #[cfg(unix)]
    {
        if let Ok(dir) = std::fs::read_dir("/dev") {
            for e in dir.flatten() {
                let n = e.file_name().to_string_lossy().into_owned();
                if n.starts_with("ttyUSB") || n.starts_with("ttyACM") {
                    out.push(format!("/dev/{n}"));
                }
            }
        }
    }
    #[cfg(windows)]
    {
        // Перечисление COM-портов — следующим шагом вместе с транспортом.
        let _ = &mut out;
    }
    out
}
