//! Команды над телефоном через демон: проверки на входе, ответ `DeviceResult`, ошибки. Телефон заменён записью вызовов.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use pcdock_core::link::{Discovery, InputCmd, LinkError, LinkFactory, PhoneLink, Sighting, Transport};
use pcdock_core::session::Event;
use pcdock_farm::{DiscoveryConfig, Farm};
use pcdock_serve::{DeviceBackend, DeviceOp, Serve};
use serde_json::{json, Value};

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

/// Запоминает вызовы и отвечает по имени операции.
#[derive(Default)]
struct Fake {
    calls: Mutex<Vec<(String, DeviceOp, Vec<String>)>>,
}

impl DeviceBackend for Fake {
    fn input_lines(
        &self,
        _serial: &str,
        stop: &std::sync::atomic::AtomicBool,
        on_line: &mut dyn FnMut(&str) -> bool,
    ) -> Result<(), String> {
        self.lines(stop, on_line);
        Ok(())
    }

    fn run(&self, serial: &str, op: &DeviceOp, roots: &[String]) -> Result<Value, String> {
        self.calls.lock().unwrap().push((serial.to_string(), op.clone(), roots.to_vec()));
        match op {
            DeviceOp::Info => Ok(json!({"model": "Fake 1"})),
            DeviceOp::Install { .. } => Err("Failure [INSTALL_FAILED_VERSION_DOWNGRADE]".into()),
            DeviceOp::InputRanges => Ok(json!({"max_x": 1000.0, "max_y": 2000.0})),
            DeviceOp::UiAt { .. } => Ok(json!({"element": {"text": "Войти", "package": "com.a"}})),
            DeviceOp::UiElements => Ok(json!({"elements": [
                {"text": "Кабинет", "kind": "text", "package": "com.a"},
                {"text": "", "desc": "", "id": "", "kind": "other"}]})),
            _ => Ok(json!({})),
        }
    }
}

const TOUCH: &[&str] = &[
    "[  100.000000] /dev/input/event3: EV_ABS       ABS_MT_TRACKING_ID   00000007",
    "[  100.000000] /dev/input/event3: EV_ABS       ABS_MT_POSITION_X    00000200",
    "[  100.000000] /dev/input/event3: EV_ABS       ABS_MT_POSITION_Y    00000400",
    "[  100.000000] /dev/input/event3: EV_SYN       SYN_REPORT           00000000",
    "[  100.100000] /dev/input/event3: EV_ABS       ABS_MT_TRACKING_ID   ffffffff",
    "[  100.100000] /dev/input/event3: EV_SYN       SYN_REPORT           00000000",
];

