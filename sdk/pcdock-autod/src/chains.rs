//! Цепочки: несколько сценариев подряд как одно задание. Шаг это обычный прогон; у цепочки свой журнал событий
//! (`chain_start`, `chain_round`, `chain_step`, `chain_end`) и итоговый `chain.json` в `chains/<id>/`.
//!
//! Описание цепочки (`RunChain`): `steps` — список шагов `{script, args?, phones?, on_fail?, retries?, pause?, timeout?}`,
//! `rounds` — сколько кругов (по умолчанию 1), `round_pause` — пауза между кругами в секундах, `name`.
//! `on_fail`: `stop` (по умолчанию), `continue` (идти дальше) или `retry` (повторить `retries` раз, потом остановить).

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

use crate::events::{RunLog, Sink};
use crate::runs::Runs;

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

#[derive(Clone, Debug, PartialEq)]
pub enum OnFail {
    Stop,
    Continue,
    Retry,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Step {
    pub script: String,
    pub args: Value,
    pub phones: Option<Vec<u32>>,
    pub on_fail: OnFail,
    pub retries: u32,
    pub pause: f64,
    pub timeout: Option<f64>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ChainSpec {
    pub name: String,
    pub steps: Vec<Step>,
    pub rounds: u32,
    pub round_pause: f64,
}

/// Разбирает и проверяет описание цепочки.
pub fn parse_spec(v: &Value) -> Result<ChainSpec, String> {
    let steps_json =
        v["steps"].as_array().filter(|a| !a.is_empty()).ok_or("steps: нужен непустой список шагов")?;
    if steps_json.len() > 200 {
        return Err("steps: не больше 200 шагов".into());
    }
    let mut steps = Vec::new();
    for (i, s) in steps_json.iter().enumerate() {
        let at = |m: &str| format!("шаг {}: {m}", i + 1);
        let script = s["script"].as_str().ok_or_else(|| at("нужен script"))?.to_string();
        let on_fail = match s["on_fail"].as_str().unwrap_or("stop") {
            "stop" => OnFail::Stop,
            "continue" => OnFail::Continue,
            "retry" => OnFail::Retry,
            other => return Err(at(&format!("on_fail {other:?}: stop, continue или retry"))),
        };
        let retries = s["retries"].as_u64().unwrap_or(if on_fail == OnFail::Retry { 1 } else { 0 });
        if retries > 20 {
            return Err(at("retries: не больше 20"));
        }
        let phones = s["phones"]
            .as_array()
            .map(|a| a.iter().filter_map(|x| x.as_u64().map(|n| n as u32)).collect::<Vec<u32>>());
        steps.push(Step {
            script,
            args: if s["args"].is_null() { json!({}) } else { s["args"].clone() },
            phones,
            on_fail,
            retries: retries as u32,
            pause: s["pause"].as_f64().unwrap_or(0.0).clamp(0.0, 86_400.0),
            timeout: s["timeout"].as_f64().filter(|t| *t > 0.0),
        });
    }
    let rounds = v["rounds"].as_u64().unwrap_or(1);
    if !(1..=10_000).contains(&rounds) {
        return Err("rounds: от 1 до 10000".into());
    }
    Ok(ChainSpec {
        name: v["name"].as_str().unwrap_or("цепочка").to_string(),
        steps,
        rounds: rounds as u32,
        round_pause: v["round_pause"].as_f64().unwrap_or(0.0).clamp(0.0, 86_400.0),
    })
}

pub struct Chain {
    pub id: String,
    pub name: String,
    pub started_ms: u64,
    pub log: Arc<RunLog>,
    stop: AtomicBool,
    finished: Mutex<Option<(u64, String)>>,
    current_run: Mutex<Option<String>>,
}

impl Chain {
    pub fn summary(&self) -> Value {
        let fin = self.finished.lock().unwrap().clone();
        json!({"chain": self.id, "name": self.name, "started": self.started_ms,
               "finished": fin.as_ref().map(|f| f.0), "status": fin.map(|f| f.1).unwrap_or_else(|| "running".into())})
    }

    pub fn is_finished(&self) -> bool {
        self.finished.lock().unwrap().is_some()
    }
}

type Notify = Box<dyn Fn(&Value) + Send + Sync>;

pub struct Chains {
    dir: PathBuf,
    runs: Arc<Runs>,
    chains: Mutex<HashMap<String, Arc<Chain>>>,
    notify: Mutex<Option<Notify>>,
}

impl Chains {
    pub fn new(data: &std::path::Path, runs: Arc<Runs>) -> Result<Arc<Self>, String> {
        let dir = data.join("chains");
        fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        Ok(Arc::new(Self { dir, runs, chains: Mutex::new(HashMap::new()), notify: Mutex::new(None) }))
    }

    pub fn set_notify(&self, f: Notify) {
        *self.notify.lock().unwrap() = Some(f);
    }

    fn notify(&self, v: Value) {
        if let Some(f) = self.notify.lock().unwrap().as_ref() {
            f(&v);
        }
    }

    pub fn get(&self, id: &str) -> Option<Arc<Chain>> {
        self.chains.lock().unwrap().get(id).cloned()
    }

    pub fn list(&self) -> Vec<Value> {
        let mut v: Vec<Value> = self.chains.lock().unwrap().values().map(|c| c.summary()).collect();
        v.sort_by_key(|x| std::cmp::Reverse(x["started"].as_u64().unwrap_or(0)));
        v
    }

    /// Итог цепочки с диска.
    pub fn report(&self, id: &str) -> Option<Value> {
        if !crate::runs::valid_artifact(id) {
            return None;
        }
        serde_json::from_str(&fs::read_to_string(self.dir.join(id).join("chain.json")).ok()?).ok()
    }

    pub fn stop(&self, id: &str) -> bool {
        match self.get(id) {
            Some(c) if !c.is_finished() => {
                c.stop.store(true, Ordering::SeqCst);
                if let Some(run) = c.current_run.lock().unwrap().clone() {
                    self.runs.stop(&run);
                }
                true
            }
            _ => false,
        }
    }

    pub fn stop_all(&self) {
        let ids: Vec<String> = self.chains.lock().unwrap().keys().cloned().collect();
        for id in ids {
            self.stop(&id);
        }
    }

    pub fn wait(&self, id: &str, timeout: Duration) -> bool {
        let end = Instant::now() + timeout;
        while Instant::now() < end {
            if self.get(id).is_some_and(|c| c.is_finished()) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        false
    }

    pub fn start(self: &Arc<Self>, spec: ChainSpec) -> Result<Arc<Chain>, String> {
        let id = format!("c{:x}", now_ms());
        let dir = self.dir.join(&id);
        fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        let chain = Arc::new(Chain {
            id: id.clone(),
            name: spec.name.clone(),
            started_ms: now_ms(),
            log: Arc::new(RunLog::new(&id, Some(&dir))),
            stop: AtomicBool::new(false),
            finished: Mutex::new(None),
            current_run: Mutex::new(None),
        });
        self.chains.lock().unwrap().insert(id.clone(), chain.clone());
        self.notify(json!({"ev": "ChainStarted", "chain": id, "name": spec.name}));
        let (me, c) = (self.clone(), chain.clone());
        std::thread::Builder::new()
            .name(format!("chain-{id}"))
            .spawn(move || {
                let (status, steps) = me.execute(&c, &spec);
                let finished = now_ms();
                let doc = json!({"chain": c.id, "name": c.name, "started": c.started_ms, "finished": finished,
                                 "status": status, "rounds": spec.rounds, "steps": steps});
                let _ = fs::write(
                    me.dir.join(&c.id).join("chain.json"),
                    serde_json::to_string_pretty(&doc).unwrap(),
                );
                c.log.emit("chain_end", json!({"status": status}));
                *c.finished.lock().unwrap() = Some((finished, status.clone()));
                me.notify(json!({"ev": "ChainEnded", "chain": c.id, "status": status}));
            })
            .map_err(|e| format!("не встал поток цепочки: {e}"))?;
        Ok(chain)
    }

    /// Ждёт паузу, не пропуская остановку.
    fn pause(&self, c: &Chain, secs: f64) {
        let end = Instant::now() + Duration::from_secs_f64(secs);
        while Instant::now() < end && !c.stop.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Один прогон шага: статус (`passed`, `failed`, `error`, `stopped`) и номер прогона.
    fn run_step(&self, c: &Chain, step: &Step) -> (String, Option<String>) {
        let timeout = step.timeout.map(Duration::from_secs_f64);
        let run = match self.runs.start(&step.script, step.phones.clone(), step.args.clone(), None, timeout) {
            Ok(r) => r,
            Err(e) => {
                c.log.emit("log", json!({"level": "error", "text": e}));
                return ("error".into(), None);
            }
        };
        *c.current_run.lock().unwrap() = Some(run.id.clone());
        while !run.is_finished() {
            std::thread::sleep(Duration::from_millis(20));
        }
        *c.current_run.lock().unwrap() = None;
        let status = run.summary()["status"].as_str().unwrap_or("error").to_string();
        (status, Some(run.id.clone()))
    }

    fn execute(&self, c: &Chain, spec: &ChainSpec) -> (String, Vec<Value>) {
        c.log.emit(
            "chain_start",
            json!({"name": spec.name, "rounds": spec.rounds, "steps": spec.steps.len()}),
        );
        let mut report = Vec::new();
        let mut any_failed = false;
        'rounds: for round in 1..=spec.rounds {
            c.log.emit("chain_round", json!({"round": round}));
            for (i, step) in spec.steps.iter().enumerate() {
                let mut attempt = 0;
                loop {
                    if c.stop.load(Ordering::SeqCst) {
                        return ("stopped".into(), report);
                    }
                    attempt += 1;
                    let (status, run) = self.run_step(c, step);
                    c.log.emit(
                        "chain_step",
                        json!({"round": round, "step": i + 1, "script": step.script, "attempt": attempt,
                               "step_run": run, "status": status}),
                    );
                    report.push(
                        json!({"round": round, "step": i + 1, "script": step.script, "attempt": attempt,
                                       "step_run": run, "status": status}),
                    );
                    if status == "passed" {
                        break;
                    }
                    if status == "stopped" || c.stop.load(Ordering::SeqCst) {
                        return ("stopped".into(), report);
                    }
                    if step.on_fail == OnFail::Retry && attempt <= step.retries {
                        continue;
                    }
                    any_failed = true;
                    if step.on_fail != OnFail::Continue {
                        return ("failed".into(), report);
                    }
                    break;
                }
                if step.pause > 0.0 {
                    self.pause(c, step.pause);
                }
            }
            if round < spec.rounds && spec.round_pause > 0.0 {
                self.pause(c, spec.round_pause);
                if c.stop.load(Ordering::SeqCst) {
                    break 'rounds;
                }
            }
        }
        if c.stop.load(Ordering::SeqCst) {
            return ("stopped".into(), report);
        }
        (if any_failed { "failed" } else { "passed" }.into(), report)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spec_is_parsed_and_checked() {
        let s = parse_spec(&json!({"name": "ночь", "rounds": 2, "round_pause": 5,
            "steps": [{"script": "a"}, {"script": "b", "on_fail": "retry", "retries": 2, "pause": 1.5, "args": {"x": 1}}]}))
        .unwrap();
        assert_eq!((s.rounds, s.steps.len(), s.round_pause), (2, 2, 5.0));
        assert_eq!(
            (s.steps[1].on_fail.clone(), s.steps[1].retries, s.steps[1].pause),
            (OnFail::Retry, 2, 1.5)
        );
        assert_eq!(s.steps[0].on_fail, OnFail::Stop);
        assert!(parse_spec(&json!({"steps": []})).is_err());
        assert!(parse_spec(&json!({"steps": [{"script": "a", "on_fail": "x"}]})).is_err());
        assert!(parse_spec(&json!({"steps": [{"script": "a"}], "rounds": 0})).is_err());
        assert!(parse_spec(&json!({"steps": [{}]})).is_err());
    }
}
