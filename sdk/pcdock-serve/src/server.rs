//! Сервер связи: TCP, веб-сокеты, unix-сокет / именованный пайп.
//!
//! Один протокол везде (см. `protocol`): JSON-команды туда, JSON-события и бинарные кадры оттуда.
//!
//! Устройство раздачи:
//! - у каждого клиента свой исходящий ящик ([`Outbox`]): события JSON не теряются, звук ограничен очередью,
//!   видео хранится одним слотом «последний кадр» (быстрый клиент получает каждый кадр, медленный всегда самый
//!   свежий, а не очередь старых);
//! - подписка на телефон несёт условия (частота, только при изменении, звук); кадры отбираются для каждого клиента
//!   отдельно, видео на телефоне идёт по спросу: нет подписчиков — выключено, иначе с частотой самого
//!   требовательного;
//! - клиент, чьи события копятся без чтения, отключается: он не может задержать остальных.

use std::collections::{HashMap, VecDeque};
use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use interprocess::local_socket::{self, traits::ListenerExt, ListenerOptions, ToNsName};
use interprocess::TryClone;
use pcdock_core::input::{DeviceKind, MouseButton, TouchPhase};
use pcdock_core::link::{InputCmd, PhoneId, PhoneState};
use pcdock_farm::{Farm, FarmEvent};
use tungstenite::Message;

use super::protocol::{
    encode_audio, encode_envelope, encode_video, ClientMsg, Request, ServerMsg, PKG_AUDIO, PKG_JSON,
    PKG_VIDEO,
};

/// Версия протокола приложения (приходит клиенту в `Hello`).
pub const PROTOCOL_VERSION: u32 = 1;

/// Сколько событий JSON (штук и байт) может накопиться у клиента, прежде чем он будет отключён как не читающий.
const JSON_QUEUE_LIMIT: usize = 4096;
const JSON_QUEUE_BYTES: usize = 2 * 1024 * 1024;
/// Сколько пакетов звука держим для клиента; лишние отбрасываются.
const AUDIO_QUEUE_LIMIT: usize = 256;

/// Исходящее сообщение клиенту.
#[derive(Clone, Debug)]
enum OutMsg {
    Json(String),
    Video(Vec<u8>),
    Audio(Vec<u8>),
}

#[derive(Default)]
struct OutboxState {
    json: VecDeque<String>,
    json_bytes: usize,
    audio: VecDeque<Vec<u8>>,
    video: Option<Vec<u8>>,
    closed: bool,
}

/// Исходящий ящик клиента. Порядок выдачи: события JSON, затем кадр видео, затем звук.
#[derive(Default)]
struct Outbox {
    state: Mutex<OutboxState>,
    ready: Condvar,
}

impl Outbox {
    /// Кладёт событие. `false` — ящик переполнен: клиент не читает и будет отключён.
    fn json(&self, text: String) -> bool {
        let mut st = self.state.lock().unwrap();
        if st.closed {
            return false;
        }
        if st.json.len() >= JSON_QUEUE_LIMIT || st.json_bytes + text.len() > JSON_QUEUE_BYTES {
            st.closed = true;
            self.ready.notify_all();
            return false;
        }
        st.json_bytes += text.len();
        st.json.push_back(text);
        self.ready.notify_one();
        true
    }

    fn audio(&self, payload: Vec<u8>) {
        let mut st = self.state.lock().unwrap();
        if st.closed {
            return;
        }
        if st.audio.len() >= AUDIO_QUEUE_LIMIT {
            st.audio.pop_front();
        }
        st.audio.push_back(payload);
        self.ready.notify_one();
    }

    /// Заменяет слот видео: незабранный кадр заменяется новым.
    fn video(&self, payload: Vec<u8>) {
        let mut st = self.state.lock().unwrap();
        if st.closed {
            return;
        }
        st.video = Some(payload);
        self.ready.notify_one();
    }

    fn close(&self) {
        self.state.lock().unwrap().closed = true;
        self.ready.notify_all();
    }

    fn take(st: &mut OutboxState) -> Option<OutMsg> {
        if let Some(j) = st.json.pop_front() {
            st.json_bytes -= j.len();
            return Some(OutMsg::Json(j));
        }
        if let Some(v) = st.video.take() {
            return Some(OutMsg::Video(v));
        }
        st.audio.pop_front().map(OutMsg::Audio)
    }

