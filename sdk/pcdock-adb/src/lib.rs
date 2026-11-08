//! ADB-транспорт: связь с телефоном через ADB-сервер.
//!
//! Как устроено: демон говорит с локальным ADB-сервером по TCP
//! (127.0.0.1:5037), просит список устройств и пробрасывает порт
//! (`forward`) на сокет приложения в телефоне. Дальше по этому TCP
//! идут те же пакеты v2, что и по USB bulk: сессия ядра разницы не видит.
//!
//! Телефонная сторона (сокет в приложении) — следующим шагом; без неё
//! `connect` честно сообщает, что трубке не во что воткнуться.

use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

use pcdock_core::input::TouchPoint;
use pcdock_core::packet::SetStream;
use pcdock_core::session::{Config, Event, Session, Transmit};
use pcdock_daemon::{InputCmd, LinkError, PhoneLink};

/// Порт приложения в телефоне (ServerSocket, localhost). Один на всех.
pub const PHONE_PORT: u16 = 47631;
/// Таймауты по умолчанию.
pub const IO_TIMEOUT: Duration = Duration::from_millis(500);

/// Минимальный клиент ADB-сервера: `devices` и `forward`. Без шелл-аутов.
pub struct Adb {
    addr: String,
}

impl Adb {
    pub fn new(addr: impl Into<String>) -> Self {
        Self { addr: addr.into() }
    }

    pub fn local() -> Self {
        Self::new("127.0.0.1:5037")
    }

    fn open(&self) -> std::io::Result<TcpStream> {
        let addr = self.addr.to_socket_addrs()?.next().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "плохой адрес ADB-сервера")
        })?;
        let s = TcpStream::connect_timeout(&addr, Duration::from_secs(3))?;
        s.set_read_timeout(Some(Duration::from_secs(5)))?;
        s.set_write_timeout(Some(Duration::from_secs(5)))?;
        Ok(s)
    }

    /// Один запрос к серверу: шлёт `service`, возвращает тело ответа.
    fn request(&self, service: &str) -> Result<String, String> {
        let mut s = self.open().map_err(|e| format!("нет ADB-сервера {}: {e}", self.addr))?;
        send_msg(&mut s, service).map_err(|e| format!("запрос {service}: {e}"))?;
        let status = read_n(&mut s, 4).map_err(|e| format!("статус {service}: {e}"))?;
        if &status == b"FAIL" {
            let body = read_msg(&mut s).unwrap_or_default();
            return Err(format!("ADB {service}: {body}"));
        }
        if &status != b"OKAY" {
            return Err(format!("ADB {service}: плохой статус"));
        }
        read_msg(&mut s).map_err(|e| format!("тело {service}: {e}"))
    }

    /// Серийники устройств в состоянии `device`.
    pub fn devices(&self) -> Result<Vec<String>, String> {
        let body = self.request("host:devices")?;
        Ok(body
            .lines()
            .filter_map(|l| {
                let mut it = l.split_whitespace();
                match (it.next(), it.next()) {
                    (Some(s), Some("device")) => Some(s.to_owned()),
                    _ => None,
                }
            })
            .collect())
    }

    /// Проброс `tcp:local -> tcp:remote` для трубки. Пустой ответ = ок.
    pub fn forward(&self, serial: &str, local_port: u16, remote_port: u16) -> Result<(), String> {
        let _ = self.request(&format!("host:transport:{serial}"))?;
        self.request(&format!("forward:tcp:{local_port};tcp:{remote_port}")).map(|_| ())
    }

    /// Снять проброс.
    pub fn kill_forward(&self, serial: &str, local_port: u16) -> Result<(), String> {
        let _ = self.request(&format!("host:transport:{serial}"))?;
        self.request(&format!("killforward:tcp:{local_port}")).map(|_| ())
    }
}

fn send_msg(s: &mut TcpStream, body: &str) -> std::io::Result<()> {
    s.write_all(format!("{:04X}{body}", body.len()).as_bytes())
}

fn read_n(s: &mut TcpStream, n: usize) -> std::io::Result<Vec<u8>> {
    let mut buf = vec![0u8; n];
    s.read_exact(&mut buf)?;
    Ok(buf)
}

fn read_msg(s: &mut TcpStream) -> std::io::Result<String> {
    let len = read_n(s, 4)?;
    let len = usize::from_str_radix(&String::from_utf8_lossy(&len), 16)
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidData, "длина ADB"))?;
    Ok(String::from_utf8_lossy(&read_n(s, len)?).into_owned())
}

