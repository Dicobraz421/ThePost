//! Команды скриптов ManageSieve на in-memory `SieveStore` (SIEVE-09).

use tp_contracts::sieve::{ScriptInfo, SieveLimits, SieveStore};

use super::managesieve::{Setup, Stand};
use crate::SieveConfig;

const SCRIPT: &str = "require \"fileinto\";\r\nif header :contains \"subject\" \"report\" {\r\n    fileinto \"Work\";\r\n}\r\n";

/// Стенд после входа.
async fn logged_in(setup: Setup) -> Stand {
    let mut s = setup.start();
    s.response().await;
    s.login().await;
    s
}

fn put(name: &str, text: &str) -> String {
    format!("PUTSCRIPT \"{name}\" {{{}+}}\r\n{text}", text.len())
}

#[tokio::test]
async fn full_cycle() {
    let mut s = logged_in(Setup::tls()).await;
    assert_eq!(
        s.cmd("LISTSCRIPTS").await,
        "OK \"Listscripts completed\"\r\n"
    );
    assert_eq!(
        s.cmd("HAVESPACE \"main\" 1024").await,
        "OK \"Havespace completed\"\r\n"
    );
    assert_eq!(
        s.cmd(&put("main", SCRIPT)).await,
        "OK \"Putscript completed\"\r\n"
    );
    assert_eq!(
        s.cmd("PUTSCRIPT \"вторая \\\"копия\\\"\" \"keep;\"").await,
        "OK \"Putscript completed\"\r\n"
    );
    assert_eq!(
        s.cmd("LISTSCRIPTS").await,
        "\"main\"\r\n\"вторая \\\"копия\\\"\"\r\nOK \"Listscripts completed\"\r\n"
    );
    assert_eq!(
        s.cmd("GETSCRIPT \"main\"").await,
        format!(
            "{{{}}}\r\n{SCRIPT}\r\nOK \"Getscript completed\"\r\n",
            SCRIPT.len()
        )
    );

    assert_eq!(
        s.cmd("SETACTIVE \"main\"").await,
        "OK \"Setactive completed\"\r\n"
    );
    assert_eq!(
        s.cmd("LISTSCRIPTS").await,
        "\"main\" ACTIVE\r\n\"вторая \\\"копия\\\"\"\r\nOK \"Listscripts completed\"\r\n"
    );
    // Замена текста сохраняет активность; переименование — тоже.
    assert_eq!(
        s.cmd(&put("main", "discard;")).await,
        "OK \"Putscript completed\"\r\n"
    );
    assert_eq!(
        s.cmd("RENAMESCRIPT \"main\" \"rules\"").await,
        "OK \"Renamescript completed\"\r\n"
    );
    let active = s
        .store
        .active(s.scope)
        .await
        .expect("active")
        .expect("есть");
    assert_eq!(
        (active.name.as_str(), active.text.as_str()),
        ("rules", "discard;")
    );

    // Активный не удаляется; после снятия активности — удаляется.
    assert_eq!(
        s.cmd("DELETESCRIPT \"rules\"").await,
        "NO (ACTIVE) \"Script is active; deactivate it first\"\r\n"
    );
    assert_eq!(
        s.cmd("SETACTIVE \"\"").await,
        "OK \"Setactive completed\"\r\n"
    );
    assert_eq!(
        s.cmd("DELETESCRIPT \"rules\"").await,
        "OK \"Deletescript completed\"\r\n"
    );
    assert_eq!(
        s.store.list(s.scope).await.expect("list"),
        vec![ScriptInfo {
            name: "вторая \"копия\"".into(),
            size: 5,
            active: false,
        }]
    );
}

