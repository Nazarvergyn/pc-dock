//! Что движку нужно от фермы: сведения о телефонах, ввод и кадры. Настоящая реализация (`FarmBackend`) говорит
//! с `pcdockd` по протоколу; в тестах подставляется фейк.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use pcdock_client::{Client, VideoFrame, KIND_VIDEO};
use serde_json::{json, Value};

/// Сведения о телефоне.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PhoneInfo {
    pub id: u32,
    pub state: String,
    pub transport: Option<String>,
    pub address: Option<String>,
    pub serial: Option<String>,
    pub name: Option<String>,
    /// Ширина и высота экрана (по картинке), поворот.
    pub screen: Option<(u32, u32)>,
    pub rotation: u32,
}

/// Кадр экрана.
#[derive(Clone, Debug)]
pub struct Frame {
    pub seq: u64,
    pub pts_us: u64,
    pub jpeg: Arc<Vec<u8>>,
    /// Хэш содержимого для «картинка изменилась».
    pub hash: u64,
}

pub fn frame_hash(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in data {
        h = (h ^ u64::from(*b)).wrapping_mul(0x100000001b3);
    }
    h
}

/// Фаза касания: 0 down, 1 move, 2 up, 3 cancel (как в протоколе демона).
pub type Phase = u32;

pub trait Backend: Send + Sync {
    /// Все телефоны фермы.
    fn phones(&self) -> Vec<PhoneInfo>;
    fn phone(&self, id: u32) -> Option<PhoneInfo> {
        self.phones().into_iter().find(|p| p.id == id)
    }
    fn touch(&self, phone: u32, phase: Phase, x: f32, y: f32, pressure: Option<f32>, size: Option<f32>, finger: u8) -> Result<(), String>;
    fn key(&self, phone: u32, usage: u16, down: bool) -> Result<(), String>;
    fn scroll(&self, phone: u32, ticks: f32, horizontal: bool) -> Result<(), String>;
    fn wake(&self, phone: u32) -> Result<(), String>;
    fn enable(&self, phone: u32, kind: u32, on: bool) -> Result<(), String>;
    /// Занимает телефон под кадры: до `release` ферма присылает кадры (подписка с условиями).
    fn acquire(&self, phone: u32) -> Result<(), String>;
    fn release(&self, phone: u32);
    /// Последний полученный кадр, без ожидания.
    fn latest_frame(&self, phone: u32) -> Option<Frame>;
    /// Ждёт кадр: самый свежий, но не старше номера `after` (если задан), не дольше `timeout`.
    fn wait_frame(&self, phone: u32, after: Option<u64>, timeout: Duration) -> Option<Frame>;
}

// ---------------------------------------------------------------------------------------------

struct FrameStore {
    frames: Mutex<HashMap<u32, Frame>>,
    changed: Condvar,
}

/// Реализация поверх клиента `pcdockd`.
pub struct FarmBackend {
    client: Client,
    phones: Arc<Mutex<HashMap<u32, PhoneInfo>>>,
    frames: Arc<FrameStore>,
    watches: Mutex<HashMap<u32, usize>>,
    /// Частота кадров, которую автоматизация просит у телефона.
    fps: f32,
    alive: Arc<AtomicBool>,
}

fn info_from(v: &Value) -> Option<PhoneInfo> {
    let d = &v["display"];
    Some(PhoneInfo {
        id: v["phone"].as_u64()? as u32,
        state: v["state"].as_str().unwrap_or("").to_string(),
        transport: v["transport"].as_str().map(String::from),
        address: v["address"].as_str().map(String::from),
        serial: v["serial"].as_str().map(String::from),
        name: v["name"].as_str().map(String::from),
        screen: match (d["screen_w"].as_u64(), d["screen_h"].as_u64()) {
            (Some(w), Some(h)) => Some((w as u32, h as u32)),
            _ => None,
        },
        rotation: d["rotation"].as_u64().unwrap_or(0) as u32,
    })
}

