//! Команды над телефоном: сведения, состояние, приложения, журнал, питание, сеть, кнопки.
//!
//! Клиент называет действие («установи APK», «закрой приложение»), а как оно сделано, решает реализация
//! `DeviceBackend`. Реализация по умолчанию (`AdbDevice`) выполняет shell-команды через сервер ADB; телефон,
//! у которого ADB недоступен, получает понятную ошибку.

use pcdock_adb::{check_remote_path, Adb};
use serde_json::{json, Value};

use crate::ui::{parse_dump, Selector};

/// Что можно сделать с телефоном. Результат это JSON (форма у каждой операции описана в `protocol.md`).
#[derive(Clone, Debug, PartialEq)]
pub enum DeviceOp {
    Info,
    Stats,
    AppInfo { packages: Vec<String> },
    ForceStop { package: String },
    ClearAppData { package: String },
    Uninstall { package: String, keep_data: bool },
    Install { path: String, allow_downgrade: bool, sha256: Option<String> },
    Logcat { lines: u32, package: Option<String>, level: Option<char> },
    SetMobileData { on: bool },
    StayAwake { on: bool },
    ScreenState,
    Button { name: String },
    LaunchApp { package: String, activity: Option<String> },
    /// `all`: все пакеты; иначе только те, у которых есть значок запуска.
    ListApps { all: bool },
    /// Какое приложение сейчас на переднем плане.
    ForegroundApp,
    UiDump,
    UiFind { selector: Selector },
    UiElements,
    UiAt { x: f64, y: f64 },
    UiFocused,
    /// Ждать появления (`gone`: исчезновения) элемента, не дольше `timeout` секунд.
    WaitFor { selector: Selector, timeout: f64, gone: bool },
    TypeText { text: String },
}

impl DeviceOp {
    /// Имя операции в ответе.
    pub fn name(&self) -> &'static str {
        match self {
            Self::Info => "DeviceInfo",
            Self::Stats => "DeviceStats",
            Self::AppInfo { .. } => "AppInfo",
            Self::ForceStop { .. } => "ForceStop",
            Self::ClearAppData { .. } => "ClearAppData",
            Self::Uninstall { .. } => "UninstallApp",
            Self::Install { .. } => "InstallApk",
            Self::Logcat { .. } => "Logcat",
            Self::SetMobileData { .. } => "SetMobileData",
            Self::StayAwake { .. } => "StayAwake",
            Self::ScreenState => "ScreenState",
            Self::Button { .. } => "Button",
            Self::LaunchApp { .. } => "LaunchApp",
            Self::ListApps { .. } => "ListApps",
            Self::ForegroundApp => "ForegroundApp",
            Self::UiDump => "UiDump",
            Self::UiFind { .. } => "UiFind",
            Self::UiElements => "UiElements",
            Self::UiAt { .. } => "UiAt",
            Self::UiFocused => "UiFocused",
            Self::WaitFor { .. } => "WaitFor",
            Self::TypeText { .. } => "TypeText",
        }
    }
}

/// Исполнитель операций над телефоном.
pub trait DeviceBackend: Send + Sync {
    /// `serial` — серийник телефона; `roots` — папки, из которых можно ставить APK.
    fn run(&self, serial: &str, op: &DeviceOp, roots: &[String]) -> Result<Value, String>;
}

/// Исполнитель по ADB.
pub struct AdbDevice {
    adb: Adb,
}

impl AdbDevice {
    /// `adb_server`: адрес сервера ADB (`None`: локальный).
    pub fn new(adb_server: Option<String>) -> Self {
        Self { adb: adb_server.map(Adb::new).unwrap_or_else(Adb::local) }
    }
}

/// Имя пакета Android: буквы, цифры, `_` и `.`; без него в shell ничего не подставляем.
pub fn check_package(name: &str) -> Result<(), String> {
    let ok = !name.is_empty()
        && name.len() <= 255
        && name.contains('.')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.')
        && !name.starts_with('.');
    if ok {
        Ok(())
    } else {
        Err(format!("плохое имя пакета: {name:?}"))
    }
}

