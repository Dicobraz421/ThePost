//! Эквивалентная команда `thepost admin …` для действия на экране (решение 118): нижняя строка интерфейса.
//!
//! Команды CLI написаны вручную (`thepost::admin_cli`), поэтому соответствие `operationId` → команда — таблица;
//! операция без команды CLI показывается методом и путём Admin API.

use crate::engine::Request;

/// Команды CLI по `operationId`: шаблон (`{параметр}` — параметр пути или поле тела) и ключи для параметров
/// запроса (`имя` → `--имя`).
const COMMANDS: &[(&str, &str)] = &[
    ("create_session", "login --user <ИМЯ_ВХОДА>"),
    ("whoami", "whoami"),
    ("list_accounts", "account list"),
    ("create_account", "account create {login}"),
    ("get_account", "account get {account}"),
    ("patch_account", "account set {account}"),
    ("list_app_passwords", "app-password list {account}"),
    ("issue", "app-password create {account}"),
    ("revoke", "app-password revoke {account} {id}"),
    (
        "put_account_policy",
        "policy set --account {account} --file <ПУТЬ>",
    ),
    ("put_org_policy", "policy set --file <ПУТЬ>"),
    ("get_org", "policy get"),
    ("list_groups", "group list"),
    ("get_group", "group get {group}"),
    ("create_group", "group create {name}"),
    ("delete_group", "group delete {group}"),
    ("list_trash", "trash list {account}"),
    ("list_hidden", "trash list {account} --hidden"),
    ("restore", "trash restore {account} {message}"),
    (
        "restore_hidden",
        "trash restore {account} {message} --hidden",
    ),
    ("search_audit", "audit search"),
    ("search_delivery", "delivery search"),
    ("list_domains", "transport domain list"),
    ("create_domain", "transport domain create {name}"),
    ("delete_domain", "transport domain delete {id}"),
    ("list_sources", "transport source list"),
    ("put_org_source", "transport source set {name}"),
    ("delete_org_source", "transport source delete {name}"),
    ("list_routes", "transport route list"),
    ("put_route", "transport route set {pattern}"),
    ("delete_route", "transport route delete {pattern}"),
    ("list_scripts", "sieve list {account}"),
    ("read", "sieve get {account} {name}"),
    ("put", "sieve put {account} {name} --file <ПУТЬ>"),
    ("activate", "sieve activate {account} {name}"),
    ("deactivate", "sieve deactivate {account}"),
    ("remove", "sieve delete {account} {name}"),
    ("org_script", "sieve org get"),
    ("set_org_script", "sieve org put --file <ПУТЬ>"),
    ("remove_org_script", "sieve org delete"),
    ("vacation", "vacation get {account}"),
    ("set_vacation", "vacation set {account}"),
];

/// Шаблон команды CLI операции `id`.
pub fn template(id: &str) -> Option<&'static str> {
    COMMANDS.iter().find(|(o, _)| *o == id).map(|(_, t)| *t)
}

/// Строка команды для запроса `req`; `home_org` — организация администратора (иная — ключ `--org`).
pub fn command(req: &Request, home_org: Option<&str>) -> String {
    let Some(t) = template(&req.op.id) else {
        return format!("нет в CLI: {} {}", req.op.method, req.op.path);
    };
    let mut out = String::from("thepost admin ");
    let mut rest = t;
    while let Some(start) = rest.find('{') {
        out.push_str(&rest[..start]);
        let Some(end) = rest[start..].find('}') else {
            break;
        };
        let name = &rest[start + 1..start + end];
        let value = req
            .param(name)
            .map(str::to_owned)
            .or_else(|| {
                req.body
                    .as_ref()
                    .and_then(|b| b.get(name))
                    .and_then(|v| v.as_str())
                    .map(str::to_owned)
            })
            .unwrap_or_else(|| format!("<{name}>"));
        out.push_str(&quote(&value));
        rest = &rest[start + end + 1..];
    }
    out.push_str(rest);
    for (k, v) in &req.query {
        out.push_str(&format!(" --{} {}", k.replace('_', "-"), quote(v)));
    }
    if let Some(org) = req.param("org")
        && home_org.is_some_and(|h| h != org)
    {
        out.push_str(&format!(" --org {org}"));
    }
    out
}

/// Значение для оболочки: с пробелами или кавычками — в одинарных кавычках.
fn quote(v: &str) -> String {
    if !v.is_empty()
        && v.chars()
            .all(|c| c.is_alphanumeric() || "@._-:/<>=,+".contains(c))
    {
        v.to_owned()
    } else {
        format!("'{}'", v.replace('\'', r"'\''"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::Context;
    use crate::schema::catalog;

    /// Каждая строка таблицы — операция схемы, все параметры шаблона известны операции.
    #[test]
    fn table_matches_spec() {
        let c = catalog();
        for (id, t) in COMMANDS {
            let op = c.get(id).unwrap_or_else(|| panic!("{id}: нет в схеме"));
            for part in t.split('{').skip(1) {
                let name = part.split('}').next().unwrap_or_default();
                let known = op.path_names().contains(&name)
                    || op
                        .body
                        .as_ref()
                        .is_some_and(|b| b.fields.iter().any(|f| f.name == name));
                assert!(known, "{id}: параметр {{{name}}} не из операции");
            }
        }
    }

    #[test]
    fn commands() {
        let c = catalog();
        let ctx = Context::from([
            ("org".to_owned(), "o1".to_owned()),
            ("account".to_owned(), "a1".to_owned()),
        ]);
        let list = c.get("list_accounts").expect("list_accounts");
        let req = Request::new(list, &ctx, vec![("prefix".into(), "iv".into())]).expect("запрос");
        assert_eq!(
            command(&req, Some("o1")),
            "thepost admin account list --prefix iv"
        );
        assert_eq!(
            command(&req, Some("o2")),
            "thepost admin account list --prefix iv --org o1"
        );
        let mut create = Request::new(c.get("create_account").expect("create"), &ctx, Vec::new())
            .expect("запрос");
        create.body = Some(serde_json::json!({"login": "Иванова Мария"}));
        assert_eq!(
            command(&create, Some("o1")),
            "thepost admin account create 'Иванова Мария'"
        );
        let units =
            Request::new(c.get("list_units").expect("units"), &ctx, Vec::new()).expect("запрос");
        assert_eq!(
            command(&units, Some("o1")),
            "нет в CLI: GET /api/v1/orgs/{org}/units"
        );
    }
}