    /// Ждёт сообщение. `None` — ящик закрыт.
    fn pop_blocking(&self) -> Option<OutMsg> {
        let mut st = self.state.lock().unwrap();
        loop {
            if st.closed {
                return None;
            }
            if let Some(m) = Self::take(&mut st) {
                return Some(m);
            }
            st = self.ready.wait(st).unwrap();
        }
    }

    /// Берёт сообщение, если оно есть (для писателя, который ещё и читает).
    fn pop_now(&self) -> Option<OutMsg> {
        let mut st = self.state.lock().unwrap();
        if st.closed {
            return None;
        }
        Self::take(&mut st)
    }

    fn is_closed(&self) -> bool {
        self.state.lock().unwrap().closed
    }
}

/// Условия подписки клиента на телефон.
struct Watch {
    /// Не чаще стольких кадров в секунду; `None` — все кадры.
    max_fps: Option<f32>,
    on_change: bool,
    audio: bool,
    last_sent: Option<Instant>,
    last_hash: Option<u64>,
}

struct Client {
    out: Arc<Outbox>,
    watches: HashMap<PhoneId, Watch>,
    /// Прошёл ли клиент проверку токена (без токена демона — всегда да).
    authed: bool,
}

type Clients = Arc<Mutex<HashMap<u64, Client>>>;

/// Что сейчас запрошено у телефона: видео, звук и частота (0 — без ограничения).
type Demand = (bool, bool, u8);

/// Сервер поверх демона. Потокобезопасен через `Arc`: слушатели и клиенты
/// живут в своих потоках, демон дёргается под мьютексом.
#[derive(Clone)]
pub struct Serve {
    daemon: Arc<Mutex<Farm>>,
    clients: Clients,
    next_id: Arc<Mutex<u64>>,
    token: Option<Arc<str>>,
    demand: Arc<Mutex<HashMap<PhoneId, Demand>>>,
}

/// Хэш кадра для условия `on_change` (FNV-1a).
fn frame_hash(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in data {
        h = (h ^ u64::from(*b)).wrapping_mul(0x100000001b3);
    }
    h
}

impl Serve {
    pub fn new(daemon: Farm) -> Self {
        Self::with_token(daemon, None)
    }

    /// С токеном: клиент первым сообщением шлёт `{"op":"Auth","token":"…"}`, до этого ему не отвечают
    /// и событий не присылают.
    pub fn with_token(daemon: Farm, token: Option<String>) -> Self {
        let serve = Self {
            daemon: Arc::new(Mutex::new(daemon)),
            clients: Arc::new(Mutex::new(HashMap::new())),
            next_id: Arc::new(Mutex::new(1)),
            token: token.filter(|t| !t.is_empty()).map(Arc::from),
            demand: Arc::new(Mutex::new(HashMap::new())),
        };
        serve.spawn_pump();
        serve
    }

    /// Доступ к демону (добавить трубку, положить команду напрямую).
    pub fn daemon(&self) -> &Arc<Mutex<Farm>> {
        &self.daemon
    }

    fn add_client(&self, out: Arc<Outbox>) -> u64 {
        let mut next = self.next_id.lock().unwrap();
        let id = *next;
        *next += 1;
        let authed = self.token.is_none();
        let hello = ServerMsg::Hello {
            protocol: PROTOCOL_VERSION,
            server: format!("pcdockd/{}", env!("CARGO_PKG_VERSION")),
        };
        out.json(to_json(&hello, None));
        self.clients.lock().unwrap().insert(id, Client { out, watches: HashMap::new(), authed });
        id
    }

    /// Убирает клиента: закрывает его ящик и снимает его подписки (видео на телефоне выключается, если
    /// больше никто не смотрит).
    fn remove_client(&self, id: u64) {
        let removed = self.clients.lock().unwrap().remove(&id);
        if let Some(c) = removed {
            c.out.close();
            for phone in c.watches.keys() {
                self.refresh_demand(*phone, false);
            }
        }
    }

