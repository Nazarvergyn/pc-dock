//! События прогона: запись в `events.jsonl`, хранение в памяти и живая рассылка подписчикам с повтором истории.

use std::fs::File;
use std::io::Write;
use std::path::Path;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

/// Куда сценарий и движок пишут события прогона.
pub trait Sink: Send + Sync {
    /// `kind`: `run_start`, `test_start`, `step`, `log`, `assert`, `artifact`, `test_end`, `run_end`; `fields`: объект.
    fn emit(&self, kind: &str, fields: Value);
}

/// Подписчик события; `false` — подписчик ушёл и его нужно убрать.
pub type Subscriber = Box<dyn Fn(&Value) -> bool + Send>;

#[derive(Default)]
struct Inner {
    events: Vec<Value>,
    subs: Vec<Subscriber>,
}

/// Журнал событий одного прогона. События получают сквозные номера `seq` с нуля.
pub struct RunLog {
    run: String,
    inner: Mutex<Inner>,
    file: Mutex<Option<File>>,
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

impl RunLog {
    /// `dir` — папка прогона для `events.jsonl` (если `None`, события только в памяти).
    pub fn new(run: &str, dir: Option<&Path>) -> Self {
        let file = dir.and_then(|d| File::create(d.join("events.jsonl")).ok());
        Self { run: run.to_string(), inner: Mutex::new(Inner::default()), file: Mutex::new(file) }
    }

    pub fn run(&self) -> &str {
        &self.run
    }

    /// Номер, который получит следующее событие (то есть сколько событий уже было).
    pub fn next_seq(&self) -> u64 {
        self.inner.lock().unwrap().events.len() as u64
    }

    /// Подписывает на события начиная с номера `from`: сначала приходит история, затем живой поток.
    pub fn subscribe(&self, from: u64, sub: Subscriber) {
        let mut inner = self.inner.lock().unwrap();
        for ev in inner.events.iter().skip(from as usize) {
            if !sub(ev) {
                return;
            }
        }
        inner.subs.push(sub);
    }

    /// Все события (для отчёта и повторного показа).
    pub fn events(&self) -> Vec<Value> {
        self.inner.lock().unwrap().events.clone()
    }
}

impl Sink for RunLog {
    fn emit(&self, kind: &str, fields: Value) {
        let mut inner = self.inner.lock().unwrap();
        let seq = inner.events.len() as u64;
        let mut ev = json!({"ev": "RunEvent", "run": self.run, "seq": seq, "ts": now_ms(), "kind": kind});
        if let (Some(obj), Some(extra)) = (ev.as_object_mut(), fields.as_object()) {
            for (k, v) in extra {
                obj.insert(k.clone(), v.clone());
            }
        }
        if let Some(f) = self.file.lock().unwrap().as_mut() {
            let _ = writeln!(f, "{ev}");
            let _ = f.flush();
        }
        inner.subs.retain(|s| s(&ev));
        inner.events.push(ev);
    }
}

/// Сборщик событий в памяти (для тестов).
#[derive(Default)]
pub struct Collect(pub Mutex<Vec<(String, Value)>>);

impl Sink for Collect {
    fn emit(&self, kind: &str, fields: Value) {
        self.0.lock().unwrap().push((kind.to_string(), fields));
    }
}

impl Collect {
    pub fn kinds(&self) -> Vec<String> {
        self.0.lock().unwrap().iter().map(|(k, _)| k.clone()).collect()
    }

    pub fn of(&self, kind: &str) -> Vec<Value> {
        self.0.lock().unwrap().iter().filter(|(k, _)| k == kind).map(|(_, v)| v.clone()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[test]
    fn late_subscriber_gets_history_then_live_events() {
        let log = RunLog::new("r1", None);
        log.emit("log", json!({"text": "a"}));
        log.emit("log", json!({"text": "b"}));
        let got = Arc::new(Mutex::new(Vec::new()));
        let g = got.clone();
        log.subscribe(
            1,
            Box::new(move |e| {
                g.lock().unwrap().push(e["text"].as_str().unwrap().to_string());
                true
            }),
        );
        log.emit("log", json!({"text": "c"}));
        assert_eq!(*got.lock().unwrap(), vec!["b", "c"], "история с номера 1 и живое событие");
        assert_eq!(log.events()[2]["seq"], 2);
        assert_eq!(log.events()[0]["run"], "r1");
    }

    #[test]
    fn gone_subscriber_is_dropped() {
        let log = RunLog::new("r", None);
        let calls = Arc::new(Mutex::new(0));
        let c = calls.clone();
        log.subscribe(
            0,
            Box::new(move |_| {
                *c.lock().unwrap() += 1;
                false
            }),
        );
        log.emit("log", json!({}));
        log.emit("log", json!({}));
        assert_eq!(*calls.lock().unwrap(), 1, "после отказа больше не вызывается");
    }

    #[test]
    fn events_are_written_as_json_lines() {
        let dir = std::env::temp_dir().join(format!("pcdock-events-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let log = RunLog::new("r", Some(&dir));
        log.emit("step", json!({"text": "x"}));
        log.emit("log", json!({"text": "y"}));
        let text = std::fs::read_to_string(dir.join("events.jsonl")).unwrap();
        let lines: Vec<Value> = text.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
        assert_eq!(lines.len(), 2);
        assert_eq!((lines[0]["kind"].as_str(), lines[1]["seq"].as_u64()), (Some("step"), Some(1)));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
