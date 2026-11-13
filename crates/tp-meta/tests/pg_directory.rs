//! META-65: каталог `Directory` поверх учётных записей, групп и оргструктуры PostgreSQL — книга «Организация» узла
//! без LDAP. Учётная запись, заведённая как в Admin API (`AccountStore::upsert`), видна в `accounts_since`; группа,
//! скрытая из GAL, — нет; курсор неизменного каталога даёт пустую порцию, изменённого — `Conflict`.

mod common;

use std::collections::BTreeMap;

use tp_contracts::Error;
use tp_contracts::addressbook::{OrgAttributes, OrgStore, OrgUnit};
use tp_contracts::auth::{DirAccount, DirChange, DirCursor, DirGroup, DirPage, Directory};
use tp_contracts::groups::{DistributionGroup, GroupMember, GroupStore, SenderRestriction};
use tp_contracts::meta::{AccountKind, AccountStore, AccountUpsert};
use tp_contracts::{AccountId, DomainId, GroupId, OrgUnitId, TenantId};
use tp_meta::PgMeta;

async fn create(
    meta: &PgMeta,
    tenant: TenantId,
    domain: DomainId,
    login: &str,
    aliases: &[&str],
) -> AccountId {
    meta.upsert(
        tenant,
        AccountUpsert {
            domain,
            login: login.to_owned(),
            display_name: format!("Пользователь {login}"),
            external_id: None,
            enabled: true,
            aliases: aliases.iter().map(|a| (*a).to_owned()).collect(),
            kind: AccountKind::User,
        },
    )
    .await
    .expect("upsert")
    .id
}

fn group(
    tenant: TenantId,
    name: &str,
    address: &str,
    members: Vec<GroupMember>,
    hidden: bool,
) -> DistributionGroup {
    DistributionGroup {
        id: GroupId::new(),
        tenant,
        name: name.to_owned(),
        address: Some(address.to_owned()),
        external_id: None,
        members,
        senders: SenderRestriction::Anyone,
        moderators: Vec::new(),
        hidden_from_gal: hidden,
    }
}

fn upserts<T>(page: DirPage<T>) -> Vec<T> {
    page.changes
        .into_iter()
        .map(|c| match c {
            DirChange::Upsert(x) => x,
            DirChange::Removed { .. } => panic!("полная выгрузка без удалений"),
        })
        .collect()
}

/// Полная выгрузка учётных записей порциями по `limit`: записи и курсор после неё.
async fn all_accounts(
    meta: &PgMeta,
    tenant: TenantId,
    limit: usize,
) -> (Vec<DirAccount>, DirCursor) {
    let mut out = Vec::new();
    let mut cursor: Option<DirCursor> = None;
    loop {
        let page = meta
            .accounts_since(tenant, cursor.as_ref(), limit)
            .await
            .expect("accounts_since");
        assert!(page.changes.len() <= limit, "порция больше limit");
        let more = page.has_more;
        cursor = Some(page.next.clone());
        out.extend(upserts(page));
        if !more {
            return (out, cursor.expect("курсор"));
        }
    }
}

async fn all_groups(meta: &PgMeta, tenant: TenantId) -> (Vec<DirGroup>, DirCursor) {
    let page = meta
        .groups_since(tenant, None, 100)
        .await
        .expect("groups_since");
    assert!(!page.has_more);
    let next = page.next.clone();
    (upserts(page), next)
}