    /// Пересчитывает, что нужно телефону, по подпискам всех клиентов и сообщает ему, если изменилось
    /// (или если `force`: телефон только что подключился и ничего не знает).
    fn refresh_demand(&self, phone: PhoneId, force: bool) {
        let (video, audio, fps) = {
            let clients = self.clients.lock().unwrap();
            let mut video = false;
            let mut audio = false;
            let mut unlimited = false;
            let mut max_fps = 0.0f32;
            for c in clients.values() {
                if let Some(w) = c.watches.get(&phone) {
                    video = true;
                    audio |= w.audio;
                    match w.max_fps {
                        Some(f) => max_fps = max_fps.max(f),
                        None => unlimited = true,
                    }
                }
            }
            let fps = if !video || unlimited { 0 } else { max_fps.ceil().clamp(1.0, 255.0) as u8 };
            (video, audio, fps)
        };
        {
            let mut demand = self.demand.lock().unwrap();
            let now = (video, audio, fps);
            if !force && demand.get(&phone) == Some(&now) {
                return;
            }
            demand.insert(phone, now);
        }
        let mut daemon = self.daemon.lock().unwrap();
        let _ = daemon.set_watching(phone, video);
        let _ = daemon.command(phone, InputCmd::SetStream { video, audio, max_fps: fps });
    }

    fn reply(&self, client: u64, id: Option<u64>, msg: ServerMsg) {
        let text = to_json(&msg, id);
        let overflow = match self.clients.lock().unwrap().get(&client) {
            Some(c) => !c.out.json(text),
            None => false,
        };
        if overflow {
            self.remove_client(client);
        }
    }

    /// Разбирает команду клиента. Отвечает напрямую в его ящик.
    fn handle(&self, client: u64, req: Request) {
        let Request { id, msg } = req;
        let fail = |detail: String| self.reply(client, id, ServerMsg::Error { detail });
        if let ClientMsg::Auth { token } = &msg {
            let ok = match &self.token {
                Some(want) => constant_time_eq(want.as_bytes(), token.as_bytes()),
                None => true,
            };
            if ok {
                if let Some(c) = self.clients.lock().unwrap().get_mut(&client) {
                    c.authed = true;
                }
                return self.reply(client, id, ServerMsg::Authed);
            }
            // Подбор пароля не должен идти быстро.
            thread::sleep(Duration::from_millis(500));
            return fail("неверный токен".into());
        }
        let authed = self.clients.lock().unwrap().get(&client).is_some_and(|c| c.authed);
        if !authed {
            return fail("нужна авторизация: первым пошлите op=Auth".into());
        }
        match msg {
            ClientMsg::List => {
                let phones = self.daemon.lock().unwrap().phones();
                self.reply(client, id, ServerMsg::Phones { phones })
            }
            ClientMsg::Watch { phone, fps, on_change, audio } => {
                if !self.daemon.lock().unwrap().phones().contains(&phone) {
                    return fail(format!("нет трубки {phone}"));
                }
                if fps.is_some_and(|f| !(f > 0.0 && f.is_finite())) {
                    return fail("плохая команда: fps должен быть больше 0".into());
                }
                if let Some(c) = self.clients.lock().unwrap().get_mut(&client) {
                    c.watches.insert(
                        phone,
                        Watch { max_fps: fps, on_change, audio, last_sent: None, last_hash: None },
                    );
                }
                self.refresh_demand(phone, false);
                self.reply(client, id, ServerMsg::Watched { phone, on: true });
                // Новый подписчик сразу получает последний кадр, чтобы не ждать изменения экрана.
                self.send_snapshot(client, phone);
            }
            ClientMsg::Unwatch { phone } => {
                if let Some(c) = self.clients.lock().unwrap().get_mut(&client) {
                    c.watches.remove(&phone);
                }
                self.refresh_demand(phone, false);
                self.reply(client, id, ServerMsg::Watched { phone, on: false });
            }
            ClientMsg::Snapshot { phone } => {
                if !self.send_snapshot(client, phone) {
                    fail(format!("нет кадра у трубки {phone}"));
                } else if id.is_some() {
                    self.reply(client, id, ServerMsg::Ok);
                }
            }
            other => {
                let (phone, cmd) = match map_input(other) {
                    Ok(v) => v,
                    Err(detail) => return fail(detail),
                };
                match self.daemon.lock().unwrap().command(phone, cmd) {
                    Ok(()) => {
                        if id.is_some() {
                            self.reply(client, id, ServerMsg::Ok);
                        }
                    }
                    Err(e) => fail(e),
                }
            }
        }
    }

