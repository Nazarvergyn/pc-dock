//! Клиент протокола демонов PC Dock (`pcdockd`, `pcdock-autod`) для Rust: TCP и локальная труба.
//!
//! ```no_run
//! use pcdock_client::Client;
//! use serde_json::json;
//!
//! let client = Client::connect_tcp("127.0.0.1:9001", None)?;
//! let phones = client.request(json!({"op": "List"}))?;          // ответ с тем же id
//! client.request(json!({"op": "Watch", "phone": 1, "fps": 2}))?;
//! let events = client.take_events().unwrap();
//! while let Ok(msg) = events.recv() { /* события JSON */ }
//! # Ok::<(), pcdock_client::Error>(())
//! ```

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use interprocess::local_socket::{traits::Stream as _, GenericNamespaced, Stream as LocalStream, ToNsName};
use interprocess::TryClone;
use pcdock_net::{envelope, KIND_JSON};
use serde_json::{json, Value};

/// Версия протокола, с которой умеет работать клиент.
pub const PROTOCOL_VERSION: u64 = 1;

/// Тип бинарного пакета: видеокадр (см. `daemon.md`).
pub const KIND_VIDEO: u8 = 1;
/// Тип бинарного пакета: звук.
pub const KIND_AUDIO: u8 = 2;
/// Тип бинарного пакета: данные файла.
pub const KIND_FILE: u8 = 3;

#[derive(Debug, Clone, PartialEq)]
pub enum Error {
    /// Ошибка соединения.
    Io(String),
    /// Сервер прислал неожиданное.
    Protocol(String),
    /// Сервер ответил ошибкой на запрос.
    Remote(String),
    /// Ответ не пришёл вовремя.
    Timeout,
    /// Соединение закрыто.
    Closed,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "ошибка соединения: {e}"),
            Self::Protocol(e) => write!(f, "ошибка протокола: {e}"),
            Self::Remote(e) => write!(f, "сервер: {e}"),
            Self::Timeout => write!(f, "ответ не пришёл вовремя"),
            Self::Closed => write!(f, "соединение закрыто"),
        }
    }
}

impl std::error::Error for Error {}

/// Видеокадр из бинарного пакета.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VideoFrame {
    pub phone: u32,
    /// Порядковый номер кадра телефона: пропуск значит потерю.
    pub seq: u64,
    pub codec: u8,
    pub key: bool,
    pub pts_us: u64,
    pub jpeg: Vec<u8>,
}

impl VideoFrame {
    /// Разбирает нагрузку видеопакета (без байта типа).
    pub fn parse(payload: &[u8]) -> Option<Self> {
        if payload.len() < 22 {
            return None;
        }
        Some(Self {
            phone: u32::from_le_bytes(payload[0..4].try_into().ok()?),
            seq: u64::from_le_bytes(payload[4..12].try_into().ok()?),
            codec: payload[12],
            key: payload[13] != 0,
            pts_us: u64::from_le_bytes(payload[14..22].try_into().ok()?),
            jpeg: payload[22..].to_vec(),
        })
    }
}

/// Бинарный пакет сервера: тип и нагрузка.
pub type Binary = (u8, Vec<u8>);

type Pending = Arc<Mutex<HashMap<u64, SyncSender<Value>>>>;

struct Inner {
    write: Mutex<Box<dyn Write + Send>>,
    pending: Pending,
    next_id: AtomicU64,
    hello: Value,
    closed: Arc<std::sync::atomic::AtomicBool>,
}

/// Соединение с демоном. Дёшево копируется.
#[derive(Clone)]
pub struct Client {
    inner: Arc<Inner>,
    events: Arc<Mutex<Option<Receiver<Value>>>>,
    binary: Arc<Mutex<Option<Receiver<Binary>>>>,
}

impl Client {
    /// Подключается по TCP. `token` — пароль демона, если он задан.
    pub fn connect_tcp(addr: &str, token: Option<&str>) -> Result<Self, Error> {
        let stream = TcpStream::connect(addr).map_err(|e| Error::Io(e.to_string()))?;
        let _ = stream.set_nodelay(true);
        let reader = stream.try_clone().map_err(|e| Error::Io(e.to_string()))?;
        Self::start(Box::new(reader), Box::new(stream), token)
    }

    /// Подключается к локальной трубе (имя как у `pcdockd --local`).
    pub fn connect_local(name: &str, token: Option<&str>) -> Result<Self, Error> {
        let ns = name.to_ns_name::<GenericNamespaced>().map_err(|e| Error::Io(format!("{e:?}")))?;
        let stream = LocalStream::connect(ns).map_err(|e| Error::Io(e.to_string()))?;
        let reader = stream.try_clone().map_err(|e| Error::Io(e.to_string()))?;
        Self::start(Box::new(reader), Box::new(stream), token)
    }

