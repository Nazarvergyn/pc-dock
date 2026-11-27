//! Расписание: сценарии и цепочки по времени. Хранится в `schedules.json` в папке данных и переживает перезапуск демона.
//!
//! Правило (`rule`): `{"every": секунды}`; `{"daily": "ЧЧ:ММ", "days": [1..7], "utc_offset_min": смещение}` (дни: 1 понедельник, …, 7 воскресенье,
//! по умолчанию все; время в поясе со смещением от UTC); `{"at": время_в_мс}` один раз.
//! Цель (`target`): `{"script": id, "args"?, "phones"?}` или `{"chain": описание_цепочки}`.
//! `skip_if_running` (по умолчанию да): не запускать, пока предыдущий запуск того же расписания не закончился.

use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

use crate::chains::{parse_spec, Chains};
use crate::runs::Runs;

const DAY_MS: i64 = 86_400_000;

pub fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

#[derive(Clone, Debug, PartialEq)]
pub enum Rule {
    Every { secs: u64 },
    Daily { minute: u32, days: Vec<u8>, utc_offset_min: i32 },
    At { ms: u64 },
}

pub fn parse_rule(v: &Value) -> Result<Rule, String> {
    if let Some(secs) = v["every"].as_u64() {
        if secs < 10 {
            return Err("every: не меньше 10 секунд".into());
        }
        return Ok(Rule::Every { secs });
    }
    if let Some(at) = v["daily"].as_str() {
        let (h, m) = at.split_once(':').ok_or("daily: формат ЧЧ:ММ")?;
        let (h, m): (u32, u32) =
            (h.parse().map_err(|_| "daily: формат ЧЧ:ММ")?, m.parse().map_err(|_| "daily: формат ЧЧ:ММ")?);
        if h > 23 || m > 59 {
            return Err("daily: время вне суток".into());
        }
        let days: Vec<u8> = match v["days"].as_array() {
            Some(a) => a.iter().filter_map(|x| x.as_u64().map(|n| n as u8)).collect(),
            None => (1..=7).collect(),
        };
        if days.is_empty() || days.iter().any(|d| !(1..=7).contains(d)) {
            return Err("days: числа от 1 (понедельник) до 7 (воскресенье)".into());
        }
        let off = v["utc_offset_min"].as_i64().unwrap_or(0);
        if !(-14 * 60..=14 * 60).contains(&off) {
            return Err("utc_offset_min: от -840 до 840".into());
        }
        return Ok(Rule::Daily { minute: h * 60 + m, days, utc_offset_min: off as i32 });
    }
    if let Some(ms) = v["at"].as_u64() {
        return Ok(Rule::At { ms });
    }
    Err("rule: every, daily или at".into())
}

/// Ближайший запуск строго после `after_ms` (для `every` от `after_ms`).
pub fn next_after(rule: &Rule, after_ms: u64) -> Option<u64> {
    match rule {
        Rule::Every { secs } => Some(after_ms + secs * 1000),
        Rule::At { ms } => (*ms > after_ms).then_some(*ms),
        Rule::Daily { minute, days, utc_offset_min } => {
            let off = *utc_offset_min as i64 * 60_000;
            let local = after_ms as i64 + off;
            let day0 = local.div_euclid(DAY_MS);
            for d in day0..=day0 + 7 {
                // 1970-01-01 был четвергом: понедельник = 1
                let weekday = ((d + 3).rem_euclid(7) + 1) as u8;
                if !days.contains(&weekday) {
                    continue;
                }
                let cand = d * DAY_MS + *minute as i64 * 60_000 - off;
                if cand > after_ms as i64 {
                    return Some(cand as u64);
                }
            }
            None
        }
    }
}

#[derive(Clone, Debug)]
pub struct Schedule {
    pub id: String,
    pub name: String,
    pub rule_json: Value,
    pub rule: Rule,
    pub target: Value,
    pub enabled: bool,
    pub skip_if_running: bool,
    pub next_ms: Option<u64>,
    pub last_ms: Option<u64>,
    pub last_status: Option<String>,
    /// Что запущено последним (`run` или `chain`) и его номер.
    running: Option<(bool, String)>,
}

