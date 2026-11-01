//! Тесты каталога LDAP: фикстуры LDIF на сервере в памяти, интеграционный — при `TEST_LDAP_URL`.

use std::path::PathBuf;
use std::sync::Mutex;

use base64::Engine;

use super::*;

// ---------- Сервер в памяти по LDIF ----------

/// Записи LDIF: `attr: значение`, `attr:: base64`, продолжение строки пробелом; `userPassword` —
/// пароль для привязки (в атрибуты не попадает).
fn parse_ldif(text: &str) -> (Vec<SearchEntry>, HashMap<String, String>) {
    let mut entries = Vec::new();
    let mut passwords = HashMap::new();
    let mut lines: Vec<String> = Vec::new();
    for raw in text.lines() {
        if let Some(cont) = raw.strip_prefix(' ') {
            if let Some(last) = lines.last_mut() {
                last.push_str(cont);
            }
        } else {
            lines.push(raw.to_owned());
        }
    }
    let mut cur: Option<SearchEntry> = None;
    for line in lines.iter().chain(std::iter::once(&String::new())) {
        if line.starts_with('#') {
            continue;
        }
        if line.trim().is_empty() {
            entries.extend(cur.take());
            continue;
        }
        let (attr, value) = line.split_once(':').expect("ldif line");
        let bytes = match value.strip_prefix(':') {
            Some(b64) => base64::engine::general_purpose::STANDARD
                .decode(b64.trim())
                .expect("base64"),
            None => value.trim_start().as_bytes().to_vec(),
        };
        if attr == "dn" {
            cur = Some(SearchEntry {
                dn: String::from_utf8(bytes).expect("dn"),
                attrs: HashMap::new(),
                bin_attrs: HashMap::new(),
            });
            continue;
        }
        let e = cur.as_mut().expect("dn first");
        if attr == "userPassword" {
            passwords.insert(
                e.dn.to_ascii_lowercase(),
                String::from_utf8(bytes).expect("pw"),
            );
            continue;
        }
        match String::from_utf8(bytes) {
            Ok(s) => e.attrs.entry(attr.to_owned()).or_default().push(s),
            Err(b) => e
                .bin_attrs
                .entry(attr.to_owned())
                .or_default()
                .push(b.into_bytes()),
        }
    }
    (entries, passwords)
}

/// Фильтр LDAP (RFC 4515) в объёме запросов каталога: `&`, `|`, `!`, `=`, `=*`, `>=`.
#[derive(Debug)]
enum Filter {
    And(Vec<Filter>),
    Or(Vec<Filter>),
    Not(Box<Filter>),
    Present(String),
    Eq(String, String),
    Ge(String, String),
}

fn parse_filter(s: &str) -> (Filter, &str) {
    let s = s.strip_prefix('(').expect("(");
    if let Some(rest) = s.strip_prefix('&') {
        let (items, rest) = parse_list(rest);
        return (Filter::And(items), rest);
    }
    if let Some(rest) = s.strip_prefix('|') {
        let (items, rest) = parse_list(rest);
        return (Filter::Or(items), rest);
    }
    if let Some(rest) = s.strip_prefix('!') {
        let (f, rest) = parse_filter(rest);
        return (Filter::Not(Box::new(f)), &rest[1..]);
    }
    let end = s.find(')').expect(")");
    let (item, rest) = (&s[..end], &s[end + 1..]);
    let unescape = |v: &str| ldap3::ldap_unescape(v).expect("escape").into_owned();
    let f = if let Some((a, v)) = item.split_once(">=") {
        Filter::Ge(a.to_owned(), unescape(v))
    } else {
        let (a, v) = item.split_once('=').expect("=");
        if v == "*" {
            Filter::Present(a.to_owned())
        } else {
            Filter::Eq(a.to_owned(), unescape(v))
        }
    };
    (f, rest)
}

fn parse_list(mut rest: &str) -> (Vec<Filter>, &str) {
    let mut items = Vec::new();
    while !rest.starts_with(')') {
        let (f, r) = parse_filter(rest);
        items.push(f);
        rest = r;
    }
    (items, &rest[1..])
}

