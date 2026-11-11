//! Проверки PC Dock: окружение, телефоны, видео, ввод. Отделено от `main`, чтобы проверять на фейковой ферме.

use std::time::{Duration, Instant};

use pcdock_core::input::{DeviceKind, TouchPhase};
use pcdock_core::link::{InputCmd, PhoneId, PhoneState};
use pcdock_farm::{Farm, FarmEvent};
use serde_json::{json, Value};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Pass,
    Warn,
    Fail,
    Skip,
}

impl Status {
    fn label(self) -> &'static str {
        match self {
            Self::Pass => "ПРОШЛО ",
            Self::Warn => "ВНИМАНИЕ",
            Self::Fail => "ПРОВАЛ ",
            Self::Skip => "ПРОПУСК",
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Pass => "pass",
            Self::Warn => "warn",
            Self::Fail => "fail",
            Self::Skip => "skip",
        }
    }
}

/// Одна проверка и её результат.
#[derive(Clone, Debug)]
pub struct Check {
    pub name: String,
    pub status: Status,
    pub detail: String,
}

impl Check {
    pub fn pass(name: impl Into<String>, detail: impl Into<String>) -> Self {
        Self { name: name.into(), status: Status::Pass, detail: detail.into() }
    }
    pub fn warn(name: impl Into<String>, detail: impl Into<String>) -> Self {
        Self { name: name.into(), status: Status::Warn, detail: detail.into() }
    }
    pub fn fail(name: impl Into<String>, detail: impl Into<String>) -> Self {
        Self { name: name.into(), status: Status::Fail, detail: detail.into() }
    }
    pub fn skip(name: impl Into<String>, detail: impl Into<String>) -> Self {
        Self { name: name.into(), status: Status::Skip, detail: detail.into() }
    }
}

/// Отчёт: проверки по порядку.
#[derive(Default)]
pub struct Report {
    pub checks: Vec<Check>,
}

impl Report {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add(&mut self, c: Check) {
        self.checks.push(c);
    }

    /// Итог: провал, если хоть одна проверка провалена.
    pub fn overall(&self) -> Status {
        if self.checks.iter().any(|c| c.status == Status::Fail) {
            Status::Fail
        } else if self.checks.iter().any(|c| c.status == Status::Warn) {
            Status::Warn
        } else {
            Status::Pass
        }
    }

    pub fn render(&self) -> String {
        let mut out = String::new();
        for c in &self.checks {
            out.push_str(&format!("[{}] {}: {}\n", c.status.label(), c.name, c.detail));
        }
        let count = |s: Status| self.checks.iter().filter(|c| c.status == s).count();
        out.push_str(&format!(
            "\nИтого: {}. Прошло {}, внимание {}, провалов {}, пропущено {}.\n",
            match self.overall() {
                Status::Fail => "ПРОВАЛ",
                Status::Warn => "прошло с замечаниями",
                _ => "всё хорошо",
            },
            count(Status::Pass),
            count(Status::Warn),
            count(Status::Fail),
            count(Status::Skip),
        ));
        out
    }

    pub fn to_json(&self) -> Value {
        json!({
            "overall": self.overall().name(),
            "os": std::env::consts::OS,
            "arch": std::env::consts::ARCH,
            "checks": self.checks.iter().map(|c| json!({"name": c.name, "status": c.status.name(), "detail": c.detail})).collect::<Vec<_>>(),
        })
    }
}

/// Параметры проверки телефонов.
pub struct Options {
    /// Сколько ждать готовности телефона.
    pub wait: Duration,
    /// Сколько кадров принять.
    pub frames: usize,
    /// Разрешён ли ввод (нажимает на телефоне).
    pub input: bool,
    /// Проверять только телефон с этим серийником.
    pub only_serial: Option<String>,
}

