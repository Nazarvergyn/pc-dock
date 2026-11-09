//! `pcdockd` — демон док-станции.
//!
//! Режимы: `--auto` находит всё сам (USB, ADB, serial-трубы),
//! без него — явный список `--phone`. С `--auto` работают `--except`
//! и `--no-usb/--no-adb/--no-serial`; без `--auto` `--phone` это «только эти».

use clap::Parser;
use pcdock_farm::{Farm, FarmConfig, PhoneSpec};
use pcdock_serve::Serve;

/// Демон PC Dock: трубки на любых транспортах, один протокол наружу.
#[derive(Debug, Parser)]
#[command(name = "pcdockd", version)]
struct Args {
    /// Найти всё сам: USB, ADB, serial-трубы.
    #[arg(long, short = 'a')]
    auto: bool,

    /// Закреплённый телефон: принимается всегда. Формат: `usb` (любой USB-телефон),
    /// `usb:1-2.3` (на этом USB-порту), `usb:СЕРИЙНИК`, `adb:СЕРИЙНИК`, `serial:ПУТЬ`.
    /// Без --auto это «только эти». Можно несколько раз.
    #[arg(long = "phone")]
    phones: Vec<String>,

    /// Пропустить при --auto: семейство (`usb`), `usb:1-2.3`, `adb:СЕРИЙНИК` или серийник.
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
    let mut pinned = Vec::new();
    for spec in &args.phones {
        match PhoneSpec::parse(spec) {
            Ok(p) => pinned.push(p),
            Err(e) => {
                log::error!("--phone {spec}: {e}");
                std::process::exit(2);
            }
        }
    }
    if !args.auto && pinned.is_empty() {
        log::error!("нужен --auto или хотя бы один --phone (см. --help)");
        std::process::exit(2);
    }
    let cfg = FarmConfig {
        name: args.name.clone(),
        video: args.video,
        audio: args.audio,
        auto: args.auto,
        except: args.except.clone(),
        pinned,
        usb: !args.no_usb,
        adb: !args.no_adb,
        serial: !args.no_serial,
        ..FarmConfig::default()
    };
    // Ферма стартует пустой: телефоны подхватываются по мере появления (plug&play).
    let daemon = match Farm::from_config(cfg) {
        Ok((farm, warnings)) => {
            for w in warnings {
                log::warn!("{w}");
            }
            farm
        }
        Err(e) => {
            log::error!("{e}");
            std::process::exit(2);
        }
    };

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
