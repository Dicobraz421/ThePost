//! Конфигурация узла (TUI-04, решение 118): `config.yaml` правится в текстовом интерфейсе на самом узле — дерево
//! секций по схеме конфигурации с подсказками и значениями по умолчанию; новое значение проверяется
//! `thepost config check` до записи, перед записью — резервная копия `config.yaml.bak`. Узел перечитывает файл без
//! перезапуска сам (событие ОС или SIGHUP — существующий механизм перечитывания).
//!
//! Файл с меткой DEP-55 в начале (`# thepost: managed-by=ansible` или `=helm`) — только чтение: его правят в
//! инвентаре Ansible или значениях чарта Helm, иначе у файла два источника.
//!
//! Схему, разбор и запись YAML и проверку даёт вызывающий код ([`ConfigHost`], подкоманда `thepost admin tui`):
//! у интерфейса нет второго разбора конфигурации.

use std::collections::BTreeSet;
use std::fmt;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::{Map, Value};

use crate::schema::{Field, Kind, Location, Resolver, describe};

/// Метка DEP-55 в начале файла.
const MARK: &str = "thepost: managed-by=";

/// Глубина вложенности секций схемы, которую показывает дерево.
const MAX_DEPTH: usize = 6;

/// Что узел даёт интерфейсу для правки своей конфигурации.
pub trait ConfigHost: Send + Sync {
    /// JSON Schema `config.yaml` (как `thepost config schema`).
    fn schema(&self) -> Value;
    /// Документ YAML как JSON.
    fn parse(&self, text: &str) -> Result<Value, String>;
    /// YAML документа.
    fn render(&self, doc: &Value) -> Result<String, String>;
    /// `thepost config check` файла `path`: ошибка — строки `путь.параметра: причина`.
    fn check(&self, path: &Path) -> Result<(), String>;
}

/// Конфигурация узла, на котором запущен интерфейс.
#[derive(Clone)]
pub struct NodeConfig {
    /// Путь к `config.yaml`.
    pub path: PathBuf,
    /// Схема, YAML и проверка.
    pub host: Arc<dyn ConfigHost>,
}

impl fmt::Debug for NodeConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NodeConfig")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

/// Можно ли править файл.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Access {
    /// Правка разрешена.
    Write,
    /// Файл помечен меткой DEP-55: управляется этим средством (`ansible`, `helm`).
    Managed(String),
    /// Нет права на запись.
    ReadOnly(String),
}

impl Access {
    /// Пояснение, почему только чтение; `None` — правка разрешена.
    pub fn reason(&self) -> Option<String> {
        match self {
            Self::Write => None,
            Self::Managed(tool) => Some(match tool.as_str() {
                "ansible" => {
                    "файл управляется Ansible: правка — в инвентаре роли thepost".to_owned()
                }
                "helm" => "файл управляется Helm: правка — в значениях чарта thepost".to_owned(),
                other => format!("файл управляется {other}: правка — там"),
            }),
            Self::ReadOnly(why) => Some(format!("нет права на запись: {why}")),
        }
    }
}

/// Параметр или секция схемы.
#[derive(Debug, Clone, PartialEq)]
pub struct Param {
    /// Путь от корня документа.
    pub path: Vec<String>,
    /// Секция (объект с параметрами).
    pub section: bool,
    /// Поле для правки значения.
    pub field: Field,
    /// Значение по умолчанию.
    pub default: Option<Value>,
}

impl Param {
    /// Путь через точку (`smtp.limits.max_size`).
    pub fn key(&self) -> String {
        self.path.join(".")
    }

    /// Имя — последний элемент пути.
    pub fn name(&self) -> &str {
        self.path.last().map_or("", String::as_str)
    }
}

/// Прочитанный файл конфигурации.
#[derive(Debug, Clone, PartialEq)]
pub struct ConfigFile {
    /// Путь.
    pub path: PathBuf,
    /// Текст на момент чтения (запись отказывает, если файл изменили после).
    pub text: String,
    /// Документ.
    pub doc: Value,
    /// Можно ли править.
    pub access: Access,
    /// Параметры схемы в порядке дерева.
    pub params: Vec<Param>,
}

