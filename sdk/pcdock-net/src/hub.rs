use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use interprocess::local_socket::{self, traits::ListenerExt, ListenerOptions, ToNsName};
use interprocess::TryClone;
use serde_json::{json, Value};
use tungstenite::Message;

use crate::outbox::{Out, Outbox};
use crate::{envelope, KIND_JSON};

/// Клиент демона: ящик, признак пройденной проверки пароля и данные демона (подписки и т. п.).
pub struct Client<D> {
    pub out: Arc<Outbox>,
    pub authed: bool,
    pub data: D,
}

/// Что демон делает с клиентами. Всё остальное (носители, пароль, приветствие, разбор запросов) делает [`Hub`].
pub trait Handler<D>: Send + Sync + 'static {
    /// Данные нового клиента.
    fn client_data(&self) -> D;
    /// Приветствие: первое сообщение клиенту, до пароля.
    fn hello(&self) -> Value;
    /// Запрос клиента, прошедшего проверку пароля. `msg` — объект с полем `op`.
    fn on_json(&self, hub: &Hub<D>, client: u64, id: Option<u64>, msg: Value);
    /// Бинарный пакет клиента (тип 1 и выше), например данные файла.
    fn on_binary(&self, _hub: &Hub<D>, _client: u64, _kind: u8, _payload: Vec<u8>) {}
    /// Клиент ушёл (закрыл соединение или отключён как не читающий).
    fn on_close(&self, _hub: &Hub<D>, _client: u64, _data: D) {}
}

struct Inner<D> {
    clients: Mutex<HashMap<u64, Client<D>>>,
    next: AtomicU64,
    token: Option<Arc<str>>,
    handler: Arc<dyn Handler<D>>,
}

/// Клиенты и носители демона. Дёшево копируется (общее состояние за `Arc`).
pub struct Hub<D>(Arc<Inner<D>>);

impl<D> Clone for Hub<D> {
    fn clone(&self) -> Self {
        Hub(self.0.clone())
    }
}

/// Сравнение без раннего выхода: время не зависит от того, сколько байт совпало.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    let mut diff = (a.len() ^ b.len()) as u8;
    for i in 0..a.len().max(b.len()) {
        diff |= a.get(i).copied().unwrap_or(0) ^ b.get(i).copied().unwrap_or(0);
    }
    diff == 0
}

fn with_id(mut v: Value, id: Option<u64>) -> String {
    if let (Some(id), Some(obj)) = (id, v.as_object_mut()) {
        obj.insert("id".into(), id.into());
    }
    v.to_string()
}

