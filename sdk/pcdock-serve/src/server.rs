//! Сервер фермы: протокол приложения поверх общей основы демонов (`pcdock-net`).
//!
//! Один протокол везде (см. `protocol`): JSON-команды туда, JSON-события и бинарные кадры оттуда.
//!
//! Устройство раздачи:
//! - подписка на телефон несёт условия (частота, только при изменении, звук); кадры отбираются для каждого клиента
//!   отдельно, видео на телефоне идёт по спросу: нет подписчиков — выключено, иначе с частотой самого
//!   требовательного;
//! - у каждого клиента свой ящик (см. `pcdock_net::Outbox`): события не теряются, видео хранится одним слотом
//!   «последний кадр», звук идёт очередью с потерями; клиент, не читающий события, отключается и не может
//!   задержать остальных.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use pcdock_core::input::{DeviceKind, MouseButton, TouchPhase};
use pcdock_core::link::{InputCmd, PhoneId, PhoneState};
use pcdock_farm::{Farm, FarmEvent};
use pcdock_net::{Handler, Hub};
use serde_json::{json, Value};

use super::protocol::{encode_audio, encode_video, ClientMsg, ServerMsg, PKG_AUDIO, PKG_VIDEO};

/// Версия протокола приложения (приходит клиенту в `Hello`).
pub const PROTOCOL_VERSION: u32 = 1;

/// Условия подписки клиента на телефон.
struct Watch {
    /// Не чаще стольких кадров в секунду; `None` — все кадры.
    max_fps: Option<f32>,
    on_change: bool,
    audio: bool,
    last_sent: Option<Instant>,
    last_hash: Option<u64>,
}

/// Данные клиента фермы: его подписки.
#[derive(Default)]
pub struct ClientState {
    watches: HashMap<PhoneId, Watch>,
}

/// Что сейчас запрошено у телефона: видео, звук и частота (0 — без ограничения).
type Demand = (bool, bool, u8);

/// Общее состояние сервера: ферма и то, что от неё запрошено.
struct Shared {
    daemon: Arc<Mutex<Farm>>,
    demand: Mutex<HashMap<PhoneId, Demand>>,
}

/// Сервер поверх фермы. Потокобезопасен через `Arc`: слушатели и клиенты живут в своих потоках,
/// ферма дёргается под мьютексом.
#[derive(Clone)]
pub struct Serve {
    shared: Arc<Shared>,
    hub: Hub<ClientState>,
}

/// Хэш кадра для условия `on_change` (FNV-1a).
fn frame_hash(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in data {
        h = (h ^ u64::from(*b)).wrapping_mul(0x100000001b3);
    }
    h
}

fn to_value(msg: &ServerMsg) -> Value {
    serde_json::to_value(msg).unwrap_or_else(|_| json!({"ev": "Error", "detail": "encode"}))
}

impl Serve {
    pub fn new(daemon: Farm) -> Self {
        Self::with_token(daemon, None)
    }

    /// С токеном: клиент первым сообщением шлёт `{"op":"Auth","token":"…"}`, до этого ему не отвечают
    /// и событий не присылают.
    pub fn with_token(daemon: Farm, token: Option<String>) -> Self {
        let shared =
            Arc::new(Shared { daemon: Arc::new(Mutex::new(daemon)), demand: Mutex::new(HashMap::new()) });
        let hub = Hub::new(Handle { shared: shared.clone() }, token);
        let serve = Self { shared, hub };
        serve.spawn_pump();
        serve
    }

    /// Доступ к ферме (добавить трубку, положить команду напрямую).
    pub fn daemon(&self) -> &Arc<Mutex<Farm>> {
        &self.shared.daemon
    }

    pub fn run_tcp(&self, addr: &str) -> std::io::Result<std::net::SocketAddr> {
        self.hub.run_tcp(addr)
    }

    pub fn run_ws(&self, addr: &str) -> std::io::Result<std::net::SocketAddr> {
        self.hub.run_ws(addr)
    }

    pub fn run_local(&self, name: &str) -> std::io::Result<()> {
        self.hub.run_local(name)
    }

    /// Поток раздачи: забирает события фермы и рассылает их по подпискам.
    fn spawn_pump(&self) {
        let (shared, hub) = (self.shared.clone(), self.hub.clone());
        thread::Builder::new()
            .name("serve-pump".into())
            .spawn(move || loop {
                let events = shared.daemon.lock().unwrap().drain();
                for ev in events {
                    if let FarmEvent::Connected { phone, .. } = &ev {
                        // Телефон (пере)подключился и снова не знает, что от него нужно.
                        shared.refresh_demand(&hub, *phone, true);
                    }
                    route(&hub, ev);
                }
                thread::sleep(Duration::from_millis(5));
            })
            .expect("не встал поток раздач");
    }
}