fn eval(f: &Filter, e: &SearchEntry) -> bool {
    match f {
        Filter::And(v) => v.iter().all(|f| eval(f, e)),
        Filter::Or(v) => v.iter().any(|f| eval(f, e)),
        Filter::Not(f) => !eval(f, e),
        Filter::Present(a) => {
            a.eq_ignore_ascii_case("objectClass")
                || values(e, a).next().is_some()
                || e.bin_attrs.keys().any(|k| k.eq_ignore_ascii_case(a))
        }
        Filter::Eq(a, want) => values(e, a).any(|v| v.eq_ignore_ascii_case(want)),
        Filter::Ge(a, want) => {
            values(e, a).any(|v| match (v.parse::<u64>(), want.parse::<u64>()) {
                (Ok(x), Ok(y)) => x >= y,
                _ => v >= want.as_str(),
            })
        }
    }
}

/// Сервер каталога в памяти. Выдаёт только запрошенные атрибуты; надгробия (`isDeleted`) — только с
/// Show Deleted, как Active Directory.
struct Fake {
    entries: Mutex<Vec<SearchEntry>>,
    passwords: HashMap<String, String>,
    queries: Mutex<Vec<Query>>,
}

impl Fake {
    fn new(ldif: &str) -> Arc<Self> {
        let (entries, passwords) = parse_ldif(ldif);
        Arc::new(Fake {
            entries: Mutex::new(entries),
            passwords,
            queries: Mutex::new(Vec::new()),
        })
    }

    fn set(&self, dn: &str, attr: &str, value: &str) {
        let mut entries = self.entries.lock().expect("lock");
        let e = entries
            .iter_mut()
            .find(|e| e.dn.eq_ignore_ascii_case(dn))
            .expect("entry");
        e.attrs.insert(attr.to_owned(), vec![value.to_owned()]);
    }
}

#[async_trait]
impl Source for Fake {
    async fn search(&self, q: &Query) -> Result<Vec<SearchEntry>> {
        self.queries.lock().expect("lock").push(q.clone());
        let (filter, rest) = parse_filter(&q.filter);
        assert!(rest.is_empty(), "filter tail: {rest}");
        let base = q.base.to_ascii_lowercase();
        let wanted = |k: &String| q.attrs.iter().any(|a| a.eq_ignore_ascii_case(k));
        Ok(self
            .entries
            .lock()
            .expect("lock")
            .iter()
            .filter(|e| {
                let dn = e.dn.to_ascii_lowercase();
                if q.base_only {
                    dn == base
                } else {
                    !dn.is_empty() && dn.ends_with(&base)
                }
            })
            .filter(|e| q.show_deleted || !values(e, "isDeleted").any(|v| v == "TRUE"))
            .filter(|e| eval(&filter, e))
            .map(|e| SearchEntry {
                dn: e.dn.clone(),
                attrs: e
                    .attrs
                    .iter()
                    .filter(|(k, _)| wanted(k))
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect(),
                bin_attrs: e
                    .bin_attrs
                    .iter()
                    .filter(|(k, _)| wanted(k))
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect(),
            })
            .collect())
    }

    async fn bind(&self, dn: &str, password: &str) -> Result<bool> {
        Ok(!password.is_empty()
            && self
                .passwords
                .get(&dn.to_ascii_lowercase())
                .map(String::as_str)
                == Some(password))
    }
}

fn openldap_cfg() -> LdapConfig {
    LdapConfig {
        base_dn: "dc=example,dc=org".into(),
        accounts: AccountMap {
            disabled: Some(DisabledRule {
                attribute: "nsAccountLock".into(),
                value: Some("true".into()),
                flag: None,
            }),
            gal: BTreeMap::from([("title".into(), "title".into())]),
            ..AccountMap::default()
        },
        aliases: Some(AliasMap {
            base: Some("ou=aliases,dc=example,dc=org".into()),
            filter: "(objectClass=mailAlias)".into(),
            address: "mail".into(),
            target: "aliasedObjectName".into(),
        }),
        domains: vec![DomainEntry {
            name: "Example.ORG".into(),
            default: true,
        }],
        ..LdapConfig::default()
    }
}

fn openldap() -> (LdapDirectory, Arc<Fake>) {
    let fake = Fake::new(include_str!("openldap.ldif"));
    let dir = LdapDirectory::with_source(TenantId::DEFAULT, openldap_cfg(), fake.clone());
    (dir, fake)
}

fn upserts<T: Clone>(page: &DirPage<T>) -> Vec<T> {
    page.changes
        .iter()
        .filter_map(|c| match c {
            DirChange::Upsert(v) => Some(v.clone()),
            DirChange::Removed { .. } => None,
        })
        .collect()
}

fn logins(page: &DirPage<DirAccount>) -> Vec<String> {
    upserts(page).into_iter().map(|a| a.login).collect()
}