#[tokio::test]
async fn response_codes() {
    let mut s = logged_in(Setup::tls()).await;
    s.cmd(&put("a", "keep;")).await;
    s.cmd(&put("b", "keep;")).await;
    let missing = "NO (NONEXISTENT) \"Script does not exist\"\r\n";
    assert_eq!(s.cmd("GETSCRIPT \"none\"").await, missing);
    assert_eq!(s.cmd("SETACTIVE \"none\"").await, missing);
    assert_eq!(s.cmd("DELETESCRIPT \"none\"").await, missing);
    assert_eq!(s.cmd("RENAMESCRIPT \"none\" \"c\"").await, missing);
    assert_eq!(
        s.cmd("RENAMESCRIPT \"a\" \"b\"").await,
        "NO (ALREADYEXISTS) \"Script with this name already exists\"\r\n"
    );
    assert_eq!(
        s.cmd("RENAMESCRIPT \"a\" \"\"").await,
        "NO \"Invalid script name\"\r\n"
    );
    assert_eq!(
        s.cmd("PUTSCRIPT \"\" \"keep;\"").await,
        "NO \"Invalid script name\"\r\n"
    );
    assert_eq!(
        s.cmd("PUTSCRIPT \"a\"").await,
        "NO \"Invalid arguments\"\r\n"
    );
    assert_eq!(
        s.cmd("HAVESPACE \"a\" \"1\"").await,
        "NO \"Invalid arguments\"\r\n"
    );
    s.send(b"GETSCRIPT {2+}\r\n\xff\xfe\r\n").await;
    assert_eq!(s.response().await, "NO \"Invalid UTF-8\"\r\n");
}

#[tokio::test]
async fn check_reports_position() {
    let mut s = logged_in(Setup::tls()).await;
    assert_eq!(
        s.cmd("CHECKSCRIPT \"keep;\"").await,
        "OK \"Script is valid\"\r\n"
    );
    // Одна ошибка — строкой в кавычках, с позицией.
    let bad = "keep;\r\nfileinto \"Work\";\r\n";
    let r = s
        .cmd(&format!("CHECKSCRIPT {{{}+}}\r\n{bad}", bad.len()))
        .await;
    assert!(r.starts_with("NO \"строка 2, столбец 1: "), "{r}");
    // Несколько ошибок — литералом, по строке на ошибку; скрипт не сохраняется.
    let bad = "fileinto \"Work\";\r\nvacation \"x\";\r\n";
    let r = s.cmd(&put("bad", bad)).await;
    assert!(r.starts_with("NO {"), "{r}");
    assert!(
        r.contains("строка 1, столбец 1: ") && r.contains("\r\nстрока 2, столбец 1: "),
        "{r}"
    );
    assert_eq!(s.cmd("NOOP").await, "OK \"NOOP completed\"\r\n");
    assert_eq!(s.store.list(s.scope).await.expect("list"), vec![]);
}

#[tokio::test]
async fn quota() {
    let setup = Setup {
        limits: SieveLimits {
            max_scripts: 2,
            max_script_size: 64,
        },
        ..Setup::tls()
    };
    let mut s = logged_in(setup).await;
    let big = format!("# {}\r\nkeep;", "x".repeat(64));
    let too_large = "NO (QUOTA/MAXSIZE) \"Script is too large\"\r\n";
    assert_eq!(s.cmd(&put("a", &big)).await, too_large);
    assert_eq!(
        s.cmd(&format!("CHECKSCRIPT {{{}+}}\r\n{big}", big.len()))
            .await,
        too_large
    );
    assert_eq!(s.cmd("HAVESPACE \"a\" 65").await, too_large);
    assert_eq!(
        s.cmd("HAVESPACE \"a\" 64").await,
        "OK \"Havespace completed\"\r\n"
    );

    s.cmd(&put("a", "keep;")).await;
    s.cmd(&put("b", "keep;")).await;
    let too_many = "NO (QUOTA/MAXSCRIPTS) \"Too many scripts\"\r\n";
    assert_eq!(s.cmd(&put("c", "keep;")).await, too_many);
    assert_eq!(s.cmd("HAVESPACE \"c\" 5").await, too_many);
    // Замена существующего в лимит укладывается.
    assert_eq!(
        s.cmd("HAVESPACE \"b\" 5").await,
        "OK \"Havespace completed\"\r\n"
    );
    assert_eq!(
        s.cmd(&put("b", "discard;")).await,
        "OK \"Putscript completed\"\r\n"
    );
}

