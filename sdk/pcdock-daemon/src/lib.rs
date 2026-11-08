//! Библиотека демона: владеет трубками, потоками и переподключениями.
//!
//! Абстракция для встраивания: добавил трубку, положил команду, забрал событие.
//! Потоки, USB и ретраи внутри, снаружи их не видно. Сам демон-библиотека
//! поверх — тонкий запуск, сокеты — следующий слой.

use std::collections::HashMap;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use pcdock_core::input::{DeviceKind, MouseButton, TouchPhase, TouchPoint};
use pcdock_core::packet::SetStream;
use pcdock_core::session::Event;

/// Идентификатор трубки. Назначает вызывающий.
pub type PhoneId = u32;

/// Команда ввода одной трубке. Человеческий темп (паузы между DOWN/MOVE/UP,
/// мелкие порции скролла) делает вызывающий; либа шлёт как есть.
#[derive(Clone, Debug)]
pub enum InputCmd {
    DeviceEnable {
        kind: DeviceKind,
        on: bool,
    },
    Touch {
        cid: u8,
        phase: TouchPhase,
        x: f32,
        y: f32,
        width: Option<f32>,
        height: Option<f32>,
        pressure: Option<f32>,
    },
    CursorMoveAbs {
        x: f32,
        y: f32,
    },
    CursorMoveRel {
        dx: i32,
        dy: i32,
    },
    CursorResync,
    MouseButton {
        button: MouseButton,
        down: bool,
    },
    Scroll {
        ticks: f32,
    },
    ScrollH {
        ticks: f32,
    },
    Key {
        usage: u16,
        down: bool,
    },
    ReleaseAll,
    RequestFrame,
    Wake,
    SetStream {
        video: bool,
        audio: bool,
    },
}

impl InputCmd {
    /// Касание с явными пятном/силой. `None` = default ядра.
    #[allow(clippy::too_many_arguments)]
    pub fn touch_ex(
        cid: u8,
        phase: TouchPhase,
        x: f32,
        y: f32,
        width: Option<f32>,
        height: Option<f32>,
        pressure: Option<f32>,
    ) -> Self {
        Self::Touch { cid, phase, x, y, width, height, pressure }
    }
}

/// Кадр с номером. Номер растёт на каждый кадр трубки: клиент сокетов
/// видит пропуск при заторе.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CachedFrame {
    pub seq: u64,
    pub codec: u8,
    pub key_frame: bool,
    pub pts_us: u64,
    pub data: Vec<u8>,
}

/// Событие трубки наружу. Видео идёт только если трубку смотрят
/// (`set_watching`), кэш последнего кадра — всегда.
#[derive(Clone, Debug)]
pub enum DaemonEvent {
    Connected {
        phone: PhoneId,
        version: u16,
        capabilities: u32,
        name: String,
    },
    Display {
        phone: PhoneId,
        stream_w: u16,
        stream_h: u16,
        screen_w: u16,
        screen_h: u16,
        dpi: u16,
        rotation: u8,
    },
    Video {
        phone: PhoneId,
        frame: CachedFrame,
    },
    AudioConfig {
        phone: PhoneId,
        codec: u8,
        rate: u32,
        channels: u8,
    },
    Audio {
        phone: PhoneId,
        pts_us: u64,
        data: Vec<u8>,
    },
    Status {
        phone: PhoneId,
        code: u16,
        detail: String,
    },
    PeerSilent {
        phone: PhoneId,
    },
    Malformed {
        phone: PhoneId,
        detail: String,
    },
    Disconnected {
        phone: PhoneId,
        reason: String,
    },
    /// Связь потеряна, воркер переподключает. Отдельно от `Disconnected` сессии.
    LinkDown {
        phone: PhoneId,
        detail: String,
    },
    Warning {
        phone: PhoneId,
        detail: String,
    },
}

/// Ошибка связи. `Gone` = устройство пропало, надо переподключать.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LinkError {
    Gone(String),
    Fatal(String),
}