const T: TenantId = TenantId::DEFAULT;
const ALICE: &str = "00000000-0000-0000-0000-0000000000a1";
const CAROL: &str = "00000000-0000-0000-0000-0000000000c1";

#[tokio::test]
async fn accounts_full_then_incremental() {
    let (dir, fake) = openldap();
    // Полная выгрузка порциями: порядок — по (modifyTimestamp, id), у alice и bob метка совпадает.
    let p1 = dir.accounts_since(T, None, 2).await.expect("p1");
    assert_eq!(logins(&p1), ["alice@example.org", "bob@example.org"]);
    assert!(p1.has_more);
    let p2 = dir.accounts_since(T, Some(&p1.next), 2).await.expect("p2");
    assert_eq!(logins(&p2), ["carol@example.org"]);
    assert!(!p2.has_more);
    let p3 = dir.accounts_since(T, Some(&p2.next), 2).await.expect("p3");
    assert!(p3.changes.is_empty() && !p3.has_more);
    assert_eq!(p3.next, p2.next);
    // Запрос с курсором выбирает только изменённые после метки.
    let q = fake
        .queries
        .lock()
        .expect("lock")
        .last()
        .cloned()
        .expect("query");
    assert!(
        q.filter.contains("(modifyTimestamp>=20261002000000Z)"),
        "{}",
        q.filter
    );

    // Изменение записи — в следующей порции.
    fake.set(
        "uid=alice,ou=people,dc=example,dc=org",
        "modifyTimestamp",
        "20261003000000Z",
    );
    fake.set("uid=alice,ou=people,dc=example,dc=org", "title", "Lead");
    let p4 = dir.accounts_since(T, Some(&p3.next), 10).await.expect("p4");
    let changed = upserts(&p4);
    assert_eq!(changed.len(), 1);
    assert_eq!(
        changed[0].attributes.get("title").map(String::as_str),
        Some("Lead")
    );
}

#[tokio::test]
async fn account_mapping() {
    let (dir, _) = openldap();
    let page = dir.accounts_since(T, None, 100).await.expect("page");
    let all = upserts(&page);
    let alice = &all[0];
    assert_eq!(alice.external_id, ALICE);
    assert_eq!(alice.display_name, "Alice Liddell");
    assert_eq!(alice.emails, ["alice@example.org", "A.Liddell@example.org"]);
    assert!(alice.enabled);
    assert_eq!(
        alice.attributes,
        BTreeMap::from([("title".to_owned(), "Engineer".to_owned())])
    );
    assert!(!all[1].enabled, "nsAccountLock: TRUE");
    assert_eq!(all[2].display_name, "Carol Кэрролл");
    assert_eq!(all.len(), 3, "запись без mail — вне фильтра");
}

#[tokio::test]
async fn groups_and_members() {
    let (dir, _) = openldap();
    let page = dir.groups_since(T, None, 100).await.expect("groups");
    let groups = upserts(&page);
    let names: Vec<_> = groups.iter().map(|g| g.name.as_str()).collect();
    assert_eq!(names, ["staff", "devs"]);
    // DN членов — в идентификаторы; неразрешённый DN (uid=ghost) пропущен, регистр DN не важен.
    assert_eq!(
        groups[0].members,
        [
            ALICE.to_owned(),
            "00000000-0000-0000-0000-0000000000e2".to_owned()
        ]
    );
    assert_eq!(groups[0].email.as_deref(), Some("staff@example.org"));
    assert_eq!(groups[1].members, [CAROL.to_owned(), ALICE.to_owned()]);
    assert_eq!(groups[1].email, None);
}

#[tokio::test]
async fn nested_groups_of_login() {
    let (dir, _) = openldap();
    let carol = dir.groups_of("carol@example.org").await.expect("carol");
    assert_eq!(carol, BTreeSet::from(["devs".into(), "staff".into()]));
    let alice = dir.groups_of("ALICE@example.org").await.expect("alice");
    assert_eq!(alice, BTreeSet::from(["devs".into(), "staff".into()]));
    assert!(
        dir.groups_of("nobody@example.org")
            .await
            .expect("none")
            .is_empty()
    );
    let shallow = LdapDirectory::with_source(
        T,
        LdapConfig {
            max_group_depth: 1,
            ..openldap_cfg()
        },
        Fake::new(include_str!("openldap.ldif")),
    );
    assert_eq!(
        shallow
            .groups_of("carol@example.org")
            .await
            .expect("depth 1"),
        BTreeSet::from(["devs".into()])
    );
}