/// Связь с одной трубкой по ADB: сессия ядра + TCP через проброс.
/// Насос как в USB-`Dock`, только внизу сокет, а не bulk-эндпоинты.
pub struct AdbLink {
    session: Session,
    adb: Adb,
    serial: String,
    local_port: u16,
    stream: Option<TcpStream>,
    buf: Vec<u8>,
    t0: std::time::Instant,
}

impl AdbLink {
    pub fn new(config: Config, serial: impl Into<String>, local_port: u16) -> Self {
        Self {
            session: Session::new(config),
            adb: Adb::local(),
            serial: serial.into(),
            local_port,
            stream: None,
            buf: vec![0u8; 65536],
            t0: std::time::Instant::now(),
        }
    }

    /// Конструктор для тестов: готовый сокет вместо проброса.
    #[cfg(test)]
    fn with_stream(session: Session, stream: TcpStream) -> Self {
        Self {
            session,
            adb: Adb::local(),
            serial: "test".into(),
            local_port: 0,
            stream: Some(stream),
            buf: vec![0u8; 65536],
            t0: std::time::Instant::now(),
        }
    }

    fn now_ms(&self) -> u64 {
        self.t0.elapsed().as_millis() as u64
    }

    /// Отправляет всё, что накопило ядро.
    fn flush(&mut self) -> Result<(), LinkError> {
        while let Some(tx) = self.session.poll_transmit() {
            let s = self.stream.as_mut().ok_or_else(|| LinkError::Gone("нет сокета".into()))?;
            match tx {
                Transmit::Stream(bytes) => {
                    s.write_all(&bytes).map_err(|e| LinkError::Gone(format!("запись: {e}")))?;
                }
                Transmit::Control(req) => {
                    // Control-запросы (HID) по ADB идут тем же каналом:
                    // телефонная сторона выполняет их как HID-события.
                    // Формат: [0xC0][request u8][value u16LE][index u16LE][len u16LE][data].
                    let mut pkt = Vec::with_capacity(8 + req.data.len());
                    pkt.push(0xC0);
                    pkt.push(req.request);
                    pkt.extend_from_slice(&req.value.to_le_bytes());
                    pkt.extend_from_slice(&req.index.to_le_bytes());
                    pkt.extend_from_slice(&(req.data.len() as u16).to_le_bytes());
                    pkt.extend_from_slice(&req.data);
                    s.write_all(&pkt).map_err(|e| LinkError::Gone(format!("запись control: {e}")))?;
                }
            }
        }
        Ok(())
    }
}

impl PhoneLink for AdbLink {
    fn is_connected(&self) -> bool {
        self.stream.is_some()
    }

    fn connect(&mut self) -> Result<(), LinkError> {
        if self.stream.is_some() {
            return Ok(());
        }
        self.adb.forward(&self.serial, self.local_port, PHONE_PORT).map_err(LinkError::Fatal)?;
        let addr = format!("127.0.0.1:{}", self.local_port);
        let s = TcpStream::connect_timeout(
            &addr
                .to_socket_addrs()
                .map_err(|e| LinkError::Fatal(format!("проброс {addr}: {e}")))?
                .next()
                .ok_or_else(|| LinkError::Fatal(format!("проброс {addr}: нет адреса")))?,
            Duration::from_secs(5),
        )
        .map_err(|e| LinkError::Gone(format!("телефон не слушает {addr}: {e}")))?;
        s.set_read_timeout(Some(IO_TIMEOUT)).map_err(|e| LinkError::Fatal(e.to_string()))?;
        s.set_write_timeout(Some(IO_TIMEOUT)).map_err(|e| LinkError::Fatal(e.to_string()))?;
        self.stream = Some(s);
        self.session.start(self.now_ms());
        self.flush()?;
        Ok(())
    }

