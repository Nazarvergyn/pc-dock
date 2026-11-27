//! Разбор экрана телефона: дерево `uiautomator dump`, поиск элементов по селектору, классификация.
//! Разбор живёт в pc-dock, чтобы клиентам (в том числе сценариям Lua) не нужно было разбирать XML самим.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// Элемент экрана. Границы в пикселях экрана.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct UiNode {
    pub text: String,
    pub desc: String,
    /// `resource-id`, например `com.app:id/button`.
    pub id: String,
    pub class: String,
    pub package: String,
    pub bounds: [i32; 4],
    pub clickable: bool,
    pub enabled: bool,
    pub checked: bool,
    pub checkable: bool,
    pub focused: bool,
    pub scrollable: bool,
    pub selected: bool,
}

/// Дерево экрана: элементы в порядке обхода и размер экрана (по корню).
#[derive(Clone, Debug, PartialEq, Default)]
pub struct UiTree {
    pub nodes: Vec<UiNode>,
    pub width: i32,
    pub height: i32,
}

fn unescape(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&#10;", "\n")
        .replace("&#13;", "\r")
        .replace("&#9;", "\t")
        .replace("&amp;", "&")
}

/// Атрибуты одного тега: `имя="значение"`.
fn attrs(tag: &str) -> Vec<(&str, String)> {
    let mut out = Vec::new();
    let mut rest = tag;
    while let Some(eq) = rest.find("=\"") {
        let name = rest[..eq].rsplit(|c: char| c.is_whitespace()).next().unwrap_or("");
        let after = &rest[eq + 2..];
        let Some(end) = after.find('"') else { break };
        out.push((name, unescape(&after[..end])));
        rest = &after[end + 1..];
    }
    out
}

/// `[l,t][r,b]`.
fn parse_bounds(s: &str) -> Option<[i32; 4]> {
    let nums: Vec<i32> = s
        .split(['[', ']', ','])
        .filter(|p| !p.is_empty())
        .map(|p| p.trim().parse::<i32>())
        .collect::<Result<_, _>>()
        .ok()?;
    (nums.len() == 4).then(|| [nums[0], nums[1], nums[2], nums[3]])
}

/// Разбирает вывод `uiautomator dump`. Мусор до первого `<hierarchy` (сообщения вроде `UI hierarchy dumped to:`) пропускается.
pub fn parse_dump(xml: &str) -> Result<UiTree, String> {
    let start = xml.find("<hierarchy").ok_or("в ответе нет дерева экрана (<hierarchy>)")?;
    let mut tree = UiTree::default();
    let mut rest = &xml[start..];
    while let Some(i) = rest.find("<node ") {
        let after = &rest[i..];
        let end = after.find('>').ok_or("оборванный тег node")?;
        let tag = &after[..end];
        let mut n = UiNode::default();
        for (k, v) in attrs(tag) {
            match k {
                "text" => n.text = v,
                "content-desc" => n.desc = v,
                "resource-id" => n.id = v,
                "class" => n.class = v,
                "package" => n.package = v,
                "bounds" => n.bounds = parse_bounds(&v).unwrap_or_default(),
                "clickable" => n.clickable = v == "true",
                "enabled" => n.enabled = v == "true",
                "checked" => n.checked = v == "true",
                "checkable" => n.checkable = v == "true",
                "focused" => n.focused = v == "true",
                "scrollable" => n.scrollable = v == "true",
                "selected" => n.selected = v == "true",
                _ => {}
            }
        }
        if tree.nodes.is_empty() {
            // корень: на весь экран
            tree.width = n.bounds[2];
            tree.height = n.bounds[3];
        }
        tree.nodes.push(n);
        rest = &after[end..];
    }
    if tree.nodes.is_empty() {
        return Err("на экране нет элементов (экран выключен или защищён от съёмки)".into());
    }
    Ok(tree)
}

/// Условие поиска. Заданные поля сочетаются по И; `any` это варианты по ИЛИ (узел должен подойти хотя бы под один).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Selector {
    /// Текст целиком.
    pub text: Option<String>,
    pub text_contains: Option<String>,
    /// `resource-id` целиком или его короткая часть после `:id/`.
    pub id: Option<String>,
    /// `content-desc` целиком.
    pub desc: Option<String>,
    pub desc_contains: Option<String>,
    pub class: Option<String>,
    pub package: Option<String>,
    pub checked: Option<bool>,
    pub enabled: Option<bool>,
    pub focused: Option<bool>,
    pub clickable: Option<bool>,
    pub scrollable: Option<bool>,
    pub selected: Option<bool>,
    /// Не различать регистр в текстах.
    pub ignore_case: bool,
    pub any: Vec<Selector>,
}

impl Selector {
    fn text_eq(&self, have: &str, want: &str) -> bool {
        if self.ignore_case {
            have.to_lowercase() == want.to_lowercase()
        } else {
            have == want
        }
    }