#[tokio::test]
async fn aliases_domains_gal() {
    let (dir, _) = openldap();
    let aliases = dir.aliases(T).await.expect("aliases");
    assert_eq!(
        aliases,
        [
            DirAlias {
                address: "postmaster@example.org".into(),
                target: ALICE.into()
            },
            DirAlias {
                address: "abuse@example.org".into(),
                target: ALICE.into()
            },
        ]
    );
    assert_eq!(
        dir.domains(T).await.expect("domains"),
        [DirDomain {
            name: "example.org".into(),
            default: true
        }]
    );
    assert_eq!(dir.gal_attributes(T).await.expect("gal"), ["title"]);
    let other = TenantId::new();
    assert_eq!(dir.aliases(other).await, Err(Error::NotFound));
    assert_eq!(
        dir.accounts_since(other, None, 1).await.map(|_| ()),
        Err(Error::NotFound)
    );
}

#[tokio::test]
async fn password_bind() {
    let (dir, _) = openldap();
    assert_eq!(
        dir.check_password("alice@example.org", "wonderland").await,
        Ok(true)
    );
    assert_eq!(
        dir.check_password("alice@example.org", "wrong").await,
        Ok(false)
    );
    assert_eq!(dir.check_password("alice@example.org", "").await, Ok(false));
    assert_eq!(
        dir.check_password("nobody@example.org", "x").await,
        Ok(false)
    );
    // Значение имени входа экранируется в фильтре (RFC 4515).
    assert_eq!(dir.check_password("*", "wonderland").await, Ok(false));
}

#[tokio::test]
async fn bad_cursor_is_conflict() {
    let (dir, _) = openldap();
    let junk = DirCursor("not a cursor".into());
    assert_eq!(
        dir.accounts_since(T, Some(&junk), 1).await.map(|_| ()),
        Err(Error::Conflict)
    );
    let usn = Mark::Usn {
        usn: 1,
        id: String::new(),
        server: String::new(),
    }
    .encode();
    assert_eq!(
        dir.accounts_since(T, Some(&usn), 1).await.map(|_| ()),
        Err(Error::Conflict)
    );
    assert!(dir.accounts_since(T, None, 0).await.is_err());
}

fn ad_cfg() -> LdapConfig {
    LdapConfig {
        base_dn: "DC=corp,DC=example".into(),
        sync: SyncMode::Usn,
        accounts: AccountMap {
            filter: "(&(objectClass=user)(mail=*))".into(),
            id: "objectGUID".into(),
            login: "userPrincipalName".into(),
            display_name: "displayName".into(),
            emails: vec!["mail".into(), "proxyAddresses".into()],
            disabled: Some(DisabledRule {
                attribute: "userAccountControl".into(),
                value: None,
                flag: Some(2),
            }),
            ..AccountMap::default()
        },
        groups: GroupMap {
            filter: "(objectClass=group)".into(),
            id: "objectGUID".into(),
            ..GroupMap::default()
        },
        ..LdapConfig::default()
    }
}

#[tokio::test]
async fn active_directory_usn() {
    let fake = Fake::new(include_str!("ad.ldif"));
    let dir = LdapDirectory::with_source(T, ad_cfg(), fake.clone());
    let page = dir.accounts_since(T, None, 10).await.expect("ad");
    // По uSNChanged: dave (110), alice (120), удалённый bob (130).
    let dave = "ffffffff-ffff-ffff-ffff-ffffffffffff";
    let alice = "03020100-0504-0706-0809-0a0b0c0d0e0f";
    let bob = "13121110-1514-1716-1819-1a1b1c1d1e1f";
    match page.changes.as_slice() {
        [
            DirChange::Upsert(d),
            DirChange::Upsert(a),
            DirChange::Removed { external_id },
        ] => {
            assert_eq!(d.external_id, dave);
            assert!(!d.enabled, "userAccountControl 514 — ACCOUNTDISABLE");
            assert_eq!(d.display_name, "dave@corp.example");
            assert_eq!(a.external_id, alice);
            assert!(a.enabled);
            assert_eq!(a.emails, ["alice@corp.example", "al@corp.example"]);
            assert_eq!(external_id, bob);
        }
        other => panic!("unexpected changes: {other:?}"),
    }
    // Надгробия ищутся с Show Deleted от корня домена.
    let q = fake.queries.lock().expect("lock").clone();
    assert!(
        q.iter()
            .any(|q| q.show_deleted && q.filter.starts_with("(&(isDeleted=TRUE)"))
    );
    let again = dir
        .accounts_since(T, Some(&page.next), 10)
        .await
        .expect("again");
    assert!(again.changes.is_empty());
    let q = fake
        .queries
        .lock()
        .expect("lock")
        .last()
        .cloned()
        .expect("q");
    assert!(q.filter.contains("(uSNChanged>=130)"), "{}", q.filter);

    // Группы: члены — GUID учётных записей.
    let groups = upserts(&dir.groups_since(T, None, 10).await.expect("groups"));
    assert_eq!(groups[0].name, "Staff");
    assert_eq!(groups[0].members, [alice.to_owned()]);

    // Курсор другого контроллера домена — устарел.
    fake.set("", "dsServiceName", "CN=NTDS Settings,CN=DC2");
    assert_eq!(
        dir.accounts_since(T, Some(&page.next), 10)
            .await
            .map(|_| ()),
        Err(Error::Conflict)
    );
}

