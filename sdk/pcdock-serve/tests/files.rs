//! Передача файлов через демон: запись на телефон, чтение, список, удаление, ограничения.
//! Телефон заменён файловой системой в памяти.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use pcdock_core::link::{Discovery, InputCmd, LinkError, LinkFactory, PhoneLink, Sighting, Transport};
use pcdock_core::session::Event;
use pcdock_farm::{DiscoveryConfig, Farm};
use pcdock_serve::{FileBackend, FileOptions, FileWriter, Serve};
use serde_json::{json, Value};

/// Телефон без связи: нужен только как запись в ферме с серийником.
struct Idle;

impl PhoneLink for Idle {
    fn is_connected(&self) -> bool {
        false
    }
    fn connect(&mut self) -> Result<(), LinkError> {
        Err(LinkError::Gone("нет".into()))
    }
    fn poll(&mut self) -> Result<Vec<Event>, LinkError> {
        Ok(vec![])
    }
    fn input(&mut self, _: &InputCmd) -> Result<(), LinkError> {
        Ok(())
    }
    fn shutdown(&mut self) {}
}

struct OnePhone(Vec<Sighting>);

impl Discovery for OnePhone {
    fn scan(&mut self) -> Vec<Sighting> {
        self.0.clone()
    }
}

struct IdleFactory;

impl LinkFactory for IdleFactory {
    fn make(&mut self, _: &Sighting) -> Result<Box<dyn PhoneLink>, String> {
        Ok(Box::new(Idle))
    }
}

/// Файловая система «телефона».
#[derive(Default)]
struct MemFs {
    files: Mutex<BTreeMap<String, Vec<u8>>>,
    /// Сколько байт файла «записалось» (для проверки обрыва).
    media: Mutex<Vec<String>>,
}

struct MemWriter {
    fs: Arc<MemFs>,
    path: String,
    data: Vec<u8>,
}

impl FileWriter for MemWriter {
    fn write(&mut self, data: &[u8]) -> Result<(), String> {
        self.data.extend_from_slice(data);
        Ok(())
    }
    fn finish(self: Box<Self>, size: u64, media: bool) -> Result<(), String> {
        if self.data.len() as u64 != size {
            return Err("размер не сошёлся".into());
        }
        if media {
            self.fs.media.lock().unwrap().push(self.path.clone());
        }
        self.fs.files.lock().unwrap().insert(self.path, self.data);
        Ok(())
    }
}

struct Backend(Arc<MemFs>);

impl FileBackend for Backend {
    fn put(&self, serial: &str, dest: &str) -> Result<Box<dyn FileWriter>, String> {
        assert_eq!(serial, "SER1");
        Ok(Box::new(MemWriter { fs: self.0.clone(), path: dest.to_string(), data: Vec::new() }))
    }
    fn get(&self, _serial: &str, src: &str) -> Result<(u64, Box<dyn Read + Send>), String> {
        let data =
            self.0.files.lock().unwrap().get(src).cloned().ok_or_else(|| format!("нет файла {src}"))?;
        Ok((data.len() as u64, Box::new(std::io::Cursor::new(data))))
    }
    fn list(&self, _serial: &str, path: &str) -> Result<Vec<String>, String> {
        let prefix = format!("{}/", path.trim_end_matches('/'));
        Ok(self
            .0
            .files
            .lock()
            .unwrap()
            .keys()
            .filter_map(|k| k.strip_prefix(&prefix).map(String::from))
            .collect())
    }
    fn delete(&self, _serial: &str, path: &str) -> Result<(), String> {
        self.0.files.lock().unwrap().remove(path);
        Ok(())
    }
}

fn serve(files: Option<FileOptions>, serial: Option<&str>) -> (Serve, std::net::SocketAddr) {
    let mut farm = Farm::new();
    let sighting = Sighting {
        transport: Transport::Adb,
        address: "ADDR".into(),
        serial: serial.map(String::from),
        issue: None,
    };
    farm.enable_discovery(
        vec![Box::new(OnePhone(vec![sighting]))],
        Box::new(IdleFactory),
        DiscoveryConfig { every: Duration::ZERO, ..Default::default() },
    );
    farm.discover_now();
    let serve = Serve::with_options(farm, None, files);
    let addr = serve.run_tcp("127.0.0.1:0").unwrap();
    (serve, addr)
}

fn with_fs() -> (Serve, std::net::SocketAddr, Arc<MemFs>) {
    let fs = Arc::new(MemFs::default());
    let opts = FileOptions {
        backend: Arc::new(Backend(fs.clone())),
        roots: vec!["/sdcard".into(), "/data/local/tmp".into()],
    };
    let (s, a) = serve(Some(opts), Some("SER1"));
    (s, a, fs)
}