impl Schedule {
    pub fn to_json(&self) -> Value {
        json!({"schedule": self.id, "name": self.name, "rule": self.rule_json, "target": self.target,
               "enabled": self.enabled, "skip_if_running": self.skip_if_running,
               "next": self.next_ms, "last": self.last_ms, "last_status": self.last_status})
    }

    fn stored(&self) -> Value {
        json!({"id": self.id, "name": self.name, "rule": self.rule_json, "target": self.target,
               "enabled": self.enabled, "skip_if_running": self.skip_if_running, "last": self.last_ms})
    }
}

pub struct Scheduler {
    file: PathBuf,
    runs: Arc<Runs>,
    chains: Arc<Chains>,
    items: Mutex<Vec<Schedule>>,
}

fn check_target(t: &Value) -> Result<(), String> {
    if t["script"].is_string() {
        return Ok(());
    }
    if t["chain"].is_object() {
        return parse_spec(&t["chain"]).map(|_| ());
    }
    Err("target: {script} или {chain}".into())
}

impl Scheduler {
    pub fn new(data: &std::path::Path, runs: Arc<Runs>, chains: Arc<Chains>) -> Result<Arc<Self>, String> {
        let file = data.join("schedules.json");
        let mut items = Vec::new();
        if let Ok(text) = fs::read_to_string(&file) {
            let list: Vec<Value> =
                serde_json::from_str(&text).map_err(|e| format!("{}: {e}", file.display()))?;
            for v in list {
                let rule = parse_rule(&v["rule"])?;
                let last = v["last"].as_u64();
                items.push(Schedule {
                    id: v["id"].as_str().unwrap_or("").to_string(),
                    name: v["name"].as_str().unwrap_or("").to_string(),
                    next_ms: next_after(&rule, now_ms()),
                    rule_json: v["rule"].clone(),
                    rule,
                    target: v["target"].clone(),
                    enabled: v["enabled"].as_bool().unwrap_or(true),
                    skip_if_running: v["skip_if_running"].as_bool().unwrap_or(true),
                    last_ms: last,
                    last_status: None,
                    running: None,
                });
            }
        }
        Ok(Arc::new(Self { file, runs, chains, items: Mutex::new(items) }))
    }

    fn save(&self, items: &[Schedule]) {
        let list: Vec<Value> = items.iter().map(Schedule::stored).collect();
        let _ = fs::write(&self.file, serde_json::to_string_pretty(&list).unwrap());
    }

    pub fn list(&self) -> Vec<Value> {
        self.items.lock().unwrap().iter().map(Schedule::to_json).collect()
    }

    /// Создаёт расписание (`id` пуст) или заменяет существующее.
    pub fn put(&self, id: Option<&str>, msg: &Value) -> Result<String, String> {
        let rule = parse_rule(&msg["rule"])?;
        check_target(&msg["target"])?;
        let mut items = self.items.lock().unwrap();
        let id = match id {
            Some(i) if items.iter().any(|s| s.id == i) => i.to_string(),
            Some(i) => return Err(format!("нет расписания {i}")),
            None => format!("s{:x}", now_ms()),
        };
        let item = Schedule {
            id: id.clone(),
            name: msg["name"].as_str().unwrap_or("расписание").to_string(),
            next_ms: next_after(&rule, now_ms()),
            rule_json: msg["rule"].clone(),
            rule,
            target: msg["target"].clone(),
            enabled: msg["enabled"].as_bool().unwrap_or(true),
            skip_if_running: msg["skip_if_running"].as_bool().unwrap_or(true),
            last_ms: None,
            last_status: None,
            running: None,
        };
        items.retain(|s| s.id != id);
        items.push(item);
        self.save(&items);
        Ok(id)
    }

    pub fn delete(&self, id: &str) -> Result<(), String> {
        let mut items = self.items.lock().unwrap();
        let before = items.len();
        items.retain(|s| s.id != id);
        if items.len() == before {
            return Err(format!("нет расписания {id}"));
        }
        self.save(&items);
        Ok(())
    }