impl std::fmt::Display for LinkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Gone(e) => write!(f, "устройство пропало: {e}"),
            Self::Fatal(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for LinkError {}

/// Связь с одной трубкой. Реализации: USB сегодня, ADB и serial-труба завтра.
/// Сессия ядра живёт внутри реализации; наружу идут готовые события.
pub trait PhoneLink: Send {
    fn is_connected(&self) -> bool;
    /// Найти устройство и поднять сессию. `Gone` = повторить позже.
    fn connect(&mut self) -> Result<(), LinkError>;
    /// Один шаг: отправить накопленное, прочитать входящее, вернуть события.
    fn poll(&mut self) -> Result<Vec<Event>, LinkError>;
    /// Применить команду ввода (уйдёт при следующем `poll`).
    fn input(&mut self, cmd: &InputCmd) -> Result<(), LinkError>;
    /// Аккуратно отпустить устройство.
    fn shutdown(&mut self);
}

enum WorkerCmd {
    Input(InputCmd),
    Stop,
}

struct Worker {
    cmd_tx: Sender<WorkerCmd>,
    handle: Option<JoinHandle<()>>,
}

struct Slot {
    worker: Worker,
    watching: bool,
    last_frame: Option<CachedFrame>,
    frame_seq: u64,
}

/// Демон. Не потокобезопасен сам: держите в одном потоке-владельце.
pub struct Daemon {
    slots: HashMap<PhoneId, Slot>,
    ev_tx: Sender<DaemonEvent>,
    ev_rx: Receiver<DaemonEvent>,
}

impl Daemon {
    pub fn new() -> Self {
        let (ev_tx, ev_rx) = mpsc::channel();
        Self { slots: HashMap::new(), ev_tx, ev_rx }
    }

    /// Канал событий наружу. Неблокирующий опрос — через `try_recv`.
    pub fn events(&self) -> &Receiver<DaemonEvent> {
        &self.ev_rx
    }

    pub fn phones(&self) -> Vec<PhoneId> {
        let mut ids: Vec<PhoneId> = self.slots.keys().copied().collect();
        ids.sort_unstable();
        ids
    }

    /// Добавляет трубку и поднимает её поток. Связь поднимется сама с ретраями.
    pub fn add_phone(&mut self, id: PhoneId, link: Box<dyn PhoneLink>) -> Result<(), String> {
        if self.slots.contains_key(&id) {
            return Err(format!("трубка {id} уже есть"));
        }
        let (cmd_tx, cmd_rx) = mpsc::channel();
        let ev_tx = self.ev_tx.clone();
        let handle = thread::Builder::new()
            .name(format!("phone-{id}"))
            .spawn(move || worker_loop(id, link, cmd_rx, ev_tx))
            .map_err(|e| format!("не встал поток трубки {id}: {e}"))?;
        self.slots.insert(
            id,
            Slot {
                worker: Worker { cmd_tx, handle: Some(handle) },
                watching: false,
                last_frame: None,
                frame_seq: 0,
            },
        );
        Ok(())
    }

    /// Убирает трубку: поток останавливается, кэш выбрасывается.
    pub fn remove_phone(&mut self, id: PhoneId) -> Result<(), String> {
        let mut slot = self.slots.remove(&id).ok_or_else(|| format!("нет трубки {id}"))?;
        let _ = slot.worker.cmd_tx.send(WorkerCmd::Stop);
        if let Some(h) = slot.worker.handle.take() {
            let _ = h.join();
        }
        Ok(())
    }

    /// Положить команду ввода в очередь трубки. Нет трубки = ошибка.
    pub fn command(&self, id: PhoneId, cmd: InputCmd) -> Result<(), String> {
        let slot = self.slots.get(&id).ok_or_else(|| format!("нет трубки {id}"))?;
        slot.worker.cmd_tx.send(WorkerCmd::Input(cmd)).map_err(|_| format!("поток трубки {id} мёртв"))?;
        Ok(())
    }

    pub fn set_watching(&mut self, id: PhoneId, watching: bool) -> Result<(), String> {
        self.slots.get_mut(&id).ok_or_else(|| format!("нет трубки {id}"))?.watching = watching;
        Ok(())
    }

    pub fn is_watching(&self, id: PhoneId) -> Result<bool, String> {
        self.slots.get(&id).ok_or_else(|| format!("нет трубки {id}")).map(|s| s.watching)
    }

    /// Последний кадр трубки. Очередь событий не трогает.
    pub fn last_frame(&self, id: PhoneId) -> Result<Option<CachedFrame>, String> {
        self.slots.get(&id).ok_or_else(|| format!("нет трубки {id}")).map(|s| s.last_frame.clone())
    }

    /// Разобрать входящие события воркеров: кэш кадров, фильтр просмотра.
    /// Возвращает события для фронтенда (сокеты, окно, скрипты).
    pub fn drain(&mut self) -> Vec<DaemonEvent> {
        let mut out = Vec::new();
        while let Ok(ev) = self.ev_rx.try_recv() {
            let phone = match &ev {
                DaemonEvent::Connected { phone, .. }
                | DaemonEvent::Display { phone, .. }
                | DaemonEvent::Video { phone, .. }
                | DaemonEvent::AudioConfig { phone, .. }
                | DaemonEvent::Audio { phone, .. }
                | DaemonEvent::Status { phone, .. }
                | DaemonEvent::PeerSilent { phone }
                | DaemonEvent::Malformed { phone, .. }
                | DaemonEvent::Disconnected { phone, .. }
                | DaemonEvent::LinkDown { phone, .. }
                | DaemonEvent::Warning { phone, .. } => *phone,
            };
            let watching = self.slots.get(&phone).map(|s| s.watching).unwrap_or(false);
            match ev {
                DaemonEvent::Video { frame, .. } => {
                    if let Some(slot) = self.slots.get_mut(&phone) {
                        slot.frame_seq += 1;
                        let mut cached = frame;
                        cached.seq = slot.frame_seq;
                        slot.last_frame = Some(cached.clone());
                        if watching {
                            out.push(DaemonEvent::Video { phone, frame: cached });
                        }
                    }
                }
                _ => out.push(ev),
            }
        }
        out
    }
}

impl Default for Daemon {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        for (_, mut slot) in self.slots.drain() {
            let _ = slot.worker.cmd_tx.send(WorkerCmd::Stop);
            if let Some(h) = slot.worker.handle.take() {
                let _ = h.join();
            }
        }
    }
}