struct Conn(TcpStream);

impl Conn {
    fn open(addr: std::net::SocketAddr) -> Self {
        let s = TcpStream::connect(addr).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut c = Self(s);
        assert_eq!(c.json()["ev"], "Hello");
        c
    }
    fn send(&mut self, kind: u8, body: &[u8]) {
        let mut env = ((1 + body.len()) as u32).to_le_bytes().to_vec();
        env.push(kind);
        env.extend_from_slice(body);
        self.0.write_all(&env).unwrap();
    }
    fn op(&mut self, v: Value) {
        self.send(0, v.to_string().as_bytes());
    }
    fn recv(&mut self) -> (u8, Vec<u8>) {
        let mut len = [0u8; 4];
        self.0.read_exact(&mut len).unwrap();
        let mut buf = vec![0u8; u32::from_le_bytes(len) as usize];
        self.0.read_exact(&mut buf).unwrap();
        (buf[0], buf[1..].to_vec())
    }
    /// Следующее JSON-сообщение, кроме рассылок о состояниях телефонов и прогресса.
    fn json(&mut self) -> Value {
        loop {
            let (k, b) = self.recv();
            if k == 0 {
                let v: Value = serde_json::from_slice(&b).unwrap();
                if !["State", "Appeared", "Lost", "FileProgress"].contains(&v["ev"].as_str().unwrap_or("")) {
                    return v;
                }
            }
        }
    }
    /// Ждёт сообщение конкретного вида.
    fn until(&mut self, ev: &str) -> Value {
        let t0 = Instant::now();
        loop {
            let v = self.json();
            if v["ev"] == ev || v["ev"] == "FileFailed" {
                return v;
            }
            assert!(t0.elapsed() < Duration::from_secs(10), "ждали {ev}, пришло {v}");
        }
    }
}

fn chunk(transfer: u64, data: &[u8]) -> Vec<u8> {
    let mut p = (transfer as u32).to_le_bytes().to_vec();
    p.extend_from_slice(data);
    p
}

fn big() -> Vec<u8> {
    (0..300_000u32).map(|i| (i % 253) as u8).collect()
}

#[test]
fn put_streams_a_large_file_to_the_phone_and_scans_media() {
    let (_s, addr, fs) = with_fs();
    let mut c = Conn::open(addr);
    let data = big();
    c.op(json!({"op": "FilePut", "phone": 1, "dest": "/sdcard/Pictures/big.bin", "size": data.len(), "media": true, "id": 5}));
    let ready = c.json();
    assert_eq!((ready["ev"].as_str(), ready["id"].as_u64()), (Some("FileReady"), Some(5)));
    let t = ready["transfer"].as_u64().unwrap();
    for part in data.chunks(40_000) {
        c.send(3, &chunk(t, part));
    }
    let done = c.until("FileDone");
    assert_eq!((done["transfer"].as_u64(), done["size"].as_u64()), (Some(t), Some(data.len() as u64)));
    assert_eq!(fs.files.lock().unwrap().get("/sdcard/Pictures/big.bin"), Some(&data));
    assert_eq!(*fs.media.lock().unwrap(), vec!["/sdcard/Pictures/big.bin"]);
}

#[test]
fn put_rejects_extra_bytes_and_unknown_transfer_data_is_ignored() {
    let (_s, addr, fs) = with_fs();
    let mut c = Conn::open(addr);
    c.op(json!({"op": "FilePut", "phone": 1, "dest": "/sdcard/a.bin", "size": 10}));
    let t = c.json()["transfer"].as_u64().unwrap();
    c.send(3, &chunk(999, "чужая передача".as_bytes())); // номер не тот: молча игнорируется
    c.send(3, &chunk(t, &[1u8; 11]));
    let r = c.until("FileDone");
    assert_eq!(r["ev"], "FileFailed");
    assert!(r["detail"].as_str().unwrap().contains("больше"), "{r}");
    assert!(fs.files.lock().unwrap().is_empty(), "неполный файл не появился");
}

#[test]
fn get_streams_the_file_back_in_order() {
    let (_s, addr, fs) = with_fs();
    let data = big();
    fs.files.lock().unwrap().insert("/sdcard/DCIM/x.bin".into(), data.clone());
    let mut c = Conn::open(addr);
    c.op(json!({"op": "FileGet", "phone": 1, "src": "/sdcard/DCIM/x.bin"}));
    let info = c.json();
    assert_eq!(info["ev"], "FileInfo");
    assert_eq!(info["size"], data.len() as u64);
    let t = info["transfer"].as_u64().unwrap() as u32;
    let mut got = Vec::new();
    loop {
        let (k, b) = c.recv();
        match k {
            3 => {
                assert_eq!(u32::from_le_bytes([b[0], b[1], b[2], b[3]]), t);
                got.extend_from_slice(&b[4..]);
            }
            0 => {
                let v: Value = serde_json::from_slice(&b).unwrap();
                if v["ev"] == "FileDone" {
                    break;
                }
            }
            _ => {}
        }
    }
    assert_eq!(got, data);
}