/// Пакет попадает в блок-лист: точное имя или маска `*` в начале/конце (`com.bad.*`, `*.cloner`).
pub fn is_blocked(list: &[String], package: &str) -> bool {
    list.iter().any(|p| match (p.strip_prefix('*'), p.strip_suffix('*')) {
        (Some(suffix), _) if !suffix.contains('*') => package.ends_with(suffix),
        (_, Some(prefix)) if !prefix.contains('*') => package.starts_with(prefix),
        _ => p == package,
    })
}

/// Кнопка по имени → код клавиши Android.
pub fn button_keycode(name: &str) -> Option<&'static str> {
    Some(match name {
        "back" => "KEYCODE_BACK",
        "home" => "KEYCODE_HOME",
        "recents" => "KEYCODE_APP_SWITCH",
        "menu" => "KEYCODE_MENU",
        "power" => "KEYCODE_POWER",
        "wakeup" => "KEYCODE_WAKEUP",
        "sleep" => "KEYCODE_SLEEP",
        "volume_up" => "KEYCODE_VOLUME_UP",
        "volume_down" => "KEYCODE_VOLUME_DOWN",
        "volume_mute" => "KEYCODE_VOLUME_MUTE",
        "enter" => "KEYCODE_ENTER",
        "delete" => "KEYCODE_DEL",
        "tab" => "KEYCODE_TAB",
        _ => return None,
    })
}

// ---------- разбор вывода команд (без телефона, проверяется тестами) ----------

/// Вывод `getprop`: строки `[ключ]: [значение]`.
pub fn parse_getprop(out: &str) -> std::collections::HashMap<String, String> {
    let mut map = std::collections::HashMap::new();
    for line in out.lines() {
        let Some((k, v)) = line.split_once("]: [") else { continue };
        let Some(k) = k.strip_prefix('[') else { continue };
        let Some(v) = v.strip_suffix(']') else { continue };
        map.insert(k.to_string(), v.to_string());
    }
    map
}

/// `wm size` / `wm density`: `Physical size: 720x1600`, `Override size: …`; побеждает `Override`.
pub fn parse_wm(out: &str, key: &str) -> Option<String> {
    let mut physical = None;
    let mut over = None;
    for line in out.lines() {
        if let Some(v) = line.trim().strip_prefix(&format!("Physical {key}:")) {
            physical = Some(v.trim().to_string());
        } else if let Some(v) = line.trim().strip_prefix(&format!("Override {key}:")) {
            over = Some(v.trim().to_string());
        }
    }
    over.or(physical)
}

/// Сведения о телефоне из `getprop`, `wm size`, `wm density`.
pub fn device_info(props: &str, size: &str, density: &str) -> Value {
    let p = parse_getprop(props);
    let get = |k: &str| p.get(k).cloned().unwrap_or_default();
    let (mut w, mut h) = (Value::Null, Value::Null);
    if let Some(s) = parse_wm(size, "size") {
        if let Some((a, b)) = s.split_once('x') {
            if let (Ok(a), Ok(b)) = (a.trim().parse::<u32>(), b.trim().parse::<u32>()) {
                (w, h) = (json!(a), json!(b));
            }
        }
    }
    let dpi = parse_wm(density, "density").and_then(|s| s.trim().parse::<u32>().ok());
    let abis: Vec<&str> =
        get_str(&p, "ro.product.cpu.abilist").split(',').filter(|s| !s.is_empty()).collect();
    json!({
        "manufacturer": get("ro.product.manufacturer"),
        "brand": get("ro.product.brand"),
        "model": get("ro.product.model"),
        "device": get("ro.product.device"),
        "android": get("ro.build.version.release"),
        "sdk": get("ro.build.version.sdk").parse::<u32>().ok(),
        "abis": abis,
        "serial": get("ro.serialno"),
        "build": get("ro.build.display.id"),
        "screen_w": w,
        "screen_h": h,
        "dpi": dpi,
    })
}

fn get_str<'a>(p: &'a std::collections::HashMap<String, String>, k: &str) -> &'a str {
    p.get(k).map(String::as_str).unwrap_or("")
}