/// Проверка окружения: система, USB, ADB.
pub fn environment(report: &mut Report) {
    report.add(Check::pass("система", format!("{} {}", std::env::consts::OS, std::env::consts::ARCH)));
    match pcdock_usb::list_phones(&pcdock_usb::ANDROID_VIDS, false) {
        Ok(phones) => report.add(Check::pass(
            "USB",
            format!("libusb работает, телефонов на шине: {}", phones.len()),
        )),
        Err(e) => report.add(Check::warn(
            "USB",
            format!("libusb недоступен: {e}. AOA-режим работать не будет (Windows: драйвер WinUSB; Linux: права на устройства)."),
        )),
    }
    match pcdock_adb::Adb::local().device_states() {
        Ok(devs) => {
            let ready = devs.iter().filter(|(_, s)| s == "device").count();
            let waiting = devs.iter().filter(|(_, s)| s == "unauthorized").count();
            let mut detail = format!("сервер ADB отвечает, устройств: {ready}");
            if waiting > 0 {
                detail.push_str(&format!(", ждут подтверждения отладки: {waiting}"));
            }
            report.add(if waiting > 0 { Check::warn("ADB", detail) } else { Check::pass("ADB", detail) });
        }
        Err(e) => report.add(Check::warn(
            "ADB",
            format!("сервер ADB недоступен: {e}. ADB-режим работать не будет (adb start-server).",),
        )),
    }
}

/// Проверки каждого найденного телефона.
pub fn run_phone_checks(farm: &mut Farm, opts: &Options, report: &mut Report) {
    let phones: Vec<PhoneId> = farm.phones();
    let selected: Vec<_> = farm
        .phones_info()
        .into_iter()
        .filter(|i| phones.contains(&i.id))
        .filter(|i| opts.only_serial.as_ref().is_none_or(|s| i.serial.as_ref() == Some(s)))
        .collect();
    if selected.is_empty() {
        report.add(Check::fail(
            "телефоны",
            "ни одного телефона не найдено: проверьте кабель, проброс USB, права (udev/WinUSB), отладку по USB и список производителей",
        ));
        return;
    }
    report.add(Check::pass("телефоны", format!("найдено: {}", selected.len())));
    for info in selected {
        check_phone(farm, info.id, opts, report);
    }
}

fn label(farm: &Farm, id: PhoneId) -> String {
    let info = farm.phones_info().into_iter().find(|i| i.id == id);
    match info {
        Some(i) => format!(
            "телефон {id} ({}, {})",
            i.transport.map(|t| t.to_string()).unwrap_or_else(|| "?".into()),
            i.serial.unwrap_or_else(|| "без серийника".into())
        ),
        None => format!("телефон {id}"),
    }
}