#[test]
fn list_and_delete_respect_the_allowed_folders() {
    let (_s, addr, fs) = with_fs();
    fs.files.lock().unwrap().insert("/sdcard/Download/a.txt".into(), vec![1]);
    fs.files.lock().unwrap().insert("/sdcard/Download/b.txt".into(), vec![2]);
    let mut c = Conn::open(addr);
    c.op(json!({"op": "FileList", "phone": 1, "path": "/sdcard/Download"}));
    let r = c.json();
    assert_eq!(r["names"], json!(["a.txt", "b.txt"]));
    c.op(json!({"op": "FileDelete", "phone": 1, "path": "/sdcard/Download/a.txt"}));
    assert_eq!(c.json()["ev"], "Ok");
    assert_eq!(fs.files.lock().unwrap().len(), 1);

    for (op, key) in [("FileList", "path"), ("FileDelete", "path"), ("FileGet", "src")] {
        for bad in ["/data/data/app/db", "/system/bin", "/sdcard/../data", "/sdcard/a b", "relative"] {
            c.op(json!({"op": op, "phone": 1, key: bad}));
            let r = c.json();
            assert_eq!(r["ev"], "Error", "{op} {bad}: {r}");
        }
    }
    c.op(json!({"op": "FilePut", "phone": 1, "dest": "/system/x", "size": 1}));
    assert_eq!(c.json()["ev"], "Error");
    assert_eq!(fs.files.lock().unwrap().len(), 1);
}

#[test]
fn files_are_off_by_default_and_need_a_known_serial() {
    let (_s, addr) = serve(None, Some("SER1"));
    let mut c = Conn::open(addr);
    c.op(json!({"op": "FileList", "phone": 1, "path": "/sdcard"}));
    let r = c.json();
    assert!(r["detail"].as_str().unwrap().contains("выключена"), "{r}");

    let fs = Arc::new(MemFs::default());
    let opts = FileOptions { backend: Arc::new(Backend(fs)), roots: vec!["/sdcard".into()] };
    let (_s2, addr2) = serve(Some(opts), None);
    let mut c2 = Conn::open(addr2);
    c2.op(json!({"op": "FileList", "phone": 1, "path": "/sdcard"}));
    let r = c2.json();
    assert!(r["detail"].as_str().unwrap().contains("серийник"), "{r}");
    c2.op(json!({"op": "FileList", "phone": 77, "path": "/sdcard"}));
    assert_eq!(c2.json()["ev"], "Error");
}

#[test]
fn cancel_stops_a_put_and_nothing_is_written() {
    let (_s, addr, fs) = with_fs();
    let mut c = Conn::open(addr);
    c.op(json!({"op": "FilePut", "phone": 1, "dest": "/sdcard/never.bin", "size": 1_000_000}));
    let t = c.json()["transfer"].as_u64().unwrap();
    c.send(3, &chunk(t, &[0u8; 1000]));
    c.op(json!({"op": "FileCancel", "transfer": t}));
    assert_eq!(c.json()["ev"], "Ok");
    // Ещё один кусок будит поток записи, который видит отмену.
    c.send(3, &chunk(t, &[0u8; 1000]));
    let r = c.until("FileDone");
    assert_eq!(r["ev"], "FileFailed");
    assert!(r["detail"].as_str().unwrap().contains("отменена"), "{r}");
    assert!(fs.files.lock().unwrap().is_empty());
    c.op(json!({"op": "FileCancel", "transfer": 12345}));
    assert_eq!(c.json()["ev"], "Error");
}

#[test]
fn a_dropped_client_aborts_its_transfer_without_stalling_others() {
    let (_s, addr, fs) = with_fs();
    {
        let mut c = Conn::open(addr);
        c.op(json!({"op": "FilePut", "phone": 1, "dest": "/sdcard/half.bin", "size": 1_000_000}));
        let t = c.json()["transfer"].as_u64().unwrap();
        c.send(3, &chunk(t, &[0u8; 1000]));
    } // клиент ушёл посреди передачи
    let mut other = Conn::open(addr);
    other.op(json!({"op": "FileList", "phone": 1, "path": "/sdcard"}));
    assert_eq!(other.json()["ev"], "Files");
    std::thread::sleep(Duration::from_millis(200));
    assert!(fs.files.lock().unwrap().is_empty(), "неполный файл не записан");
}
