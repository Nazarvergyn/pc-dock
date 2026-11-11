//! Хранилище сценариев: файлы `<id>.lua` в папке `scripts/` и `index.json` рядом с ними, где каждому id
//! соответствует читаемое имя. Имена файлов выдаёт хранилище; путь от клиента никогда не используется.

use std::fs;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

/// Предельный размер сценария.
pub const MAX_SOURCE: usize = 1024 * 1024;

/// Запись индекса сценария.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScriptMeta {
    pub id: String,
    pub name: String,
    pub created_ms: u64,
    pub updated_ms: u64,
    pub size: usize,
}

impl ScriptMeta {
    pub fn to_json(&self) -> Value {
        json!({"id": self.id, "name": self.name, "created": self.created_ms, "updated": self.updated_ms, "size": self.size})
    }
}

pub struct ScriptStore {
    dir: PathBuf,
    lock: Mutex<()>,
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// Допустимый id: латиница, цифры, `_` и `-`, до 64 символов.
pub fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

impl ScriptStore {
    /// Открывает хранилище в папке `data/scripts` (создаёт при необходимости).
    pub fn open(data: &std::path::Path) -> Result<Self, String> {
        let dir = data.join("scripts");
        fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        Ok(Self { dir, lock: Mutex::new(()) })
    }

    fn index_path(&self) -> PathBuf {
        self.dir.join("index.json")
    }

    fn file(&self, id: &str) -> PathBuf {
        self.dir.join(format!("{id}.lua"))
    }

    fn read_index(&self) -> Result<Vec<ScriptMeta>, String> {
        let text = match fs::read_to_string(self.index_path()) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(format!("index.json: {e}")),
        };
        let v: Value = serde_json::from_str(&text).map_err(|e| format!("index.json повреждён: {e}"))?;
        let arr = v["scripts"].as_array().ok_or("index.json: нет списка scripts")?;
        Ok(arr
            .iter()
            .filter_map(|s| {
                Some(ScriptMeta {
                    id: s["id"].as_str()?.to_string(),
                    name: s["name"].as_str()?.to_string(),
                    created_ms: s["created"].as_u64().unwrap_or(0),
                    updated_ms: s["updated"].as_u64().unwrap_or(0),
                    size: s["size"].as_u64().unwrap_or(0) as usize,
                })
            })
            .collect())
    }

    /// Пишет индекс атомарно: во временный файл, затем переименование.
    fn write_index(&self, list: &[ScriptMeta]) -> Result<(), String> {
        let v = json!({"scripts": list.iter().map(ScriptMeta::to_json).collect::<Vec<_>>()});
        let tmp = self.dir.join("index.json.tmp");
        fs::write(&tmp, serde_json::to_string_pretty(&v).unwrap())
            .map_err(|e| format!("запись индекса: {e}"))?;
        fs::rename(&tmp, self.index_path()).map_err(|e| format!("запись индекса: {e}"))
    }

    pub fn list(&self) -> Result<Vec<ScriptMeta>, String> {
        let _g = self.lock.lock().unwrap();
        self.read_index()
    }

    pub fn get(&self, id: &str) -> Result<(ScriptMeta, String), String> {
        let _g = self.lock.lock().unwrap();
        if !valid_id(id) {
            return Err(format!("плохой id сценария: {id:?}"));
        }
        let meta = self
            .read_index()?
            .into_iter()
            .find(|m| m.id == id)
            .ok_or_else(|| format!("нет сценария {id}"))?;
        let source = fs::read_to_string(self.file(id)).map_err(|e| format!("сценарий {id}: {e}"))?;
        Ok((meta, source))
    }

    /// Сохраняет сценарий: новый (без `id`; id выдаёт хранилище) или обновляет существующий.
    pub fn put(&self, id: Option<&str>, name: &str, source: &str) -> Result<ScriptMeta, String> {
        if source.len() > MAX_SOURCE {
            return Err(format!("сценарий больше {} байт", MAX_SOURCE));
        }
        let name = name.trim();
        if name.is_empty() || name.len() > 200 {
            return Err("нужно имя сценария (до 200 символов)".into());
        }
        let _g = self.lock.lock().unwrap();
        let mut list = self.read_index()?;
        let now = now_ms();
        let meta = match id {
            Some(id) => {
                if !valid_id(id) {
                    return Err(format!("плохой id сценария: {id:?}"));
                }
                let m = list.iter_mut().find(|m| m.id == id).ok_or_else(|| format!("нет сценария {id}"))?;
                m.name = name.to_string();
                m.updated_ms = now;
                m.size = source.len();
                m.clone()
            }
            None => {
                let mut n = now;
                let new_id = loop {
                    let candidate = format!("s{n:x}");
                    if !list.iter().any(|m| m.id == candidate) && !self.file(&candidate).exists() {
                        break candidate;
                    }
                    n += 1;
                };
                let m = ScriptMeta {
                    id: new_id,
                    name: name.to_string(),
                    created_ms: now,
                    updated_ms: now,
                    size: source.len(),
                };
                list.push(m.clone());
                m
            }
        };
        // Сначала файл, потом индекс: оборванная запись не оставляет в индексе несуществующий сценарий.
        fs::write(self.file(&meta.id), source).map_err(|e| format!("запись сценария: {e}"))?;
        self.write_index(&list)?;
        Ok(meta)
    }