fn check_phone(farm: &mut Farm, id: PhoneId, opts: &Options, report: &mut Report) {
    let who = label(farm, id);
    // Готовность: ждём `ready`, по дороге запоминаем подсказки.
    let _ = farm.set_watching(id, true);
    let t0 = Instant::now();
    let mut hint = String::new();
    let mut frames: Vec<(u64, usize)> = Vec::new();
    let mut display = None;
    let mut ready_at = None;
    while t0.elapsed() < opts.wait {
        for ev in farm.drain() {
            match ev {
                FarmEvent::State { phone, state, detail } if phone == id => {
                    if !detail.is_empty() {
                        hint = detail;
                    }
                    if state == PhoneState::Ready && ready_at.is_none() {
                        ready_at = Some(t0.elapsed());
                    }
                }
                FarmEvent::Display { phone, screen_w, screen_h, stream_w, stream_h, rotation, .. }
                    if phone == id =>
                {
                    display = Some((screen_w, screen_h, stream_w, stream_h, rotation));
                }
                FarmEvent::Video { phone, frame } if phone == id => {
                    frames.push((frame.seq, frame.data.len()))
                }
                _ => {}
            }
        }
        if farm.state(id) == Ok(PhoneState::Ready) && ready_at.is_none() {
            ready_at = Some(t0.elapsed());
        }
        if ready_at.is_some() && frames.len() >= opts.frames && display.is_some() {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    match ready_at {
        Some(t) => report
            .add(Check::pass(format!("{who}: готовность"), format!("ready через {:.1} с", t.as_secs_f32()))),
        None => {
            let state = farm.state(id).map(|s| format!("{s:?}")).unwrap_or_else(|e| e);
            let detail = if hint.is_empty() {
                format!("не стал ready за {} с, состояние: {state}", opts.wait.as_secs())
            } else {
                format!("состояние {state}: {hint}")
            };
            report.add(Check::fail(format!("{who}: готовность"), detail));
            return;
        }
    }
    match display {
        Some((sw, sh, w, h, rot)) => report
            .add(Check::pass(format!("{who}: экран"), format!("{sw}x{sh}, поток {w}x{h}, поворот {rot}"))),
        None => report.add(Check::warn(format!("{who}: экран"), "размер экрана не пришёл")),
    }
    if frames.len() >= opts.frames {
        let total: usize = frames.iter().map(|f| f.1).sum();
        report.add(Check::pass(
            format!("{who}: видео"),
            format!("кадров {}, в среднем {} КБ", frames.len(), total / frames.len() / 1024),
        ));
    } else {
        // Кадры идут только при изменении экрана: просим повторить последний.
        let _ = farm.command(id, InputCmd::RequestFrame);
        let end = Instant::now() + Duration::from_secs(3);
        while Instant::now() < end && frames.is_empty() {
            for ev in farm.drain() {
                if let FarmEvent::Video { phone, frame } = ev {
                    if phone == id {
                        frames.push((frame.seq, frame.data.len()));
                    }
                }
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        if frames.is_empty() {
            report.add(Check::fail(format!("{who}: видео"), "кадры не приходят"));
        } else {
            report.add(Check::warn(
                format!("{who}: видео"),
                format!(
                    "получено кадров: {} (меньше {}): экран, возможно, не меняется",
                    frames.len(),
                    opts.frames
                ),
            ));
        }
    }
    if !opts.input {
        report.add(Check::skip(
            format!("{who}: ввод"),
            "выключено (добавьте --input: свайп вниз и вверх на экране телефона)",
        ));
        return;
    }
    check_input(farm, id, &who, report);
}

/// Свайп вниз открывает шторку уведомлений, свайп вверх и клавиша Назад закрывают её; смотрим, что картинка менялась.
fn check_input(farm: &mut Farm, id: PhoneId, who: &str, report: &mut Report) {
    let before = farm.last_frame(id).ok().flatten().map(|f| (f.seq, f.data.len()));
    let _ = farm.command(id, InputCmd::DeviceEnable { kind: DeviceKind::Touch, on: true });
    let send =
        |farm: &Farm, phase, y: f32| farm.command(id, InputCmd::touch_ex(0, phase, 0.5, y, None, None, None));
    let mut ok = send(farm, TouchPhase::Down, 0.01).is_ok();
    for i in 1..=10 {
        ok &= send(farm, TouchPhase::Move, 0.01 + 0.5 * i as f32 / 10.0).is_ok();
        std::thread::sleep(Duration::from_millis(30));
    }
    ok &= send(farm, TouchPhase::Up, 0.51).is_ok();
    // Ждём, пока придут кадры анимации.
    let mut seen = 0;
    let end = Instant::now() + Duration::from_secs(3);
    while Instant::now() < end {
        for ev in farm.drain() {
            if matches!(ev, FarmEvent::Video { phone, .. } if phone == id) {
                seen += 1;
            }
        }
        std::thread::sleep(Duration::from_millis(30));
    }
    // Закрываем: свайп вверх и Назад.
    let _ = send(farm, TouchPhase::Down, 0.6);
    for i in 1..=10 {
        let _ = send(farm, TouchPhase::Move, 0.6 - 0.55 * i as f32 / 10.0);
        std::thread::sleep(Duration::from_millis(30));
    }
    let _ = send(farm, TouchPhase::Up, 0.05);
    let _ = farm.command(id, InputCmd::Key { usage: 0x29, down: true });
    let _ = farm.command(id, InputCmd::Key { usage: 0x29, down: false });
    let _ = before;
    if !ok {
        report.add(Check::fail(format!("{who}: ввод"), "команды ввода отклонены"));
    } else if seen > 0 {
        report.add(Check::pass(
            format!("{who}: ввод"),
            format!("свайп принят, экран реагировал (кадров анимации: {seen})"),
        ));
    } else {
        report.add(Check::warn(
            format!("{who}: ввод"),
            "команды приняты, но экран не изменился (экран выключен или заблокирован?)",
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pcdock_core::link::{LinkError, PhoneLink};
    use pcdock_core::session::Event;
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    struct Fake {
        connected: bool,
        events: Arc<Mutex<VecDeque<Event>>>,
    }

    impl PhoneLink for Fake {
        fn is_connected(&self) -> bool {
            self.connected
        }
        fn connect(&mut self) -> Result<(), LinkError> {
            self.connected = true;
            Ok(())
        }
        fn poll(&mut self) -> Result<Vec<Event>, LinkError> {
            std::thread::sleep(Duration::from_millis(2));
            Ok(self.events.lock().unwrap().pop_front().into_iter().collect())
        }
        fn input(&mut self, _: &InputCmd) -> Result<(), LinkError> {
            Ok(())
        }
        fn shutdown(&mut self) {}
    }

    fn farm_with(events: Vec<Event>) -> Farm {
        let q = Arc::new(Mutex::new(events.into()));
        let mut farm = Farm::new();
        farm.add_phone(1, Box::new(Fake { connected: false, events: q })).unwrap();
        farm
    }

    fn video(i: u64) -> Event {
        Event::Video(pcdock_core::packet::VideoFrame {
            codec: 1,
            key_frame: true,
            pts_us: i,
            data: vec![1; 2048],
        })
    }

    fn opts() -> Options {
        Options { wait: Duration::from_secs(5), frames: 3, input: false, only_serial: None }
    }

    #[test]
    fn healthy_phone_passes_readiness_screen_and_video() {
        let mut events = vec![
            Event::Connected { version: 2, capabilities: 0, peer_name: "fake".into() },
            Event::Display(pcdock_core::packet::DisplayInfo {
                stream_width: 360,
                stream_height: 820,
                screen_width: 720,
                screen_height: 1640,
                density_dpi: 320,
                rotation: 0,
            }),
        ];
        events.extend((0..5).map(video));
        let mut farm = farm_with(events);
        let mut r = Report::new();
        run_phone_checks(&mut farm, &opts(), &mut r);
        let by = |n: &str| {
            r.checks
                .iter()
                .find(|c| c.name.contains(n))
                .unwrap_or_else(|| panic!("нет проверки {n}: {}", r.render()))
                .status
        };
        assert_eq!(by("готовность"), Status::Pass, "{}", r.render());
        assert_eq!(by("экран"), Status::Pass);
        assert_eq!(by("видео"), Status::Pass);
        assert_eq!(by("ввод"), Status::Skip, "без --input ничего не нажимается");
        assert_ne!(r.overall(), Status::Fail);
    }

    #[test]
    fn silent_phone_fails_with_its_state() {
        let mut farm = farm_with(vec![]);
        let mut r = Report::new();
        let o = Options { wait: Duration::from_millis(300), ..opts() };
        run_phone_checks(&mut farm, &o, &mut r);
        assert_eq!(r.overall(), Status::Fail);
        assert!(r.render().contains("не стал ready"), "{}", r.render());
    }

    #[test]
    fn empty_farm_reports_no_phones() {
        let mut farm = Farm::new();
        let mut r = Report::new();
        run_phone_checks(&mut farm, &opts(), &mut r);
        assert_eq!(r.overall(), Status::Fail);
        assert!(r.render().contains("ни одного телефона"));
    }

    #[test]
    fn input_check_reports_screen_reaction() {
        let mut events = vec![Event::Connected { version: 2, capabilities: 0, peer_name: "fake".into() }];
        events.extend((0..3).map(video));
        let mut farm = farm_with(events);
        // Кадры анимации приходят после свайпа: добавляем их позже через общую очередь нельзя, поэтому проверяем
        // только, что без реакции экрана выдаётся предупреждение, а не провал.
        let mut r = Report::new();
        let o = Options { input: true, ..opts() };
        run_phone_checks(&mut farm, &o, &mut r);
        let input = r.checks.iter().find(|c| c.name.contains("ввод")).unwrap();
        assert!(matches!(input.status, Status::Warn | Status::Pass), "{}", r.render());
    }

    #[test]
    fn report_json_and_text_agree() {
        let mut r = Report::new();
        r.add(Check::pass("a", "x"));
        r.add(Check::warn("b", "y"));
        r.add(Check::skip("c", "z"));
        assert_eq!(r.overall(), Status::Warn);
        let j = r.to_json();
        assert_eq!(j["overall"], "warn");
        assert_eq!(j["checks"].as_array().unwrap().len(), 3);
        assert!(r.render().contains("Прошло 1, внимание 1, провалов 0, пропущено 1"));
        r.add(Check::fail("d", "w"));
        assert_eq!(r.overall(), Status::Fail);
    }
}