/// `dumpsys battery`.
pub fn parse_battery(out: &str) -> Value {
    let field = |name: &str| -> Option<String> {
        out.lines().find_map(|l| l.trim().strip_prefix(&format!("{name}:")).map(|v| v.trim().to_string()))
    };
    let num = |name: &str| field(name).and_then(|v| v.parse::<i64>().ok());
    let flag = |name: &str| field(name).map(|v| v == "true");
    let powered = flag("AC powered").unwrap_or(false)
        || flag("USB powered").unwrap_or(false)
        || flag("Wireless powered").unwrap_or(false);
    json!({
        "level": num("level"),
        // `temperature` в десятых долях градуса
        "temperature_c": num("temperature").map(|t| t as f64 / 10.0),
        "charging": powered,
        "status": num("status"),
    })
}

/// `/proc/meminfo` → килобайты.
pub fn parse_meminfo(out: &str) -> (Option<u64>, Option<u64>) {
    let kb = |name: &str| {
        out.lines().find_map(|l| {
            l.strip_prefix(name)
                .and_then(|r| r.trim_start_matches(':').split_whitespace().next())
                .and_then(|n| n.parse::<u64>().ok())
        })
    };
    (kb("MemTotal"), kb("MemAvailable"))
}

/// Загрузка процессора в процентах по двум снимкам `/proc/stat` (первая строка `cpu …`).
pub fn cpu_percent(a: &str, b: &str) -> Option<f64> {
    fn times(s: &str) -> Option<(u64, u64)> {
        let line = s.lines().find(|l| l.starts_with("cpu "))?;
        let n: Vec<u64> = line.split_whitespace().skip(1).filter_map(|x| x.parse().ok()).collect();
        if n.len() < 4 {
            return None;
        }
        let idle = n[3] + n.get(4).copied().unwrap_or(0);
        Some((n.iter().sum(), idle))
    }
    let (t1, i1) = times(a)?;
    let (t2, i2) = times(b)?;
    let total = t2.checked_sub(t1)?;
    if total == 0 {
        return None;
    }
    let idle = i2.saturating_sub(i1);
    Some(((total - idle.min(total)) as f64 / total as f64 * 1000.0).round() / 10.0)
}

/// `dumpsys package <имя>` → установлено ли, версия, включено ли.
pub fn parse_package(name: &str, out: &str) -> Value {
    if !out.contains(&format!("Package [{name}]")) {
        return json!({"package": name, "installed": false});
    }
    let field = |key: &str| -> Option<String> {
        out.lines()
            .find_map(|l| l.split_whitespace().find_map(|w| w.strip_prefix(key).map(|v| v.to_string())))
    };
    let version_code = field("versionCode=").and_then(|v| v.parse::<u64>().ok());
    // `enabled=` у пользователя: 0 по умолчанию, 1 включено; 2, 3, 4 выключено.
    let enabled = !matches!(
        out.lines().find_map(|l| l.trim().strip_prefix("enabled=")).and_then(|v| v.split_whitespace().next()),
        Some("2" | "3" | "4")
    );
    json!({
        "package": name,
        "installed": true,
        "version_code": version_code,
        "version_name": field("versionName="),
        "enabled": enabled,
        "first_install": line_value(out, "firstInstallTime="),
        "last_update": line_value(out, "lastUpdateTime="),
    })
}

fn line_value(out: &str, key: &str) -> Option<String> {
    out.lines().find_map(|l| l.trim().strip_prefix(key).map(|v| v.trim().to_string()))
}

/// Итог `pm install` / `pm uninstall`: `Success` или `Failure [ПРИЧИНА]`.
pub fn pm_result(out: &str) -> Result<(), String> {
    let text = out.trim();
    if text.lines().any(|l| l.trim() == "Success") {
        Ok(())
    } else {
        Err(text.lines().last().unwrap_or("нет ответа от pm").trim().to_string())
    }
}

/// Состояние экрана по `dumpsys power` и `dumpsys window`.
pub fn parse_screen_state(power: &str, window: &str) -> Value {
    let wake = power.lines().find_map(|l| l.trim().strip_prefix("mWakefulness=")).map(str::trim);
    let locked = window.lines().any(|l| {
        let l = l.trim();
        [
            "isKeyguardShowing=true",
            "mKeyguardShowing=true",
            "mShowingLockscreen=true",
            "mDreamingLockscreen=true",
        ]
        .iter()
        .any(|k| l.contains(k))
    });
    json!({
        "wakefulness": wake,
        "on": wake.map(|w| w == "Awake"),
        "locked": locked,
    })
}