    /// Последний кадр телефона из кэша — одному клиенту. `false`, если кадра нет.
    fn send_snapshot(&self, client: u64, phone: PhoneId) -> bool {
        let frame = self.daemon.lock().unwrap().last_frame(phone).ok().flatten();
        let Some(f) = frame else { return false };
        let payload = encode_video(phone, f.seq, f.codec, f.key_frame, f.pts_us, &f.data);
        if let Some(c) = self.clients.lock().unwrap().get(&client) {
            c.out.video(payload);
        }
        true
    }

    /// Поток раздачи: забирает события фермы и рассылает их по подпискам.
    fn spawn_pump(&self) {
        let me = self.clone();
        thread::Builder::new()
            .name("serve-pump".into())
            .spawn(move || loop {
                let events = me.daemon.lock().unwrap().drain();
                for ev in events {
                    if let FarmEvent::Connected { phone, .. } = &ev {
                        // Телефон (пере)подключился и снова не знает, что от него нужно.
                        me.refresh_demand(*phone, true);
                    }
                    me.route(ev);
                }
                thread::sleep(Duration::from_millis(5));
            })
            .expect("не встал поток раздач");
    }

    /// Маршрутизация события фермы по подпискам. Событие, которое не помещается в ящик не читающего клиента,
    /// отключает этого клиента.
    fn route(&self, ev: FarmEvent) {
        let mut overflowed: Vec<u64> = Vec::new();
        {
            let mut map = self.clients.lock().unwrap();
            match ev {
                FarmEvent::Video { phone, frame } => {
                    let payload = encode_video(
                        phone,
                        frame.seq,
                        frame.codec,
                        frame.key_frame,
                        frame.pts_us,
                        &frame.data,
                    );
                    let now = Instant::now();
                    let mut hash = None;
                    for c in map.values_mut().filter(|c| c.authed) {
                        let Some(w) = c.watches.get_mut(&phone) else { continue };
                        if let (Some(fps), Some(t)) = (w.max_fps, w.last_sent) {
                            // Небольшой допуск, чтобы 30 к/с из 30 к/с не терялись на дрожании времени.
                            if now.duration_since(t).as_secs_f32() < 0.9 / fps {
                                continue;
                            }
                        }
                        if w.on_change {
                            let h = *hash.get_or_insert_with(|| frame_hash(&frame.data));
                            if w.last_hash == Some(h) {
                                continue;
                            }
                            w.last_hash = Some(h);
                        }
                        w.last_sent = Some(now);
                        c.out.video(payload.clone());
                    }
                }
                FarmEvent::Audio { phone, pts_us, data } => {
                    let payload = encode_audio(phone, pts_us, &data);
                    for c in map.values().filter(|c| c.authed) {
                        if c.watches.get(&phone).is_some_and(|w| w.audio) {
                            c.out.audio(payload.clone());
                        }
                    }
                }
                // Обнаружение и состояния касаются всех клиентов: подписки на телефон ещё может не быть.
                ev @ (FarmEvent::Appeared { .. } | FarmEvent::State { .. } | FarmEvent::Lost { .. }) => {
                    let (_, msg) = map_daemon_event(ev);
                    let text = to_json(&msg, None);
                    for (id, c) in map.iter().filter(|(_, c)| c.authed) {
                        if !c.out.json(text.clone()) {
                            overflowed.push(*id);
                        }
                    }
                }
                other => {
                    let (phone, msg) = map_daemon_event(other);
                    let text = to_json(&msg, None);
                    for (id, c) in map.iter().filter(|(_, c)| c.authed && c.watches.contains_key(&phone)) {
                        if !c.out.json(text.clone()) {
                            overflowed.push(*id);
                        }
                    }
                }
            }
        }
        for id in overflowed {
            self.remove_client(id);
        }
    }

    /// Читает пакеты клиента из потока и передаёт их на разбор. Общий код TCP и локальной трубы.
    fn read_loop(&self, id: u64, rd: &mut impl Read) {
        loop {
            let mut len = [0u8; 4];
            if rd.read_exact(&mut len).is_err() {
                break;
            }
            let len = u32::from_le_bytes(len) as usize;
            if len == 0 || len > 8 * 1024 * 1024 {
                break;
            }
            let mut buf = vec![0u8; len];
            if rd.read_exact(&mut buf).is_err() {
                break;
            }
            if buf[0] != PKG_JSON {
                continue;
            }
            self.on_text(id, &buf[1..]);
        }
        self.remove_client(id);
    }