    fn start(
        mut reader: Box<dyn Read + Send>,
        writer: Box<dyn Write + Send>,
        token: Option<&str>,
    ) -> Result<Self, Error> {
        // Первым приходит приветствие с версией.
        let (kind, body) = read_packet(&mut reader).map_err(|e| Error::Io(e.to_string()))?;
        let hello: Value = serde_json::from_slice(&body).map_err(|e| Error::Protocol(e.to_string()))?;
        if kind != KIND_JSON || hello["ev"] != "Hello" {
            return Err(Error::Protocol(format!("ждали Hello, пришло {hello}")));
        }
        let version = hello["protocol"].as_u64().unwrap_or(0);
        if version != PROTOCOL_VERSION {
            return Err(Error::Protocol(format!(
                "версия протокола сервера {version}, клиент знает {PROTOCOL_VERSION}"
            )));
        }

        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let (events_tx, events_rx) = mpsc::channel::<Value>();
        let (bin_tx, bin_rx) = mpsc::sync_channel::<Binary>(256);
        let closed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        {
            let (pending, closed) = (pending.clone(), closed.clone());
            thread::Builder::new()
                .name("pcdock-client-read".into())
                .spawn(move || {
                    while let Ok((kind, body)) = read_packet(&mut reader) {
                        if kind != KIND_JSON {
                            // Медленному потребителю бинарные пакеты не копятся: лишние отбрасываются.
                            let _ = bin_tx.try_send((kind, body));
                            continue;
                        }
                        let Ok(v) = serde_json::from_slice::<Value>(&body) else { continue };
                        if let Some(id) = v.get("id").and_then(Value::as_u64) {
                            if let Some(tx) = pending.lock().unwrap().remove(&id) {
                                let _ = tx.send(v);
                                continue;
                            }
                        }
                        if events_tx.send(v).is_err() {
                            // Никто не слушает события: читаем дальше ради ответов.
                        }
                    }
                    closed.store(true, Ordering::SeqCst);
                    // Все ждущие получают отказ: отправитель отпускается.
                    pending.lock().unwrap().clear();
                })
                .map_err(|e| Error::Io(e.to_string()))?;
        }
        let client = Self {
            inner: Arc::new(Inner {
                write: Mutex::new(writer),
                pending,
                next_id: AtomicU64::new(1),
                hello,
                closed,
            }),
            events: Arc::new(Mutex::new(Some(events_rx))),
            binary: Arc::new(Mutex::new(Some(bin_rx))),
        };
        if let Some(t) = token {
            client.request(json!({"op": "Auth", "token": t}))?;
        }
        Ok(client)
    }

    /// Приветствие сервера (`{"ev":"Hello","protocol":…,"server":…}`).
    pub fn hello(&self) -> &Value {
        &self.inner.hello
    }

    pub fn is_closed(&self) -> bool {
        self.inner.closed.load(Ordering::SeqCst)
    }

    /// Отправляет команду без ожидания ответа (без `id`).
    pub fn send(&self, msg: Value) -> Result<(), Error> {
        self.write(&msg)
    }

    fn write(&self, msg: &Value) -> Result<(), Error> {
        if self.is_closed() {
            return Err(Error::Closed);
        }
        let bytes = envelope(KIND_JSON, msg.to_string().as_bytes());
        let mut w = self.inner.write.lock().unwrap();
        w.write_all(&bytes).and_then(|_| w.flush()).map_err(|e| Error::Io(e.to_string()))
    }

    /// Отправляет бинарный пакет (например, данные файла).
    pub fn send_binary(&self, kind: u8, payload: &[u8]) -> Result<(), Error> {
        if self.is_closed() {
            return Err(Error::Closed);
        }
        let mut w = self.inner.write.lock().unwrap();
        w.write_all(&envelope(kind, payload)).and_then(|_| w.flush()).map_err(|e| Error::Io(e.to_string()))
    }

    /// Запрос с ответом: сам назначает `id`, ждёт ответ до 10 секунд. Ответ `Error` превращается в `Err(Remote)`.
    pub fn request(&self, msg: Value) -> Result<Value, Error> {
        self.request_timeout(msg, Duration::from_secs(10))
    }