    fn text_in(&self, have: &str, want: &str) -> bool {
        if self.ignore_case {
            have.to_lowercase().contains(&want.to_lowercase())
        } else {
            have.contains(want)
        }
    }

    pub fn matches(&self, n: &UiNode) -> bool {
        let own = self.text.as_ref().is_none_or(|w| self.text_eq(&n.text, w))
            && self.text_contains.as_ref().is_none_or(|w| self.text_in(&n.text, w))
            && self.desc.as_ref().is_none_or(|w| self.text_eq(&n.desc, w))
            && self.desc_contains.as_ref().is_none_or(|w| self.text_in(&n.desc, w))
            && self.id.as_ref().is_none_or(|w| n.id == *w || n.id.ends_with(&format!(":id/{w}")))
            && self.class.as_ref().is_none_or(|w| n.class == *w || n.class.ends_with(&format!(".{w}")))
            && self.package.as_ref().is_none_or(|w| n.package == *w)
            && self.checked.is_none_or(|w| n.checked == w)
            && self.enabled.is_none_or(|w| n.enabled == w)
            && self.focused.is_none_or(|w| n.focused == w)
            && self.clickable.is_none_or(|w| n.clickable == w)
            && self.scrollable.is_none_or(|w| n.scrollable == w)
            && self.selected.is_none_or(|w| n.selected == w);
        own && (self.any.is_empty() || self.any.iter().any(|a| a.matches(n)))
    }
}

impl UiNode {
    pub fn visible(&self, tree: &UiTree) -> bool {
        let [l, t, r, b] = self.bounds;
        r > l && b > t && r > 0 && b > 0 && l < tree.width && t < tree.height
    }

    /// Вид для показа: кнопка, поле, переключатель, картинка, текст, список, другое.
    pub fn kind(&self) -> &'static str {
        let c = self.class.as_str();
        if c.ends_with("EditText") || c.ends_with("AutoCompleteTextView") {
            "field"
        } else if self.checkable
            || c.ends_with("Switch")
            || c.ends_with("CheckBox")
            || c.ends_with("ToggleButton")
        {
            "switch"
        } else if c.ends_with("Button") || self.clickable {
            "button"
        } else if c.ends_with("ImageView") || c.ends_with("ImageButton") {
            "image"
        } else if self.scrollable
            || c.ends_with("RecyclerView")
            || c.ends_with("ListView")
            || c.ends_with("ScrollView")
        {
            "list"
        } else if c.ends_with("TextView") || !self.text.is_empty() {
            "text"
        } else {
            "other"
        }
    }

    /// Элемент для ответа: границы и центр в долях экрана (0..1), готовые для касания.
    pub fn to_json(&self, tree: &UiTree) -> Value {
        let (w, h) = (tree.width.max(1) as f64, tree.height.max(1) as f64);
        let [l, t, r, b] = self.bounds;
        let frac = |v: i32, d: f64| ((v as f64 / d) * 10000.0).round() / 10000.0;
        json!({
            "text": self.text,
            "desc": self.desc,
            "id": self.id,
            "class": self.class,
            "package": self.package,
            "kind": self.kind(),
            "bounds": [l, t, r, b],
            "x": frac((l + r) / 2, w),
            "y": frac((t + b) / 2, h),
            "w": frac(r - l, w),
            "h": frac(b - t, h),
            "clickable": self.clickable,
            "enabled": self.enabled,
            "checked": self.checked,
            "focused": self.focused,
            "scrollable": self.scrollable,
        })
    }
}

impl UiTree {
    /// Видимые элементы, подходящие под селектор.
    pub fn find(&self, sel: &Selector) -> Vec<&UiNode> {
        self.nodes.iter().filter(|n| n.visible(self) && sel.matches(n)).collect()
    }

    /// Самый маленький видимый элемент, внутри которого лежит точка (доли экрана).
    pub fn at(&self, x: f64, y: f64) -> Option<&UiNode> {
        let (px, py) = ((x * self.width as f64) as i32, (y * self.height as f64) as i32);
        self.nodes
            .iter()
            .filter(|n| {
                let [l, t, r, b] = n.bounds;
                n.visible(self) && px >= l && px < r && py >= t && py < b
            })
            .min_by_key(|n| (n.bounds[2] - n.bounds[0]) as i64 * (n.bounds[3] - n.bounds[1]) as i64)
    }

    /// Готовый список для показа: видимые элементы с текстом, описанием, идентификатором или нажимаемые.
    pub fn elements(&self) -> Vec<&UiNode> {
        self.nodes
            .iter()
            .filter(|n| {
                n.visible(self)
                    && (!n.text.is_empty() || !n.desc.is_empty() || !n.id.is_empty() || n.clickable)
            })
            .collect()
    }
}