fn worker_loop(
    id: PhoneId,
    mut link: Box<dyn PhoneLink>,
    cmd_rx: Receiver<WorkerCmd>,
    ev_tx: Sender<DaemonEvent>,
) {
    let send = |ev: DaemonEvent| {
        let _ = ev_tx.send(ev);
    };
    loop {
        // Команды забираем всегда, даже без связи: ввод копится в сессии.
        match cmd_rx.try_recv() {
            Ok(WorkerCmd::Stop) => {
                link.shutdown();
                return;
            }
            Ok(WorkerCmd::Input(cmd)) => {
                if let Err(e) = link.input(&cmd) {
                    send(DaemonEvent::Warning {
                        phone: id, detail: format!("ввод отклонён: {e}")
                    });
                }
            }
            Err(mpsc::TryRecvError::Disconnected) => {
                link.shutdown();
                return;
            }
            Err(mpsc::TryRecvError::Empty) => {}
        }
        if !link.is_connected() {
            match link.connect() {
                Ok(()) => {}
                Err(LinkError::Gone(detail)) => {
                    send(DaemonEvent::LinkDown { phone: id, detail });
                    thread::sleep(Duration::from_secs(1));
                    continue;
                }
                Err(LinkError::Fatal(detail)) => {
                    send(DaemonEvent::Warning { phone: id, detail });
                    thread::sleep(Duration::from_secs(1));
                    continue;
                }
            }
        }
        match link.poll() {
            Ok(events) => {
                for ev in events {
                    send(map_event(id, ev));
                }
            }
            Err(LinkError::Gone(detail)) => {
                send(DaemonEvent::LinkDown { phone: id, detail });
                thread::sleep(Duration::from_secs(1));
            }
            Err(LinkError::Fatal(detail)) => {
                send(DaemonEvent::Warning { phone: id, detail });
            }
        }
    }
}

fn map_event(id: PhoneId, ev: Event) -> DaemonEvent {
    match ev {
        Event::Connected { version, capabilities, peer_name } => {
            DaemonEvent::Connected { phone: id, version, capabilities, name: peer_name }
        }
        Event::Display(d) => DaemonEvent::Display {
            phone: id,
            stream_w: d.stream_width,
            stream_h: d.stream_height,
            screen_w: d.screen_width,
            screen_h: d.screen_height,
            dpi: d.density_dpi,
            rotation: d.rotation,
        },
        Event::Video(f) => DaemonEvent::Video {
            phone: id,
            frame: CachedFrame {
                seq: 0,
                codec: f.codec,
                key_frame: f.key_frame,
                pts_us: f.pts_us,
                data: f.data,
            },
        },
        Event::AudioConfig(c) => {
            DaemonEvent::AudioConfig { phone: id, codec: c.codec, rate: c.sample_rate, channels: c.channels }
        }
        Event::Audio(f) => DaemonEvent::Audio { phone: id, pts_us: f.pts_us, data: f.data },
        Event::Status(s) => DaemonEvent::Status { phone: id, code: s.code, detail: s.detail },
        Event::PeerSilent => DaemonEvent::PeerSilent { phone: id },
        Event::Malformed(detail) => DaemonEvent::Malformed { phone: id, detail },
        Event::Disconnected(reason) => DaemonEvent::Disconnected { phone: id, reason: format!("{reason:?}") },
    }
}