/// Текст для `input text`: пробел это `%s`, остальное экранируется для shell. Только ASCII.
pub fn input_text_arg(text: &str) -> Result<String, String> {
    if !text.is_ascii() || text.chars().any(|c| c.is_control()) {
        return Err("TypeText по ADB: только печатный ASCII (для остального нужен компаньон)".into());
    }
    let mut out = String::from("'");
    for c in text.chars() {
        match c {
            ' ' => out.push_str("%s"),
            '\'' => out.push_str("'\\''"),
            c => out.push(c),
        }
    }
    out.push('\'');
    Ok(out)
}

/// Пакеты из `pm list packages`: строки `package:имя`.
pub fn parse_package_list(out: &str) -> Vec<String> {
    let mut v: Vec<String> =
        out.lines().filter_map(|l| l.trim().strip_prefix("package:")).map(|p| p.trim().to_string()).collect();
    v.sort();
    v.dedup();
    v
}

/// Пакеты со значком запуска из `cmd package query-activities`: строки `packageName=имя`.
pub fn parse_launchable(out: &str) -> Vec<String> {
    let mut v: Vec<String> = out
        .lines()
        .filter_map(|l| l.trim().strip_prefix("packageName="))
        .map(|p| p.trim().to_string())
        .collect();
    v.sort();
    v.dedup();
    v
}

/// Окно в фокусе из `dumpsys window`: `mCurrentFocus=Window{… u0 пакет/Активность}`.
pub fn parse_foreground(out: &str) -> Value {
    let line = out
        .lines()
        .find(|l| l.contains("mCurrentFocus=") && !l.contains("mCurrentFocus=null"))
        .or_else(|| out.lines().find(|l| l.contains("mFocusedApp=") && !l.contains("mFocusedApp=null")));
    let comp = line.and_then(|l| {
        let inner = l.trim_end().trim_end_matches('}');
        inner.rsplit(' ').next().map(str::to_string)
    });
    match comp.as_deref().and_then(|c| c.split_once('/')) {
        Some((pkg, act)) => json!({"package": pkg, "activity": act}),
        None => json!({"package": comp, "activity": null}),
    }
}

// ---------- исполнение по ADB ----------

impl AdbDevice {
    fn sh(&self, serial: &str, cmd: &str) -> Result<String, String> {
        self.adb.exec_text(serial, cmd)
    }

    fn info(&self, serial: &str) -> Result<Value, String> {
        let props = self.sh(serial, "getprop")?;
        let size = self.sh(serial, "wm size").unwrap_or_default();
        let density = self.sh(serial, "wm density").unwrap_or_default();
        Ok(device_info(&props, &size, &density))
    }

    fn stats(&self, serial: &str) -> Result<Value, String> {
        let battery = parse_battery(&self.sh(serial, "dumpsys battery")?);
        let cpu = self.sh(serial, "cat /proc/stat; sleep 0.3; echo ====; cat /proc/stat")?;
        let (a, b) = cpu.split_once("====").unwrap_or((&cpu, ""));
        let (total, available) = parse_meminfo(&self.sh(serial, "cat /proc/meminfo")?);
        Ok(json!({
            "battery": battery,
            "cpu_percent": cpu_percent(a, b),
            "mem_total_kb": total,
            "mem_available_kb": available,
        }))
    }
}

impl AdbDevice {
    fn tree(&self, serial: &str) -> Result<crate::ui::UiTree, String> {
        parse_dump(&self.sh(serial, "uiautomator dump /dev/tty")?)
    }

    fn wait_for(&self, serial: &str, sel: &Selector, timeout: f64, gone: bool) -> Result<Value, String> {
        let end = std::time::Instant::now() + std::time::Duration::from_secs_f64(timeout.clamp(0.0, 120.0));
        loop {
            // Пустой экран (блокировка, переход) это не ошибка, а «пока ничего нет».
            let tree = self.tree(serial).ok();
            let found: Vec<Value> =
                tree.as_ref().map(|t| t.find(sel).iter().map(|n| n.to_json(t)).collect()).unwrap_or_default();
            if found.is_empty() == gone {
                return Ok(json!({ "ok": true, "elements": found }));
            }
            if std::time::Instant::now() >= end {
                return Ok(json!({ "ok": false, "elements": found }));
            }
            std::thread::sleep(std::time::Duration::from_millis(300));
        }
    }
}