#[test]
fn config_check_and_parse() {
    let cfg = openldap_cfg();
    assert_eq!(cfg.check(Profile::Prod), Ok(false));
    let plain = LdapConfig {
        url: "ldap://dc".into(),
        ..openldap_cfg()
    };
    assert!(plain.check(Profile::Prod).is_err());
    let starttls = LdapConfig {
        starttls: true,
        ..plain.clone()
    };
    assert_eq!(starttls.check(Profile::Prod), Ok(false));
    let no_secret = LdapConfig {
        bind_dn: "cn=svc".into(),
        ..openldap_cfg()
    };
    assert!(no_secret.check(Profile::Prod).is_err());

    let yaml = r#"{"url": "ldaps://dc:636", "base_dn": "dc=x", "bind_dn": "cn=svc",
        "bind_password": {"secret": "thepost/ldap#password"}, "timeout": "10s", "sync": "usn"}"#;
    let cfg: LdapConfig = serde_json::from_str(yaml).expect("parse");
    assert_eq!(cfg.timeout, Duration::from_secs(10));
    assert_eq!(cfg.sync, SyncMode::Usn);
    assert_eq!(cfg.check(Profile::Prod), Ok(false));
    // Пароль открытым текстом — ошибка разбора.
    let bad = r#"{"base_dn": "dc=x", "bind_dn": "cn=svc", "bind_password": "hunter2"}"#;
    assert!(serde_json::from_str::<LdapConfig>(bad).is_err());
}

#[test]
fn guid_format() {
    let b: Vec<u8> = (0..16).collect();
    assert_eq!(
        guid(&b).as_deref(),
        Some("03020100-0504-0706-0809-0a0b0c0d0e0f")
    );
    assert_eq!(guid(&b[..4]), None);
}

// ---------- Интеграционный: живой сервер ----------