impl ConfigFile {
    /// Значение параметра в документе; нет — `None`.
    pub fn value(&self, p: &Param) -> Option<&Value> {
        p.path.iter().try_fold(&self.doc, |v, k| v.get(k))
    }

    /// Видимые строки дерева: параметры, все секции-предки которых раскрыты (`open` — ключи секций).
    pub fn visible(&self, open: &BTreeSet<String>) -> Vec<&Param> {
        self.params
            .iter()
            .filter(|p| (1..p.path.len()).all(|n| open.contains(&p.path[..n].join("."))))
            .collect()
    }
}

/// Отказ записи.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SaveError {
    /// Ошибка относится к правленому параметру (иначе — к файлу целиком).
    pub at_field: bool,
    /// Текст.
    pub message: String,
}

impl SaveError {
    fn file(message: impl Into<String>) -> Self {
        Self {
            at_field: false,
            message: message.into(),
        }
    }
}

/// Записанный файл.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Saved {
    /// Резервная копия прежнего файла.
    pub backup: PathBuf,
}

/// Метка DEP-55 в начальных строках-комментариях файла: средство, которое им управляет.
pub fn managed_by(text: &str) -> Option<String> {
    text.lines()
        .map(str::trim)
        .take_while(|l| l.is_empty() || l.starts_with('#'))
        .find_map(|l| {
            let rest = l.trim_start_matches('#').trim();
            let tool = rest.strip_prefix(MARK)?;
            tool.split_whitespace().next().map(str::to_lowercase)
        })
}

fn writable(path: &Path) -> Result<(), String> {
    fs::OpenOptions::new()
        .append(true)
        .open(path)
        .map(drop)
        .map_err(|e| e.to_string())
}

/// Прочитать файл узла: текст, документ, метка, право на запись и параметры схемы.
pub fn load(node: &NodeConfig) -> Result<ConfigFile, String> {
    let text =
        fs::read_to_string(&node.path).map_err(|e| format!("{}: {e}", node.path.display()))?;
    let doc = node.host.parse(&text)?;
    let access = match (managed_by(&text), writable(&node.path)) {
        (Some(tool), _) => Access::Managed(tool),
        (None, Err(e)) => Access::ReadOnly(e),
        (None, Ok(())) => Access::Write,
    };
    Ok(ConfigFile {
        path: node.path.clone(),
        text,
        doc,
        access,
        params: params(&node.host.schema()),
    })
}

/// Параметры схемы конфигурации в порядке дерева: секция, затем её параметры.
pub fn params(schema: &Value) -> Vec<Param> {
    let empty = Map::new();
    let defs = schema
        .get("$defs")
        .and_then(Value::as_object)
        .unwrap_or(&empty);
    let r = Resolver::new(defs);
    let mut out = Vec::new();
    walk(&r, schema, &mut Vec::new(), None, &mut out);
    out
}

fn walk(
    r: &Resolver<'_>,
    schema: &Value,
    path: &mut Vec<String>,
    defaults: Option<&Value>,
    out: &mut Vec<Param>,
) {
    let Some((props, _)) = r.object(schema) else {
        return;
    };
    let mut props = props;
    props.sort_by_key(|(n, _)| *n);
    for (name, p) in props {
        path.push(name.to_owned());
        let resolved = r.deref(p);
        let default = p
            .get("default")
            .or_else(|| resolved.get("default"))
            .or_else(|| defaults.and_then(|d| d.get(name)))
            .cloned();
        let section = path.len() < MAX_DEPTH && r.object(p).is_some_and(|(pp, _)| !pp.is_empty());
        let (kind, nullable, choices) = r.kind(p);
        out.push(Param {
            path: path.clone(),
            section,
            field: Field {
                name: path.join("."),
                location: Location::Body,
                kind: if section { Kind::Json } else { kind },
                required: false,
                nullable,
                choices,
                minimum: resolved.get("minimum").and_then(Value::as_f64),
                description: describe(p)
                    .or_else(|| describe(resolved))
                    .unwrap_or_default(),
            },
            default: default.clone(),
        });
        if section {
            walk(r, p, path, default.as_ref(), out);
        }
        path.pop();
    }
}

