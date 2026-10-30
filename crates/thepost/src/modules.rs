//! Дополнительные модули (SRS 1.3, 2.5): cargo feature + `modules.<имя>.enabled`.
//!
//! Модуль попадает в сборку строкой в [`BUILTIN`] под своей feature (`#[cfg(feature = "mod-<имя>")]`), его секция
//! `modules.<имя>` регистрируется в реестре секций. При запуске включённый модуль получает миграции своей схемы
//! `mod_<имя>` и регистрирует расширения в [`Registry`]; невключённый модуль ничего не создаёт.
//! Модуль, указанный в config.yaml с `enabled: true`, но не собранный, — ошибка конфигурации (платформа).

use std::sync::Arc;

use serde_json::{Map, Value};
use sqlx::PgPool;
use tp_contracts::Result;
use tp_contracts::ext::{Module, ModuleCtx, Registry, validate_module_name};
use tp_contracts::role::watch;
use tp_platform::{Config, SectionRegistry};

/// Секция `modules.<имя>` без ключа `enabled`: модуль разбирает её сам ([`ModuleCtx::config`]).
pub type ModuleSection = Map<String, Value>;

/// Модуль сборки: имя секции и конструктор.
#[derive(Debug, Clone, Copy)]
pub struct Builtin {
    /// Имя модуля (`modules.<имя>`).
    pub name: &'static str,
    /// Конструктор модуля.
    pub make: fn() -> Arc<dyn Module>,
}

/// Модули этой сборки. Базовая поставка модулей не содержит.
pub const BUILTIN: &[Builtin] = &[];

/// Регистрирует секции `modules.<имя>` модулей сборки.
pub fn register_sections(r: &mut SectionRegistry) -> Result<()> {
    register_sections_of(r, BUILTIN)
}

/// Регистрирует секции модулей `builtin`.
pub fn register_sections_of(r: &mut SectionRegistry, builtin: &[Builtin]) -> Result<()> {
    for m in builtin {
        validate_module_name(m.name)?;
        r.add_module::<ModuleSection>(m.name)?;
    }
    Ok(())
}

/// Включённые модули сборки: миграции схем и регистрация расширений.
pub async fn activate(cfg: &Config, migrations: &PgPool) -> Result<Registry> {
    activate_of(cfg, migrations, BUILTIN).await
}

/// Как [`activate`] для модулей `builtin`.
pub async fn activate_of(
    cfg: &Config,
    migrations: &PgPool,
    builtin: &[Builtin],
) -> Result<Registry> {
    let mut registry = Registry::default();
    for m in builtin.iter().filter(|m| cfg.module_enabled(m.name)) {
        let module = (m.make)();
        let applied = tp_meta::modules::migrate_module(migrations, module.as_ref()).await?;
        let section = cfg.module::<ModuleSection>(m.name)?;
        // Секция модуля без hot reload: канал закрыт, значение остаётся в силе.
        let (_, rx) = watch::channel(Value::Object((*section).clone()));
        module
            .register(&mut registry, &ModuleCtx::new(rx))
            .map_err(|e| crate::infra::context(&format!("modules.{}", m.name), e))?;
        tracing::info!(module = m.name, migrations = applied, "модуль включён");
    }
    Ok(registry)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tp_contracts::ext::Migration;

    #[derive(Debug)]
    struct Demo;

    impl Module for Demo {
        fn name(&self) -> &str {
            "demo"
        }

        fn migrations(&self) -> &[Migration] {
            &[]
        }

        fn register(&self, _registry: &mut Registry, ctx: &ModuleCtx) -> Result<()> {
            let section: ModuleSection = ctx.config()?;
            assert_eq!(section.get("level"), Some(&Value::from(3)));
            Ok(())
        }
    }

    const DEMO: &[Builtin] = &[Builtin {
        name: "demo",
        make: || Arc::new(Demo),
    }];

    fn registry() -> SectionRegistry {
        let mut r = crate::sections::registry().expect("registry");
        register_sections_of(&mut r, DEMO).expect("demo");
        r
    }

    const BASE: &str = "domain: example.org\nprofile: dev\ndatabase:\n  url: postgres://x@y/z?sslmode=disable\n  tls: { allow_plaintext: true }\nblob:\n  url: file:///tmp/b\n";

    #[test]
    fn module_switch_and_section() {
        let cfg = registry()
            .parse(
                &format!("{BASE}modules:\n  demo:\n    enabled: true\n    level: 3\n"),
                Vec::new(),
            )
            .expect("config");
        assert!(cfg.module_enabled("demo"));
        let section = cfg.module::<ModuleSection>("demo").expect("section");
        assert_eq!(section.get("level"), Some(&Value::from(3)));
        assert!(section.get("enabled").is_none());

        let cfg = registry().parse(BASE, Vec::new()).expect("config");
        assert!(!cfg.module_enabled("demo"));
        // Включение модуля, которого нет в сборке, — ошибка.
        let e = crate::sections::registry()
            .expect("registry")
            .parse(
                &format!("{BASE}modules:\n  demo:\n    enabled: true\n"),
                Vec::new(),
            )
            .expect_err("not built");
        assert!(e.to_string().contains("modules.demo"), "{e}");
    }
}
