//! Сквозной тест связи: демон с фейковой трубкой, TCP-клиент,
//! подписка, команда ввода, кадр с номером.

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use pcdock_core::session::Event;
use pcdock_daemon::{Daemon, InputCmd, PhoneLink};
use pcdock_serve::protocol::{ClientMsg, ServerMsg, PKG_JSON, PKG_VIDEO};
use pcdock_serve::Serve;

struct FakeLink {
    connected: bool,
    events: Arc<Mutex<VecDeque<Event>>>,
    inputs: Arc<Mutex<Vec<String>>>,
}

impl PhoneLink for FakeLink {
    fn is_connected(&self) -> bool {
        self.connected
    }
    fn connect(&mut self) -> Result<(), pcdock_daemon::LinkError> {
        self.connected = true;
        Ok(())
    }
    fn poll(&mut self) -> Result<Vec<Event>, pcdock_daemon::LinkError> {
        // Чуть притормаживаем busy-loop воркера, чтобы не жечь CPU.
        std::thread::sleep(std::time::Duration::from_millis(5));
        Ok(self.events.lock().unwrap().pop_front().into_iter().collect())
    }
    fn input(&mut self, cmd: &InputCmd) -> Result<(), pcdock_daemon::LinkError> {
        self.inputs.lock().unwrap().push(format!("{cmd:?}"));
        Ok(())
    }
    fn shutdown(&mut self) {
        self.connected = false;
    }
}

fn video(pts: u64) -> Event {
    Event::Video(pcdock_core::packet::VideoFrame {
        codec: 1,
        key_frame: true,
        pts_us: pts,
        data: vec![7, 7, 7],
    })
}

fn read_envelope(rd: &mut TcpStream) -> (u8, Vec<u8>) {
    let mut len = [0u8; 4];
    rd.read_exact(&mut len).unwrap();
    let len = u32::from_le_bytes(len) as usize;
    let mut buf = vec![0u8; len];
    rd.read_exact(&mut buf).unwrap();
    (buf[0], buf[1..].to_vec())
}

fn send_json(wr: &mut TcpStream, msg: &ClientMsg) {
    let body = serde_json::to_vec(msg).unwrap();
    let mut env = Vec::with_capacity(4 + 1 + body.len());
    env.extend_from_slice(&((1 + body.len()) as u32).to_le_bytes());
    env.push(PKG_JSON);
    env.extend_from_slice(&body);
    wr.write_all(&env).unwrap();
}

#[test]
fn tcp_watch_input_and_frame() {
    let mut daemon = Daemon::new();
    let inputs = Arc::new(Mutex::new(Vec::new()));
    let shared: Arc<Mutex<VecDeque<Event>>> = Arc::new(Mutex::new(VecDeque::new()));
    let link = FakeLink { connected: false, events: shared.clone(), inputs: inputs.clone() };
    daemon.add_phone(7, Box::new(link)).unwrap();
    let serve = Serve::new(daemon);
    let addr = serve.run_tcp("127.0.0.1:0").unwrap();

    let mut sock = TcpStream::connect(addr).unwrap();
    sock.set_read_timeout(Some(Duration::from_secs(5))).unwrap();

    // Список трубок.
    send_json(&mut sock, &ClientMsg::List);
    let (kind, body) = read_envelope(&mut sock);
    assert_eq!(kind, PKG_JSON);
    assert_eq!(serde_json::from_slice::<ServerMsg>(&body).unwrap(), ServerMsg::Phones { phones: vec![7] });

    // Подписка.
    send_json(&mut sock, &ClientMsg::Watch { phone: 7 });
    let (kind, body) = read_envelope(&mut sock);
    assert_eq!(kind, PKG_JSON);
    assert_eq!(
        serde_json::from_slice::<ServerMsg>(&body).unwrap(),
        ServerMsg::Watched { phone: 7, on: true }
    );

    // Кадры кладём уже после подписки: иначе демон отфильтрует их как несмотримые.
    shared.lock().unwrap().push_back(video(10));
    shared.lock().unwrap().push_back(video(11));

    // Команда ввода доходит до трубки.
    send_json(&mut sock, &ClientMsg::Scroll { phone: 7, ticks: 3.0 });
    std::thread::sleep(Duration::from_millis(300));
    assert!(
        inputs.lock().unwrap().iter().any(|s| s.contains("Scroll")),
        "{}",
        inputs.lock().unwrap().join("\n")
    );

    // Два кадра с номерами по порядку.
    for (want_seq, want_pts) in [(1u64, 10u64), (2, 11)] {
        let (kind, body) = read_envelope(&mut sock);
        assert_eq!(kind, PKG_VIDEO);
        let (phone, seq, codec, key, pts, jpeg) = pcdock_serve::protocol::decode_video(&body).unwrap();
        assert_eq!((phone, seq, codec, key, pts), (7, want_seq, 1, true, want_pts));
        assert_eq!(jpeg, &[7, 7, 7]);
    }
}