/// Сервер LDAP по `TEST_LDAP_URL` (например, OpenLDAP в контейнере): `TEST_LDAP_BIND_DN`, `TEST_LDAP_PASSWORD` —
/// учётная запись с правом записи, `TEST_LDAP_BASE` — корень; `TEST_LDAP_CA` — CA для `ldaps://`.
/// Без `TEST_LDAP_URL` тест пропускается.
#[tokio::test]
async fn live_ldap() {
    let Ok(url) = std::env::var("TEST_LDAP_URL") else {
        eprintln!("TEST_LDAP_URL не задан — пропуск");
        return;
    };
    let env = |k: &str| std::env::var(k).unwrap_or_else(|_| panic!("{k}"));
    let (bind_dn, password, base) = (
        env("TEST_LDAP_BIND_DN"),
        env("TEST_LDAP_PASSWORD"),
        env("TEST_LDAP_BASE"),
    );
    let ca = std::env::var("TEST_LDAP_CA").ok().map(PathBuf::from);
    let plaintext = url.starts_with("ldap://");
    let tls = InternalTls {
        ca_file: ca,
        allow_plaintext: plaintext,
        ..InternalTls::default()
    };
    let profile = if plaintext {
        Profile::Dev
    } else {
        Profile::Prod
    };

    // Подготовка: своё поддерево с двумя учётными записями и группой.
    let ou = format!("ou=tp{},{base}", std::process::id());
    let admin = Ldap3 {
        url: url.clone(),
        starttls: false,
        bind_dn: bind_dn.clone(),
        bind_password: Some(Secret::new(password.clone())),
        tls: if plaintext {
            None
        } else {
            let t = tls.clone();
            Some(Arc::new(
                tokio::task::spawn_blocking(move || client_tls(&t))
                    .await
                    .expect("join")
                    .expect("tls"),
            ))
        },
        page_size: 2,
        connect_timeout: Duration::from_secs(5),
        timeout: Duration::from_secs(10),
    };
    let mut ldap = admin.service().await.expect("admin bind");
    let add = |dn: String, attrs: Vec<(&'static str, Vec<String>)>| (dn, attrs);
    let people = [("ann", "Ann One"), ("ben", "Ben Two"), ("cid", "Cid Three")];
    let mut entries = vec![add(
        ou.clone(),
        vec![
            ("objectClass", vec!["organizationalUnit".into()]),
            ("ou", vec![ou[3..ou.find(',').expect(",")].to_owned()]),
        ],
    )];
    for (uid, cn) in people {
        entries.push(add(
            format!("uid={uid},{ou}"),
            vec![
                ("objectClass", vec!["inetOrgPerson".into()]),
                ("uid", vec![uid.into()]),
                ("cn", vec![cn.into()]),
                ("sn", vec![cn.into()]),
                ("mail", vec![format!("{uid}@example.org")]),
                ("userPassword", vec![format!("pw-{uid}")]),
            ],
        ));
    }
    entries.push(add(
        format!("cn=team,{ou}"),
        vec![
            ("objectClass", vec!["groupOfNames".into()]),
            ("cn", vec!["team".into()]),
            (
                "member",
                vec![format!("uid=ann,{ou}"), format!("uid=ben,{ou}")],
            ),
        ],
    ));
    for (dn, attrs) in &entries {
        let attrs: Vec<(&str, HashSet<&str>)> = attrs
            .iter()
            .map(|(k, v)| (*k, v.iter().map(String::as_str).collect()))
            .collect();
        ldap.add(dn, attrs)
            .await
            .expect("add")
            .success()
            .expect("add rc");
    }

    let secrets = tp_testkit::MemSecretStore::new(Arc::new(tp_testkit::SystemClock));
    let secret = SecretRef::new("thepost/ldap", "password").expect("ref");
    secrets.set(&secret, password);
    let cfg = LdapConfig {
        url,
        bind_dn,
        bind_password: Some(secret),
        tls,
        base_dn: ou.clone(),
        page_size: 2,
        ..LdapConfig::default()
    };
    let result = async {
        let dir = LdapDirectory::connect(T, cfg, profile, &secrets).await?;
        let full = dir.accounts_since(T, None, 100).await?;
        let mut got = logins(&full);
        got.sort();
        assert_eq!(
            got,
            ["ann@example.org", "ben@example.org", "cid@example.org"]
        );
        let again = dir.accounts_since(T, Some(&full.next), 100).await?;
        assert!(again.changes.is_empty(), "{:?}", again.changes);
        let groups = upserts(&dir.groups_since(T, None, 100).await?);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].members.len(), 2);
        assert_eq!(
            dir.groups_of("ann@example.org").await?,
            BTreeSet::from(["team".to_owned()])
        );
        assert!(dir.check_password("ann@example.org", "pw-ann").await?);
        assert!(!dir.check_password("ann@example.org", "nope").await?);
        // Изменение после курсора (точность modifyTimestamp — секунда).
        tokio::time::sleep(Duration::from_millis(1100)).await;
        let mut admin = admin.service().await?;
        admin
            .modify(
                &format!("uid=cid,{ou}"),
                vec![ldap3::Mod::Replace("cn", HashSet::from(["Cid Changed"]))],
            )
            .await
            .map_err(unavailable)?;
        let changed = dir.accounts_since(T, Some(&again.next), 100).await?;
        assert_eq!(logins(&changed), ["cid@example.org"]);
        Ok::<(), Error>(())
    }
    .await;

    for (dn, _) in entries.iter().rev() {
        let _ = ldap.delete(dn).await;
    }
    let _ = ldap.unbind().await;
    result.expect("live LDAP");
}

// ---------- Группы рассылки из каталога (AUTH-07) ----------

mod group_sync {
    use tp_contracts::groups::{
        DistributionGroup, GroupMember, GroupStore, HeldMessage, SenderRestriction,
    };
    use tp_contracts::{AccountId, BlobHash, BlobKey, Gen, HeldId};
    use tp_testkit::{Clock, MemGroups, SystemClock};

    use super::*;

    const STAFF: &str = "00000000-0000-0000-0000-0000000000e1";
    const DEVS: &str = "00000000-0000-0000-0000-0000000000e2";
    const STAFF_DN: &str = "cn=staff,ou=groups,dc=example,dc=org";
    const DEVS_DN: &str = "cn=devs,ou=groups,dc=example,dc=org";
    const ALICE_DN: &str = "uid=alice,ou=people,dc=example,dc=org";
    const CAROL_DN: &str = "uid=carol,ou=people,dc=example,dc=org";