impl DeviceBackend for AdbDevice {
    fn run(&self, serial: &str, op: &DeviceOp, roots: &[String]) -> Result<Value, String> {
        match op {
            DeviceOp::Info => self.info(serial),
            DeviceOp::Stats => self.stats(serial),
            DeviceOp::AppInfo { packages } => {
                let mut apps = Vec::new();
                for p in packages {
                    check_package(p)?;
                    apps.push(parse_package(p, &self.sh(serial, &format!("dumpsys package {p}"))?));
                }
                Ok(json!({ "apps": apps }))
            }
            DeviceOp::ForceStop { package } => {
                check_package(package)?;
                self.sh(serial, &format!("am force-stop {package}"))?;
                Ok(json!({}))
            }
            DeviceOp::ClearAppData { package } => {
                check_package(package)?;
                pm_result(&self.sh(serial, &format!("pm clear {package}"))?)?;
                Ok(json!({}))
            }
            DeviceOp::Uninstall { package, keep_data } => {
                check_package(package)?;
                let k = if *keep_data { " -k" } else { "" };
                pm_result(&self.sh(serial, &format!("pm uninstall{k} {package}"))?)?;
                Ok(json!({}))
            }
            DeviceOp::Install { path, allow_downgrade, sha256 } => {
                check_remote_path(path)?;
                if !roots.iter().any(|r| path.starts_with(&format!("{}/", r.trim_end_matches('/')))) {
                    return Err(format!("путь {path} вне разрешённых папок ({})", roots.join(", ")));
                }
                if let Some(want) = sha256 {
                    if want.len() != 64 || !want.chars().all(|c| c.is_ascii_hexdigit()) {
                        return Err("sha256: нужно 64 шестнадцатеричных символа".into());
                    }
                    let got = self.sh(serial, &format!("sha256sum '{path}'"))?;
                    let got = got.split_whitespace().next().unwrap_or("").to_ascii_lowercase();
                    if got != want.to_ascii_lowercase() {
                        return Err(format!("контрольная сумма APK не совпала: на телефоне {got}"));
                    }
                }
                let d = if *allow_downgrade { " -d" } else { "" };
                pm_result(&self.sh(serial, &format!("pm install -r{d} '{path}'"))?)?;
                Ok(json!({}))
            }
            DeviceOp::Logcat { lines, package, level } => {
                let lines = (*lines).clamp(1, 5000);
                let mut cmd = format!("logcat -d -v time -t {lines}");
                if let Some(pkg) = package {
                    check_package(pkg)?;
                    let pid = self.sh(serial, &format!("pidof {pkg}"))?;
                    let pid = pid.split_whitespace().next().unwrap_or("");
                    if pid.is_empty() {
                        return Ok(json!({ "text": "" }));
                    }
                    cmd.push_str(&format!(" --pid={pid}"));
                }
                if let Some(l) = level {
                    if !"VDIWEF".contains(*l) {
                        return Err("level: одна из V D I W E F".into());
                    }
                    cmd.push_str(&format!(" '*:{l}'"));
                }
                Ok(json!({ "text": self.sh(serial, &cmd)? }))
            }
            DeviceOp::SetMobileData { on } => {
                self.sh(serial, if *on { "svc data enable" } else { "svc data disable" })?;
                Ok(json!({}))
            }
            DeviceOp::StayAwake { on } => {
                let previous =
                    self.sh(serial, "settings get global stay_on_while_plugged_in")?.trim().to_string();
                self.sh(serial, if *on { "svc power stayon true" } else { "svc power stayon false" })?;
                if *on {
                    self.sh(serial, "input keyevent KEYCODE_WAKEUP")?;
                }
                Ok(json!({ "previous": previous }))
            }
            DeviceOp::ScreenState => {
                let power = self.sh(serial, "dumpsys power")?;
                let window = self.sh(serial, "dumpsys window").unwrap_or_default();
                Ok(parse_screen_state(&power, &window))
            }
            DeviceOp::LaunchApp { package, activity } => {
                check_package(package)?;
                if let Some(a) = activity {
                    if !a.chars().all(|c| c.is_ascii_alphanumeric() || "._$".contains(c)) {
                        return Err(format!("плохое имя активности: {a:?}"));
                    }
                    let out = self.sh(serial, &format!("am start -n {package}/{a}"))?;
                    if out.contains("Error") {
                        return Err(out.trim().to_string());
                    }
                } else {
                    let out = self.sh(serial, &format!("monkey -p {package} -c android.intent.category.LAUNCHER 1"))?;
                    if out.contains("No activities found") || out.contains("monkey aborted") {
                        return Err(format!("{package}: нет значка запуска или приложение не установлено"));
                    }
                }
                Ok(json!({}))
            }
            DeviceOp::ListApps { all } => {
                let out = if *all {
                    parse_package_list(&self.sh(serial, "pm list packages")?)
                } else {
                    parse_launchable(&self.sh(
                        serial,
                        "cmd package query-activities -a android.intent.action.MAIN -c android.intent.category.LAUNCHER",
                    )?)
                };
                Ok(json!({ "packages": out }))
            }
            DeviceOp::ForegroundApp => {
                Ok(parse_foreground(&self.sh(serial, "dumpsys window | grep -E 'mCurrentFocus|mFocusedApp'")?))
            }
            DeviceOp::UiDump => {
                let tree = self.tree(serial)?;
                Ok(json!({ "width": tree.width, "height": tree.height,
                           "elements": tree.nodes.iter().map(|n| n.to_json(&tree)).collect::<Vec<_>>() }))
            }
            DeviceOp::UiFind { selector } => {
                let tree = self.tree(serial)?;
                Ok(json!({ "elements": tree.find(selector).iter().map(|n| n.to_json(&tree)).collect::<Vec<_>>() }))
            }
            DeviceOp::UiElements => {
                let tree = self.tree(serial)?;
                Ok(json!({ "width": tree.width, "height": tree.height,
                           "elements": tree.elements().iter().map(|n| n.to_json(&tree)).collect::<Vec<_>>() }))
            }
            DeviceOp::UiAt { x, y } => {
                let tree = self.tree(serial)?;
                Ok(json!({ "element": tree.at(*x, *y).map(|n| n.to_json(&tree)) }))
            }
            DeviceOp::UiFocused => {
                let tree = self.tree(serial)?;
                let sel = Selector { focused: Some(true), class: Some("EditText".into()), ..Default::default() };
                Ok(json!({ "element": tree.find(&sel).first().map(|n| n.to_json(&tree)) }))
            }
            DeviceOp::WaitFor { selector, timeout, gone } => self.wait_for(serial, selector, *timeout, *gone),
            DeviceOp::TypeText { text } => {
                self.sh(serial, &format!("input text {}", input_text_arg(text)?))?;
                Ok(json!({}))
            }
            DeviceOp::Button { name } => {
                let code = button_keycode(name).ok_or_else(|| format!("неизвестная кнопка: {name}"))?;
                self.sh(serial, &format!("input keyevent {code}"))?;
                Ok(json!({}))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn package_names_are_checked() {
        assert!(check_package("com.android.chrome").is_ok());
        assert!(check_package("a.b_c1").is_ok());
        for bad in ["", "nodots", "com.x; rm -rf /", "com.x y", ".com.x", "com.x$(id)"] {
            assert!(check_package(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn getprop_and_wm_are_parsed() {
        let props = "[ro.product.model]: [Redmi A5]\n[ro.build.version.release]: [15]\n[ro.build.version.sdk]: [35]\n\
                     [ro.product.cpu.abilist]: [arm64-v8a,armeabi-v7a]\n[ro.serialno]: [ABC]\nмусор\n";
        let v = device_info(
            props,
            "Physical size: 720x1600\nOverride size: 540x1200\n",
            "Physical density: 320\n",
        );
        assert_eq!(v["model"], "Redmi A5");
        assert_eq!(v["sdk"], 35);
        assert_eq!(v["abis"], json!(["arm64-v8a", "armeabi-v7a"]));
        assert_eq!((v["screen_w"].as_u64(), v["screen_h"].as_u64()), (Some(540), Some(1200)));
        assert_eq!(v["dpi"], 320);
        let empty = device_info("", "", "");
        assert!(empty["screen_w"].is_null() && empty["sdk"].is_null());
    }

    #[test]
    fn battery_memory_and_cpu_are_parsed() {
        let b = parse_battery("Current Battery Service state:\n  AC powered: false\n  USB powered: true\n  status: 2\n  level: 87\n  temperature: 291\n");
        assert_eq!(b["level"], 87);
        assert_eq!(b["temperature_c"], 29.1);
        assert_eq!(b["charging"], true);
        assert_eq!(
            parse_meminfo("MemTotal:        3800000 kB\nMemFree: 1 kB\nMemAvailable:    1900000 kB\n"),
            (Some(3_800_000), Some(1_900_000))
        );
        let a = "cpu  100 0 100 800 0 0 0 0 0 0\ncpu0 1 1 1 1\n";
        let b2 = "cpu  150 0 150 900 0 0 0 0 0 0\n";
        assert_eq!(cpu_percent(a, b2), Some(50.0));
        assert_eq!(cpu_percent(a, a), None);
    }

    #[test]
    fn package_dump_is_parsed() {
        let dump = "Packages:\n  Package [com.example.app] (abc):\n    versionCode=42 minSdk=24 targetSdk=35\n    versionName=1.2.3\n    firstInstallTime=2026-11-01 10:00:00\n    lastUpdateTime=2026-11-10 11:00:00\n";
        let v = parse_package("com.example.app", dump);
        assert_eq!((v["installed"].clone(), v["version_code"].clone()), (json!(true), json!(42)));
        assert_eq!(v["version_name"], "1.2.3");
        assert_eq!(v["last_update"], "2026-11-10 11:00:00");
        assert_eq!(parse_package("com.none.app", "Unable to find package: com.none.app")["installed"], false);
    }

    #[test]
    fn pm_results_and_screen_state() {
        assert!(pm_result("Performing Streamed Install\nSuccess\n").is_ok());
        assert_eq!(
            pm_result("Failure [INSTALL_FAILED_VERSION_DOWNGRADE]\n").unwrap_err(),
            "Failure [INSTALL_FAILED_VERSION_DOWNGRADE]"
        );
        let s = parse_screen_state("  mWakefulness=Awake\n", "  mShowingLockscreen=true\n");
        assert_eq!((s["on"].clone(), s["locked"].clone()), (json!(true), json!(true)));
        let s = parse_screen_state("  mWakefulness=Asleep\n", "");
        assert_eq!((s["on"].clone(), s["locked"].clone()), (json!(false), json!(false)));
    }

    #[test]
    fn text_for_input_is_quoted_and_lists_are_parsed() {
        assert_eq!(input_text_arg("a b").unwrap(), "'a%sb'");
        assert_eq!(input_text_arg("it's").unwrap(), "'it'\\''s'");
        assert!(input_text_arg("привет").is_err());
        assert_eq!(parse_package_list("package:b.b\npackage:a.a\n"), vec!["a.a", "b.b"]);
        assert_eq!(parse_launchable("  packageName=x.y\n  name=z\n  packageName=x.y\n"), vec!["x.y"]);
        let f = parse_foreground("  mCurrentFocus=Window{a1b2 u0 com.app/com.app.Main}\n");
        assert_eq!((f["package"].clone(), f["activity"].clone()), (json!("com.app"), json!("com.app.Main")));
        assert!(parse_foreground("  mCurrentFocus=null\n")["package"].is_null());
    }

    #[test]
    fn blocklist_matches_names_and_masks() {
        let l = vec!["com.bad.app".to_string(), "org.evil.*".to_string(), "*.cloner".to_string()];
        assert!(is_blocked(&l, "com.bad.app") && is_blocked(&l, "org.evil.x") && is_blocked(&l, "a.b.cloner"));
        assert!(!is_blocked(&l, "com.bad.app2") && !is_blocked(&l, "org.good") && !is_blocked(&[], "x.y"));
    }

    #[test]
    fn buttons_have_codes() {
        assert_eq!(button_keycode("back"), Some("KEYCODE_BACK"));
        assert_eq!(button_keycode("rm -rf"), None);
    }
}