    pub fn rename(&self, id: &str, name: &str) -> Result<(), String> {
        let name = name.trim();
        if name.is_empty() || name.len() > 200 {
            return Err("нужно имя сценария (до 200 символов)".into());
        }
        let _g = self.lock.lock().unwrap();
        if !valid_id(id) {
            return Err(format!("плохой id сценария: {id:?}"));
        }
        let mut list = self.read_index()?;
        let m = list.iter_mut().find(|m| m.id == id).ok_or_else(|| format!("нет сценария {id}"))?;
        m.name = name.to_string();
        m.updated_ms = now_ms();
        self.write_index(&list)
    }

    pub fn delete(&self, id: &str) -> Result<(), String> {
        let _g = self.lock.lock().unwrap();
        if !valid_id(id) {
            return Err(format!("плохой id сценария: {id:?}"));
        }
        let mut list = self.read_index()?;
        let before = list.len();
        list.retain(|m| m.id != id);
        if list.len() == before {
            return Err(format!("нет сценария {id}"));
        }
        self.write_index(&list)?;
        let _ = fs::remove_file(self.file(id));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("pcdock-autod-{name}-{}-{}", std::process::id(), now_ms()));
        let _ = fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn put_get_rename_delete_and_index_json() {
        let dir = tmp("store");
        let st = ScriptStore::open(&dir).unwrap();
        assert!(st.list().unwrap().is_empty());
        let m = st.put(None, "Главный экран", "log.info('hi')").unwrap();
        assert!(valid_id(&m.id));
        // Файл `<id>.lua` и соответствие имени в index.json лежат рядом.
        assert!(dir.join("scripts").join(format!("{}.lua", m.id)).exists());
        let index = fs::read_to_string(dir.join("scripts/index.json")).unwrap();
        assert!(index.contains("Главный экран") && index.contains(&m.id), "{index}");

        let (meta, src) = st.get(&m.id).unwrap();
        assert_eq!((meta.name.as_str(), src.as_str()), ("Главный экран", "log.info('hi')"));

        let m2 = st.put(Some(&m.id), "Главный экран v2", "log.info('v2')").unwrap();
        assert_eq!(m2.id, m.id);
        assert_eq!(st.get(&m.id).unwrap().1, "log.info('v2')");
        st.rename(&m.id, "Новое имя").unwrap();
        assert_eq!(st.list().unwrap()[0].name, "Новое имя");

        st.delete(&m.id).unwrap();
        assert!(st.list().unwrap().is_empty());
        assert!(!dir.join("scripts").join(format!("{}.lua", m.id)).exists());
        assert!(st.get(&m.id).is_err());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn ids_cannot_escape_the_scripts_folder() {
        let dir = tmp("escape");
        let st = ScriptStore::open(&dir).unwrap();
        for bad in ["../x", "a/b", "", "a b", "..", &"x".repeat(65)] {
            assert!(st.get(bad).is_err(), "{bad:?}");
            assert!(st.put(Some(bad), "n", "s").is_err(), "{bad:?}");
            assert!(st.delete(bad).is_err(), "{bad:?}");
        }
        assert!(st.put(None, "", "s").is_err(), "имя обязательно");
        assert!(st.put(None, "n", &"x".repeat(MAX_SOURCE + 1)).is_err(), "размер ограничен");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn two_new_scripts_get_different_ids() {
        let dir = tmp("ids");
        let st = ScriptStore::open(&dir).unwrap();
        let a = st.put(None, "a", "1").unwrap();
        let b = st.put(None, "b", "2").unwrap();
        assert_ne!(a.id, b.id);
        assert_eq!(st.list().unwrap().len(), 2);
        let _ = fs::remove_dir_all(&dir);
    }
}