/// Задать (`Some`) или убрать (`None` — значение по умолчанию) значение по пути; недостающие секции создаются.
pub fn set(doc: &mut Value, path: &[String], value: Option<Value>) {
    let Some((last, parents)) = path.split_last() else {
        return;
    };
    let mut at = doc;
    for k in parents {
        if !at.is_object() {
            *at = Value::Object(Map::new());
        }
        let Some(obj) = at.as_object_mut() else {
            return;
        };
        at = obj
            .entry(k.clone())
            .or_insert_with(|| Value::Object(Map::new()));
    }
    if !at.is_object() {
        *at = Value::Object(Map::new());
    }
    if let Some(obj) = at.as_object_mut() {
        match value {
            Some(v) => {
                obj.insert(last.clone(), v);
            }
            None => {
                obj.remove(last);
            }
        }
    }
}

/// Начальные строки-комментарии файла (заголовок), которые сохраняются при записи.
fn header(text: &str) -> String {
    text.lines()
        .take_while(|l| l.trim_start().starts_with('#'))
        .map(|l| format!("{l}\n"))
        .collect()
}

fn sibling(path: &Path, suffix: &str) -> PathBuf {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "config.yaml".to_owned());
    path.with_file_name(format!("{name}{suffix}"))
}

/// Записать значение параметра `path` (`None` — убрать, действует значение по умолчанию): файл не изменился с
/// чтения (`loaded`), не помечен и доступен на запись. Замена атомарная, как у `visudo`: новый текст пишется во
/// временный файл в том же каталоге (права и владелец — как у прежнего файла, данные сброшены на диск), временный
/// файл проходит `thepost config check` (ошибка у поля — строка проверки с путём параметра), прежний файл
/// копируется в `config.yaml.bak`, временный переименовывается поверх. Узел и сбой посередине видят либо прежний
/// файл, либо новый целиком. Символическая ссылка остаётся ссылкой: заменяется файл, на который она указывает.
pub fn save(
    node: &NodeConfig,
    loaded: &str,
    path: &[String],
    value: Option<Value>,
) -> Result<Saved, SaveError> {
    let file = &node.path;
    let text = fs::read_to_string(file)
        .map_err(|e| SaveError::file(format!("{}: {e}", file.display())))?;
    if text != loaded {
        return Err(SaveError::file(
            "файл изменён после чтения — F2 перечитает его",
        ));
    }
    if let Some(reason) = managed_by(&text)
        .map(Access::Managed)
        .and_then(|a| a.reason())
    {
        return Err(SaveError::file(reason));
    }
    writable(file).map_err(|e| SaveError::file(format!("нет права на запись: {e}")))?;
    let mut doc = node.host.parse(&text).map_err(SaveError::file)?;
    set(&mut doc, path, value);
    let body = node.host.render(&doc).map_err(SaveError::file)?;
    let new_text = format!("{}{body}", header(&text));
    let target =
        fs::canonicalize(file).map_err(|e| SaveError::file(format!("{}: {e}", file.display())))?;
    let tmp = sibling(&target, &format!(".{}.tmp", std::process::id()));
    let backup = sibling(file, ".bak");
    let replaced = replace(node, &target, &tmp, &backup, &new_text, &path.join("."));
    if replaced.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    replaced.map(|()| Saved { backup })
}

