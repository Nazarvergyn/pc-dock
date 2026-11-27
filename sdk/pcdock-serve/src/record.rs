//! Запись сценария с телефона: касания человека превращаются в шаги и готовый Lua.
//!
//! Касания читаются из `getevent` (см. `AdbDevice::input_lines`), шаги можно править, удалять и дополнять проверками
//! «на экране видно X». Всё это команды протокола, интерфейс не нужен: клиентом может быть и скрипт.

use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

/// Пределы координат сенсора (`ABS_MT_POSITION_X/Y`): из них получаются доли экрана.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Ranges {
    pub max_x: f64,
    pub max_y: f64,
}

/// `getevent -pl`: строки `ABS_MT_POSITION_X : value 0, min 0, max 1079, …`.
pub fn parse_ranges(out: &str) -> Option<Ranges> {
    let max = |name: &str| {
        out.lines().filter(|l| l.contains(name)).find_map(|l| {
            let rest = l.split("max ").nth(1)?;
            rest.split(|c: char| c == ',' || c.is_whitespace()).next()?.parse::<f64>().ok()
        })
    };
    Some(Ranges { max_x: max("ABS_MT_POSITION_X")?, max_y: max("ABS_MT_POSITION_Y")? })
}

/// Законченное касание: начало, конец, время в секундах по часам телефона.
#[derive(Clone, Debug, PartialEq)]
pub struct Gesture {
    pub t0: f64,
    pub t1: f64,
    pub x0: f64,
    pub y0: f64,
    pub x1: f64,
    pub y1: f64,
}

/// Что вышло из потока `getevent -lt`.
#[derive(Clone, Debug, PartialEq)]
pub enum Input {
    /// Палец лёг (первая точка нового касания): по ней успевают прочитать экран.
    Down {
        t: f64,
        x: f64,
        y: f64,
    },
    Gesture(Gesture),
    /// Системная кнопка: `back`, `home`, `recents`.
    Key {
        t: f64,
        name: &'static str,
    },
}

/// Разбор потока `getevent -lt` по строке; один палец (запись жестов в несколько пальцев не ведётся).
pub struct Tracker {
    ranges: Ranges,
    down: Option<(f64, f64, f64)>,
    last: (f64, f64),
    announced: bool,
    cur: (Option<f64>, Option<f64>),
}

impl Tracker {
    pub fn new(ranges: Ranges) -> Self {
        Self { ranges, down: None, last: (0.0, 0.0), announced: false, cur: (None, None) }
    }

    /// Строка вида `[  123.456789] /dev/input/event3: EV_ABS ABS_MT_POSITION_X 000001f4`.
    pub fn feed(&mut self, line: &str) -> Vec<Input> {
        let mut out = Vec::new();
        let Some(close) = line.find(']') else { return out };
        let Ok(t) = line[..close].trim_start_matches('[').trim().parse::<f64>() else { return out };
        let mut w = line[close + 1..].split_whitespace().skip(1);
        let (Some(ev), Some(code), Some(val)) = (w.next(), w.next(), w.next()) else { return out };
        let hex = || u32::from_str_radix(val, 16).ok();
        match (ev, code) {
            ("EV_ABS", "ABS_MT_POSITION_X") => self.cur.0 = hex().map(f64::from),
            ("EV_ABS", "ABS_MT_POSITION_Y") => self.cur.1 = hex().map(f64::from),
            ("EV_ABS", "ABS_MT_TRACKING_ID") => {
                if val.eq_ignore_ascii_case("ffffffff") {
                    if let Some((t0, x0, y0)) = self.down.take() {
                        let (x1, y1) = self.last;
                        out.push(Input::Gesture(Gesture { t0, t1: t, x0, y0, x1, y1 }));
                    }
                    self.announced = false;
                } else if self.down.is_none() {
                    self.cur = (None, None);
                    self.down = Some((t, -1.0, -1.0));
                }
            }
            ("EV_KEY", "KEY_BACK" | "KEY_HOMEPAGE" | "KEY_HOME" | "KEY_APPSELECT" | "KEY_MENU")
                if val == "DOWN" =>
            {
                let name = match code {
                    "KEY_BACK" => "back",
                    "KEY_HOMEPAGE" | "KEY_HOME" => "home",
                    _ => "recents",
                };
                out.push(Input::Key { t, name });
            }
            ("EV_SYN", "SYN_REPORT") => {
                if let (Some(d), (Some(x), Some(y))) = (self.down.as_mut(), self.cur) {
                    let (fx, fy) =
                        ((x / self.ranges.max_x).clamp(0.0, 1.0), (y / self.ranges.max_y).clamp(0.0, 1.0));
                    if d.1 < 0.0 {
                        d.1 = fx;
                        d.2 = fy;
                    }
                    self.last = (fx, fy);
                    if !self.announced {
                        self.announced = true;
                        out.push(Input::Down { t: d.0, x: d.1, y: d.2 });
                    }
                }
            }
            _ => {}
        }
        out
    }
}