    fn on_text(&self, id: u64, body: &[u8]) {
        match serde_json::from_slice::<Request>(body) {
            Ok(req) => self.handle(id, req),
            Err(e) => self.reply(id, None, ServerMsg::Error { detail: format!("плохая команда: {e}") }),
        }
    }

    /// TCP-слушатель: свой поток, на клиента — два потока (чтение/запись).
    pub fn run_tcp(&self, addr: &str) -> std::io::Result<std::net::SocketAddr> {
        let listener = TcpListener::bind(addr)?;
        let local = listener.local_addr()?;
        let me = self.clone();
        thread::Builder::new()
            .name(format!("tcp-{local}"))
            .spawn(move || {
                for s in listener.incoming().flatten() {
                    me.serve_tcp(s);
                }
            })
            .expect("не встал TCP-слушатель");
        Ok(local)
    }

    fn serve_tcp(&self, stream: TcpStream) {
        let out = Arc::new(Outbox::default());
        let id = self.add_client(out.clone());
        let me = self.clone();
        let mut rd = stream.try_clone().expect("клонирование TCP-потока");
        thread::Builder::new()
            .name(format!("tcp-r-{id}"))
            .spawn(move || me.read_loop(id, &mut rd))
            .expect("не встал поток чтения TCP");
        thread::Builder::new()
            .name(format!("tcp-w-{id}"))
            .spawn(move || {
                let mut wr = stream;
                while let Some(msg) = out.pop_blocking() {
                    if wr.write_all(&encode_out(&msg)).is_err() {
                        break;
                    }
                }
                // Отключённого клиента (не читал события) отпускаем и на уровне сокета.
                out.close();
                let _ = wr.shutdown(Shutdown::Both);
            })
            .expect("не встал поток записи TCP");
    }

    /// Веб-сокеты для браузеров: текст — JSON, бинарные фреймы — `[тип][нагрузка]` (тип 1 видео, 2 звук).
    pub fn run_ws(&self, addr: &str) -> std::io::Result<std::net::SocketAddr> {
        let listener = TcpListener::bind(addr)?;
        let local = listener.local_addr()?;
        let me = self.clone();
        thread::Builder::new()
            .name(format!("ws-{local}"))
            .spawn(move || {
                for s in listener.incoming().flatten() {
                    let me2 = me.clone();
                    thread::spawn(move || {
                        let mut ws = match tungstenite::accept(s) {
                            Ok(ws) => ws,
                            Err(_) => return,
                        };
                        // Один поток на обе стороны: чтение с таймаутом, запись сливом ящика. Отдельный писатель
                        // невозможен: чтение держало бы Mutex сокета.
                        let _ = ws.get_ref().set_read_timeout(Some(Duration::from_millis(20)));
                        let out = Arc::new(Outbox::default());
                        let id = me2.add_client(out.clone());
                        loop {
                            match ws.read() {
                                Ok(Message::Text(text)) => me2.on_text(id, text.as_bytes()),
                                Ok(Message::Close(_)) => break,
                                Ok(_) => {}
                                Err(tungstenite::Error::Io(e))
                                    if e.kind() == std::io::ErrorKind::TimedOut
                                        || e.kind() == std::io::ErrorKind::WouldBlock => {}
                                Err(_) => break,
                            }
                            if out.is_closed() {
                                break;
                            }
                            while let Some(msg) = out.pop_now() {
                                let frame = match msg {
                                    OutMsg::Json(text) => Message::Text(text),
                                    OutMsg::Video(p) => Message::Binary(typed(PKG_VIDEO, p)),
                                    OutMsg::Audio(p) => Message::Binary(typed(PKG_AUDIO, p)),
                                };
                                if ws.send(frame).is_err() {
                                    me2.remove_client(id);
                                    return;
                                }
                            }
                        }
                        me2.remove_client(id);
                    });
                }
            })
            .expect("не встал WS-слушатель");
        Ok(local)
    }
}

