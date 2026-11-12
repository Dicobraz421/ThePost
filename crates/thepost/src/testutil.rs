//! Общее для тестов приложения: адрес тестовой БД для профиля dev, перенос пароля в хранилище секретов.

/// Адрес тестовой БД без `sslmode` — открытый канал (`sslmode=disable`, профиль dev).
pub fn dev_url(url: &str) -> String {
    if url.contains("sslmode=") {
        return url.to_owned();
    }
    let sep = if url.contains('?') { '&' } else { '?' };
    format!("{url}{sep}sslmode=disable")
}

/// Адрес без пароля и пароль (с раскрытыми `%XX`), если он был в `user:password@`.
pub fn split_password(url: &str) -> (String, Option<String>) {
    let Some((scheme, rest)) = url.split_once("://") else {
        return (url.to_owned(), None);
    };
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (authority, tail) = rest.split_at(end);
    let Some((userinfo, host)) = authority.rsplit_once('@') else {
        return (url.to_owned(), None);
    };
    let Some((user, password)) = userinfo.split_once(':') else {
        return (url.to_owned(), None);
    };
    (
        format!("{scheme}://{user}@{host}{tail}"),
        Some(percent_decode(password)),
    )
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && let Some(b) = s
                .get(i + 1..i + 3)
                .and_then(|h| u8::from_str_radix(h, 16).ok())
        {
            out.push(b);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[test]
fn password_split() {
    assert_eq!(
        split_password("postgres://u:p%40w@h:5432/db"),
        ("postgres://u@h:5432/db".to_owned(), Some("p@w".to_owned()))
    );
    assert_eq!(split_password("postgres://u@h/db").1, None);
    assert_eq!(
        dev_url("postgres://h/db"),
        "postgres://h/db?sslmode=disable"
    );
}

/// Отдельная база для теста на сервере `url` (`TEST_DATABASE_URL` с `sslmode`): имя и адрес. Узлы и снимки
/// настроек транспорта теста не видят чужих записей (организация по умолчанию, состав кластера).
pub async fn create_database(url: &str) -> (String, String) {
    let name = format!(
        "app_{}",
        tp_contracts::ids::DomainId::new().as_uuid().simple()
    );
    let admin = sqlx::PgPool::connect(url).await.expect("admin pool");
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE DATABASE \"{name}\"")))
        .execute(&admin)
        .await
        .expect("create database");
    admin.close().await;
    let (head, query) = match url.split_once('?') {
        Some((h, q)) => (h, format!("?{q}")),
        None => (url, String::new()),
    };
    let scheme_end = head.find("://").map_or(0, |i| i + 3);
    let base = match head[scheme_end..].find('/') {
        Some(i) => &head[..scheme_end + i],
        None => head,
    };
    (name.clone(), format!("{base}/{name}{query}"))
}

/// Удаляет базу теста `name` (соединения узлов закрываются принудительно).
pub async fn drop_database(url: &str, name: &str) {
    if let Ok(admin) = sqlx::PgPool::connect(url).await {
        let _ = sqlx::query(sqlx::AssertSqlSafe(format!(
            "DROP DATABASE IF EXISTS \"{name}\" WITH (FORCE)"
        )))
        .execute(&admin)
        .await;
        admin.close().await;
    }
}

/// Пул `tp-meta` к базе `url` без учёта ожидания соединения (наблюдатель-пустышка): метрик в тесте нет.
pub async fn db_pool(url: &str) -> tp_meta::pool::DbPool {
    let pool = sqlx::PgPool::connect(url).await.expect("pool");
    tp_meta::pool::DbPool::new(pool, std::sync::Arc::new(tp_meta::pool::NoAcquireObserver))
}

/// Перечисление записей учёта BLOB поверх `MemBlobIndex` по записанным ключам (`keys`); помеченные
/// повреждёнными — в `damaged`.
pub struct MemInventory {
    index: tp_testkit::MemBlobIndex,
    pub keys: std::sync::Mutex<Vec<tp_contracts::BlobKey>>,
    pub damaged: std::sync::Mutex<std::collections::BTreeSet<tp_contracts::BlobKey>>,
}

impl MemInventory {
    pub fn new(index: tp_testkit::MemBlobIndex) -> Self {
        Self {
            index,
            keys: std::sync::Mutex::default(),
            damaged: std::sync::Mutex::default(),
        }
    }
}

#[async_trait::async_trait]
impl tp_blob::BlobInventory for MemInventory {
    fn live(
        &self,
    ) -> futures::stream::BoxStream<'static, tp_contracts::Result<tp_blob::IndexedBlob>> {
        use futures::StreamExt;
        let rows: Vec<_> = self
            .keys
            .lock()
            .expect("lock")
            .iter()
            .filter_map(|k| {
                let r = self.index.row(k.tenant, &k.hash)?;
                (r.state == tp_testkit::blob::BlobState::Live && r.generation == k.generation)
                    .then_some(Ok(tp_blob::IndexedBlob {
                        key: *k,
                        size: r.size,
                    }))
            })
            .collect();
        futures::stream::iter(rows).boxed()
    }

    async fn mark_damaged(&self, key: &tp_contracts::BlobKey) -> tp_contracts::Result<()> {
        self.damaged.lock().expect("lock").insert(*key);
        Ok(())
    }
}