    fn poll(&mut self) -> Result<Vec<Event>, LinkError> {
        self.flush()?;
        let s = self.stream.as_mut().ok_or_else(|| LinkError::Gone("нет сокета".into()))?;
        let n = match s.read(&mut self.buf) {
            Ok(n) => n,
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                0
            }
            Err(e) => return Err(LinkError::Gone(format!("чтение: {e}"))),
        };
        let now = self.now_ms();
        if n > 0 {
            self.session.handle_input(now, &self.buf[..n]);
        }
        if self.session.next_timeout().is_some_and(|t| now >= t) {
            self.session.handle_timeout(now);
        }
        self.flush()?;
        let mut events = Vec::new();
        while let Some(ev) = self.session.poll_event() {
            events.push(ev);
        }
        Ok(events)
    }

    fn input(&mut self, cmd: &InputCmd) -> Result<(), LinkError> {
        let s = &mut self.session;
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
                s.set_stream(SetStream { video: *video, audio: *audio, ..SetStream::default() });
                Ok(())
            }
        };
        r.map_err(LinkError::Fatal)
    }

    fn shutdown(&mut self) {
        self.session.close();
        self.stream = None;
        let _ = self.adb.kill_forward(&self.serial, self.local_port);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pcdock_core::input::{MouseButton, TouchPhase};
    use pcdock_core::packet::{Hello, Role};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    /// Фейковый ADB-сервер: отвечает OKAY на devices/transport/forward.
    fn fake_adb(devices: &str) -> (String, Arc<Mutex<Vec<String>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let seen2 = seen.clone();
        let body = devices.to_owned();
        std::thread::spawn(move || {
            for stream in listener.incoming().take(8) {
                let mut s = match stream {
                    Ok(s) => s,
                    Err(_) => break,
                };
                s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
                let len = match read_n(&mut s, 4) {
                    Ok(l) => l,
                    Err(_) => break,
                };
                let len = usize::from_str_radix(&String::from_utf8_lossy(&len), 16).unwrap_or(0);
                let mut svc = vec![0u8; len];
                if s.read_exact(&mut svc).is_err() {
                    break;
                }
                let svc = String::from_utf8_lossy(&svc).into_owned();
                seen2.lock().unwrap().push(svc.clone());
                let reply = if svc == "host:devices" { body.clone() } else { String::new() };
                let _ = s.write_all(b"OKAY");
                let _ = s.write_all(format!("{:04X}{reply}", reply.len()).as_bytes());
            }
        });
        (addr, seen)
    }

    #[test]
    fn adb_client_lists_and_forwards() {
        let (addr, seen) = fake_adb("ABC123\tdevice\nXYZ\tunauthorized\n");
        let adb = Adb::new(addr);
        assert_eq!(adb.devices().unwrap(), vec!["ABC123"]);
        adb.forward("ABC123", 45631, PHONE_PORT).unwrap();
        adb.kill_forward("ABC123", 45631).unwrap();
        let got = seen.lock().unwrap().join("\n");
        assert!(got.contains("host:devices"), "{got}");
        assert!(got.contains("host:transport:ABC123"), "{got}");
        assert!(got.contains("forward:tcp:45631"), "{got}");
        assert!(got.contains("killforward:tcp:45631"), "{got}");
    }

    #[test]
    fn adb_link_pumps_session_over_tcp() {
        // Пара сокетов: один конец у связи, другой играет телефон.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let phone = std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            s.set_read_timeout(Some(Duration::from_millis(800))).unwrap();
            // Читаем HELLO от хоста, отвечаем своим HELLO.
            let mut buf = [0u8; 4096];
            let n = s.read(&mut buf).unwrap();
            assert!(n > 0);
            let hb = Hello {
                version_max: 2,
                version_min: 2,
                capabilities: 0,
                role: Role::Device,
                name: "phone".into(),
            }
            .to_packet()
            .encode();
            s.write_all(&hb).unwrap();
            // Держим сокет открытым, пока хост качает ввод: вычитываем всё
            // до закрытия хостом (Ok(0)) или нескольких таймаутов подряд.
            let mut idle = 0;
            loop {
                match s.read(&mut buf) {
                    Ok(0) => break,
                    Ok(_) => idle = 0,
                    Err(e)
                        if e.kind() == std::io::ErrorKind::TimedOut
                            || e.kind() == std::io::ErrorKind::WouldBlock =>
                    {
                        idle += 1;
                        if idle >= 10 {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });
        let stream = TcpStream::connect(addr).unwrap();
        stream.set_read_timeout(Some(IO_TIMEOUT)).unwrap();
        let mut link = AdbLink::with_stream(Session::new(Config::default()), stream);
        link.session.start(1000);
        // Насос: HELLO ушёл, ответ разобран — сессия активна.
        link.poll().unwrap();
        link.poll().unwrap();
        assert!(link.is_connected());
        // Ввод уходит без ошибок (кнопка мыши при включённой мыши по умолчанию).
        link.input(&InputCmd::MouseButton { button: MouseButton::Left, down: true }).unwrap();
        link.input(&InputCmd::Scroll { ticks: 1.0 }).unwrap();
        link.input(&InputCmd::Touch {
            cid: 0,
            phase: TouchPhase::Down,
            x: 0.5,
            y: 0.5,
            width: None,
            height: None,
            pressure: None,
        })
        .unwrap();
        link.poll().unwrap();
        link.shutdown();
        assert!(!link.is_connected());
        phone.join().unwrap();
    }
}