    pub fn request_timeout(&self, mut msg: Value, timeout: Duration) -> Result<Value, Error> {
        let id = self.inner.next_id.fetch_add(1, Ordering::SeqCst);
        let obj = msg.as_object_mut().ok_or_else(|| Error::Protocol("запрос должен быть объектом".into()))?;
        obj.insert("id".into(), id.into());
        let (tx, rx) = mpsc::sync_channel::<Value>(1);
        self.inner.pending.lock().unwrap().insert(id, tx);
        if let Err(e) = self.write(&msg) {
            self.inner.pending.lock().unwrap().remove(&id);
            return Err(e);
        }
        match rx.recv_timeout(timeout) {
            Ok(v) if v["ev"] == "Error" => Err(Error::Remote(v["detail"].as_str().unwrap_or("").to_string())),
            Ok(v) => Ok(v),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                self.inner.pending.lock().unwrap().remove(&id);
                Err(Error::Timeout)
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => Err(Error::Closed),
        }
    }

    /// Получатель событий JSON (сообщения без `id`). Забирается один раз; повторный вызов вернёт `None`.
    pub fn take_events(&self) -> Option<Receiver<Value>> {
        self.events.lock().unwrap().take()
    }

    /// Получатель бинарных пакетов (видео, звук, данные файлов). Забирается один раз.
    pub fn take_binary(&self) -> Option<Receiver<Binary>> {
        self.binary.lock().unwrap().take()
    }
}

fn read_packet(rd: &mut impl Read) -> std::io::Result<(u8, Vec<u8>)> {
    let mut len = [0u8; 4];
    rd.read_exact(&mut len)?;
    let len = u32::from_le_bytes(len) as usize;
    if len == 0 || len > 64 * 1024 * 1024 {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "плохая длина пакета"));
    }
    let mut buf = vec![0u8; len];
    rd.read_exact(&mut buf)?;
    let kind = buf[0];
    buf.remove(0);
    Ok((kind, buf))
}

#[cfg(test)]
mod tests {
    use super::*;
    use pcdock_core::link::{InputCmd, LinkError, PhoneLink};
    use pcdock_core::session::Event;
    use pcdock_farm::Farm;
    use pcdock_serve::Serve;
    use std::collections::VecDeque;

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
            thread::sleep(Duration::from_millis(2));
            Ok(self.events.lock().unwrap().pop_front().into_iter().collect())
        }
        fn input(&mut self, _: &InputCmd) -> Result<(), LinkError> {
            Ok(())
        }
        fn shutdown(&mut self) {}
    }

    fn serve() -> (Serve, Arc<Mutex<VecDeque<Event>>>, std::net::SocketAddr) {
        let events = Arc::new(Mutex::new(VecDeque::new()));
        let mut farm = Farm::new();
        farm.add_phone(1, Box::new(Fake { connected: false, events: events.clone() })).unwrap();
        let serve = Serve::new(farm);
        let addr = serve.run_tcp("127.0.0.1:0").unwrap();
        (serve, events, addr)
    }

    #[test]
    fn request_returns_the_matching_reply_and_remote_errors() {
        let (_s, _e, addr) = serve();
        let c = Client::connect_tcp(&addr.to_string(), None).unwrap();
        assert_eq!(c.hello()["protocol"], 1);
        let r = c.request(json!({"op": "List"})).unwrap();
        assert_eq!(r["phones"], json!([1]));
        let err = c.request(json!({"op": "Wake", "phone": 99})).unwrap_err();
        assert!(matches!(err, Error::Remote(_)), "{err:?}");
    }

    #[test]
    fn frames_arrive_on_the_binary_channel() {
        let (_s, events, addr) = serve();
        let c = Client::connect_tcp(&addr.to_string(), None).unwrap();
        let bin = c.take_binary().unwrap();
        c.request(json!({"op": "Watch", "phone": 1})).unwrap();
        events.lock().unwrap().push_back(Event::Video(pcdock_core::packet::VideoFrame {
            codec: 1,
            key_frame: true,
            pts_us: 5,
            data: vec![1, 2, 3],
        }));
        let (kind, payload) = bin.recv_timeout(Duration::from_secs(3)).unwrap();
        assert_eq!(kind, KIND_VIDEO);
        let f = VideoFrame::parse(&payload).unwrap();
        assert_eq!((f.phone, f.pts_us, f.jpeg), (1, 5, vec![1, 2, 3]));
    }

    #[test]
    fn token_is_sent_on_connect() {
        let events = Arc::new(Mutex::new(VecDeque::new()));
        let mut farm = Farm::new();
        farm.add_phone(1, Box::new(Fake { connected: false, events })).unwrap();
        let serve = Serve::with_token(farm, Some("pw".into()));
        let addr = serve.run_tcp("127.0.0.1:0").unwrap();
        assert!(Client::connect_tcp(&addr.to_string(), Some("bad")).is_err());
        let c = Client::connect_tcp(&addr.to_string(), Some("pw")).unwrap();
        assert_eq!(c.request(json!({"op": "List"})).unwrap()["phones"], json!([1]));
    }
}