impl Shared {
    /// Пересчитывает, что нужно телефону, по подпискам всех клиентов и сообщает ему, если изменилось
    /// (или если `force`: телефон только что подключился и ничего не знает).
    fn refresh_demand(&self, hub: &Hub<ClientState>, phone: PhoneId, force: bool) {
        let (video, audio, fps) = hub.with_clients(|clients| {
            let mut video = false;
            let mut audio = false;
            let mut unlimited = false;
            let mut max_fps = 0.0f32;
            for c in clients.values() {
                if let Some(w) = c.data.watches.get(&phone) {
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
        });
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

    /// Последний кадр телефона из кэша — одному клиенту. `false`, если кадра нет.
    fn send_snapshot(&self, hub: &Hub<ClientState>, client: u64, phone: PhoneId) -> bool {
        let frame = self.daemon.lock().unwrap().last_frame(phone).ok().flatten();
        let Some(f) = frame else { return false };
        let payload = encode_video(phone, f.seq, f.codec, f.key_frame, f.pts_us, &f.data);
        hub.with_clients(|clients| {
            if let Some(c) = clients.get(&client) {
                c.out.latest(PKG_VIDEO, payload);
            }
        });
        true
    }
}

struct Handle {
    shared: Arc<Shared>,
}

impl Handler<ClientState> for Handle {
    fn client_data(&self) -> ClientState {
        ClientState::default()
    }

    fn hello(&self) -> Value {
        to_value(&ServerMsg::Hello {
            protocol: PROTOCOL_VERSION,
            server: format!("pcdockd/{}", env!("CARGO_PKG_VERSION")),
        })
    }

    fn on_close(&self, hub: &Hub<ClientState>, _client: u64, data: ClientState) {
        // Подписки ушедшего клиента снимаются: если смотреть больше некому, видео на телефоне выключается.
        for phone in data.watches.keys() {
            self.shared.refresh_demand(hub, *phone, false);
        }
    }

    fn on_json(&self, hub: &Hub<ClientState>, client: u64, id: Option<u64>, msg: Value) {
        let msg: ClientMsg = match serde_json::from_value(msg) {
            Ok(m) => m,
            Err(e) => return hub.error(client, id, format!("плохая команда: {e}")),
        };
        let reply = |m: ServerMsg| hub.reply(client, id, to_value(&m));
        let fail = |detail: String| hub.error(client, id, detail);
        match msg {
            ClientMsg::Auth { .. } => reply(ServerMsg::Authed),
            ClientMsg::List => {
                let phones = self.shared.daemon.lock().unwrap().phones();
                reply(ServerMsg::Phones { phones })
            }
            ClientMsg::Watch { phone, fps, on_change, audio } => {
                if !self.shared.daemon.lock().unwrap().phones().contains(&phone) {
                    return fail(format!("нет трубки {phone}"));
                }
                if fps.is_some_and(|f| !(f > 0.0 && f.is_finite())) {
                    return fail("плохая команда: fps должен быть больше 0".into());
                }
                hub.with_clients(|clients| {
                    if let Some(c) = clients.get_mut(&client) {
                        c.data.watches.insert(
                            phone,
                            Watch { max_fps: fps, on_change, audio, last_sent: None, last_hash: None },
                        );
                    }
                });
                self.shared.refresh_demand(hub, phone, false);
                reply(ServerMsg::Watched { phone, on: true });
                // Новый подписчик сразу получает последний кадр, чтобы не ждать изменения экрана.
                self.shared.send_snapshot(hub, client, phone);
            }
            ClientMsg::Unwatch { phone } => {
                hub.with_clients(|clients| {
                    if let Some(c) = clients.get_mut(&client) {
                        c.data.watches.remove(&phone);
                    }
                });
                self.shared.refresh_demand(hub, phone, false);
                reply(ServerMsg::Watched { phone, on: false });
            }
            ClientMsg::Snapshot { phone } => {
                if !self.shared.send_snapshot(hub, client, phone) {
                    fail(format!("нет кадра у трубки {phone}"));
                } else if id.is_some() {
                    reply(ServerMsg::Ok);
                }
            }
            other => {
                let (phone, cmd) = match map_input(other) {
                    Ok(v) => v,
                    Err(detail) => return fail(detail),
                };
                let result = self.shared.daemon.lock().unwrap().command(phone, cmd);
                match result {
                    Ok(()) => {
                        if id.is_some() {
                            reply(ServerMsg::Ok);
                        }
                    }
                    Err(e) => fail(e),
                }
            }
        }
    }
}

/// Маршрутизация события фермы по подпискам.
fn route(hub: &Hub<ClientState>, ev: FarmEvent) {
    match ev {
        FarmEvent::Video { phone, frame } => {
            let payload =
                encode_video(phone, frame.seq, frame.codec, frame.key_frame, frame.pts_us, &frame.data);
            let now = Instant::now();
            let mut hash = None;
            hub.with_clients(|map| {
                for c in map.values_mut().filter(|c| c.authed) {
                    let Some(w) = c.data.watches.get_mut(&phone) else { continue };
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
                    c.out.latest(PKG_VIDEO, payload.clone());
                }
            });
        }
        FarmEvent::Audio { phone, pts_us, data } => {
            let payload = encode_audio(phone, pts_us, &data);
            hub.with_clients(|map| {
                for c in map.values().filter(|c| c.authed) {
                    if c.data.watches.get(&phone).is_some_and(|w| w.audio) {
                        c.out.lossy(PKG_AUDIO, payload.clone());
                    }
                }
            });
        }
        // Обнаружение и состояния касаются всех клиентов: подписки на телефон ещё может не быть.
        ev @ (FarmEvent::Appeared { .. } | FarmEvent::State { .. } | FarmEvent::Lost { .. }) => {
            let (_, msg) = map_daemon_event(ev);
            hub.broadcast(|_| true, &to_value(&msg));
        }
        other => {
            let (phone, msg) = map_daemon_event(other);
            hub.broadcast(|c| c.data.watches.contains_key(&phone), &to_value(&msg));
        }
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
