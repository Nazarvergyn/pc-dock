//! Параметры сценария: схема из комментариев `-- @param` и проверка значений при запуске.
//!
//! Формат строки: `-- @param имя тип[?] [= по_умолчанию] [описание]`, тип `string`, `number` или `boolean`.
//! `?` или значение по умолчанию делают параметр необязательным. Пример:
//!
//! ```lua
//! -- @param count number = 3 Сколько раз нажать
//! -- @param caption string? Подпись
//! ```

use serde_json::{json, Map, Value};

#[derive(Clone, Debug, PartialEq)]
pub struct Param {
    pub name: String,
    pub kind: String,
    pub required: bool,
    pub default: Option<Value>,
    pub description: String,
}

impl Param {
    pub fn to_json(&self) -> Value {
        json!({"name": self.name, "type": self.kind, "required": self.required, "default": self.default,
               "description": self.description})
    }
}

fn parse_default(kind: &str, raw: &str) -> Result<Value, String> {
    match kind {
        "number" => raw
            .parse::<f64>()
            .map(|n| json!(n))
            .map_err(|_| format!("значение по умолчанию {raw:?} не число")),
        "boolean" => match raw {
            "true" => Ok(json!(true)),
            "false" => Ok(json!(false)),
            _ => Err(format!("значение по умолчанию {raw:?} не true/false")),
        },
        _ => Ok(json!(raw.trim_matches('"'))),
    }
}

/// Схема параметров из исходного текста сценария.
pub fn parse_schema(source: &str) -> Result<Vec<Param>, String> {
    let mut out: Vec<Param> = Vec::new();
    for (n, line) in source.lines().enumerate() {
        let Some(rest) = line.trim().strip_prefix("--").map(str::trim).and_then(|l| l.strip_prefix("@param"))
        else {
            continue;
        };
        let bad = |what: &str| format!("строка {}: @param: {what}", n + 1);
        let mut it = rest.split_whitespace();
        let name = it.next().ok_or_else(|| bad("нет имени"))?;
        if !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
            || name.starts_with(|c: char| c.is_ascii_digit())
        {
            return Err(bad(&format!("плохое имя {name:?}")));
        }
        let kind_raw = it.next().ok_or_else(|| bad("нет типа"))?;
        let (kind, optional) = match kind_raw.strip_suffix('?') {
            Some(k) => (k, true),
            None => (kind_raw, false),
        };
        if !["string", "number", "boolean"].contains(&kind) {
            return Err(bad(&format!("тип {kind:?}: string, number или boolean")));
        }
        let mut rest: Vec<&str> = it.collect();
        let mut default = None;
        if rest.first() == Some(&"=") && rest.len() >= 2 {
            default = Some(parse_default(kind, rest[1]).map_err(|e| bad(&e))?);
            rest.drain(..2);
        }
        if out.iter().any(|p| p.name == name) {
            return Err(bad(&format!("параметр {name} объявлен дважды")));
        }
        out.push(Param {
            name: name.to_string(),
            kind: kind.to_string(),
            required: !optional && default.is_none(),
            default,
            description: rest.join(" "),
        });
    }
    Ok(out)
}

/// Проверяет `args` по схеме и подставляет значения по умолчанию. Без схемы `args` проходят как есть.
pub fn apply(schema: &[Param], args: &Value) -> Result<Value, String> {
    if schema.is_empty() {
        return Ok(args.clone());
    }
    let given = match args {
        Value::Null => Map::new(),
        Value::Object(m) => m.clone(),
        _ => return Err("args: нужен объект".into()),
    };
    if let Some(k) = given.keys().find(|k| !schema.iter().any(|p| &p.name == *k)) {
        return Err(format!("параметр {k:?} сценарию не объявлен"));
    }
    let mut out = Map::new();
    for p in schema {
        match given.get(&p.name) {
            Some(v) if !v.is_null() => {
                let ok = match p.kind.as_str() {
                    "number" => v.is_number(),
                    "boolean" => v.is_boolean(),
                    _ => v.is_string(),
                };
                if !ok {
                    return Err(format!("параметр {}: ожидается {}", p.name, p.kind));
                }
                out.insert(p.name.clone(), v.clone());
            }
            _ => match &p.default {
                Some(d) => {
                    out.insert(p.name.clone(), d.clone());
                }
                None if p.required => return Err(format!("не задан обязательный параметр {}", p.name)),
                None => {}
            },
        }
    }
    Ok(Value::Object(out))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SRC: &str = "-- @param count number = 3 Сколько раз\n-- @param caption string? Подпись\n-- @param user string\n-- @param loud boolean = false\nprint(1)\n";

    #[test]
    fn schema_is_read_from_comments() {
        let s = parse_schema(SRC).unwrap();
        assert_eq!(s.len(), 4);
        assert_eq!((s[0].required, s[0].default.clone()), (false, Some(json!(3.0))));
        assert_eq!(s[0].description, "Сколько раз");
        assert_eq!((s[1].required, s[2].required), (false, true));
        assert!(parse_schema("-- @param x date").is_err());
        assert!(parse_schema("-- @param x number = abc").is_err());
        assert!(parse_schema("-- @param x number\n-- @param x string").is_err());
        assert!(parse_schema("print(1)").unwrap().is_empty());
    }

    #[test]
    fn args_are_checked_and_defaults_applied() {
        let s = parse_schema(SRC).unwrap();
        let ok = apply(&s, &json!({"user": "ann"})).unwrap();
        assert_eq!(ok, json!({"count": 3.0, "user": "ann", "loud": false}));
        assert!(apply(&s, &json!({})).unwrap_err().contains("user"));
        assert!(apply(&s, &json!({"user": "a", "count": "5"})).unwrap_err().contains("number"));
        assert!(apply(&s, &json!({"user": "a", "bogus": 1})).unwrap_err().contains("bogus"));
        // без схемы всё проходит
        assert_eq!(apply(&[], &json!({"any": 1})).unwrap(), json!({"any": 1}));
    }
}