    /// Запускает то, что подошло ко времени `now`. Возвращает номера запущенных прогонов и цепочек.
    pub fn tick(&self, now: u64) -> Vec<String> {
        let mut started = Vec::new();
        let mut items = self.items.lock().unwrap();
        for s in items.iter_mut() {
            // Закончившийся запуск запоминаем как последний итог.
            if let Some((is_chain, id)) = s.running.clone() {
                let status = if is_chain {
                    self.chains.get(&id).filter(|c| c.is_finished()).map(|c| c.summary()["status"].clone())
                } else {
                    self.runs.get(&id).filter(|r| r.is_finished()).map(|r| r.summary()["status"].clone())
                };
                if let Some(st) = status {
                    s.last_status = st.as_str().map(String::from);
                    s.running = None;
                }
            }
            let Some(due) = s.next_ms else { continue };
            if !s.enabled || due > now {
                continue;
            }
            // Следующий запуск считаем от сейчас: пропущенные за время простоя не наверстываем.
            s.next_ms = next_after(&s.rule, now);
            if s.skip_if_running && s.running.is_some() {
                continue;
            }
            let launched = if let Some(script) = s.target["script"].as_str() {
                let phones = s.target["phones"]
                    .as_array()
                    .map(|a| a.iter().filter_map(|x| x.as_u64().map(|n| n as u32)).collect::<Vec<u32>>());
                let args = if s.target["args"].is_null() { json!({}) } else { s.target["args"].clone() };
                self.runs.start(script, phones, args, None, None).map(|r| (false, r.id.clone()))
            } else {
                parse_spec(&s.target["chain"])
                    .and_then(|spec| self.chains.start(spec))
                    .map(|c| (true, c.id.clone()))
            };
            s.last_ms = Some(now);
            match launched {
                Ok((is_chain, id)) => {
                    started.push(id.clone());
                    s.running = Some((is_chain, id));
                    s.last_status = Some("running".into());
                }
                Err(e) => s.last_status = Some(format!("не запущено: {e}")),
            }
        }
        self.save(&items);
        started
    }

    /// Поток, который раз в `every` секунд вызывает `tick`.
    pub fn spawn(self: &Arc<Self>, every: std::time::Duration) {
        let me = self.clone();
        std::thread::Builder::new()
            .name("scheduler".into())
            .spawn(move || loop {
                std::thread::sleep(every);
                me.tick(now_ms());
            })
            .expect("не встал поток расписания");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // 2026-11-12 (четверг) 00:00:00 UTC
    const THU: u64 = 1_794_441_600_000;

    #[test]
    fn rules_are_parsed() {
        assert_eq!(parse_rule(&json!({"every": 60})).unwrap(), Rule::Every { secs: 60 });
        assert!(parse_rule(&json!({"every": 1})).is_err());
        assert!(parse_rule(&json!({"daily": "25:00"})).is_err());
        assert!(parse_rule(&json!({"daily": "09:00", "days": [8]})).is_err());
        assert!(parse_rule(&json!({})).is_err());
    }

    #[test]
    fn daily_rule_respects_days_and_offset() {
        // каждый день в 09:30 UTC: от полуночи четверга это тот же день
        let r = parse_rule(&json!({"daily": "09:30"})).unwrap();
        assert_eq!(next_after(&r, THU), Some(THU + (9 * 60 + 30) * 60_000));
        // после 09:30 следующий запуск завтра
        assert_eq!(
            next_after(&r, THU + (10 * 60) * 60_000),
            Some(THU + DAY_MS as u64 + (9 * 60 + 30) * 60_000)
        );
        // только по понедельникам (1): от четверга через 4 дня
        let r = parse_rule(&json!({"daily": "00:00", "days": [1]})).unwrap();
        assert_eq!(next_after(&r, THU), Some(THU + 4 * DAY_MS as u64));
        // смещение +180 минут: 09:30 по местному это 06:30 UTC
        let r = parse_rule(&json!({"daily": "09:30", "utc_offset_min": 180})).unwrap();
        assert_eq!(next_after(&r, THU), Some(THU + (6 * 60 + 30) * 60_000));
        // один раз: в прошлом нет следующего
        assert_eq!(next_after(&Rule::At { ms: THU }, THU), None);
    }
}