/// Ссылка (URL) в тексте: первая `https://…` до пробела. Для сценариев «взять ссылку с экрана».
pub fn first_url(text: &str) -> Option<String> {
    let i = text.find("https://").or_else(|| text.find("http://"))?;
    let rest = &text[i..];
    let end = rest.find(|c: char| c.is_whitespace() || c == '"' || c == '<').unwrap_or(rest.len());
    Some(rest[..end].trim_end_matches(['.', ',', ')']).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    const DUMP: &str = r#"UI hierarchy dumped to: /dev/tty
<?xml version='1.0' encoding='UTF-8' standalone='yes' ?><hierarchy rotation="0">
<node index="0" text="" resource-id="" class="android.widget.FrameLayout" package="com.app" content-desc="" checkable="false" checked="false" clickable="false" enabled="true" focusable="false" focused="false" scrollable="false" selected="false" bounds="[0,0][1000,2000]">
<node index="0" text="Войти &amp; продолжить" resource-id="com.app:id/login" class="android.widget.Button" package="com.app" content-desc="" checkable="false" checked="false" clickable="true" enabled="true" focusable="true" focused="false" scrollable="false" selected="false" bounds="[100,1000][900,1200]" />
<node index="1" text="" resource-id="com.app:id/name" class="android.widget.EditText" package="com.app" content-desc="Имя" checkable="false" checked="false" clickable="true" enabled="true" focusable="true" focused="true" scrollable="false" selected="false" bounds="[100,400][900,500]" />
<node index="2" text="Тихо" resource-id="com.app:id/mute" class="android.widget.Switch" package="com.app" content-desc="" checkable="true" checked="true" clickable="true" enabled="true" focusable="true" focused="false" scrollable="false" selected="false" bounds="[100,1500][900,1600]" />
<node index="3" text="За экраном" resource-id="" class="android.widget.TextView" package="com.app" content-desc="" checkable="false" checked="false" clickable="false" enabled="true" focusable="false" focused="false" scrollable="false" selected="false" bounds="[0,2100][1000,2200]" />
</node></hierarchy>"#;

    #[test]
    fn dump_is_parsed_with_noise_before_the_tree() {
        let t = parse_dump(DUMP).unwrap();
        assert_eq!((t.width, t.height, t.nodes.len()), (1000, 2000, 5));
        assert_eq!(t.nodes[1].text, "Войти & продолжить");
        assert_eq!(t.nodes[1].bounds, [100, 1000, 900, 1200]);
        assert!(parse_dump("ERROR: null root node returned by UiTestAutomationBridge.").is_err());
    }

    #[test]
    fn selectors_match_by_text_id_state_and_alternatives() {
        let t = parse_dump(DUMP).unwrap();
        let by = |s: Selector| t.find(&s).len();
        assert_eq!(by(Selector { text: Some("Войти & продолжить".into()), ..Default::default() }), 1);
        assert_eq!(
            by(Selector { text_contains: Some("ОЙТИ".into()), ignore_case: true, ..Default::default() }),
            1
        );
        assert_eq!(by(Selector { id: Some("login".into()), ..Default::default() }), 1);
        assert_eq!(by(Selector { id: Some("com.app:id/login".into()), ..Default::default() }), 1);
        assert_eq!(
            by(Selector { class: Some("EditText".into()), focused: Some(true), ..Default::default() }),
            1
        );
        assert_eq!(by(Selector { checked: Some(true), ..Default::default() }), 1);
        // варианты «или»
        let any = Selector {
            any: vec![
                Selector { text: Some("нет такого".into()), ..Default::default() },
                Selector { desc: Some("Имя".into()), ..Default::default() },
            ],
            ..Default::default()
        };
        assert_eq!(by(any), 1);
        // за пределами экрана не виден
        assert_eq!(by(Selector { text: Some("За экраном".into()), ..Default::default() }), 0);
    }

    #[test]
    fn element_json_has_fractions_ready_for_touch() {
        let t = parse_dump(DUMP).unwrap();
        let v = t.nodes[1].to_json(&t);
        assert_eq!((v["x"].as_f64(), v["y"].as_f64()), (Some(0.5), Some(0.55)));
        assert_eq!(v["kind"], "button");
        assert_eq!(t.nodes[2].kind(), "field");
        assert_eq!(t.nodes[3].kind(), "switch");
        assert_eq!(t.at(0.5, 0.55).unwrap().text, "Войти & продолжить");
        assert_eq!(t.elements().len(), 3);
    }

    #[test]
    fn urls_are_taken_from_text() {
        assert_eq!(
            first_url("смотри https://example.com/p/abc/. тут").as_deref(),
            Some("https://example.com/p/abc/")
        );
        assert_eq!(first_url("нет ссылки"), None);
    }
}