fn ws_send<S>(ws: &mut tungstenite::WebSocket<S>, msg: &ClientMsg)
where
    S: std::io::Read + std::io::Write,
{
    ws.send(tungstenite::Message::Text(serde_json::to_string(msg).unwrap().into())).unwrap();
}

fn ws_next_json<S>(ws: &mut tungstenite::WebSocket<S>) -> ServerMsg
where
    S: std::io::Read + std::io::Write,
{
    loop {
        match ws.read().unwrap() {
            tungstenite::Message::Text(t) => return serde_json::from_str(&t).unwrap(),
            _ => continue,
        }
    }
}

fn ws_next_binary<S>(ws: &mut tungstenite::WebSocket<S>) -> Vec<u8>
where
    S: std::io::Read + std::io::Write,
{
    loop {
        match ws.read().unwrap() {
            tungstenite::Message::Binary(b) => return b.to_vec(),
            _ => continue,
        }
    }
}

#[test]
fn ws_watch_and_frame() {
    let mut daemon = Daemon::new();
    let shared: Arc<Mutex<VecDeque<Event>>> = Arc::new(Mutex::new(VecDeque::new()));
    let inputs = Arc::new(Mutex::new(Vec::new()));
    daemon
        .add_phone(9, Box::new(FakeLink { connected: false, events: shared.clone(), inputs: inputs.clone() }))
        .unwrap();
    let serve = Serve::new(daemon);
    let addr = serve.run_ws("127.0.0.1:0").unwrap();

    let (mut ws, _) = tungstenite::connect(format!("ws://{addr}/")).map_err(|e| format!("{e:?}")).unwrap();
    ws_send(&mut ws, &ClientMsg::List);
    assert_eq!(ws_next_json(&mut ws), ServerMsg::Phones { phones: vec![9] });
    ws_send(&mut ws, &ClientMsg::Watch { phone: 9 });
    assert_eq!(ws_next_json(&mut ws), ServerMsg::Watched { phone: 9, on: true });

    shared.lock().unwrap().push_back(video(20));
    ws_send(&mut ws, &ClientMsg::Key { phone: 9, usage: 0xE1, down: true });
    std::thread::sleep(Duration::from_millis(300));
    assert!(
        inputs.lock().unwrap().iter().any(|s| s.contains("Key")),
        "{}",
        inputs.lock().unwrap().join("\n")
    );

    let body = ws_next_binary(&mut ws);
    let (phone, seq, codec, key, pts, jpeg) = pcdock_serve::protocol::decode_video(&body).unwrap();
    assert_eq!((phone, seq, codec, key, pts), (9, 1, 1, true, 20));
    assert_eq!(jpeg, &[7, 7, 7]);
}

#[test]
fn local_watch_and_frame() {
    use interprocess::local_socket::{
        traits::Stream as _, GenericNamespaced, Stream as LocalStream, ToNsName,
    };
    use std::io::{Read, Write};

    let mut daemon = Daemon::new();
    let shared: Arc<Mutex<VecDeque<Event>>> = Arc::new(Mutex::new(VecDeque::new()));
    daemon
        .add_phone(
            5,
            Box::new(FakeLink {
                connected: false,
                events: shared.clone(),
                inputs: Arc::new(Mutex::new(Vec::new())),
            }),
        )
        .unwrap();
    let serve = Serve::new(daemon);
    let name = format!("pcdock-test-{}", std::process::id());
    serve.run_local(&name).unwrap();
    std::thread::sleep(Duration::from_millis(100));

    let ns = Box::leak(name.clone().into_boxed_str()) as &'static str;
    let mut sock = LocalStream::connect(ns.to_ns_name::<GenericNamespaced>().unwrap()).unwrap();

    let send = |sock: &mut LocalStream, msg: &ClientMsg| {
        let body = serde_json::to_vec(msg).unwrap();
        let mut env = Vec::with_capacity(4 + 1 + body.len());
        env.extend_from_slice(&((1 + body.len()) as u32).to_le_bytes());
        env.push(PKG_JSON);
        env.extend_from_slice(&body);
        sock.write_all(&env).unwrap();
    };
    let recv = |sock: &mut LocalStream| -> (u8, Vec<u8>) {
        let mut len = [0u8; 4];
        sock.read_exact(&mut len).unwrap();
        let len = u32::from_le_bytes(len) as usize;
        let mut buf = vec![0u8; len];
        sock.read_exact(&mut buf).unwrap();
        (buf[0], buf[1..].to_vec())
    };

    send(&mut sock, &ClientMsg::Watch { phone: 5 });
    let (kind, body) = recv(&mut sock);
    assert_eq!(kind, PKG_JSON);
    assert_eq!(
        serde_json::from_slice::<ServerMsg>(&body).unwrap(),
        ServerMsg::Watched { phone: 5, on: true }
    );

    shared.lock().unwrap().push_back(video(30));
    let (kind, body) = recv(&mut sock);
    assert_eq!(kind, PKG_VIDEO);
    let (phone, seq, _, _, pts, _) = pcdock_serve::protocol::decode_video(&body).unwrap();
    assert_eq!((phone, seq, pts), (5, 1, 30));
}