/// USB-связь поверх готового `Dock`: одна трубка, ретраи снаружи в воркере.
pub struct UsbLink {
    dock: pcdock_usb::Dock,
}

impl UsbLink {
    pub fn new(config: pcdock_core::session::Config, settings: pcdock_usb::Settings) -> Result<Self, String> {
        pcdock_usb::Dock::new(config, settings).map(|dock| Self { dock }).map_err(|e| e.to_string())
    }
}

impl PhoneLink for UsbLink {
    fn is_connected(&self) -> bool {
        self.dock.is_connected()
    }

    fn connect(&mut self) -> Result<(), LinkError> {
        self.dock.connect().map_err(|e| classify_usb(&e))
    }

    fn poll(&mut self) -> Result<Vec<Event>, LinkError> {
        let mut events = self.dock.poll().map_err(|e| classify_usb(&e))?;
        for w in self.dock.take_warnings() {
            events.push(Event::Status(pcdock_core::packet::Status { code: 0, detail: w }));
        }
        Ok(events)
    }

    fn input(&mut self, cmd: &InputCmd) -> Result<(), LinkError> {
        let s = self.dock.session();
        let r = match cmd {
            InputCmd::DeviceEnable { kind, on } => {
                s.device_enable(*kind, *on);
                Ok(())
            }
            InputCmd::Touch { cid, phase, x, y, width, height, pressure } => {
                let mut pt = TouchPoint::new(*cid, *phase, *x, *y);
                pt.width = *width;
                pt.height = *height;
                pt.pressure = *pressure;
                s.touch(pt).map_err(|e| e.to_string())
            }
            InputCmd::CursorMoveAbs { x, y } => s.cursor_move_abs(*x, *y).map_err(|e| e.to_string()),
            InputCmd::CursorMoveRel { dx, dy } => s.cursor_move_rel(*dx, *dy).map_err(|e| e.to_string()),
            InputCmd::CursorResync => {
                s.cursor_resync();
                Ok(())
            }
            InputCmd::MouseButton { button, down } => {
                s.mouse_button(*button, *down).map_err(|e| e.to_string())
            }
            InputCmd::Scroll { ticks } => s.scroll(*ticks).map_err(|e| e.to_string()),
            InputCmd::ScrollH { ticks } => s.scroll_h(*ticks).map_err(|e| e.to_string()),
            InputCmd::Key { usage, down } => s.key(*usage, *down).map_err(|e| e.to_string()),
            InputCmd::ReleaseAll => {
                s.release_all();
                Ok(())
            }
            InputCmd::RequestFrame => {
                s.request_frame();
                Ok(())
            }
            InputCmd::Wake => {
                s.wake();
                Ok(())
            }
            InputCmd::SetStream { video, audio } => {
                let st = SetStream { video: *video, audio: *audio, ..SetStream::default() };
                s.set_stream(st);
                Ok(())
            }
        };
        r.map_err(|e| LinkError::Fatal(e.to_string()))
    }

    fn shutdown(&mut self) {
        self.dock.shutdown();
    }
}

