//! `pcdockd` — демон док-станции.
//!
//! Режимы: `--auto` находит всё сам (USB, ADB, serial-трубы),
//! без него — явный список `--phone`. С `--auto` работают `--except`
//! и `--no-usb/--no-adb/--no-serial`; без `--auto` `--phone` это «только эти».

use clap::Parser;
use pcdock_core::session::Config;
use pcdock_daemon::{Daemon, UsbLink};
use pcdock_serve::Serve;
use pcdock_usb::{Settings, ACCESSORY_PIDS, ANDROID_VIDS};
use rusb::UsbContext;

/// Демон PC Dock: трубки на любых транспортах, один протокол наружу.
#[derive(Debug, Parser)]
#[command(name = "pcdockd", version)]
struct Args {
    /// Найти всё сам: USB, ADB, serial-трубы.
    #[arg(long, short = 'a')]
    auto: bool,

    /// Явная трубка (без --auto: «только эти»). Формат: `usb`, `adb:СЕРИЙНИК`,
    /// `serial:ПУТЬ`. Можно несколько раз.
    #[arg(long = "phone")]
    phones: Vec<String>,

    /// Пропустить при --auto. Формат тот же, что у --phone.
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

    /// Видео/звук по умолчанию для новых трубок.
    #[arg(long, default_value_t = true)]
    video: bool,
    #[arg(long, default_value_t = false)]
    audio: bool,
}

fn main() {
    let args = Args::parse();
    if !args.auto && args.phones.is_empty() {
        eprintln!("нужен --auto или хотя бы один --phone (см. --help)");
        std::process::exit(2);
    }
    if args.auto && !args.phones.is_empty() {
        eprintln!("--phone без --auto это «только эти»; с --auto используй --except");
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
        eprintln!("нет трубок: демон слушать нечего (трубки можно добавить позже — пока выхожу)");
        std::process::exit(1);
    }

    let serve = Serve::new(daemon);
    if !args.no_tcp {
        match args.tcp.as_deref().unwrap_or("") {
            "" => {}
            addr => match serve.run_tcp(addr) {
                Ok(bound) => println!("TCP: {bound}"),
                Err(e) => eprintln!("TCP {addr} не встал: {e}"),
            },
        }
    }
    if !args.no_ws {
        match args.ws.as_deref().unwrap_or("") {
            "" => {}
            addr => match serve.run_ws(addr) {
                Ok(bound) => println!("WS: {bound}"),
                Err(e) => eprintln!("WS {addr} не встал: {e}"),
            },
        }
    }
    if !args.no_local {
        match args.local.as_deref().unwrap_or("") {
            "" => {}
            name => match serve.run_local(name) {
                Ok(()) => println!("local: {name}"),
                Err(e) => eprintln!("local {name} не встал: {e}"),
            },
        }
    }

    println!("pcdockd работает, трубки: {:?}", serve.daemon().lock().unwrap().phones());
    // Воркеры и слушатели живут в своих потоках; главный ждёт вечно.
    loop {
        std::thread::sleep(std::time::Duration::from_secs(3600));
    }
}

/// Автообнаружение всех семейств. Возвращает следующий свободный id.
fn auto_add(daemon: &mut Daemon, args: &Args, mut next_id: u32) -> u32 {
    if !args.no_usb {
        for dev in usb_scan() {
            if args.except.iter().any(|e| e == "usb" || e == &dev) {
                continue;
            }
            next_id = add_usb(daemon, args, next_id, &dev);
            // Честное ограничение v1: транспорт открывает первое устройство,
            // поэтому вторую USB-трубку осознанно не добавляем (см. ниже).
            break;
        }
    }
    if !args.no_adb {
        for serial in adb_scan() {
            let spec = format!("adb:{serial}");
            if args.except.iter().any(|e| e == &spec || e == "adb") {
                continue;
            }
            println!("ADB {serial}: транспорт не готов, пропускаю (трубка не добавлена)");
        }
    }
    if !args.no_serial {
        for port in serial_scan() {
            let spec = format!("serial:{port}");
            if args.except.iter().any(|e| e == &spec || e == "serial") {
                continue;
            }
            println!("serial {port}: транспорт не готов, пропускаю (трубка не добавлена)");
        }
    }
    next_id
}

/// Явный список «только эти».
fn add_spec(daemon: &mut Daemon, args: &Args, spec: &str, next_id: u32) -> u32 {
    if spec == "usb" || spec.starts_with("usb:") {
        return add_usb(daemon, args, next_id, spec);
    }
    if let Some(serial) = spec.strip_prefix("adb:") {
        if serial.is_empty() {
            eprintln!("пустой серийник в --phone {spec}");
            return next_id;
        }
        println!("ADB {serial}: транспорт не готов, пропускаю");
        return next_id;
    }
    if let Some(path) = spec.strip_prefix("serial:") {
        if path.is_empty() {
            eprintln!("пустой путь в --phone {spec}");
            return next_id;
        }
        println!("serial {path}: транспорт не готов, пропускаю");
        return next_id;
    }
    eprintln!("неизвестный формат --phone {spec} (жди usb, adb:СЕРИЙНИК, serial:ПУТЬ)");
    next_id
}

fn add_usb(daemon: &mut Daemon, args: &Args, id: u32, _label: &str) -> u32 {
    let cfg = Config {
        name: args.name.clone(),
        stream: Some(pcdock_core::packet::SetStream {
            video: args.video,
            audio: args.audio,
            ..Default::default()
        }),
        ..Default::default()
    };
    let link = match UsbLink::new(cfg, Settings::default()) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("USB-трубка не создалась: {e}");
            return id;
        }
    };
    match daemon.add_phone(id, Box::new(link)) {
        Ok(()) => {
            // Поток поднимет сессию сам; video/audio по умолчанию из Config.
            // Живое переключение — командой SetStream по сокету.
            println!("трубка {id}: USB");
            id + 1
        }
        Err(e) => {
            eprintln!("трубка {id} не добавилась: {e}");
            id
        }
    }
}

/// Видимые USB-устройства-телефоны: `usb:VID:PID` (+ серийник при наличии).
/// Ограничение v1: транспорт открывает первое совпавшее устройство,
/// поэтому авторежим берёт только первое (остальные — после прицельного открытия).
fn usb_scan() -> Vec<String> {
    let mut out = Vec::new();
    let ctx = match rusb::Context::new() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("USB-скан не удался: {e}");
            return out;
        }
    };
    for dev in ctx.devices().map(|d| d.iter().collect::<Vec<_>>()).unwrap_or_default() {
        let desc = match dev.device_descriptor() {
            Ok(d) => d,
            Err(_) => continue,
        };
        let (vid, pid) = (desc.vendor_id(), desc.product_id());
        if !ANDROID_VIDS.contains(&vid) {
            continue;
        }
        let mode = if ACCESSORY_PIDS.contains(&pid) { "accessory" } else { "normal" };
        out.push(format!("usb:{vid:04x}:{pid:04x} ({mode})"));
    }
    out
}

/// Серийники `adb devices`. Транспорт позже, пока только список.
fn adb_scan() -> Vec<String> {
    let out = std::process::Command::new("adb").arg("devices").output();
    let Ok(out) = out else { return Vec::new() };
    let text = String::from_utf8_lossy(&out.stdout);
    text.lines()
        .skip(1)
        .filter_map(|l| {
            let mut it = l.split_whitespace();
            match (it.next(), it.next()) {
                (Some(s), Some("device")) => Some(s.to_owned()),
                _ => None,
            }
        })
        .collect()
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