/// Бинарный фрейм WebSocket: байт типа и нагрузка.
fn typed(kind: u8, mut payload: Vec<u8>) -> Vec<u8> {
    payload.insert(0, kind);
    payload
}

fn encode_out(msg: &OutMsg) -> Vec<u8> {
    match msg {
        OutMsg::Json(text) => encode_envelope(PKG_JSON, text.as_bytes()),
        OutMsg::Video(payload) => encode_envelope(PKG_VIDEO, payload),
        OutMsg::Audio(payload) => encode_envelope(PKG_AUDIO, payload),
    }
}

/// JSON сообщения; если задан `id` запроса, он добавляется в объект.
fn to_json(msg: &ServerMsg, id: Option<u64>) -> String {
    let mut v =
        serde_json::to_value(msg).unwrap_or_else(|_| serde_json::json!({"ev": "Error", "detail": "encode"}));
    if let (Some(id), Some(obj)) = (id, v.as_object_mut()) {
        obj.insert("id".into(), id.into());
    }
    v.to_string()
}

/// Локальная труба: unix-сокет на Linux, именованный пайп на Windows.
/// Один код на обе платформы через `interprocess` (имена в пространстве имён ОС).
impl Serve {
    /// Локальный слушатель: имя в пространстве имён ОС (файл сокета / пайп).
    pub fn run_local(&self, name: &str) -> std::io::Result<()> {
        // Имя должно жить вечно: слушатель уезжает в поток.
        let owned: &'static str = Box::leak(name.to_owned().into_boxed_str());
        let ns = owned.to_ns_name::<local_socket::GenericNamespaced>().map_err(|e| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, format!("плохое имя трубы: {e:?}"))
        })?;
        let listener = ListenerOptions::new().name(ns).create_sync()?;
        let me = self.clone();
        thread::Builder::new()
            .name("local".into())
            .spawn(move || {
                for s in listener.incoming().flatten() {
                    me.serve_stream(s);
                }
            })
            .expect("не встал локальный слушатель");
        Ok(())
    }

    fn serve_stream(&self, mut stream: interprocess::local_socket::Stream) {
        let out = Arc::new(Outbox::default());
        let id = self.add_client(out.clone());
        let me = self.clone();
        let mut rd = stream.try_clone().expect("клонирование локального потока");
        thread::Builder::new()
            .name(format!("local-r-{id}"))
            .spawn(move || me.read_loop(id, &mut rd))
            .expect("не встал поток чтения");
        thread::Builder::new()
            .name(format!("local-w-{id}"))
            .spawn(move || {
                while let Some(msg) = out.pop_blocking() {
                    if stream.write_all(&encode_out(&msg)).is_err() {
                        break;
                    }
                }
                out.close();
            })
            .expect("не встал поток записи");
    }
}

fn state_name(s: PhoneState) -> &'static str {
    match s {
        PhoneState::Appeared => "appeared",
        PhoneState::Connecting => "connecting",
        PhoneState::Ready => "ready",
        PhoneState::NeedsPermission => "needs_permission",
        PhoneState::NeedsApp => "needs_app",
        PhoneState::NeedsAccess => "needs_access",
        PhoneState::NeedsAuth => "needs_auth",
        PhoneState::Lost => "lost",
    }
}

fn map_daemon_event(ev: FarmEvent) -> (PhoneId, ServerMsg) {
    match ev {
        FarmEvent::Connected { phone, version, capabilities, name } => {
            (phone, ServerMsg::Connected { phone, version, caps: capabilities, name })
        }
        FarmEvent::Display { phone, stream_w, stream_h, screen_w, screen_h, dpi, rotation } => {
            (phone, ServerMsg::Display { phone, stream_w, stream_h, screen_w, screen_h, dpi, rotation })
        }
        FarmEvent::AudioConfig { phone, codec, rate, channels } => {
            (phone, ServerMsg::AudioConfig { phone, codec, rate, channels })
        }
        FarmEvent::Status { phone, code, detail } => (phone, ServerMsg::Status { phone, code, detail }),
        FarmEvent::PeerSilent { phone } => (phone, ServerMsg::PeerSilent { phone }),
        FarmEvent::Malformed { phone, detail } => (phone, ServerMsg::Malformed { phone, detail }),
        FarmEvent::Disconnected { phone, reason } => (phone, ServerMsg::Disconnected { phone, reason }),
        FarmEvent::LinkDown { phone, detail } => (phone, ServerMsg::LinkDown { phone, detail }),
        FarmEvent::Warning { phone, detail } => (phone, ServerMsg::Warning { phone, detail }),
        FarmEvent::Appeared { phone, transport, address, serial } => {
            (phone, ServerMsg::Appeared { phone, transport: transport.to_string(), address, serial })
        }
        FarmEvent::State { phone, state, detail } => {
            (phone, ServerMsg::State { phone, state: state_name(state).into(), detail })
        }
        FarmEvent::Lost { phone } => (phone, ServerMsg::Lost { phone }),
        FarmEvent::Video { .. } | FarmEvent::Audio { .. } => {
            unreachable!("видео и звук идут бинарными пакетами")
        }
    }
}