#[tokio::test]
async fn requires_login_and_isolates_accounts() {
    let mut s = Setup::tls().start();
    s.response().await;
    for command in [
        "LISTSCRIPTS",
        "GETSCRIPT \"a\"",
        "PUTSCRIPT \"a\" \"keep;\"",
        "CHECKSCRIPT \"keep;\"",
    ] {
        assert_eq!(
            s.cmd(command).await,
            "NO \"Authenticate first\"\r\n",
            "{command}"
        );
    }
    // Скрипт другой учётной записи той же организации не виден.
    let other = tp_contracts::meta::AccountScope {
        tenant: s.scope.tenant,
        account: tp_contracts::AccountId::new(),
    };
    s.store
        .put(other, "foreign", "keep;".into())
        .await
        .expect("put");
    s.login().await;
    assert_eq!(
        s.cmd("LISTSCRIPTS").await,
        "OK \"Listscripts completed\"\r\n"
    );
    assert_eq!(
        s.cmd("GETSCRIPT \"foreign\"").await,
        "NO (NONEXISTENT) \"Script does not exist\"\r\n"
    );
    // После UNAUTHENTICATE команды скриптов снова недоступны.
    s.cmd("UNAUTHENTICATE").await;
    assert_eq!(s.cmd("LISTSCRIPTS").await, "NO \"Authenticate first\"\r\n");
}

/// SIEVE-11: текст проверяется с пределами секции `sieve` узла, а не `SieveConfig::default()`.
#[tokio::test]
async fn section_limits() {
    let check = format!("CHECKSCRIPT {{{}+}}\r\n{SCRIPT}", SCRIPT.len());

    // Пределы по умолчанию: текст принимается.
    let mut s = logged_in(Setup::tls()).await;
    assert_eq!(s.cmd(&check).await, "OK \"Script is valid\"\r\n");
    assert_eq!(
        s.cmd(&put("main", SCRIPT)).await,
        "OK \"Putscript completed\"\r\n"
    );

    // Уменьшен предел разбора (`max_commands`): тот же текст — `NO`, в хранилище не попадает.
    let mut s = logged_in(Setup {
        tls: true,
        config: SieveConfig {
            max_commands: 2,
            ..SieveConfig::default()
        },
        ..Setup::default()
    })
    .await;
    assert!(s.cmd(&check).await.starts_with("NO \""));
    assert!(s.cmd(&put("main", SCRIPT)).await.starts_with("NO \""));
    assert_eq!(s.store.list(s.scope).await.expect("list"), Vec::new());

    // Уменьшен размер текста в секции (предел хранилища — по умолчанию): `QUOTA/MAXSIZE`, `HAVESPACE` — по нему же.
    let mut s = logged_in(Setup {
        tls: true,
        config: SieveConfig {
            max_script_size: SCRIPT.len() - 1,
            ..SieveConfig::default()
        },
        ..Setup::default()
    })
    .await;
    let too_large = "NO (QUOTA/MAXSIZE) \"Script is too large\"\r\n";
    assert_eq!(s.cmd(&check).await, too_large);
    assert_eq!(s.cmd(&put("main", SCRIPT)).await, too_large);
    assert_eq!(
        s.cmd(&format!("HAVESPACE \"main\" {}", SCRIPT.len())).await,
        too_large
    );
    assert_eq!(
        s.cmd(&format!("HAVESPACE \"main\" {}", SCRIPT.len() - 1))
            .await,
        "OK \"Havespace completed\"\r\n"
    );
    assert_eq!(s.store.list(s.scope).await.expect("list"), Vec::new());
}