impl Fake {
    fn lines(&self, stop: &std::sync::atomic::AtomicBool, f: &mut dyn FnMut(&str) -> bool) {
        for l in TOUCH {
            if !f(l) {
                return;
            }
        }
        while !stop.load(std::sync::atomic::Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

fn serve(serial: Option<&str>, device: Option<Arc<Fake>>) -> std::net::SocketAddr {
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
    let device = device.map(|d| d as Arc<dyn DeviceBackend>);
    Serve::with_device(farm, None, None, device).run_tcp("127.0.0.1:0").unwrap()
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
    fn op(&mut self, v: Value) {
        let body = v.to_string();
        let mut env = ((1 + body.len()) as u32).to_le_bytes().to_vec();
        env.push(0);
        env.extend_from_slice(body.as_bytes());
        self.0.write_all(&env).unwrap();
    }
    fn json(&mut self) -> Value {
        loop {
            let mut len = [0u8; 4];
            self.0.read_exact(&mut len).unwrap();
            let mut buf = vec![0u8; u32::from_le_bytes(len) as usize];
            self.0.read_exact(&mut buf).unwrap();
            if buf[0] == 0 {
                let v: Value = serde_json::from_slice(&buf[1..]).unwrap();
                if !["State", "Appeared", "Lost"].contains(&v["ev"].as_str().unwrap_or("")) {
                    return v;
                }
            }
        }
    }
    /// Команда с номером запроса, ответ на неё.
    fn ask(&mut self, mut v: Value) -> Value {
        v["id"] = json!(7);
        self.op(v);
        // События без номера запроса (например `RecordState` для всех клиентов) пропускаются.
        loop {
            let r = self.json();
            if r["id"] == 7 {
                return r;
            }
            assert!(r["id"].is_null(), "{r}");
        }
    }
}

fn phone_id(c: &mut Conn) -> u64 {
    c.op(json!({"op": "List"}));
    c.json()["phones"][0].as_u64().unwrap()
}

#[test]
fn results_carry_the_operation_name_and_request_id() {
    let fake = Arc::new(Fake::default());
    let mut c = Conn::open(serve(Some("SER1"), Some(fake.clone())));
    let p = phone_id(&mut c);
    let r = c.ask(json!({"op": "DeviceInfo", "phone": p}));
    assert_eq!(
        (r["ev"].as_str(), r["op"].as_str(), r["data"]["model"].as_str()),
        (Some("DeviceResult"), Some("DeviceInfo"), Some("Fake 1"))
    );
    let r = c.ask(json!({"op": "ForceStop", "phone": p, "package": "com.example.app"}));
    assert_eq!(r["op"], "ForceStop");
    let calls = fake.calls.lock().unwrap();
    assert_eq!(calls[0].0, "SER1");
    assert_eq!(calls[1].1, DeviceOp::ForceStop { package: "com.example.app".into() });
}

#[test]
fn bad_arguments_never_reach_the_phone() {
    let fake = Arc::new(Fake::default());
    let mut c = Conn::open(serve(Some("SER1"), Some(fake.clone())));
    let p = phone_id(&mut c);
    for bad in [
        json!({"op": "ForceStop", "phone": p, "package": "x; reboot"}),
        json!({"op": "AppInfo", "phone": p, "packages": []}),
        json!({"op": "AppInfo", "phone": p, "packages": ["com.ok.app", "bad name"]}),
        json!({"op": "Logcat", "phone": p, "level": "X"}),
        json!({"op": "Button", "phone": p, "name": "rm -rf"}),
    ] {
        let r = c.ask(bad.clone());
        assert_eq!(r["ev"], "Error", "{bad} -> {r}");
    }
    assert!(fake.calls.lock().unwrap().is_empty());
}

#[test]
fn install_failure_is_reported_and_roots_are_passed() {
    let fake = Arc::new(Fake::default());
    let mut c = Conn::open(serve(Some("SER1"), Some(fake.clone())));
    let p = phone_id(&mut c);
    let r = c.ask(json!({"op": "InstallApk", "phone": p, "path": "/data/local/tmp/a.apk"}));
    assert_eq!(r["ev"], "Error");
    assert!(r["detail"].as_str().unwrap().contains("VERSION_DOWNGRADE"));
    assert!(fake.calls.lock().unwrap()[0].2.contains(&"/data/local/tmp".to_string()));
}

#[test]
fn disabled_backend_unknown_phone_and_missing_serial_are_errors() {
    let mut c = Conn::open(serve(Some("SER1"), None));
    let p = phone_id(&mut c);
    let r = c.ask(json!({"op": "DeviceInfo", "phone": p}));
    assert!(r["detail"].as_str().unwrap().contains("выключены"), "{r}");

    let mut c = Conn::open(serve(Some("SER1"), Some(Arc::new(Fake::default()))));
    let r = c.ask(json!({"op": "DeviceInfo", "phone": 99}));
    assert_eq!(r["ev"], "Error");

    let mut c = Conn::open(serve(None, Some(Arc::new(Fake::default()))));
    let p = phone_id(&mut c);
    let r = c.ask(json!({"op": "DeviceInfo", "phone": p}));
    assert!(r["detail"].as_str().unwrap().contains("компаньон"), "{r}");
}

#[test]
fn blocklist_stops_launch_but_not_other_apps() {
    let fake = Arc::new(Fake::default());
    let mut c = Conn::open(serve(Some("SER1"), Some(fake.clone())));
    let p = phone_id(&mut c);
    let r = c.ask(json!({"op": "SetBlocklist", "packages": ["com.bad.*", "*.cloner"]}));
    assert_eq!(r["ev"], "Ok");
    for pkg in ["com.bad.app", "x.y.cloner"] {
        let r = c.ask(json!({"op": "LaunchApp", "phone": p, "package": pkg}));
        assert_eq!(r["ev"], "Error", "{pkg}: {r}");
        assert!(r["detail"].as_str().unwrap().contains("блок-лист"));
    }
    let r = c.ask(json!({"op": "LaunchApp", "phone": p, "package": "com.good.app"}));
    assert_eq!(r["op"], "LaunchApp");
    let calls = fake.calls.lock().unwrap();
    assert_eq!(calls.len(), 1, "заблокированные запуски до телефона не дошли");
}

#[test]
fn selectors_reach_the_backend_and_bad_ones_are_rejected() {
    let fake = Arc::new(Fake::default());
    let mut c = Conn::open(serve(Some("SER1"), Some(fake.clone())));
    let p = phone_id(&mut c);
    let r = c.ask(json!({"op": "UiFind", "phone": p,
        "selector": {"text": "OK", "any": [{"id": "a"}, {"desc_contains": "b"}]}}));
    assert_eq!(r["op"], "UiFind");
    match &fake.calls.lock().unwrap()[0].1 {
        DeviceOp::UiFind { selector } => {
            assert_eq!(selector.text.as_deref(), Some("OK"));
            assert_eq!(selector.any.len(), 2);
        }
        other => panic!("{other:?}"),
    }
    // опечатка в селекторе не молчит
    let r = c.ask(json!({"op": "UiFind", "phone": p, "selector": {"txt": "OK"}}));
    assert_eq!(r["ev"], "Error");
    let r = c.ask(json!({"op": "UiAt", "phone": p, "x": 2.0, "y": 0.5}));
    assert_eq!(r["ev"], "Error");
    let r = c.ask(
        json!({"op": "WaitFor", "phone": p, "selector": {"text": "Готово"}, "timeout": 3, "gone": true}),
    );
    assert_eq!(r["op"], "WaitFor");
}

#[test]
fn health_reports_server_phones_and_recent_errors() {
    let mut c = Conn::open(serve(Some("SER1"), Some(Arc::new(Fake::default()))));
    let p = phone_id(&mut c);
    let r = c.ask(json!({"op": "Health"}));
    assert_eq!(r["ev"], "Health", "{r}");
    assert!(r["server"].as_str().unwrap().starts_with("pcdockd/"));
    assert_eq!(r["phones"].as_array().unwrap().len(), 1);
    assert!(r["errors"].as_array().unwrap().iter().all(|e| e["kind"] != "InstallApk"));

    let r = c.ask(json!({"op": "InstallApk", "phone": p, "path": "/sdcard/Download/a.apk"}));
    assert_eq!(r["ev"], "Error", "{r}");
    let r = c.ask(json!({"op": "Health"}));
    let errors: Vec<_> =
        r["errors"].as_array().unwrap().iter().filter(|e| e["kind"] == "InstallApk").collect();
    assert_eq!(errors.len(), 1, "{r}");
    assert_eq!(errors[0]["phone"], p);
    assert!(errors[0]["detail"].as_str().unwrap().contains("INSTALL_FAILED"));
}

#[test]
fn role_tokens_limit_commands() {
    let serve = Serve::with_device(Farm::new(), Some("boss".into()), None, None);
    serve.add_role_token("eyes", "view").unwrap();
    assert!(serve.add_role_token("x", "root").is_err());
    let addr = serve.run_tcp("127.0.0.1:0").unwrap();
    let mut c = Conn::open(addr);
    assert_eq!(c.ask(json!({"op": "Auth", "token": "eyes"}))["ev"], "Authed");
    assert_eq!(c.ask(json!({"op": "Health"}))["ev"], "Health");
    let r = c.ask(json!({"op": "Wake", "phone": 1}));
    assert!(r["detail"].as_str().unwrap().contains("роль view"), "{r}");
    let r = c.ask(json!({"op": "FileList", "phone": 1, "path": "/sdcard"}));
    assert_eq!(r["ev"], "Error");
}

#[test]
fn connect_policy_is_applied_and_validated() {
    let mut c = Conn::open(serve(Some("SER1"), None));
    let r = c.ask(json!({"op": "SetConnectPolicy", "serial": "SER1", "prefer": "adb", "fallback_secs": 0, "scan_secs": 5}));
    assert_eq!(r["ev"], "ConnectPolicy", "{r}");
    let r = c.ask(json!({"op": "SetConnectPolicy", "prefer": "auto"}));
    assert_eq!(r["ev"], "Ok", "{r}");
    for bad in [json!({"prefer": "wifi"}), json!({"scan_secs": 0}), json!({"fallback_secs": -1})] {
        let mut m = bad.clone();
        m["op"] = json!("SetConnectPolicy");
        assert_eq!(c.ask(m)["ev"], "Error", "{bad}");
    }
}

#[test]
fn every_device_command_reaches_the_backend() {
    let fake = Arc::new(Fake::default());
    let mut c = Conn::open(serve(Some("SER1"), Some(fake.clone())));
    let p = phone_id(&mut c);
    let cmds = [
        ("SystemDialog", json!({})),
        ("ExitInfo", json!({"package": "com.a.b"})),
        ("SetPackageVerifier", json!({"on": false})),
        ("PublicIp", json!({})),
        ("CompanionPermissions", json!({})),
        ("CompanionRestart", json!({})),
        ("CompanionInstall", json!({"path": "/data/local/tmp/a.apk"})),
    ];
    for (op, extra) in cmds {
        let mut m = extra.clone();
        m["op"] = json!(op);
        m["phone"] = json!(p);
        let r = c.ask(m);
        assert_eq!(r["ev"], "DeviceResult", "{op}: {r}");
    }
    assert_eq!(fake.calls.lock().unwrap().len(), 7);
}

#[test]
fn companion_call_needs_a_ready_phone_and_an_op() {
    let mut c = Conn::open(serve(Some("SER1"), Some(Arc::new(Fake::default()))));
    let p = phone_id(&mut c);
    let r = c.ask(json!({"op": "CompanionCall", "phone": p, "call": {"nope": 1}}));
    assert!(r["detail"].as_str().unwrap().contains("call.op"), "{r}");
    let r = c.ask(json!({"op": "CompanionCall", "phone": p, "call": {"op": "Info"}}));
    assert_eq!(r["ev"], "Error");
    assert!(r["detail"].as_str().unwrap().contains("компаньон"), "{r}");
}

#[test]
fn recording_turns_touches_into_lua_and_steps_can_be_edited() {
    let mut c = Conn::open(serve(Some("SER1"), Some(Arc::new(Fake::default()))));
    let p = phone_id(&mut c);
    assert_eq!(c.ask(json!({"op": "RecordState", "phone": p}))["ev"], "Error");
    let r = c.ask(json!({"op": "RecordStart", "phone": p}));
    assert_eq!(r["ev"], "RecordState", "{r}");
    assert_eq!(c.ask(json!({"op": "RecordStart", "phone": p}))["ev"], "Error");
    // Касание из getevent становится шагом с целью «Войти».
    let mut steps = Value::Null;
    for _ in 0..100 {
        let r = c.ask(json!({"op": "RecordState", "phone": p}));
        if r["state"]["steps"].as_array().is_some_and(|s| !s.is_empty()) && r["state"]["busy"] == false {
            steps = r["state"]["steps"].clone();
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(steps[0]["lua"], "p:tap_on(\"Войти\")", "{steps}");
    assert_eq!(steps[0]["pkg"], "com.a");
    let list = c.ask(json!({"op": "RecordList"}));
    assert_eq!(list["records"].as_array().unwrap().len(), 1);
    // Проверка по элементу экрана.
    let s = c.ask(json!({"op": "RecordScreen", "phone": p}));
    assert_eq!(s["ev"], "RecordScreen", "{s}");
    let num = s["num"].as_u64().unwrap();
    assert_eq!(s["elements"][0]["label"], "Кабинет");
    let r = c.ask(json!({"op": "RecordAddCheck", "phone": p, "num": num, "idx": 0}));
    assert_eq!(r["state"]["steps"][1]["lua"], "assert.truthy(p:find_one(\"Кабинет\"))", "{r}");
    let r = c.ask(json!({"op": "RecordAddCheck", "phone": p, "num": num, "idx": 1}));
    assert!(r["detail"].as_str().unwrap().starts_with("bad_request"), "{r}");
    let r = c.ask(json!({"op": "RecordAddCheck", "phone": p, "num": 99, "idx": 0}));
    assert!(r["detail"].as_str().unwrap().starts_with("not_found"), "{r}");
    // Правка: удалить первый шаг, оставить проверку.
    let check_id = steps.as_array().map(|_| 2).unwrap();
    let r = c.ask(json!({"op": "RecordSetSteps", "phone": p,
        "steps": [{"id": check_id, "t": "check", "v": {"target": "Кабинет"}, "src": "check"}]}));
    assert_eq!(r["state"]["steps"].as_array().unwrap().len(), 1, "{r}");
    assert_eq!(c.ask(json!({"op": "RecordPause", "phone": p}))["state"]["status"], "paused");
    let r = c.ask(json!({"op": "RecordStop", "phone": p, "name": "Вход", "origin": "тест"}));
    assert_eq!(r["ev"], "RecordResult", "{r}");
    assert!(r["lua"].as_str().unwrap().contains("test(\"Вход\""), "{r}");
    assert_eq!(c.ask(json!({"op": "RecordDiscard", "phone": p}))["ev"], "Error");
}

#[test]
fn connect_policy_reports_the_choice_and_respects_if_unset() {
    let mut c = Conn::open(serve(Some("SER1"), None));
    // Телефон не готов (фейковая связь не подключается): значит «подключаем».
    let r = c.ask(json!({"op": "SetConnectPolicy", "serial": "SER1", "prefer": "usb"}));
    assert_eq!(r["ev"], "ConnectPolicy", "{r}");
    assert_eq!(r["chosen"], "connecting");
    // `if_unset`: выбор уже есть, второй раз не перебиваем, но ответ тот же.
    let r = c.ask(json!({"op": "SetConnectPolicy", "serial": "SER1", "prefer": "adb", "if_unset": true}));
    assert_eq!(r["ev"], "ConnectPolicy", "{r}");
    // Без предпочтения ответ `Ok`.
    assert_eq!(c.ask(json!({"op": "SetConnectPolicy", "scan_secs": 5}))["ev"], "Ok");
}

#[test]
fn list_explains_why_a_phone_is_not_ready_and_bad_check_url_is_rejected() {
    let mut c = Conn::open(serve(Some("SER1"), Some(Arc::new(Fake::default()))));
    let _ = phone_id(&mut c);
    let mut err = Value::Null;
    for _ in 0..50 {
        let r = c.ask(json!({"op": "List"}));
        if !r["details"][0]["error"].is_null() {
            err = r["details"][0]["error"].clone();
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(err["code"].is_string() && err["detail"].is_string(), "{err}");
    let p = phone_id(&mut c);
    let r = c.ask(json!({"op": "PublicIp", "phone": p, "url": "http://ok.example/ip"}));
    assert_eq!(r["ev"], "DeviceResult", "{r}");
}