/// Атомарная замена `target` текстом `text` через временный файл `tmp` (шаги — у [`save`]); при ошибке `tmp`
/// убирает вызывающий код.
fn replace(
    node: &NodeConfig,
    target: &Path,
    tmp: &Path,
    backup: &Path,
    text: &str,
    key: &str,
) -> Result<(), SaveError> {
    let meta =
        fs::metadata(target).map_err(|e| SaveError::file(format!("{}: {e}", target.display())))?;
    write_temp(tmp, text, &meta).map_err(|e| SaveError::file(format!("{}: {e}", tmp.display())))?;
    node.host.check(tmp).map_err(|e| field_error(&e, key))?;
    fs::copy(target, backup)
        .map_err(|e| SaveError::file(format!("резервная копия {}: {e}", backup.display())))?;
    fs::rename(tmp, target).map_err(|e| SaveError::file(format!("{}: {e}", target.display())))?;
    // Запись в каталоге (новое имя файла) — тоже на диск; файл уже заменён, поэтому сбой здесь не ошибка записи.
    if let Some(dir) = target.parent() {
        let _ = sync_dir(dir);
    }
    Ok(())
}

/// Временный файл с текстом `text`: владелец и права — как у `meta` (до этого — только владельцу), данные на диске.
fn write_temp(tmp: &Path, text: &str, meta: &fs::Metadata) -> io::Result<()> {
    let mut opts = fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(tmp)?;
    f.write_all(text.as_bytes())?;
    // Сначала владелец: смена владельца сбрасывает биты setuid/setgid, права ставятся после.
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, fchown};
        fchown(&f, Some(meta.uid()), Some(meta.gid()))?;
    }
    f.set_permissions(meta.permissions())?;
    f.sync_all()
}

#[cfg(unix)]
fn sync_dir(dir: &Path) -> io::Result<()> {
    fs::File::open(dir)?.sync_all()
}

/// Каталог на Windows не открыть как файл; переименование там журналируется файловой системой.
#[cfg(not(unix))]
fn sync_dir(_dir: &Path) -> io::Result<()> {
    Ok(())
}

