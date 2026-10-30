//! Миграции модулей в схеме mod_<имя> (META-13).

mod common;

use sqlx::PgPool;
use tp_contracts::ext::{Migration, Module, ModuleCtx, Registry};
use tp_contracts::{Error, Result};
use tp_meta::modules::{applied_version, drop_module, migrate_module};
use uuid::Uuid;

struct TestModule {
    name: String,
    migrations: Vec<Migration>,
}

impl Module for TestModule {
    fn name(&self) -> &str {
        &self.name
    }

    fn migrations(&self) -> &[Migration] {
        &self.migrations
    }

    fn register(&self, _: &mut Registry, _: &ModuleCtx) -> Result<()> {
        Ok(())
    }
}

const M1: Migration = Migration {
    version: 1,
    name: "notes",
    sql: "CREATE TABLE notes (tenant_id uuid NOT NULL, body text NOT NULL);
          CREATE INDEX notes_tenant ON notes (tenant_id);",
};
const M2: Migration = Migration {
    version: 2,
    name: "notes_at",
    sql: "ALTER TABLE notes ADD COLUMN at timestamptz NOT NULL DEFAULT now();",
};
const M3: Migration = Migration {
    version: 3,
    name: "tags",
    sql: "CREATE TABLE tags (name text PRIMARY KEY);",
};

fn module(name: &str, migrations: &[Migration]) -> TestModule {
    TestModule {
        name: name.into(),
        migrations: migrations.to_vec(),
    }
}

fn name() -> String {
    format!("t{}", &Uuid::now_v7().simple().to_string()[..20])
}

async fn exists(pool: &PgPool, relation: String) -> bool {
    sqlx::query_scalar("SELECT to_regclass($1) IS NOT NULL")
        .bind(relation)
        .fetch_one(pool)
        .await
        .expect("to_regclass")
}

/// Состояние схемы ядра: миграции и таблицы `public`.
async fn core_state(pool: &PgPool) -> (i64, i64) {
    sqlx::query_as(
        "SELECT (SELECT count(*) FROM _sqlx_migrations),
                (SELECT count(*) FROM pg_tables WHERE schemaname = 'public')",
    )
    .fetch_one(pool)
    .await
    .expect("core")
}

#[tokio::test]
async fn apply_upgrade_and_drop() {
    let Some(pool) = common::pool().await else {
        return;
    };
    let core = core_state(&pool).await;
    let name = name();
    let schema = format!("mod_{name}");
    assert_eq!(
        applied_version(&pool, &name)
            .await
            .expect("applied_version"),
        None
    );

    assert_eq!(
        migrate_module(&pool, &module(&name, &[M1, M2]))
            .await
            .expect("migrate_module"),
        2
    );
    assert_eq!(
        applied_version(&pool, &name)
            .await
            .expect("applied_version"),
        Some(2)
    );
    assert!(exists(&pool, format!("{schema}.notes")).await);
    assert!(!exists(&pool, "public.notes".into()).await);
    // Повторное применение и параллельный старт узлов — пустая операция.
    let m = module(&name, &[M1, M2]);
    let (a, b) = tokio::join!(migrate_module(&pool, &m), migrate_module(&pool, &m));
    assert_eq!(
        (a.expect("migrate_module"), b.expect("migrate_module")),
        (0, 0)
    );

    assert_eq!(
        migrate_module(&pool, &module(&name, &[M1, M2, M3]))
            .await
            .expect("migrate_module"),
        1
    );
    assert_eq!(
        applied_version(&pool, &name)
            .await
            .expect("applied_version"),
        Some(3)
    );
    // Узел предыдущей версии работает со схемой новее.
    assert_eq!(
        migrate_module(&pool, &module(&name, &[M1]))
            .await
            .expect("migrate_module"),
        0
    );

    let changed = Migration {
        sql: "ALTER TABLE notes ADD COLUMN other text;",
        ..M2
    };
    assert_eq!(
        migrate_module(&pool, &module(&name, &[M1, changed, M3])).await,
        Err(Error::Conflict)
    );
    let m4 = Migration {
        version: 5,
        name: "late",
        sql: "CREATE TABLE late (id int);",
    };
    assert_eq!(
        migrate_module(&pool, &module(&name, &[M1, M2, M3, m4]))
            .await
            .expect("migrate_module"),
        1
    );
    let gap = Migration { version: 4, ..m4 };
    assert_eq!(
        migrate_module(&pool, &module(&name, &[M1, M2, M3, gap, m4])).await,
        Err(Error::Conflict)
    );

    // Откат модуля — удаление схемы; ядро не изменилось.
    drop_module(&pool, &name).await.expect("drop_module");
    assert_eq!(
        applied_version(&pool, &name)
            .await
            .expect("applied_version"),
        None
    );
    assert!(!exists(&pool, format!("{schema}.notes")).await);
    drop_module(&pool, &name).await.expect("drop_module");
    assert_eq!(core_state(&pool).await, core);
}

#[tokio::test]
async fn failed_migration_rolls_back() {
    let Some(pool) = common::pool().await else {
        return;
    };
    let name = name();
    let bad = Migration {
        version: 2,
        name: "bad",
        sql: "CREATE TABLE broken (;",
    };
    assert!(
        migrate_module(&pool, &module(&name, &[M1, bad]))
            .await
            .is_err()
    );
    assert_eq!(
        applied_version(&pool, &name)
            .await
            .expect("applied_version"),
        None
    );
    assert!(!exists(&pool, format!("mod_{name}.notes")).await);
    drop_module(&pool, &name).await.expect("drop_module");
}

#[tokio::test]
async fn invalid_modules() {
    let Some(pool) = common::pool().await else {
        return;
    };
    assert!(matches!(
        migrate_module(&pool, &module("Bad-Name", &[M1])).await,
        Err(Error::Invalid(_))
    ));
    let name = name();
    for order in [
        &[M2, M1][..],
        &[M1, M1][..],
        &[Migration { version: 0, ..M1 }][..],
    ] {
        assert!(matches!(
            migrate_module(&pool, &module(&name, order)).await,
            Err(Error::Invalid(_))
        ));
    }
    assert_eq!(
        applied_version(&pool, &name)
            .await
            .expect("applied_version"),
        None
    );
}
