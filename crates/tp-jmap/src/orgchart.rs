//! Оргструктура организации: `OrgUnit/get` (capability `urn:thepost:orgchart`; JMAP-17, SRS 4.4, решение
//! 2026-11-04-exchange-parity «Иерархическая адресная книга»).
//!
//! Объект `OrgUnit` — подразделение: `id`, `name`, `parentId` (`null` — корневое; дерево клиент строит по
//! `parentId`) и `members` — учётные записи подразделения в порядке старшинства
//! ([`sort_members`](tp_contracts::addressbook::sort_members): по убыванию старшинства, без старшинства — в конце).
//! Член подразделения: `principalId` (идентификатор `Principal`, RFC 9670), `name`, `email`, `title` (должность),
//! `managerId` (руководитель, `principalId`) и `seniority`. Отключённые учётные записи в состав не входят.
//!
//! Только чтение: источник — каталог организации и Admin API ([`OrgStore`](tp_contracts::addressbook::OrgStore),
//! [`Jmap::with_org`](crate::Jmap::with_org)). `ids: null` — все подразделения. `state` — отпечаток дерева:
//! меняется при любом изменении подразделений и их состава.

use serde_json::{Map, Value, json};
use tp_contracts::addressbook::{OrgMember, OrgUnit};

use crate::core::error::MethodResult;
use crate::core::request::{GetArgs, object, parse_args, select};
use crate::core::{Ctx, digest};

/// Член подразделения; учётная запись не видна как участник (нет, отключена) — `None`.
async fn member(ctx: &Ctx<'_>, m: &OrgMember) -> MethodResult<Option<Value>> {
    let id = m.account.to_string();
    let Some(a) = crate::principal::find(ctx.jmap, ctx.scope.tenant, &id).await? else {
        return Ok(None);
    };
    Ok(Some(json!({
        "principalId": id,
        "name": a.display_name,
        "email": a.login,
        "title": m.org.title,
        "managerId": m.org.manager.map(|x| x.to_string()),
        "seniority": m.org.seniority,
    })))
}

async fn unit_object(ctx: &Ctx<'_>, u: &OrgUnit) -> MethodResult<Map<String, Value>> {
    let store = ctx.jmap.org_store()?;
    let mut members = Vec::new();
    for m in store.org_members(u.tenant, Some(u.id), false).await? {
        members.extend(member(ctx, &m).await?);
    }
    Ok(object(json!({
        "id": u.id.to_string(),
        "name": u.name,
        "parentId": u.parent.map(|p| p.to_string()),
        "members": members,
    })))
}