/// Ошибка проверки у правленого параметра `key`, если строка проверки называет его (или вложенный параметр).
fn field_error(message: &str, key: &str) -> SaveError {
    let line = message.lines().map(str::trim).find(|l| {
        l.strip_prefix(key).is_some_and(|rest| {
            rest.starts_with(':') || rest.starts_with('.') || rest.starts_with('[')
        })
    });
    match line {
        Some(l) => SaveError {
            at_field: true,
            message: l.to_owned(),
        },
        None => SaveError::file(
            message
                .lines()
                .map(str::trim)
                .rfind(|l| !l.is_empty())
                .unwrap_or("config check: ошибка")
                .to_owned(),
        ),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Узел для тестов: документ — JSON (YAML — надмножество JSON), проверка — `smtp.max_size` целое не больше
    /// 100.
    pub(crate) struct FakeHost;

    impl ConfigHost for FakeHost {
        fn schema(&self) -> Value {
            json!({
                "type": "object",
                "properties": {
                    "domain": {"type": "string", "description": "Домен установки."},
                    "smtp": {"$ref": "#/$defs/Smtp", "default": {"max_size": 10, "greeting": "hi"}},
                    "profile": {"enum": ["prod", "dev"], "description": "Профиль."}
                },
                "$defs": {
                    "Smtp": {
                        "type": "object",
                        "properties": {
                            "max_size": {"type": "integer", "description": "Предельный размер письма."},
                            "greeting": {"type": "string"},
                            "limits": {"type": "object", "properties": {"burst": {"type": "integer"}}}
                        }
                    }
                }
            })
        }

        fn parse(&self, text: &str) -> Result<Value, String> {
            let body: String = text
                .lines()
                .filter(|l| !l.trim_start().starts_with('#'))
                .collect();
            serde_json::from_str(&body).map_err(|e| e.to_string())
        }

        fn render(&self, doc: &Value) -> Result<String, String> {
            Ok(format!("{doc}\n"))
        }

        fn check(&self, path: &Path) -> Result<(), String> {
            let text = fs::read_to_string(path).map_err(|e| e.to_string())?;
            let doc = self.parse(&text)?;
            match doc.pointer("/smtp/max_size") {
                Some(v) if v.as_u64().is_none_or(|n| n > 100) => Err(format!(
                    "{}: ошибки конфигурации:\nsmtp.max_size: invalid value: {v}, expected not more than 100",
                    path.display()
                )),
                _ => Ok(()),
            }
        }
    }

    /// Временный каталог теста (удаляется в конце).
    pub(crate) struct Dir(pub PathBuf);

    impl Dir {
        pub(crate) fn new() -> Self {
            static N: AtomicUsize = AtomicUsize::new(0);
            let p = std::env::temp_dir().join(format!(
                "tp-tui-node-{}-{}",
                std::process::id(),
                N.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&p).expect("каталог");
            Self(p)
        }

        pub(crate) fn node(&self, text: &str) -> NodeConfig {
            let path = self.0.join("config.yaml");
            fs::write(&path, text).expect("запись");
            NodeConfig {
                path,
                host: Arc::new(FakeHost),
            }
        }
    }

    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn key(p: &[&str]) -> Vec<String> {
        p.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn schema_tree_with_defaults() {
        let ps = params(&FakeHost.schema());
        let keys: Vec<String> = ps.iter().map(Param::key).collect();
        assert_eq!(
            keys,
            [
                "domain",
                "profile",
                "smtp",
                "smtp.greeting",
                "smtp.limits",
                "smtp.limits.burst",
                "smtp.max_size"
            ]
        );
        let max = ps
            .iter()
            .find(|p| p.key() == "smtp.max_size")
            .expect("max_size");
        assert_eq!(max.default, Some(json!(10)));
        assert_eq!(max.field.kind, Kind::Integer);
        assert_eq!(max.field.description, "Предельный размер письма.");
        let profile = ps.iter().find(|p| p.key() == "profile").expect("profile");
        assert_eq!(profile.field.choices, ["prod", "dev"]);
        assert!(
            ps.iter()
                .find(|p| p.key() == "smtp")
                .is_some_and(|p| p.section)
        );
    }

    #[test]
    fn managed_mark() {
        assert_eq!(
            managed_by("# thepost: managed-by=ansible\n# Ansible managed\ndomain: x\n").as_deref(),
            Some("ansible")
        );
        assert_eq!(
            managed_by("\n# The Post\n#  thepost: managed-by=Helm \ndomain: x\n").as_deref(),
            Some("helm")
        );
        // Метка ниже первой строки данных — не метка.
        assert_eq!(
            managed_by("domain: x\n# thepost: managed-by=ansible\n"),
            None
        );
    }

    /// Неверное значение не записывается: ошибка проверки — у поля, файл и копии не тронуты.
    #[test]
    fn invalid_value_is_not_written() {
        let dir = Dir::new();
        let text = "# Узел 1\n{\"domain\": \"it-task.ru\"}\n";
        let node = dir.node(text);
        let file = load(&node).expect("чтение");
        assert_eq!(file.access, Access::Write);
        let err = save(
            &node,
            &file.text,
            &key(&["smtp", "max_size"]),
            Some(json!(500)),
        )
        .expect_err("отказ");
        assert!(err.at_field, "{err:?}");
        assert!(
            err.message.starts_with("smtp.max_size: invalid value"),
            "{err:?}"
        );
        assert_eq!(fs::read_to_string(&node.path).expect("файл"), text);
        assert_eq!(names(&dir), ["config.yaml"], "временный файл убран");
    }

    /// Имена файлов каталога теста по порядку.
    fn names(dir: &Dir) -> Vec<String> {
        let mut out: Vec<String> = fs::read_dir(&dir.0)
            .expect("каталог")
            .map(|e| {
                e.expect("запись")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        out.sort();
        out
    }

    /// Верное значение: резервная копия прежнего файла, новый текст с заголовком-комментарием.
    #[test]
    fn backup_before_write() {
        let dir = Dir::new();
        let text = "# Узел 1\n{\"domain\": \"it-task.ru\"}\n";
        let node = dir.node(text);
        let file = load(&node).expect("чтение");
        let saved = save(
            &node,
            &file.text,
            &key(&["smtp", "max_size"]),
            Some(json!(50)),
        )
        .expect("запись");
        assert_eq!(saved.backup, dir.0.join("config.yaml.bak"));
        assert_eq!(fs::read_to_string(&saved.backup).expect("копия"), text);
        // Временный файл переименован поверх: рядом — только файл и резервная копия.
        assert_eq!(names(&dir), ["config.yaml", "config.yaml.bak"]);
        let new = load(&node).expect("чтение");
        assert!(new.text.starts_with("# Узел 1\n"), "{}", new.text);
        assert_eq!(
            new.doc,
            json!({"domain": "it-task.ru", "smtp": {"max_size": 50}})
        );
        // Пустое значение — параметр убирается, действует значение по умолчанию.
        save(&node, &new.text, &key(&["smtp", "max_size"]), None).expect("запись");
        assert_eq!(
            load(&node).expect("чтение").doc,
            json!({"domain": "it-task.ru", "smtp": {}})
        );
        // Файл изменили после чтения — запись отказывает.
        let err = save(&node, &new.text, &key(&["domain"]), Some(json!("x"))).expect_err("отказ");
        assert!(err.message.contains("изменён после чтения"), "{err:?}");
    }

    /// Файл заменяется, а не переписывается на месте: новый inode, права и владелец прежнего файла; ссылка
    /// остаётся ссылкой, заменяется файл, на который она указывает.
    #[cfg(unix)]
    #[test]
    fn atomic_replace_keeps_mode_owner_and_symlink() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};

        let dir = Dir::new();
        let real = dir.0.join("node.yaml");
        fs::write(&real, "{\"domain\": \"it-task.ru\"}\n").expect("запись");
        fs::set_permissions(&real, fs::Permissions::from_mode(0o640)).expect("права");
        let link = dir.0.join("config.yaml");
        symlink(&real, &link).expect("ссылка");
        let node = NodeConfig {
            path: link.clone(),
            host: Arc::new(FakeHost),
        };
        let before = fs::metadata(&real).expect("файл");
        let file = load(&node).expect("чтение");
        save(&node, &file.text, &key(&["domain"]), Some(json!("x.ru"))).expect("запись");

        assert!(
            fs::symlink_metadata(&link)
                .expect("ссылка")
                .file_type()
                .is_symlink()
        );
        let after = fs::metadata(&real).expect("файл");
        assert_ne!(after.ino(), before.ino(), "замена, а не запись на месте");
        assert_eq!(after.mode() & 0o7777, 0o640);
        assert_eq!((after.uid(), after.gid()), (before.uid(), before.gid()));
        assert_eq!(load(&node).expect("чтение").doc, json!({"domain": "x.ru"}));
        assert_eq!(names(&dir), ["config.yaml", "config.yaml.bak", "node.yaml"]);
    }

    /// Помеченный файл — только чтение: запись отказывает, файл не тронут.
    #[test]
    fn managed_file_is_read_only() {
        let dir = Dir::new();
        let text = "# thepost: managed-by=ansible\n{\"domain\": \"it-task.ru\"}\n";
        let node = dir.node(text);
        let file = load(&node).expect("чтение");
        assert_eq!(file.access, Access::Managed("ansible".into()));
        assert!(file.access.reason().is_some_and(|r| r.contains("Ansible")));
        let err = save(&node, &file.text, &key(&["domain"]), Some(json!("x"))).expect_err("отказ");
        assert!(!err.at_field && err.message.contains("Ansible"), "{err:?}");
        assert_eq!(fs::read_to_string(&node.path).expect("файл"), text);
        assert!(!dir.0.join("config.yaml.bak").exists());
    }

    #[test]
    fn visible_rows_follow_open_sections() {
        let dir = Dir::new();
        let file = load(&dir.node("{}")).expect("чтение");
        let names = |open: &BTreeSet<String>| -> Vec<String> {
            file.visible(open).iter().map(|p| p.key()).collect()
        };
        assert_eq!(names(&BTreeSet::new()), ["domain", "profile", "smtp"]);
        let open = BTreeSet::from(["smtp".to_owned()]);
        assert_eq!(
            names(&open),
            [
                "domain",
                "profile",
                "smtp",
                "smtp.greeting",
                "smtp.limits",
                "smtp.max_size"
            ]
        );
    }
}