/// Шаг записи.
#[derive(Clone, Debug, PartialEq)]
pub struct Step {
    pub id: u32,
    /// Секунды от начала записи.
    pub t: f64,
    /// Вид: `tap`, `long`, `swipe`, `button`, `text`, `check`, `wait`.
    pub kind: String,
    pub v: Value,
    pub warn: Option<String>,
    /// `gesture`, `edit` или `check`.
    pub src: String,
    pub pkg: Option<String>,
    pub lua: String,
    pub problem: Option<String>,
}

/// Строка Lua в двойных кавычках.
pub fn lua_str(s: &str) -> String {
    let mut o = String::from("\"");
    for c in s.chars() {
        match c {
            '\\' => o.push_str("\\\\"),
            '"' => o.push_str("\\\""),
            '\n' => o.push_str("\\n"),
            '\r' => o.push_str("\\r"),
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

fn num(v: &Value, k: &str) -> Option<f64> {
    v.get(k).and_then(Value::as_f64)
}

fn n4(x: f64) -> String {
    let s = format!("{x:.4}");
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

/// Lua для шага: `Err` с причиной, если шаг не перевести.
pub fn step_lua(kind: &str, v: &Value) -> Result<String, String> {
    let need = |k: &str| num(v, k).ok_or_else(|| format!("нет поля {k}"));
    let target = v.get("target").and_then(Value::as_str).filter(|s| !s.is_empty());
    match kind {
        "tap" => match target {
            Some(t) => Ok(format!("p:tap_on({})", lua_str(t))),
            None => Ok(format!("p:tap({}, {})", n4(need("x")?), n4(need("y")?))),
        },
        "long" => Ok(format!(
            "p:tap({}, {}, {{hold = {}}})",
            n4(need("x")?),
            n4(need("y")?),
            n4(num(v, "hold").unwrap_or(0.8))
        )),
        "swipe" => Ok(format!(
            "p:swipe({}, {}, {}, {}, {{duration = {}}})",
            n4(need("x1")?),
            n4(need("y1")?),
            n4(need("x2")?),
            n4(need("y2")?),
            n4(num(v, "duration").unwrap_or(0.3))
        )),
        "button" => {
            let n = v.get("name").and_then(Value::as_str).ok_or("нет поля name")?;
            if crate::device::button_keycode(n).is_none() {
                return Err(format!("неизвестная кнопка {n}"));
            }
            Ok(format!("p:button({})", lua_str(n)))
        }
        "text" => Ok(format!(
            "p:type_text({})",
            lua_str(v.get("text").and_then(Value::as_str).ok_or("нет поля text")?)
        )),
        "check" => {
            let t = target.ok_or("нет поля target")?;
            Ok(format!("assert.truthy(p:find_one({}))", lua_str(t)))
        }
        "wait" => Ok(format!("sleep({})", n4(need("seconds")?))),
        other => Err(format!("этот шаг не перевести в Lua: вид {other}")),
    }
}

/// Цель поиска для элемента экрана (как принимает Lua `find_one`): текст, подпись или id.
pub fn element_target(e: &Value) -> Option<String> {
    let s = |k: &str| e.get(k).and_then(Value::as_str).filter(|s| !s.is_empty());
    s("text")
        .map(str::to_string)
        .or_else(|| s("desc").map(|d| format!("desc:{d}")))
        .or_else(|| s("id").map(|i| format!("id:{}", i.rsplit(":id/").next().unwrap_or(i))))
}

fn now_s() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Запись одного телефона.
pub struct Recording {
    pub status: &'static str,
    pub steps: Vec<Step>,
    next_id: u32,
    pub busy: u32,
    pub notice: Option<String>,
    pub foreground: Option<String>,
    pub started_at: u64,
    pub app: Option<String>,
    screens: HashMap<u32, Vec<Value>>,
    next_screen: u32,
}

impl Recording {
    pub fn new() -> Self {
        Self {
            status: "recording",
            steps: Vec::new(),
            next_id: 1,
            busy: 0,
            notice: None,
            foreground: None,
            started_at: now_s(),
            app: None,
            screens: HashMap::new(),
            next_screen: 1,
        }
    }

    pub fn add(
        &mut self,
        t: f64,
        kind: &str,
        v: Value,
        src: &str,
        pkg: Option<String>,
        warn: Option<String>,
    ) -> u32 {
        let id = self.next_id;
        self.next_id += 1;
        let (lua, problem) = match step_lua(kind, &v) {
            Ok(l) => (l, None),
            Err(e) => (String::new(), Some(e)),
        };
        if self.app.is_none() {
            self.app = pkg.clone();
        }
        self.steps.push(Step { id, t, kind: kind.into(), v, warn, src: src.into(), pkg, lua, problem });
        id
    }

    /// Правка: шаги из `list` заменяют текущие; `id: 0` новый шаг, не названный `id` удалён,
    /// у шага с новыми `t`/`v` Lua строится заново.
    pub fn set_steps(&mut self, list: &[Value]) -> Result<(), String> {
        let mut out = Vec::new();
        for s in list {
            let id = s.get("id").and_then(Value::as_u64).ok_or("шаг без id")? as u32;
            let kind = s.get("t").and_then(Value::as_str).ok_or("шаг без t (вид)")?.to_string();
            let v = s.get("v").cloned().unwrap_or(Value::Null);
            let warn = s.get("warn").and_then(Value::as_str).map(str::to_string);
            let src = s.get("src").and_then(Value::as_str).unwrap_or("edit").to_string();
            let pkg = s.get("pkg").and_then(Value::as_str).map(str::to_string);
            let time = s.get("time").and_then(Value::as_f64);
            let old = self.steps.iter().find(|o| o.id == id && id != 0);
            let new_id = if id == 0 {
                let n = self.next_id;
                self.next_id += 1;
                n
            } else if old.is_some() {
                id
            } else {
                return Err(format!("нет шага {id}"));
            };
            let (lua, problem) = match old {
                Some(o) if o.kind == kind && o.v == v => (o.lua.clone(), o.problem.clone()),
                _ => match step_lua(&kind, &v) {
                    Ok(l) => (l, None),
                    Err(e) => (String::new(), Some(e)),
                },
            };
            let t =
                time.or(old.map(|o| o.t)).unwrap_or_else(|| out.last().map(|s: &Step| s.t).unwrap_or(0.0));
            out.push(Step { id: new_id, t, kind, v, warn, src, pkg, lua, problem });
        }
        self.steps = out;
        Ok(())
    }

    /// Запомнить снимок экрана для выбора проверки; возвращает номер снимка.
    pub fn remember_screen(&mut self, elements: Vec<Value>) -> u32 {
        let n = self.next_screen;
        self.next_screen += 1;
        self.screens.insert(n, elements);
        self.screens.retain(|k, _| *k + 5 > n);
        n
    }

    pub fn screen_element(&self, num: u32, idx: usize) -> Result<&Value, String> {
        self.screens
            .get(&num)
            .ok_or_else(|| format!("нет снимка экрана {num}"))?
            .get(idx)
            .ok_or_else(|| format!("в снимке {num} нет элемента {idx}"))
    }

    pub fn snapshot(&self, phone: u32) -> Value {
        let app = self.app.clone();
        json!({
            "phone": phone,
            "status": self.status,
            "steps": self.steps.iter().map(|s| json!({
                "id": s.id, "t": s.kind, "time": s.t, "v": s.v, "warn": s.warn, "src": s.src,
                "pkg": s.pkg, "lua": s.lua, "problem": s.problem,
            })).collect::<Vec<_>>(),
            "busy": self.busy > 0,
            "notice": self.notice,
            "foreground": self.foreground,
            "started_at": self.started_at,
            "app": app,
            "default_name": format!("Запись {}", app.as_deref().unwrap_or("сценарий")),
        })
    }

    /// Готовый сценарий. `problem`: первый шаг, который не перевёлся (такие шаги в Lua пропускаются).
    pub fn script(&self, name: &str, origin: &str) -> (String, Option<String>) {
        let mut lua = String::new();
        for l in origin.lines() {
            lua.push_str(&format!("-- {l}\n"));
        }
        lua.push_str(&format!("test({}, function()\n  local p = phones()[1]\n", lua_str(name)));
        let mut problem = None;
        let mut prev = 0.0;
        for s in &self.steps {
            if let Some(pr) = &s.problem {
                problem.get_or_insert_with(|| format!("шаг {}: {pr}", s.id));
                continue;
            }
            let gap = s.t - prev;
            if s.kind != "wait" && s.src == "gesture" && gap >= 0.6 {
                lua.push_str(&format!("  sleep({})\n", n4((gap.min(5.0) * 10.0).round() / 10.0)));
            }
            prev = s.t;
            lua.push_str(&format!("  {}\n", s.lua));
        }
        lua.push_str("end)\n");
        (lua, problem)
    }
}

impl Default for Recording {
    fn default() -> Self {
        Self::new()
    }
}

/// Касание → вид и параметры шага.
pub fn gesture_step(g: &Gesture) -> (&'static str, Value) {
    let dist = ((g.x1 - g.x0).powi(2) + (g.y1 - g.y0).powi(2)).sqrt();
    let dur = (g.t1 - g.t0).max(0.0);
    if dist < 0.02 {
        if dur >= 0.5 {
            ("long", json!({"x": g.x0, "y": g.y0, "hold": (dur * 100.0).round() / 100.0}))
        } else {
            ("tap", json!({"x": g.x0, "y": g.y0}))
        }
    } else {
        (
            "swipe",
            json!({"x1": g.x0, "y1": g.y0, "x2": g.x1, "y2": g.y1, "duration": ((dur.max(0.05)) * 100.0).round() / 100.0}),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
[  100.000000] /dev/input/event3: EV_ABS       ABS_MT_TRACKING_ID   00000007
[  100.000000] /dev/input/event3: EV_ABS       ABS_MT_POSITION_X    00000200
[  100.000000] /dev/input/event3: EV_ABS       ABS_MT_POSITION_Y    00000400
[  100.000000] /dev/input/event3: EV_SYN       SYN_REPORT           00000000
[  100.080000] /dev/input/event3: EV_ABS       ABS_MT_POSITION_Y    00000402
[  100.080000] /dev/input/event3: EV_SYN       SYN_REPORT           00000000
[  100.100000] /dev/input/event3: EV_ABS       ABS_MT_TRACKING_ID   ffffffff
[  100.100000] /dev/input/event3: EV_SYN       SYN_REPORT           00000000
[  102.000000] /dev/input/event2: EV_KEY       KEY_BACK             DOWN
";

    fn tracker() -> Tracker {
        Tracker::new(Ranges { max_x: 1000.0, max_y: 2000.0 })
    }

    #[test]
    fn ranges_come_from_getevent_pl() {
        let out = "  ABS (0003):\n    ABS_MT_POSITION_X     : value 0, min 0, max 1079, fuzz 0, flat 0, resolution 0\n    ABS_MT_POSITION_Y     : value 0, min 0, max 2411, fuzz 0, flat 0, resolution 0\n";
        assert_eq!(parse_ranges(out), Some(Ranges { max_x: 1079.0, max_y: 2411.0 }));
        assert_eq!(parse_ranges("нет"), None);
    }

    #[test]
    fn getevent_lines_become_down_gesture_and_key() {
        let mut t = tracker();
        let all: Vec<Input> = SAMPLE.lines().flat_map(|l| t.feed(l)).collect();
        assert!(
            matches!(all[0], Input::Down { x, y, .. } if (x - 0.512).abs() < 1e-9 && (y - 0.512).abs() < 1e-9),
            "{all:?}"
        );
        let Input::Gesture(g) = &all[1] else { panic!("{all:?}") };
        assert!((g.t1 - g.t0 - 0.1).abs() < 1e-9);
        let (kind, v) = gesture_step(g);
        assert_eq!(kind, "tap");
        assert_eq!(step_lua(kind, &v).unwrap(), "p:tap(0.512, 0.512)");
        assert_eq!(all[2], Input::Key { t: 102.0, name: "back" });
    }

    #[test]
    fn gestures_classify_as_tap_long_swipe() {
        let g = |t1: f64, x1: f64| Gesture { t0: 0.0, t1, x0: 0.5, y0: 0.5, x1, y1: 0.5 };
        assert_eq!(gesture_step(&g(0.1, 0.5)).0, "tap");
        assert_eq!(gesture_step(&g(0.9, 0.5)).0, "long");
        assert_eq!(gesture_step(&g(0.3, 0.9)).0, "swipe");
    }

    #[test]
    fn lua_is_built_quoted_and_problems_are_reported() {
        assert_eq!(
            step_lua("tap", &json!({"x": 0.1, "y": 0.2, "target": "Войти \"в\" кабинет"})).unwrap(),
            "p:tap_on(\"Войти \\\"в\\\" кабинет\")"
        );
        assert_eq!(step_lua("button", &json!({"name": "back"})).unwrap(), "p:button(\"back\")");
        assert_eq!(
            step_lua("check", &json!({"target": "id:ok"})).unwrap(),
            "assert.truthy(p:find_one(\"id:ok\"))"
        );
        assert!(step_lua("swipe", &json!({"x1": 0.1})).is_err());
        assert!(step_lua("mouse", &json!({})).unwrap_err().contains("не перевести"));
        assert_eq!(element_target(&json!({"text": "", "desc": "Ring", "id": ""})), Some("desc:Ring".into()));
        assert_eq!(
            element_target(&json!({"text": "", "desc": "", "id": "com.a:id/go"})),
            Some("id:go".into())
        );
    }

    #[test]
    fn steps_are_edited_removed_added_and_the_script_is_assembled() {
        let mut r = Recording::new();
        let a = r.add(1.0, "tap", json!({"x": 0.5, "y": 0.5}), "gesture", Some("com.a".into()), None);
        let b = r.add(3.0, "button", json!({"name": "back"}), "gesture", None, None);
        let c = r.add(4.0, "swipe", json!({"x1": 0.5}), "gesture", None, None);
        assert!(r.steps[2].problem.is_some());
        r.set_steps(&[
            json!({"id": a, "t": "tap", "v": {"x": 0.5, "y": 0.5, "target": "Войти"}, "src": "gesture", "pkg": "com.a"}),
            json!({"id": 0, "t": "check", "v": {"target": "Кабинет"}, "src": "check"}),
            json!({"id": b, "t": "button", "v": {"name": "back"}, "src": "gesture"}),
        ])
        .unwrap();
        assert_eq!(r.steps.len(), 3, "шаг {c} удалён");
        assert_eq!(r.steps[0].lua, "p:tap_on(\"Войти\")");
        assert_eq!(r.steps[1].id, 4);
        assert!(r.set_steps(&[json!({"id": 99, "t": "tap", "v": {}})]).is_err());
        let (lua, problem) = r.script("Вход", "из теста");
        assert!(problem.is_none());
        assert!(lua.starts_with("-- из теста\ntest(\"Вход\", function()"), "{lua}");
        assert!(
            lua.contains("  p:tap_on(\"Войти\")\n")
                && lua.contains("  assert.truthy(p:find_one(\"Кабинет\"))\n"),
            "{lua}"
        );
        assert!(lua.ends_with("end)\n"));
        let snap = r.snapshot(1);
        assert_eq!(snap["status"], "recording");
        assert_eq!(snap["steps"][0]["lua"], "p:tap_on(\"Войти\")");
        assert_eq!(snap["app"], "com.a");
    }

    #[test]
    fn screens_are_remembered_for_checks() {
        let mut r = Recording::new();
        let n = r.remember_screen(vec![json!({"text": "A"}), json!({"text": "B"})]);
        assert_eq!(r.screen_element(n, 1).unwrap()["text"], "B");
        assert!(r.screen_element(n, 5).is_err());
        assert!(r.screen_element(99, 0).is_err());
    }
}