#[tokio::test]
async fn accounts_groups_and_cursors() {
    let Some(meta) = common::meta().await else {
        return;
    };
    let tenant = common::tenant(meta.pool()).await;
    let foreign = common::tenant(meta.pool()).await;
    let (domain, name) = common::domain(meta.pool(), tenant).await;
    let (foreign_domain, foreign_name) = common::domain(meta.pool(), foreign).await;
    create(
        &meta,
        foreign,
        foreign_domain,
        &format!("stranger@{foreign_name}"),
        &[],
    )
    .await;

    // Пустой каталог: пустая порция, курсор неизменного каталога — снова пустая порция.
    let (none, empty) = all_accounts(&meta, tenant, 10).await;
    assert!(none.is_empty());
    let again = meta
        .accounts_since(tenant, Some(&empty), 10)
        .await
        .expect("accounts_since");
    assert!(again.changes.is_empty() && !again.has_more);

    let ivan = create(
        &meta,
        tenant,
        domain,
        &format!("Ivan@{name}"),
        &[&format!("i.petrov@{name}")],
    )
    .await;
    let maria = create(&meta, tenant, domain, &format!("maria@{name}"), &[]).await;
    let oleg = create(&meta, tenant, domain, &format!("oleg@{name}"), &[]).await;

    // Новая учётная запись меняет версию каталога: прежний курсор — полная выгрузка.
    assert!(matches!(
        meta.accounts_since(tenant, Some(&empty), 10).await,
        Err(Error::Conflict)
    ));

    let unit = OrgUnitId::new();
    meta.put_org_unit(OrgUnit {
        id: unit,
        tenant,
        name: "Бухгалтерия".into(),
        parent: None,
        external_id: None,
    })
    .await
    .expect("put_org_unit");
    meta.set_org(
        tenant,
        maria,
        OrgAttributes {
            department: Some(unit),
            title: Some("Главный бухгалтер".into()),
            manager: Some(ivan),
            seniority: Some(5),
            attributes: BTreeMap::from([(
                "telephoneNumber".to_owned(),
                "+7 495 000-00-00".to_owned(),
            )]),
        },
    )
    .await
    .expect("set_org");

    // Порции по одной: каждая учётная запись ровно один раз, по возрастанию идентификатора; чужой организации нет.
    let (accounts, cursor) = all_accounts(&meta, tenant, 1).await;
    let mut ids = [ivan, maria, oleg];
    ids.sort_by_key(|a| a.to_string());
    assert_eq!(
        accounts
            .iter()
            .map(|a| a.external_id.clone())
            .collect::<Vec<_>>(),
        ids.iter().map(ToString::to_string).collect::<Vec<_>>()
    );
    let by_id = |id: AccountId| {
        accounts
            .iter()
            .find(|a| a.external_id == id.to_string())
            .expect("запись")
    };
    let ivan_dir = by_id(ivan);
    assert_eq!(
        ivan_dir.emails,
        vec![format!("ivan@{name}"), format!("i.petrov@{name}")],
        "основной адрес первым, затем псевдонимы"
    );
    assert!(ivan_dir.enabled && !ivan_dir.hidden_from_gal);
    let maria_dir = by_id(maria);
    assert_eq!(maria_dir.org.department, Some(unit.to_string()));
    assert_eq!(maria_dir.org.manager, Some(ivan.to_string()));
    assert_eq!(maria_dir.org.title.as_deref(), Some("Главный бухгалтер"));
    assert_eq!(maria_dir.org.seniority, Some(5));
    assert_eq!(maria_dir.attributes["telephoneNumber"], "+7 495 000-00-00");

    // Неизменный каталог: пустая порция с прежним курсором.
    let same = meta
        .accounts_since(tenant, Some(&cursor), 1)
        .await
        .expect("accounts_since");
    assert!(same.changes.is_empty() && !same.has_more);
    assert_eq!(same.next, cursor);

    // Изменение оргатрибутов — тоже новая версия; курсор чужого каталога — полная выгрузка.
    meta.set_org(
        tenant,
        oleg,
        OrgAttributes {
            title: Some("Инженер".into()),
            ..Default::default()
        },
    )
    .await
    .expect("set_org");
    assert!(matches!(
        meta.accounts_since(tenant, Some(&cursor), 1).await,
        Err(Error::Conflict)
    ));
    assert!(matches!(
        meta.accounts_since(tenant, Some(&DirCursor("ldap-cookie".into())), 1)
            .await,
        Err(Error::Conflict)
    ));

    // Группы: скрытая из GAL в каталог не попадает; участники — учётные записи и вложенные группы.
    let staff = group(
        tenant,
        "Сотрудники",
        &format!("staff@{name}"),
        vec![GroupMember::Account(ivan), GroupMember::Account(maria)],
        false,
    );
    let board = group(
        tenant,
        "Правление",
        &format!("board@{name}"),
        vec![GroupMember::Account(ivan)],
        true,
    );
    let all = group(
        tenant,
        "Все",
        &format!("all@{name}"),
        vec![GroupMember::Group(staff.id), GroupMember::Account(oleg)],
        false,
    );
    for g in [&staff, &board, &all] {
        meta.put(g.clone()).await.expect("put");
    }
    let (groups, gcursor) = all_groups(&meta, tenant).await;
    let mut names: Vec<&str> = groups.iter().map(|g| g.name.as_str()).collect();
    names.sort_unstable();
    assert_eq!(names, ["Все", "Сотрудники"]);
    let all_dir = groups
        .iter()
        .find(|g| g.external_id == all.id.to_string())
        .expect("группа");
    assert_eq!(
        all_dir.email.as_deref(),
        Some(format!("all@{name}").as_str())
    );
    assert_eq!(
        all_dir.members,
        vec![staff.id.to_string(), oleg.to_string()]
    );

    // Удаление группы — новая версия; после полной выгрузки её нет.
    meta.remove(tenant, staff.id).await.expect("remove");
    assert!(matches!(
        meta.groups_since(tenant, Some(&gcursor), 100).await,
        Err(Error::Conflict)
    ));
    let (groups, _) = all_groups(&meta, tenant).await;
    assert_eq!(
        groups.iter().map(|g| g.name.as_str()).collect::<Vec<_>>(),
        ["Все"]
    );

    // Псевдонимы, домены, атрибуты, подразделения.
    let aliases = meta.aliases(tenant).await.expect("aliases");
    assert_eq!(aliases.len(), 1);
    assert_eq!(
        (aliases[0].address.as_str(), aliases[0].target.as_str()),
        (
            format!("i.petrov@{name}").as_str(),
            ivan.to_string().as_str()
        )
    );
    let domains = meta.domains(tenant).await.expect("domains");
    assert_eq!(domains.len(), 1);
    assert_eq!(domains[0].name, name.to_lowercase());
    assert!(domains[0].default);
    assert_eq!(
        meta.gal_attributes(tenant).await.expect("gal_attributes"),
        ["telephoneNumber"]
    );
    let units = meta.units(tenant).await.expect("units");
    assert_eq!(units.len(), 1);
    assert_eq!(
        (units[0].external_id.as_str(), units[0].name.as_str()),
        (unit.to_string().as_str(), "Бухгалтерия")
    );
}
