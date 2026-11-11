//! `pcdock-check`: проверка PC Dock на этой машине. Находит телефоны, проверяет окружение (USB, ADB), ожидание
//! готовности, размер экрана, поток кадров и, по запросу, ввод. Печатает отчёт и возвращает код 0, только если
//! всё обязательное прошло. Один и тот же файл запускается на Linux, macOS и Windows.
//!
//! По умолчанию только читает: ничего не нажимает на телефоне. Ввод включается `--input`.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use clap::Parser;
use pcdock_check::{environment, run_phone_checks, Check, Options, Report, Status};
use pcdock_farm::{Farm, FarmConfig};

/// Проверка PC Dock.
#[derive(Debug, Parser)]
#[command(name = "pcdock-check", version)]
struct Args {
    /// APK сервера для ADB-режима (или переменная `PCDOCK_SERVER_APK`).
    #[arg(long)]
    server_apk: Option<PathBuf>,

    /// Проверять только этот телефон (серийник). Без параметра: все найденные.
    #[arg(long)]
    phone: Option<String>,

    /// Только USB (AOA) / только ADB.
    #[arg(long)]
    no_adb: bool,
    #[arg(long)]
    no_usb: bool,

    /// Сколько ждать появления телефона и готовности, секунд.
    #[arg(long, default_value_t = 60)]
    wait: u64,

    /// Сколько кадров принять для проверки видео.
    #[arg(long, default_value_t = 5)]
    frames: usize,

    /// Разрешить проверку ввода (свайп вниз/вверх и клавиша Назад на телефоне).
    #[arg(long)]
    input: bool,

    /// Записать отчёт в JSON-файл.
    #[arg(long)]
    json: Option<PathBuf>,
}

fn main() {
    let args = Args::parse();
    let mut report = Report::new();
    environment(&mut report);

    let cfg = FarmConfig {
        usb: !args.no_usb,
        adb: !args.no_adb,
        server_apk: args.server_apk.clone().or(FarmConfig::default().server_apk),
        // Пока один телефон не стал рабочим, запасной переход на другой путь только мешает чтению отчёта.
        fallback_after: None,
        ..FarmConfig::default()
    };
    let (mut farm, warnings) = match Farm::from_config(cfg) {
        Ok(f) => f,
        Err(e) => {
            report.add(Check::fail("ферма", format!("не запустилась: {e}")));
            return finish(report, &args);
        }
    };
    for w in warnings {
        report.add(Check::warn("ферма", w));
    }

    // Ждём телефоны: хотя бы один должен появиться.
    let opts = Options {
        wait: Duration::from_secs(args.wait),
        frames: args.frames,
        input: args.input,
        only_serial: args.phone.clone(),
    };
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_secs(args.wait.min(15)) {
        farm.drain();
        if !farm.phones().is_empty() {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    run_phone_checks(&mut farm, &opts, &mut report);
    drop(farm); // возвращает телефоны в обычный режим
    finish(report, &args);
}

fn finish(report: Report, args: &Args) {
    print!("{}", report.render());
    if let Some(p) = &args.json {
        match std::fs::write(p, serde_json::to_string_pretty(&report.to_json()).unwrap()) {
            Ok(()) => println!("отчёт записан: {}", p.display()),
            Err(e) => eprintln!("отчёт не записан ({}): {e}", p.display()),
        }
    }
    let code = match report.overall() {
        Status::Fail => 1,
        _ => 0,
    };
    std::process::exit(code);
}
