//! Проверка на живом телефоне: подключается, печатает события, по желанию отправляет тестовый тап.
//!
//!   pcdock-probe [--seconds N] [--tap] [--no-touch] [--no-mouse] [--no-keyboard]
//!                [--max-size 720x1280] [--quality 80] [--fps 10]
//!                [--port 1-2.3] [--serial СЕРИЙНИК]
//!   pcdock-probe --list [--serials]
//!
//! `--list` печатает телефоны на USB (порт, VID:PID, режим); `--serials` ещё и читает серийники
//! (открывает устройства). `--port`/`--serial` выбирают телефон, если их несколько.

use pcdock_core::input::{DeviceKind, TouchPhase, TouchPoint};
use pcdock_core::session::{Config, Event};
use pcdock_usb::{list_phones, Dock, PhoneSelector, Settings, ANDROID_VIDS};
use std::time::{Duration, Instant};

/// Размер JPEG из заголовка (маркеры SOF0..SOF3, SOF5..SOF7, SOF9..SOF11 ...).
fn jpeg_dims(data: &[u8]) -> Option<(u16, u16)> {
    let mut i = 2;
    while i + 9 < data.len() {
        if data[i] != 0xFF {
            i += 1;
            continue;
        }
        let marker = data[i + 1];
        if (0xC0..=0xCF).contains(&marker) && !matches!(marker, 0xC4 | 0xC8 | 0xCC) {
            let h = u16::from_be_bytes([data[i + 5], data[i + 6]]);
            let w = u16::from_be_bytes([data[i + 7], data[i + 8]]);
            return Some((w, h));
        }
        let len = u16::from_be_bytes([data[i + 2], data[i + 3]]) as usize;
        i += 2 + len;
    }
    None
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let has = |f: &str| args.iter().any(|a| a == f);
    let seconds: u64 = args
        .iter()
        .position(|a| a == "--seconds")
        .and_then(|i| args.get(i + 1))
        .and_then(|v| v.parse().ok())
        .unwrap_or(20);

    let value_of = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned();
    if has("--list") {
        match list_phones(&ANDROID_VIDS, has("--serials")) {
            Ok(phones) if phones.is_empty() => println!("телефонов не видно"),
            Ok(phones) => {
                for p in phones {
                    let serial = p.serial.as_deref().unwrap_or("-");
                    println!("{}\t{:04x}:{:04x}\t{:?}\t{serial}", p.port, p.vid, p.pid, p.mode);
                }
            }
            Err(e) => eprintln!("не удалось перечислить USB: {e}"),
        }
        return;
    }
    let port = match value_of("--port").map(|p| p.parse()).transpose() {
        Ok(p) => p,
        Err(e) => return eprintln!("--port: {e}"),
    };
    let serial = value_of("--serial");
    let target = (port.is_some() || serial.is_some()).then_some(PhoneSelector { port, serial });
    let mut config = Config::default();
    if let Some(size) = value_of("--max-size") {
        // формат: 1280x720
        let (w, h) = size.split_once('x').unwrap_or(("0", "0"));
        config.stream = Some(pcdock_core::packet::SetStream {
            max_width: w.parse().unwrap_or(0),
            max_height: h.parse().unwrap_or(0),
            jpeg_quality: value_of("--quality").and_then(|q| q.parse().ok()).unwrap_or(0),
            max_fps: value_of("--fps").and_then(|q| q.parse().ok()).unwrap_or(0),
            ..Default::default()
        });
    }
    config.hid.enabled = [!has("--no-touch"), !has("--no-mouse"), !has("--no-keyboard")];

    let t0 = Instant::now();
    let log = |msg: String| println!("[{:6.2}s] {msg}", t0.elapsed().as_secs_f32());

    let mut dock = match Dock::new(config, Settings { target, ..Settings::default() }) {
        Ok(d) => d,
        Err(e) => return eprintln!("не удалось запустить USB: {e}"),
    };
    log("ищу телефон и перевожу его в режим аксессуара...".into());
    if let Err(e) = dock.connect() {
        return eprintln!("не удалось подключиться: {e}");
    }
    log("подключён, сессия запущена".into());
    let mut connected_at = Instant::now();

    let deadline = t0 + Duration::from_secs(seconds);
    let mut tapped = false;
    let mut last_dims = None;
    let mut frames = 0u32;
    while Instant::now() < deadline {
        if !dock.is_connected() {
            log("связь потеряна, подключаюсь заново".into());
            if let Err(e) = dock.connect() {
                log(format!("не получилось: {e}"));
                break;
            }
            connected_at = Instant::now();
            log("подключён заново".into());
            continue;
        }
        if has("--tap") && !tapped && connected_at.elapsed() > Duration::from_millis(4500) {
            tapped = true;
            let down = TouchPoint { pressure: Some(0.7), ..TouchPoint::new(0, TouchPhase::Down, 0.5, 0.25) };
            let r1 = dock.session().touch(down);
            std::thread::sleep(Duration::from_millis(80));
            let r2 = dock.session().touch(TouchPoint::new(0, TouchPhase::Up, 0.5, 0.25));
            log(format!("тестовый тап (0.5, 0.25): {r1:?} {r2:?}"));
        }
        match dock.poll() {
            Ok(events) => {
                for e in events {
                    match e {
                        Event::Video(f) => {
                            frames += 1;
                            let dims = jpeg_dims(&f.data);
                            if dims != last_dims {
                                log(format!("кадры теперь {dims:?} (кадр №{frames}, {} байт)", f.data.len()));
                                last_dims = dims;
                            }
                        }
                        Event::Disconnected(r) => log(format!("отключено: {r:?}")),
                        other => log(format!("{other:?}")),
                    }
                }
            }
            Err(e) => {
                log(format!("ошибка: {e}"));
                break;
            }
        }
        for w in dock.take_warnings() {
            log(format!("предупреждение: {w}"));
        }
    }
    log(format!(
        "формы ввода: тачскрин {}, мышь {}, клавиатура {}",
        dock.session_ref().input().is_registered(DeviceKind::Touch),
        dock.session_ref().input().is_registered(DeviceKind::Mouse),
        dock.session_ref().input().is_registered(DeviceKind::Keyboard)
    ));
    log("завершаю: снимаю устройства ввода, сбрасываю USB".into());
    dock.shutdown();
    log("готово".into());
}