impl<D: Send + 'static> Hub<D> {
    /// С паролем: клиент первым сообщением шлёт `{"op":"Auth","token":"…"}`, до этого ему не отвечают.
    pub fn new(handler: impl Handler<D>, token: Option<String>) -> Self {
        Hub(Arc::new(Inner {
            clients: Mutex::new(HashMap::new()),
            next: AtomicU64::new(1),
            token: token.filter(|t| !t.is_empty()).map(Arc::from),
            handler: Arc::new(handler),
        }))
    }

    /// Работа с клиентами под замком. Внутри не вызывайте методы `Hub`, которые берут тот же замок.
    pub fn with_clients<R>(&self, f: impl FnOnce(&mut HashMap<u64, Client<D>>) -> R) -> R {
        f(&mut self.0.clients.lock().unwrap())
    }

    /// Ответ клиенту; если у запроса был `id`, ответ его повторяет. Не читающий клиент отключается.
    pub fn reply(&self, client: u64, id: Option<u64>, msg: Value) {
        let text = with_id(msg, id);
        let overflow = match self.0.clients.lock().unwrap().get(&client) {
            Some(c) => !c.out.json(text),
            None => false,
        };
        if overflow {
            self.remove_client(client);
        }
    }

    pub fn error(&self, client: u64, id: Option<u64>, detail: impl Into<String>) {
        self.reply(client, id, json!({"ev": "Error", "detail": detail.into()}));
    }

    /// Рассылка события тем клиентам, прошедшим пароль, для которых `pick` вернул `true`.
    pub fn broadcast(&self, pick: impl Fn(&Client<D>) -> bool, msg: &Value) {
        let text = msg.to_string();
        let mut overflowed = Vec::new();
        {
            let map = self.0.clients.lock().unwrap();
            for (id, c) in map.iter().filter(|(_, c)| c.authed && pick(c)) {
                if !c.out.json(text.clone()) {
                    overflowed.push(*id);
                }
            }
        }
        for id in overflowed {
            self.remove_client(id);
        }
    }

    pub fn add_client(&self, out: Arc<Outbox>) -> u64 {
        let id = self.0.next.fetch_add(1, Ordering::SeqCst);
        let authed = self.0.token.is_none();
        out.json(self.0.handler.hello().to_string());
        let data = self.0.handler.client_data();
        self.0.clients.lock().unwrap().insert(id, Client { out, authed, data });
        id
    }

    /// Убирает клиента: закрывает его ящик и сообщает демону. Повторный вызов ничего не делает.
    pub fn remove_client(&self, id: u64) {
        let removed = self.0.clients.lock().unwrap().remove(&id);
        if let Some(c) = removed {
            c.out.close();
            self.0.handler.on_close(self, id, c.data);
        }
    }

    /// Разбор пакета клиента.
    fn on_packet(&self, id: u64, kind: u8, body: &[u8]) {
        if kind != KIND_JSON {
            let authed = self.0.clients.lock().unwrap().get(&id).is_some_and(|c| c.authed);
            if authed {
                self.0.handler.on_binary(self, id, kind, body.to_vec());
            }
            return;
        }
        let msg: Value = match serde_json::from_slice(body) {
            Ok(v) => v,
            Err(e) => return self.error(id, None, format!("плохая команда: {e}")),
        };
        let req_id = msg.get("id").and_then(Value::as_u64);
        let Some(op) = msg.get("op").and_then(Value::as_str) else {
            return self.error(id, req_id, "плохая команда: нет поля op");
        };
        if op == "Auth" {
            let given = msg.get("token").and_then(Value::as_str).unwrap_or("");
            let ok = match &self.0.token {
                Some(want) => constant_time_eq(want.as_bytes(), given.as_bytes()),
                None => true,
            };
            if ok {
                if let Some(c) = self.0.clients.lock().unwrap().get_mut(&id) {
                    c.authed = true;
                }
                return self.reply(id, req_id, json!({"ev": "Authed"}));
            }
            // Подбор пароля не должен идти быстро.
            thread::sleep(Duration::from_millis(500));
            return self.error(id, req_id, "неверный токен");
        }
        let authed = self.0.clients.lock().unwrap().get(&id).is_some_and(|c| c.authed);
        if !authed {
            return self.error(id, req_id, "нужна авторизация: первым пошлите op=Auth");
        }
        self.0.handler.on_json(self, id, req_id, msg);
    }

    /// Читает пакеты клиента из потока. Общий код TCP и локальной трубы.
    fn read_loop(&self, id: u64, rd: &mut impl Read) {
        loop {
            let mut len = [0u8; 4];
            if rd.read_exact(&mut len).is_err() {
                break;
            }
            let len = u32::from_le_bytes(len) as usize;
            if len == 0 || len > 32 * 1024 * 1024 {
                break;
            }
            let mut buf = vec![0u8; len];
            if rd.read_exact(&mut buf).is_err() {
                break;
            }
            self.on_packet(id, buf[0], &buf[1..]);
        }
        self.remove_client(id);
    }

    /// TCP-слушатель: свой поток, на клиента два потока (чтение и запись).
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
        let out = Arc::new(Outbox::new());
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
                    if wr.write_all(&encode(&msg)).is_err() {
                        break;
                    }
                }
                // Отключённого клиента (не читал события) отпускаем и на уровне сокета.
                out.close();
                let _ = wr.shutdown(Shutdown::Both);
            })
            .expect("не встал поток записи TCP");
    }

    /// Веб-сокеты для браузеров: текст — JSON, бинарные фреймы — `[тип][нагрузка]`.
    pub fn run_ws(&self, addr: &str) -> std::io::Result<std::net::SocketAddr> {
        let listener = TcpListener::bind(addr)?;
        let local = listener.local_addr()?;
        let me = self.clone();
        thread::Builder::new()
            .name(format!("ws-{local}"))
            .spawn(move || {
                for s in listener.incoming().flatten() {
                    let me2 = me.clone();
                    thread::spawn(move || me2.serve_ws(s));
                }
            })
            .expect("не встал WS-слушатель");
        Ok(local)
    }

    fn serve_ws(&self, s: TcpStream) {
        let mut ws = match tungstenite::accept(s) {
            Ok(ws) => ws,
            Err(_) => return,
        };
        // Один поток на обе стороны: чтение с таймаутом, запись сливом ящика. Отдельный писатель
        // невозможен: чтение держало бы Mutex сокета.
        let _ = ws.get_ref().set_read_timeout(Some(Duration::from_millis(20)));
        let out = Arc::new(Outbox::new());
        let id = self.add_client(out.clone());
        loop {
            match ws.read() {
                Ok(Message::Text(text)) => self.on_packet(id, KIND_JSON, text.as_bytes()),
                Ok(Message::Binary(b)) if !b.is_empty() => self.on_packet(id, b[0], &b[1..]),
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
                    Out::Json(text) => Message::Text(text),
                    Out::Binary { kind, mut payload } => {
                        payload.insert(0, kind);
                        Message::Binary(payload)
                    }
                };
                if ws.send(frame).is_err() {
                    self.remove_client(id);
                    return;
                }
            }
        }
        self.remove_client(id);
    }

    /// Локальная труба: имя в пространстве имён ОС (Linux: абстрактный unix-сокет; Windows: именованный канал).
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
        let out = Arc::new(Outbox::new());
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
                    if stream.write_all(&encode(&msg)).is_err() {
                        break;
                    }
                }
                out.close();
            })
            .expect("не встал поток записи");
    }
}

fn encode(msg: &Out) -> Vec<u8> {
    match msg {
        Out::Json(text) => envelope(KIND_JSON, text.as_bytes()),
        Out::Binary { kind, payload } => envelope(*kind, payload),
    }
}