impl FarmBackend {
    /// Подключается к `pcdockd` и начинает следить за телефонами. `fps` — частота кадров, которую просит автоматизация.
    pub fn new(client: Client, fps: f32) -> Result<Arc<Self>, String> {
        let phones: Arc<Mutex<HashMap<u32, PhoneInfo>>> = Arc::new(Mutex::new(HashMap::new()));
        let frames = Arc::new(FrameStore { frames: Mutex::new(HashMap::new()), changed: Condvar::new() });
        let alive = Arc::new(AtomicBool::new(true));
        let events = client.take_events().ok_or("события уже забраны")?;
        let binary = client.take_binary().ok_or("бинарные пакеты уже забраны")?;
        let me = Arc::new(Self { client: client.clone(), phones: phones.clone(), frames: frames.clone(), watches: Mutex::new(HashMap::new()), fps, alive: alive.clone() });
        me.refresh()?;

        // События о телефонах: обновляют состояние.
        {
            let (phones, alive) = (phones.clone(), alive.clone());
            thread::Builder::new()
                .name("autod-farm-events".into())
                .spawn(move || {
                    while let Ok(v) = events.recv() {
                        let id = v["phone"].as_u64().map(|x| x as u32);
                        let Some(id) = id else { continue };
                        let mut map = phones.lock().unwrap();
                        match v["ev"].as_str() {
                            Some("Appeared") => {
                                let e = map.entry(id).or_insert_with(|| PhoneInfo { id, ..Default::default() });
                                e.transport = v["transport"].as_str().map(String::from);
                                e.address = v["address"].as_str().map(String::from);
                                e.serial = v["serial"].as_str().map(String::from);
                            }
                            Some("State") => {
                                let e = map.entry(id).or_insert_with(|| PhoneInfo { id, ..Default::default() });
                                e.state = v["state"].as_str().unwrap_or("").to_string();
                            }
                            Some("Lost") => {
                                map.remove(&id);
                            }
                            Some("Display") => {
                                let e = map.entry(id).or_insert_with(|| PhoneInfo { id, ..Default::default() });
                                if let (Some(w), Some(h)) = (v["screen_w"].as_u64(), v["screen_h"].as_u64()) {
                                    e.screen = Some((w as u32, h as u32));
                                }
                                e.rotation = v["rotation"].as_u64().unwrap_or(0) as u32;
                            }
                            _ => {}
                        }
                    }
                    alive.store(false, Ordering::SeqCst);
                })
                .map_err(|e| e.to_string())?;
        }
        // Кадры: последний кадр каждого телефона.
        {
            let (frames, alive) = (frames.clone(), alive.clone());
            thread::Builder::new()
                .name("autod-farm-frames".into())
                .spawn(move || {
                    while let Ok((kind, payload)) = binary.recv() {
                        if kind != KIND_VIDEO {
                            continue;
                        }
                        let Some(f) = VideoFrame::parse(&payload) else { continue };
                        let hash = frame_hash(&f.jpeg);
                        frames.frames.lock().unwrap().insert(
                            f.phone,
                            Frame { seq: f.seq, pts_us: f.pts_us, jpeg: Arc::new(f.jpeg), hash },
                        );
                        frames.changed.notify_all();
                    }
                    alive.store(false, Ordering::SeqCst);
                    frames.changed.notify_all();
                })
                .map_err(|e| e.to_string())?;
        }
        Ok(me)
    }

    /// Жива ли связь с `pcdockd`.
    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::SeqCst) && !self.client.is_closed()
    }

    /// Перечитывает список телефонов со всеми подробностями.
    pub fn refresh(&self) -> Result<(), String> {
        let r = self.client.request(json!({"op": "List"})).map_err(|e| e.to_string())?;
        let mut map = self.phones.lock().unwrap();
        map.clear();
        for d in r["details"].as_array().into_iter().flatten() {
            if let Some(i) = info_from(d) {
                map.insert(i.id, i);
            }
        }
        Ok(())
    }

    fn op(&self, msg: Value) -> Result<(), String> {
        self.client.request(msg).map(|_| ()).map_err(|e| e.to_string())
    }
}

impl Backend for FarmBackend {
    fn phones(&self) -> Vec<PhoneInfo> {
        let mut v: Vec<PhoneInfo> = self.phones.lock().unwrap().values().cloned().collect();
        v.sort_by_key(|p| p.id);
        v
    }

    fn touch(&self, phone: u32, phase: Phase, x: f32, y: f32, pressure: Option<f32>, size: Option<f32>, finger: u8) -> Result<(), String> {
        self.op(json!({"op": "TouchEx", "phone": phone, "cid": finger, "phase": phase, "x": x, "y": y,
                       "width": size, "height": size, "pressure": pressure}))
    }

    fn key(&self, phone: u32, usage: u16, down: bool) -> Result<(), String> {
        self.op(json!({"op": "Key", "phone": phone, "usage": usage, "down": down}))
    }

    fn scroll(&self, phone: u32, ticks: f32, horizontal: bool) -> Result<(), String> {
        self.op(json!({"op": if horizontal { "ScrollH" } else { "Scroll" }, "phone": phone, "ticks": ticks}))
    }

    fn wake(&self, phone: u32) -> Result<(), String> {
        self.op(json!({"op": "Wake", "phone": phone}))
    }

    fn enable(&self, phone: u32, kind: u32, on: bool) -> Result<(), String> {
        self.op(json!({"op": "DeviceEnable", "phone": phone, "kind": kind, "on": on}))
    }

    fn acquire(&self, phone: u32) -> Result<(), String> {
        let mut w = self.watches.lock().unwrap();
        let n = w.entry(phone).or_insert(0);
        if *n == 0 {
            // Подписка с умеренной частотой: автоматизация не требует плавного видео и не заставляет телефон его давать.
            self.op(json!({"op": "Watch", "phone": phone, "fps": self.fps}))?;
        }
        *n += 1;
        Ok(())
    }

    fn release(&self, phone: u32) {
        let mut w = self.watches.lock().unwrap();
        if let Some(n) = w.get_mut(&phone) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                w.remove(&phone);
                let _ = self.op(json!({"op": "Unwatch", "phone": phone}));
                self.frames.frames.lock().unwrap().remove(&phone);
            }
        }
    }

    fn latest_frame(&self, phone: u32) -> Option<Frame> {
        self.frames.frames.lock().unwrap().get(&phone).cloned()
    }

    fn wait_frame(&self, phone: u32, after: Option<u64>, timeout: Duration) -> Option<Frame> {
        let deadline = Instant::now() + timeout;
        let mut map = self.frames.frames.lock().unwrap();
        loop {
            if let Some(f) = map.get(&phone) {
                if after.is_none_or(|a| f.seq > a) {
                    return Some(f.clone());
                }
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() || !self.alive.load(Ordering::SeqCst) {
                return None;
            }
            map = self.frames.changed.wait_timeout(map, left.min(Duration::from_millis(200))).unwrap().0;
        }
    }
}