    impl Fake {
        fn set_all(&self, dn: &str, attr: &str, vals: &[&str]) {
            let mut entries = self.entries.lock().expect("lock");
            let e = entries
                .iter_mut()
                .find(|e| e.dn.eq_ignore_ascii_case(dn))
                .expect("entry");
            e.attrs.insert(
                attr.to_owned(),
                vals.iter().map(|v| (*v).to_owned()).collect(),
            );
        }

        fn delete(&self, dn: &str) {
            self.entries
                .lock()
                .expect("lock")
                .retain(|e| !e.dn.eq_ignore_ascii_case(dn));
        }
    }

    struct Env {
        sync: GroupSync,
        fake: Arc<Fake>,
        groups: MemGroups,
        alice: AccountId,
        carol: AccountId,
    }

    async fn env(cfg: LdapConfig) -> Env {
        let fake = Fake::new(include_str!("openldap.ldif"));
        let dir = LdapDirectory::with_source(T, cfg, fake.clone());
        let (meta, _, ids) = crate::testutil::meta_accounts(&[
            ("alice@example.org", true),
            ("carol@example.org", true),
        ])
        .await;
        let groups = MemGroups::new();
        Env {
            sync: GroupSync::new(dir, Arc::new(meta), Arc::new(groups.clone())),
            fake,
            groups,
            alice: ids[0],
            carol: ids[1],
        }
    }

    async fn group(groups: &MemGroups, external_id: &str) -> DistributionGroup {
        groups
            .by_external_id(T, external_id)
            .await
            .expect("by_external_id")
            .expect("group")
    }

    fn sorted<V: Ord>(mut v: Vec<V>) -> Vec<V> {
        v.sort();
        v
    }

    #[tokio::test]
    async fn nested_group_and_member_removal() {
        let Env {
            sync,
            fake,
            groups,
            alice,
            carol,
        } = env(openldap_cfg()).await;
        let full = sync.run(None).await.expect("full");
        assert_eq!((full.upserted, full.removed, full.skipped), (2, 0, 0));
        assert!(!full.full);
        // staff ссылается на devs раньше её записи (порядок modifyTimestamp): заготовка, затем заполнение;
        // uid=ghost не разрешается и пропускается.
        let staff = group(&groups, STAFF).await;
        let devs = group(&groups, DEVS).await;
        assert_eq!(
            staff.members,
            sorted(vec![
                GroupMember::Account(alice),
                GroupMember::Group(devs.id)
            ])
        );
        assert_eq!(staff.address.as_deref(), Some("staff@example.org"));
        assert_eq!(devs.name, "devs");
        assert_eq!(devs.address, None);
        assert_eq!(
            devs.members,
            sorted(vec![
                GroupMember::Account(alice),
                GroupMember::Account(carol)
            ])
        );
        assert_eq!(
            groups.expand(T, staff.id).await.expect("expand"),
            sorted(vec![alice, carol])
        );
        assert_eq!(groups.list(T).await.expect("list").len(), 2);

        // Удаление участника: carol выходит из devs — и из staff через вложенность.
        fake.set_all(DEVS_DN, "member", &[ALICE_DN]);
        fake.set(DEVS_DN, "modifyTimestamp", "20261005000000Z");
        let inc = sync.run(Some(&full.cursor)).await.expect("incremental");
        assert_eq!((inc.upserted, inc.removed), (1, 0));
        let devs2 = group(&groups, DEVS).await;
        assert_eq!(devs2.id, devs.id, "идентификатор группы сохраняется");
        assert_eq!(devs2.members, [GroupMember::Account(alice)]);
        assert_eq!(groups.expand(T, staff.id).await.expect("expand"), [alice]);
        let idle = sync.run(Some(&inc.cursor)).await.expect("idle");
        assert_eq!((idle.upserted, idle.removed), (0, 0));

        // Удалённую группу (режим modifyTimestamp) выявляет полная выгрузка; письма её очереди
        // модерации возвращаются вызывающему.
        let held = HeldMessage {
            id: HeldId::new(),
            group: devs.id,
            sender: "x@example.net".into(),
            sender_account: None,
            message_id: None,
            blob: BlobKey {
                tenant: T,
                hash: BlobHash([7; 32]),
                generation: Gen(1),
            },
            size: 10,
            held_at: SystemClock.now(),
        };
        groups.hold(T, held.clone()).await.expect("hold");
        fake.delete(DEVS_DN);
        let again = sync.run(None).await.expect("full again");
        assert_eq!((again.upserted, again.removed), (1, 1));
        assert_eq!(again.held, [held]);
        assert_eq!(groups.get(T, devs.id).await.expect("get"), None);
        assert_eq!(
            group(&groups, STAFF).await.members,
            [GroupMember::Account(alice)]
        );
    }