/// `OrgUnit/get`.
pub(crate) async fn get(
    ctx: &mut Ctx<'_>,
    args: Map<String, Value>,
) -> MethodResult<Map<String, Value>> {
    let a: GetArgs = parse_args(args)?;
    ctx.check_account(&a.account_id)?;
    ctx.check_get_limit(a.ids.as_ref())?;
    let store = ctx.jmap.org_store()?;
    let mut all = Vec::new();
    for u in store.org_units(ctx.scope.tenant).await? {
        all.push(unit_object(ctx, &u).await?);
    }
    let state = digest(json!(all).to_string().as_bytes());
    let (list, not_found) = match &a.ids {
        None => (all, Vec::new()),
        Some(ids) => {
            let mut list = Vec::new();
            let mut not_found = Vec::new();
            for id in ids {
                match all.iter().find(|u| u["id"] == id.as_str()) {
                    Some(u) => list.push(u.clone()),
                    None => not_found.push(id.clone()),
                }
            }
            (list, not_found)
        }
    };
    let list: Vec<Value> = list
        .into_iter()
        .map(|o| Value::Object(select(o, a.properties.as_deref())))
        .collect();
    Ok(object(json!({
        "accountId": a.account_id,
        "state": state,
        "list": list,
        "notFound": not_found,
    })))
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};
    use tp_contracts::addressbook::{OrgAttributes, OrgStore, OrgUnit};
    use tp_contracts::meta::AccountStore;
    use tp_contracts::{OrgUnitId, TenantId};

    use crate::testenv::Env;

    const URI: &str = "urn:thepost:orgchart";

    async fn call(env: &Env, args: Value) -> Value {
        let resp = env
            .request(json!({
                "using": ["urn:ietf:params:jmap:core", URI],
                "methodCalls": [["OrgUnit/get", args, "c0"]],
            }))
            .await;
        resp["methodResponses"][0][1].clone()
    }

    async fn unit(env: &Env, name: &str, parent: Option<OrgUnitId>) -> OrgUnitId {
        let id = OrgUnitId::new();
        env.org
            .put_org_unit(OrgUnit {
                id,
                tenant: env.scope.tenant,
                name: name.into(),
                parent,
                external_id: None,
            })
            .await
            .expect("unit");
        id
    }

    /// Дерево подразделений: `parentId`, члены в порядке старшинства с должностью и руководителем; отключённая
    /// учётная запись в состав не входит; изменение состава меняет `state`.
    #[tokio::test]
    async fn tree_with_members() {
        let env = Env::new().await;
        let acc = env.account();
        let tenant: TenantId = env.scope.tenant;
        let root = unit(&env, "Компания", None).await;
        let dev = unit(&env, "Разработка", Some(root)).await;
        let boss = env.user("boss@example.org").await.account;
        let junior = env.user("junior@example.org").await.account;
        let gone = env.user("gone@example.org").await.account;
        let org = |seniority: Option<i32>, title: &str, manager| OrgAttributes {
            department: Some(dev),
            title: Some(title.into()),
            manager,
            seniority,
            ..OrgAttributes::default()
        };
        for (who, attrs) in [
            (junior, org(None, "Стажёр", Some(boss))),
            (env.scope.account, org(Some(5), "Инженер", Some(boss))),
            (boss, org(Some(10), "Руководитель", None)),
            (gone, org(Some(20), "Уволен", None)),
        ] {
            env.org.set_org(tenant, who, attrs).await.expect("org");
        }
        let until = time::OffsetDateTime::now_utc() + time::Duration::days(30);
        env.meta
            .disable_account(tenant, gone, until)
            .await
            .expect("disable");

        let g = call(&env, json!({"accountId": acc, "ids": null})).await;
        let list = g["list"].as_array().expect("list");
        assert_eq!(list.len(), 2, "{g}");
        let by_name = |n: &str| {
            list.iter()
                .find(|u| u["name"] == n)
                .unwrap_or_else(|| panic!("нет {n}: {g}"))
        };
        assert_eq!(by_name("Компания")["parentId"], Value::Null);
        assert_eq!(by_name("Компания")["members"], json!([]));
        let d = by_name("Разработка");
        assert_eq!(d["id"], dev.to_string());
        assert_eq!(d["parentId"], root.to_string());
        assert_eq!(
            d["members"],
            json!([
                {"principalId": boss.to_string(), "name": "boss@example.org", "email": "boss@example.org",
                 "title": "Руководитель", "managerId": null, "seniority": 10},
                {"principalId": acc, "name": "user@example.org", "email": "user@example.org",
                 "title": "Инженер", "managerId": boss.to_string(), "seniority": 5},
                {"principalId": junior.to_string(), "name": "junior@example.org", "email": "junior@example.org",
                 "title": "Стажёр", "managerId": boss.to_string(), "seniority": null},
            ]),
            "{d}"
        );

        let one = call(
            &env,
            json!({"accountId": acc, "ids": [root.to_string(), "нет"], "properties": ["name"]}),
        )
        .await;
        assert_eq!(
            one["list"],
            json!([{"id": root.to_string(), "name": "Компания"}])
        );
        assert_eq!(one["notFound"], json!(["нет"]));
        assert_eq!(one["state"], g["state"]);

        env.org
            .set_org(tenant, junior, OrgAttributes::default())
            .await
            .expect("org");
        let after = call(&env, json!({"accountId": acc})).await;
        assert_ne!(after["state"], g["state"]);
    }

    /// Capability объявлена при подключённой оргструктуре; без неё — нет; чужая учётная запись — `accountNotFound`.
    #[tokio::test]
    async fn capability() {
        let env = Env::new().await;
        let session = env.jmap.session(&env.principal, "https://mail.example.org");
        assert!(session["capabilities"][URI].is_object(), "{session}");
        assert_eq!(session["primaryAccounts"][URI], env.account());
        let g = call(&env, json!({"accountId": "чужая"})).await;
        assert_eq!(g["type"], "accountNotFound");
        let g = call(&env, json!({"accountId": env.account()})).await;
        assert_eq!(g["list"], json!([]));

        let bare = Env::bare().await;
        let session = bare
            .jmap
            .session(&bare.principal, "https://mail.example.org");
        assert!(session["capabilities"][URI].is_null());
    }
}