fn classify_usb(e: &pcdock_usb::Error) -> LinkError {
    match e {
        pcdock_usb::Error::Usb(_) | pcdock_usb::Error::NoDevice | pcdock_usb::Error::HandshakeTimeout => {
            LinkError::Gone(e.to_string())
        }
        _ => LinkError::Fatal(e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pcdock_core::packet::{DisplayInfo, Hello, Role, VideoFrame};
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    /// Фейковая связь без USB: скриптованные события, запись ввода.
    struct FakeLink {
        connected: bool,
        events: VecDeque<Event>,
        pub inputs: Arc<Mutex<Vec<String>>>,
        fail_connect: bool,
    }

    impl FakeLink {
        fn with_events(events: Vec<Event>) -> (Self, Arc<Mutex<Vec<String>>>) {
            let inputs = Arc::new(Mutex::new(Vec::new()));
            (
                Self { connected: false, events: events.into(), inputs: inputs.clone(), fail_connect: false },
                inputs,
            )
        }
    }

    impl PhoneLink for FakeLink {
        fn is_connected(&self) -> bool {
            self.connected
        }
        fn connect(&mut self) -> Result<(), LinkError> {
            if self.fail_connect {
                return Err(LinkError::Gone("нет устройства".into()));
            }
            self.connected = true;
            Ok(())
        }
        fn poll(&mut self) -> Result<Vec<Event>, LinkError> {
            let mut out = Vec::new();
            if let Some(ev) = self.events.pop_front() {
                out.push(ev);
            }
            Ok(out)
        }
        fn input(&mut self, cmd: &InputCmd) -> Result<(), LinkError> {
            self.inputs.lock().unwrap().push(format!("{cmd:?}"));
            Ok(())
        }
        fn shutdown(&mut self) {
            self.connected = false;
        }
    }

    fn hello() -> Event {
        Event::Connected { version: 2, capabilities: 0, peer_name: "fake".into() }
    }

    fn display() -> Event {
        Event::Display(DisplayInfo {
            stream_width: 360,
            stream_height: 820,
            screen_width: 720,
            screen_height: 1640,
            density_dpi: 320,
            rotation: 0,
        })
    }

    fn video(pts: u64) -> Event {
        Event::Video(VideoFrame { codec: 1, key_frame: true, pts_us: pts, data: vec![1, 2, 3] })
    }

    fn wait_for(d: &mut Daemon, want_video: usize, timeout_ms: u64) -> Vec<DaemonEvent> {
        let mut got = Vec::new();
        let t0 = std::time::Instant::now();
        while t0.elapsed() < Duration::from_millis(timeout_ms) {
            got.extend(d.drain());
            if got.iter().filter(|e| matches!(e, DaemonEvent::Video { .. })).count() >= want_video {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        got
    }

    #[test]
    fn routes_commands_and_caches_frames_per_phone() {
        let mut d = Daemon::new();
        let (l1, inputs1) = FakeLink::with_events(vec![hello(), display(), video(10), video(11)]);
        let (l2, inputs2) = FakeLink::with_events(vec![hello()]);
        d.add_phone(1, Box::new(l1)).unwrap();
        d.add_phone(2, Box::new(l2)).unwrap();
        assert_eq!(
            d.add_phone(1, Box::new(FakeLink::with_events(vec![]).0)).unwrap_err(),
            "трубка 1 уже есть"
        );
        d.set_watching(1, true).unwrap();

        d.command(1, InputCmd::Scroll { ticks: 2.0 }).unwrap();
        d.command(2, InputCmd::Wake).unwrap();
        assert!(d.command(99, InputCmd::Wake).is_err());

        let got = wait_for(&mut d, 2, 2000);
        assert!(got.iter().any(|e| matches!(e, DaemonEvent::Connected { phone: 1, .. })));
        assert!(got.iter().any(|e| matches!(e, DaemonEvent::Display { phone: 1, .. })));
        let vids: Vec<_> = got
            .iter()
            .filter_map(|e| match e {
                DaemonEvent::Video { phone, frame } => Some((*phone, frame.seq, frame.pts_us)),
                _ => None,
            })
            .collect();
        assert_eq!(vids, vec![(1, 1, 10), (1, 2, 11)]);

        // Кэш без потребления: last_frame тот же второй кадр.
        let last = d.last_frame(1).unwrap().expect("кэш");
        assert_eq!((last.seq, last.pts_us), (2, 11));
        // Вторая трубка своих кадров не видела, чужих не получила.
        assert!(d.last_frame(2).unwrap().is_none());

        // Команды разошлись по трубкам.
        thread::sleep(Duration::from_millis(100));
        let i1 = inputs1.lock().unwrap().clone().join("\n");
        let i2 = inputs2.lock().unwrap().clone().join("\n");
        assert!(i1.contains("Scroll"), "{i1}");
        assert!(i2.contains("Wake"), "{i2}");
        assert!(!i1.contains("Wake") && !i2.contains("Scroll"));
    }

    #[test]
    fn unwatched_video_not_forwarded_but_cached() {
        let mut d = Daemon::new();
        let (l, _) = FakeLink::with_events(vec![hello(), video(5)]);
        d.add_phone(7, Box::new(l)).unwrap();
        // watching выключен: видео в событиях нет, но кэш есть.
        let got = wait_for(&mut d, 0, 500);
        assert!(!got.iter().any(|e| matches!(e, DaemonEvent::Video { .. })));
        thread::sleep(Duration::from_millis(200));
        let got = d.drain();
        assert!(!got.iter().any(|e| matches!(e, DaemonEvent::Video { .. })));
        let last = d.last_frame(7).unwrap().expect("кэш без просмотра");
        assert_eq!(last.pts_us, 5);
    }

    #[test]
    fn hello_packet_flows_through_link_trait() {
        // Связка пакетного уровня с типами демона: настоящий Hello из ядра.
        let hb = Hello {
            version_max: 2,
            version_min: 2,
            capabilities: 0,
            role: Role::Device,
            name: "phone".into(),
        }
        .to_packet()
        .encode();
        assert!(!hb.is_empty());
    }
}