/// Команда протокола в команду демона. Коды: kind 0 тач, 1 мышь, 2 клавиатура;
/// phase 0/1/2/3 down/move/up/cancel; button 0/1/2 левая/правая/средняя.
/// Сравнение без раннего выхода: время не зависит от того, сколько байт совпало.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    let mut diff = (a.len() ^ b.len()) as u8;
    for i in 0..a.len().max(b.len()) {
        diff |= a.get(i).copied().unwrap_or(0) ^ b.get(i).copied().unwrap_or(0);
    }
    diff == 0
}

fn map_input(msg: ClientMsg) -> Result<(PhoneId, InputCmd), String> {
    let bad = |what: &str| format!("плохая команда: {what}");
    let parse_kind = |k: u32| match k {
        0 => Ok(DeviceKind::Touch),
        1 => Ok(DeviceKind::Mouse),
        2 => Ok(DeviceKind::Keyboard),
        _ => Err(bad("kind")),
    };
    let parse_phase = |p: u32| match p {
        0 => Ok(TouchPhase::Down),
        1 => Ok(TouchPhase::Move),
        2 => Ok(TouchPhase::Up),
        3 => Ok(TouchPhase::Cancel),
        _ => Err(bad("phase")),
    };
    let parse_button = |b: u32| match b {
        0 => Ok(MouseButton::Left),
        1 => Ok(MouseButton::Right),
        2 => Ok(MouseButton::Middle),
        _ => Err(bad("button")),
    };
    Ok(match msg {
        ClientMsg::TouchEx { phone, cid, phase, x, y, width, height, pressure } => {
            (phone, InputCmd::touch_ex(cid, parse_phase(phase)?, x, y, width, height, pressure))
        }
        ClientMsg::CursorMoveAbs { phone, x, y } => (phone, InputCmd::CursorMoveAbs { x, y }),
        ClientMsg::CursorMoveRel { phone, dx, dy } => (phone, InputCmd::CursorMoveRel { dx, dy }),
        ClientMsg::CursorResync { phone } => (phone, InputCmd::CursorResync),
        ClientMsg::MouseButton { phone, button, down } => {
            (phone, InputCmd::MouseButton { button: parse_button(button)?, down })
        }
        ClientMsg::Scroll { phone, ticks } => (phone, InputCmd::Scroll { ticks }),
        ClientMsg::ScrollH { phone, ticks } => (phone, InputCmd::ScrollH { ticks }),
        ClientMsg::Key { phone, usage, down } => (phone, InputCmd::Key { usage, down }),
        ClientMsg::DeviceEnable { phone, kind, on } => {
            (phone, InputCmd::DeviceEnable { kind: parse_kind(kind)?, on })
        }
        ClientMsg::ReleaseAll { phone } => (phone, InputCmd::ReleaseAll),
        ClientMsg::RequestFrame { phone } => (phone, InputCmd::RequestFrame),
        ClientMsg::Wake { phone } => (phone, InputCmd::Wake),
        ClientMsg::SetStream { phone, video, audio } => {
            (phone, InputCmd::SetStream { video, audio, max_fps: 0 })
        }
        ClientMsg::Auth { .. }
        | ClientMsg::List
        | ClientMsg::Watch { .. }
        | ClientMsg::Unwatch { .. }
        | ClientMsg::Snapshot { .. } => return Err(bad("не команда ввода")),
    })
}