    #[tokio::test]
    async fn restrictions_moderators_and_hiding_from_directory() {
        let flag = |attribute: &str| {
            Some(FlagRule {
                attribute: attribute.into(),
                value: Some("TRUE".into()),
                flag: None,
            })
        };
        let cfg = LdapConfig {
            groups: GroupMap {
                moderators: Some("tpModerator".into()),
                senders: vec!["tpSender".into()],
                internal_only: flag("tpInternal"),
                hidden: flag("tpHidden"),
                ..GroupMap::default()
            },
            ..openldap_cfg()
        };
        cfg.check(Profile::Prod).expect("check");
        let Env {
            sync,
            fake,
            groups,
            alice,
            carol,
        } = env(cfg.clone()).await;
        fake.set(STAFF_DN, "tpHidden", "true");
        fake.set(STAFF_DN, "tpModerator", CAROL_DN);
        fake.set_all(STAFF_DN, "tpSender", &[ALICE_DN, DEVS_DN]);
        fake.set(DEVS_DN, "tpInternal", "TRUE");
        sync.run(None).await.expect("sync");
        let staff = group(&groups, STAFF).await;
        let devs = group(&groups, DEVS).await;
        assert!(staff.hidden_from_gal);
        assert_eq!(staff.moderators, [carol]);
        assert!(staff.requires_moderation(Some(alice)));
        assert_eq!(
            staff.senders,
            SenderRestriction::Only {
                accounts: vec![alice],
                groups: vec![devs.id],
            }
        );
        // carol — участник devs из списка отправителей; внешний — нет.
        assert!(
            groups
                .sender_allowed(T, &staff, Some(carol))
                .await
                .expect("carol")
        );
        assert!(
            !groups
                .sender_allowed(T, &staff, None)
                .await
                .expect("external")
        );
        assert_eq!(devs.senders, SenderRestriction::Internal);
        assert!(!devs.hidden_from_gal);
        assert!(devs.moderators.is_empty());

        // Признак с value и flag сразу — ошибка секции.
        let mut bad = cfg;
        bad.groups.hidden = Some(FlagRule {
            attribute: "tpHidden".into(),
            value: Some("TRUE".into()),
            flag: Some(1),
        });
        assert!(matches!(bad.check(Profile::Prod), Err(Error::Invalid(_))));
    }

    #[tokio::test]
    async fn unmapped_fields_keep_admin_settings() {
        let Env {
            sync,
            fake,
            groups,
            alice,
            ..
        } = env(openldap_cfg()).await;
        let full = sync.run(None).await.expect("full");
        // Настройки Admin API (ADM-07) для полей без сопоставления в секции.
        let mut staff = group(&groups, STAFF).await;
        staff.hidden_from_gal = true;
        staff.moderators = vec![alice];
        staff.senders = SenderRestriction::Internal;
        groups.put(staff).await.expect("put");
        fake.set(STAFF_DN, "cn", "Staff All");
        fake.set(STAFF_DN, "modifyTimestamp", "20261005000000Z");
        sync.run(Some(&full.cursor)).await.expect("incremental");
        let after = group(&groups, STAFF).await;
        assert_eq!(after.name, "Staff All");
        assert!(after.hidden_from_gal);
        assert_eq!(after.moderators, [alice]);
        assert_eq!(after.senders, SenderRestriction::Internal);
    }

    #[tokio::test]
    async fn stale_cursor_and_empty_listing() {
        let Env {
            sync, fake, groups, ..
        } = env(openldap_cfg()).await;
        let junk = DirCursor("not a cursor".into());
        let r = sync.run(Some(&junk)).await.expect("resync");
        assert!(r.full);
        assert_eq!(r.upserted, 2);
        // Пустая выгрузка (неверная база, фильтр) не стирает группы.
        fake.delete(STAFF_DN);
        fake.delete(DEVS_DN);
        let empty = sync.run(None).await.expect("empty");
        assert_eq!(empty.removed, 0);
        assert_eq!(groups.list(T).await.expect("list").len(), 2);
    }
}
