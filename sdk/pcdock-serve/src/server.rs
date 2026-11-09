//! Сервер связи: TCP, веб-сокеты, unix-сокет / именованный пайп.
//!
//! Один протокол везде (см. `protocol`): JSON-команды туда, JSON-события
//! и бинарные кадры оттуда. Видео и звук шлются только подписанным;
//! медленному клиенту кадры сбрасываются (пропуск виден по номеру кадра),
//! события уходят всегда.

use std::collections::{HashMap, HashSet};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc::{self, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use interprocess::local_socket::{self, traits::ListenerExt, ListenerOptions, ToNsName};
use interprocess::TryClone;
use pcdock_core::input::{DeviceKind, MouseButton, TouchPhase};
use pcdock_core::link::{InputCmd, PhoneId, PhoneState};
use pcdock_farm::{Farm, FarmEvent};
use tungstenite::Message;

use super::protocol::{
    encode_audio, encode_envelope, encode_video, ClientMsg, ServerMsg, PKG_AUDIO, PKG_JSON, PKG_VIDEO,
};

/// Исходящее сообщение клиенту.
#[derive(Clone, Debug)]
enum OutMsg {
    Json(String),
    Video(Vec<u8>),
    Audio(Vec<u8>),
}

struct Client {
    tx: SyncSender<OutMsg>,
    watched: HashSet<PhoneId>,
}

type Clients = Arc<Mutex<HashMap<u64, Client>>>;

/// Сервер поверх демона. Потокобезопасен через `Arc`: слушатели и клиенты
/// живут в своих потоках, демон дёргается под мьютексом.
#[derive(Clone)]
pub struct Serve {
    daemon: Arc<Mutex<Farm>>,
    clients: Clients,
    next_id: Arc<Mutex<u64>>,
}

impl Serve {
    pub fn new(daemon: Farm) -> Self {
        let serve = Self {
            daemon: Arc::new(Mutex::new(daemon)),
            clients: Arc::new(Mutex::new(HashMap::new())),
            next_id: Arc::new(Mutex::new(1)),
        };
        serve.spawn_pump();
        serve
    }

    /// Доступ к демону (добавить трубку, положить команду напрямую).
    pub fn daemon(&self) -> &Arc<Mutex<Farm>> {
        &self.daemon
    }

    fn add_client(&self, tx: SyncSender<OutMsg>) -> u64 {
        let mut next = self.next_id.lock().unwrap();
        let id = *next;
        *next += 1;
        self.clients.lock().unwrap().insert(id, Client { tx, watched: HashSet::new() });
        id
    }

    fn remove_client(&self, id: u64) {
        self.clients.lock().unwrap().remove(&id);
    }

    /// Разбирает команду клиента. Отвечает напрямую в его канал.
    fn handle(&self, client: u64, msg: ClientMsg) {
        let reply = |m: ServerMsg| self.send_json(client, m);
        let err = |detail: String| self.send_json(client, ServerMsg::Error { detail });
        let mut daemon = self.daemon.lock().unwrap();
        match msg {
            ClientMsg::List => reply(ServerMsg::Phones { phones: daemon.phones() }),
            ClientMsg::Watch { phone } => match daemon.set_watching(phone, true) {
                Ok(()) => {
                    self.clients.lock().unwrap().get_mut(&client).map(|c| c.watched.insert(phone));
                    reply(ServerMsg::Watched { phone, on: true });
                }
                Err(e) => err(e),
            },
            ClientMsg::Unwatch { phone } => match daemon.set_watching(phone, false) {
                Ok(()) => {
                    self.clients.lock().unwrap().get_mut(&client).map(|c| c.watched.remove(&phone));
                    reply(ServerMsg::Watched { phone, on: false });
                }
                Err(e) => err(e),
            },
            other => {
                let (phone, cmd) = match map_input(other) {
                    Ok(v) => v,
                    Err(detail) => return err(detail),
                };
                if let Err(e) = daemon.command(phone, cmd) {
                    err(e);
                }
            }
        }
    }

    fn send_json(&self, client: u64, msg: ServerMsg) {
        let text =
            serde_json::to_string(&msg).unwrap_or_else(|_| r#"{"ev":"Error","detail":"encode"}"#.into());
        if let Some(c) = self.clients.lock().unwrap().get(&client) {
            let _ = c.tx.send(OutMsg::Json(text));
        }
    }

    /// Канал событий демона: кэш и фильтр просмотра уже внутри `drain`,
    /// здесь только маршрутизация по подпискам.
    fn spawn_pump(&self) {
        let daemon = self.daemon.clone();
        let clients = self.clients.clone();
        thread::Builder::new()
            .name("serve-pump".into())
            .spawn(move || loop {
                let events = daemon.lock().unwrap().drain();
                for ev in events {
                    route(&clients, ev);
                }
                thread::sleep(Duration::from_millis(5));
            })
            .expect("не встал поток раздач");
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
        let (tx, rx) = mpsc::sync_channel::<OutMsg>(64);
        let id = self.add_client(tx);
        let me = self.clone();
        let mut rd = stream.try_clone().expect("клонирование TCP-потока");
        thread::Builder::new()
            .name(format!("tcp-r-{id}"))
            .spawn(move || {
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
                    match serde_json::from_slice::<ClientMsg>(&buf[1..]) {
                        Ok(msg) => me.handle(id, msg),
                        Err(e) => {
                            me.send_json(id, ServerMsg::Error { detail: format!("плохая команда: {e}") })
                        }
                    }
                }
                me.remove_client(id);
            })
            .expect("не встал поток чтения TCP");
        thread::Builder::new()
            .name(format!("tcp-w-{id}"))
            .spawn(move || {
                let mut wr = stream;
                for msg in rx {
                    let bytes = match &msg {
                        OutMsg::Json(text) => encode_envelope(PKG_JSON, text.as_bytes()),
                        OutMsg::Video(payload) => encode_envelope(PKG_VIDEO, payload),
                        OutMsg::Audio(payload) => encode_envelope(PKG_AUDIO, payload),
                    };
                    if wr.write_all(&bytes).is_err() {
                        break;
                    }
                }
            })
            .expect("не встал поток записи TCP");
    }

    /// Веб-сокеты для браузеров: текст — JSON, бинарные фреймы — кадры.
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
                        // Один поток на обе стороны: чтение с таймаутом,
                        // запись — сливом канала. Отдельный писатель
                        // невозможен: чтение держало бы Mutex сокета.
                        let _ = ws.get_ref().set_read_timeout(Some(Duration::from_millis(20)));
                        let (tx, rx) = mpsc::sync_channel::<OutMsg>(64);
                        let id = me2.add_client(tx);
                        let send_out = |ws: &mut tungstenite::WebSocket<std::net::TcpStream>,
                                        msg: &OutMsg| {
                            let out = match msg {
                                OutMsg::Json(text) => Message::Text(text.clone()),
                                OutMsg::Video(payload) | OutMsg::Audio(payload) => {
                                    Message::Binary(payload.clone())
                                }
                            };
                            ws.send(out).is_ok()
                        };
                        loop {
                            match ws.read() {
                                Ok(Message::Text(text)) => match serde_json::from_str::<ClientMsg>(&text) {
                                    Ok(msg) => me2.handle(id, msg),
                                    Err(e) => me2.send_json(
                                        id,
                                        ServerMsg::Error {
                                            detail: format!("плохая команда: {e}")
                                        },
                                    ),
                                },
                                Ok(Message::Close(_)) => break,
                                Ok(_) => {}
                                Err(tungstenite::Error::Io(e))
                                    if e.kind() == std::io::ErrorKind::TimedOut
                                        || e.kind() == std::io::ErrorKind::WouldBlock => {}
                                Err(_) => break,
                            }
                            while let Ok(msg) = rx.try_recv() {
                                if !send_out(&mut ws, &msg) {
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
        let (tx, rx) = mpsc::sync_channel::<OutMsg>(64);
        let id = self.add_client(tx);
        let me = self.clone();
        let mut rd = stream.try_clone().expect("клонирование локального потока");
        thread::Builder::new()
            .name(format!("local-r-{id}"))
            .spawn(move || {
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
                    match serde_json::from_slice::<ClientMsg>(&buf[1..]) {
                        Ok(msg) => me.handle(id, msg),
                        Err(e) => {
                            me.send_json(id, ServerMsg::Error { detail: format!("плохая команда: {e}") })
                        }
                    }
                }
                me.remove_client(id);
            })
            .expect("не встал поток чтения");
        thread::Builder::new()
            .name(format!("local-w-{id}"))
            .spawn(move || {
                for msg in rx {
                    let bytes = match &msg {
                        OutMsg::Json(text) => encode_envelope(PKG_JSON, text.as_bytes()),
                        OutMsg::Video(payload) => encode_envelope(PKG_VIDEO, payload),
                        OutMsg::Audio(payload) => encode_envelope(PKG_AUDIO, payload),
                    };
                    if stream.write_all(&bytes).is_err() {
                        break;
                    }
                }
            })
            .expect("не встал поток записи");
    }
}

/// Маршрутизация события демона по подпискам. Видео/звук — только подписанным
/// (сброс при переполнении канала: пропуск виден по номеру кадра).
fn route(clients: &Clients, ev: FarmEvent) {
    let map = clients.lock().unwrap();
    match ev {
        FarmEvent::Video { phone, frame } => {
            let payload =
                encode_video(phone, frame.seq, frame.codec, frame.key_frame, frame.pts_us, &frame.data);
            for c in map.values() {
                if c.watched.contains(&phone) {
                    let _ = c.tx.try_send(OutMsg::Video(payload.clone()));
                }
            }
        }
        FarmEvent::Audio { phone, pts_us, data } => {
            let payload = encode_audio(phone, pts_us, &data);
            for c in map.values() {
                if c.watched.contains(&phone) {
                    let _ = c.tx.try_send(OutMsg::Audio(payload.clone()));
                }
            }
        }
        // Обнаружение и состояния касаются всех клиентов: подписки на телефон ещё может не быть.
        ev @ (FarmEvent::Appeared { .. } | FarmEvent::State { .. } | FarmEvent::Lost { .. }) => {
            let (_, msg) = map_daemon_event(ev);
            let text =
                serde_json::to_string(&msg).unwrap_or_else(|_| r#"{"ev":"Error","detail":"encode"}"#.into());
            for c in map.values() {
                let _ = c.tx.send(OutMsg::Json(text.clone()));
            }
        }
        other => {
            let (phone, msg) = map_daemon_event(other);
            let text =
                serde_json::to_string(&msg).unwrap_or_else(|_| r#"{"ev":"Error","detail":"encode"}"#.into());
            for c in map.values() {
                if c.watched.contains(&phone) {
                    let _ = c.tx.send(OutMsg::Json(text.clone()));
                }
            }
        }
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
        FarmEvent::State { phone, state } => {
            let state = match state {
                PhoneState::Appeared => "appeared",
                PhoneState::Connecting => "connecting",
                PhoneState::Ready => "ready",
                PhoneState::NeedsPermission => "needs_permission",
                PhoneState::Lost => "lost",
            };
            (phone, ServerMsg::State { phone, state: state.into() })
        }
        FarmEvent::Lost { phone } => (phone, ServerMsg::Lost { phone }),
        FarmEvent::Video { .. } | FarmEvent::Audio { .. } => {
            unreachable!("видео и звук идут бинарными пакетами")
        }
    }
}

/// Команда протокола в команду демона. Коды: kind 0 тач, 1 мышь, 2 клавиатура;
/// phase 0/1/2/3 down/move/up/cancel; button 0/1/2 левая/правая/средняя.
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
        ClientMsg::SetStream { phone, video, audio } => (phone, InputCmd::SetStream { video, audio }),
        ClientMsg::List | ClientMsg::Watch { .. } | ClientMsg::Unwatch { .. } => {
            return Err(bad("не команда ввода"))
        }
    })
}
