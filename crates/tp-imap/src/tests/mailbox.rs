//! Операции с папками (IMAP-03).

use tp_contracts::meta::{Flag, MailboxStore, SpecialUse};

use super::{Client, Env};

const PLAIN: &[u8] = b"From: a@example.org\r\nTo: b@example.org\r\nSubject: hi\r\n\r\nHello\r\n";

async fn env_client() -> (Env, Client) {
    let env = Env::new().await;
    let c = env.logged_in().await;
    (env, c)
}

#[tokio::test]
async fn list_system_folders() {
    let (_env, mut c) = env_client().await;
    assert_eq!(
        c.cmd("A1 LIST \"\" \"\"").await,
        "* LIST (\\Noselect) \"/\" \"\"\r\nA1 OK LIST completed\r\n"
    );
    assert_eq!(
        c.cmd("A2 LIST \"\" *").await,
        "* LIST (\\HasNoChildren \\Drafts) \"/\" Drafts\r\n\
         * LIST (\\HasNoChildren) \"/\" INBOX\r\n\
         * LIST (\\HasNoChildren \\Junk) \"/\" Junk\r\n\
         * LIST (\\HasNoChildren \\Sent) \"/\" Sent\r\n\
         * LIST (\\HasNoChildren \\Trash) \"/\" Trash\r\n\
         A2 OK LIST completed\r\n"
    );
    // INBOX — без учёта регистра.
    assert_eq!(
        c.cmd("A3 LIST \"\" inbox").await,
        "* LIST (\\HasNoChildren) \"/\" INBOX\r\nA3 OK LIST completed\r\n"
    );
    // RFC 6154: только папки с назначением.
    let r = c.cmd("A4 LIST (SPECIAL-USE) \"\" *").await;
    assert!(!r.contains("INBOX") && r.contains("\\Sent"), "{r}");
}

#[tokio::test]
async fn create_hierarchy_and_names() {
    let (_env, mut c) = env_client().await;
    assert_eq!(
        c.cmd("A1 CREATE Projects/2026/Q1").await,
        "A1 OK CREATE completed\r\n"
    );
    assert_eq!(
        c.cmd("A2 LIST \"\" Projects*").await,
        "* LIST (\\HasChildren) \"/\" Projects\r\n\
         * LIST (\\HasChildren) \"/\" Projects/2026\r\n\
         * LIST (\\HasNoChildren) \"/\" Projects/2026/Q1\r\n\
         A2 OK LIST completed\r\n"
    );
    assert_eq!(
        c.cmd("A3 LIST Projects/ %").await,
        "* LIST (\\HasChildren) \"/\" Projects/2026\r\nA3 OK LIST completed\r\n"
    );
    assert!(c.cmd("A4 CREATE projects/2026/Q1/").await.contains("A4 OK"));
    assert!(
        c.cmd("A5 CREATE Projects/2026/")
            .await
            .starts_with("A5 NO [ALREADYEXISTS]")
    );
    assert!(
        c.cmd("A6 CREATE inbox")
            .await
            .starts_with("A6 NO [ALREADYEXISTS]")
    );
    assert!(c.cmd("A7 CREATE a//b").await.starts_with("A7 BAD"));
    // Вложенные во INBOX.
    assert!(c.cmd("A8 CREATE INBOX/Lists").await.contains("A8 OK"));
    let r = c.cmd("A9 LIST \"\" INBOX*").await;
    assert!(r.contains("(\\HasChildren) \"/\" INBOX\r\n"), "{r}");
    assert!(r.contains("\"/\" INBOX/Lists\r\n"), "{r}");

    // Кириллица: IMAP4rev1 — модифицированный UTF-7, IMAP4rev2 — UTF-8.
    let r = c
        .cmd("B1 CREATE &BB4EQgQ,BEAEMAQyBDsENQQ9BD0ESwQ1-/&BBAEQARFBDgEMg-")
        .await;
    assert!(r.contains("B1 OK"), "{r}");
    let r = c
        .cmd("B2 LIST \"\" &BB4EQgQ,BEAEMAQyBDsENQQ9BD0ESwQ1-/%")
        .await;
    assert!(
        r.contains("\"/\" &BB4EQgQ,BEAEMAQyBDsENQQ9BD0ESwQ1-/&BBAEQARFBDgEMg-\r\n"),
        "{r}"
    );
    assert!(c.cmd("B3 CREATE &Bad").await.starts_with("B3 BAD"));
    let (_env, mut c) = env_client().await;
    assert!(c.cmd("C1 ENABLE IMAP4rev2").await.contains("C1 OK"));
    assert!(
        c.cmd("C2 CREATE \"Отправленные/Архив\"")
            .await
            .contains("C2 OK")
    );
    let r = c.cmd("C3 LIST \"\" \"Отправленные/*\"").await;
    // Не-ASCII имя imap-codec передаёт литералом.
    assert!(r.contains("{35}\r\nОтправленные/Архив\r\n"), "{r}");
}

#[tokio::test]
async fn create_special_use() {
    let (_env, mut c) = env_client().await;
    let r = c.cmd("A1 CREATE Old (USE (\\Archive))").await;
    assert_eq!(r, "A1 OK CREATE completed\r\n");
    let r = c.cmd("A2 LIST \"\" Old").await;
    assert!(r.contains("(\\HasNoChildren \\Archive) \"/\" Old"), "{r}");
    let r = c.cmd("A3 CREATE MySent (USE (\\Sent))").await;
    assert!(r.starts_with("A3 NO [USEATTR]"), "{r}");
    let r = c.cmd("A4 CREATE X (USE (\\Important))").await;
    assert!(r.starts_with("A4 NO [USEATTR]"), "{r}");
}

#[tokio::test]
async fn subscriptions_and_list_extended() {
    let (_env, mut c) = env_client().await;
    // Новые и системные папки хранилище создаёт подписанными.
    for cmd in [
        "A1 CREATE Lists/rust",
        "A2 CREATE Lists/go",
        "A3 UNSUBSCRIBE Lists",
        "A4 UNSUBSCRIBE Lists/go",
        "A5 UNSUBSCRIBE Drafts",
        "A6 UNSUBSCRIBE Junk",
        "A7 UNSUBSCRIBE Sent",
        "A8 UNSUBSCRIBE Trash",
    ] {
        assert!(c.cmd(cmd).await.contains(" OK "), "{cmd}");
    }
    assert_eq!(
        c.cmd("B1 LSUB \"\" *").await,
        "* LSUB () \"/\" INBOX\r\n* LSUB () \"/\" Lists/rust\r\nB1 OK LSUB completed\r\n"
    );
    assert_eq!(
        c.cmd("B2 LIST (SUBSCRIBED) \"\" *").await,
        "* LIST (\\HasNoChildren \\Subscribed) \"/\" INBOX\r\n\
         * LIST (\\HasNoChildren \\Subscribed) \"/\" Lists/rust\r\n\
         B2 OK LIST completed\r\n"
    );
    // RFC 5258, 5, пример 4: родитель подписанной папки — с CHILDINFO.
    assert_eq!(
        c.cmd("B3 LIST (SUBSCRIBED RECURSIVEMATCH) \"\" %").await,
        "* LIST (\\HasNoChildren \\Subscribed) \"/\" INBOX\r\n\
         * LIST (\\HasChildren) \"/\" Lists (\"CHILDINFO\" (\"SUBSCRIBED\"))\r\n\
         B3 OK LIST completed\r\n"
    );
    let r = c
        .cmd("B4 LIST \"\" Lists/% RETURN (SUBSCRIBED STATUS (MESSAGES SIZE))")
        .await;
    assert_eq!(
        r,
        "* LIST (\\HasNoChildren) \"/\" Lists/go\r\n\
         * STATUS Lists/go (MESSAGES 0 SIZE 0)\r\n\
         * LIST (\\HasNoChildren \\Subscribed) \"/\" Lists/rust\r\n\
         * STATUS Lists/rust (MESSAGES 0 SIZE 0)\r\n\
         B4 OK LIST completed\r\n"
    );
    assert!(c.cmd("B5 UNSUBSCRIBE Lists/rust").await.contains("B5 OK"));
    assert!(!c.cmd("B6 LSUB \"\" *").await.contains("rust"));
    assert!(
        c.cmd("B7 SUBSCRIBE Nope")
            .await
            .starts_with("B7 NO [NONEXISTENT]")
    );
}

#[tokio::test]
async fn select_examine_status() {
    let (env, mut c) = env_client().await;
    let inbox = env.mailbox(SpecialUse::Inbox);
    env.deliver(inbox, PLAIN, [Flag::Seen].into()).await;
    env.deliver(inbox, PLAIN, [Flag::Deleted].into()).await;
    let st = env.meta.status(env.scope(), inbox).await.expect("status");
    let validity = st.uid_validity.0;
    let r = c.cmd("A1 SELECT INBOX").await;
    let r = r.replace(&format!("[UIDVALIDITY {validity}]"), "[UIDVALIDITY 1]");
    assert_eq!(
        r,
        "* FLAGS (\\Answered \\Flagged \\Deleted \\Seen \\Draft)\r\n\
         * OK [PERMANENTFLAGS (\\Answered \\Flagged \\Deleted \\Seen \\Draft \\*)] Flags permitted\r\n\
         * 2 EXISTS\r\n\
         * 0 RECENT\r\n\
         * OK [UNSEEN 2] First unseen\r\n\
         * OK [UIDVALIDITY 1] UIDs valid\r\n\
         * OK [UIDNEXT 3] Predicted next UID\r\n\
         A1 OK [READ-WRITE] SELECT completed\r\n"
    );
    let r = c.cmd("A2 EXAMINE Sent").await;
    assert!(
        r.starts_with("* OK [CLOSED] Previous mailbox is now closed\r\n"),
        "{r}"
    );
    assert!(
        r.contains("[PERMANENTFLAGS ()]") && r.contains("* 0 EXISTS"),
        "{r}"
    );
    assert!(
        r.ends_with("A2 OK [READ-ONLY] EXAMINE completed\r\n"),
        "{r}"
    );
    let r = c.cmd("A3 SELECT Nope").await;
    assert!(
        r.contains("[CLOSED]") && r.contains("A3 NO [NONEXISTENT]"),
        "{r}"
    );
    assert!(c.cmd("A4 UNSELECT").await.starts_with("A4 BAD"));
    let size = 2 * PLAIN.len();
    assert_eq!(
        c.cmd("A5 STATUS INBOX (MESSAGES UNSEEN UIDNEXT UIDVALIDITY SIZE DELETED RECENT)")
            .await,
        format!(
            "* STATUS INBOX (MESSAGES 2 UNSEEN 1 UIDNEXT 3 UIDVALIDITY {validity} SIZE {size} DELETED 1 RECENT 0)\r\n\
             A5 OK STATUS completed\r\n"
        )
    );
    // IMAP4rev2: без RECENT и UNSEEN, с LIST.
    assert!(c.cmd("A6 ENABLE IMAP4rev2").await.contains("A6 OK"));
    let r = c.cmd("A7 SELECT inbox").await;
    assert!(!r.contains("RECENT") && !r.contains("[UNSEEN"), "{r}");
    assert!(r.contains("* LIST () \"/\" INBOX\r\n"), "{r}");
    assert!(c.cmd("A8 UNSELECT").await.starts_with("A8 OK"));
    assert!(c.cmd("A9 CHECK").await.starts_with("A9 BAD"));
}

#[tokio::test]
async fn delete_and_rename() {
    let (_env, mut c) = env_client().await;
    for cmd in ["A1 CREATE Work/Old", "A2 CREATE Tmp"] {
        assert!(c.cmd(cmd).await.contains(" OK "), "{cmd}");
    }
    assert!(
        c.cmd("B1 DELETE Work")
            .await
            .starts_with("B1 NO [HASCHILDREN]")
    );
    assert!(c.cmd("B2 DELETE Trash").await.starts_with("B2 NO [CANNOT]"));
    assert!(c.cmd("B3 DELETE INBOX").await.starts_with("B3 NO [CANNOT]"));
    assert!(
        c.cmd("B4 DELETE Nope")
            .await
            .starts_with("B4 NO [NONEXISTENT]")
    );
    assert!(c.cmd("B5 SELECT Tmp").await.contains("B5 OK"));
    let r = c.cmd("B6 DELETE Tmp").await;
    assert_eq!(
        r,
        "* OK [CLOSED] Mailbox deleted\r\nB6 OK DELETE completed\r\n"
    );
    assert!(!c.cmd("B7 LIST \"\" *").await.contains("Tmp"));

    // RENAME создаёт промежуточные папки.
    assert!(
        c.cmd("C1 RENAME Work/Old Archive/2025/Work")
            .await
            .contains("C1 OK")
    );
    let r = c.cmd("C2 LIST \"\" *").await;
    assert!(
        r.contains("\"/\" Archive/2025/Work\r\n") && r.contains("(\\HasNoChildren) \"/\" Work\r\n"),
        "{r}"
    );
    assert!(
        c.cmd("C3 RENAME Work Archive")
            .await
            .starts_with("C3 NO [ALREADYEXISTS]")
    );
    assert!(
        c.cmd("C4 RENAME Archive Archive/2025/Work/x")
            .await
            .starts_with("C4 NO [CANNOT]")
    );
    assert!(
        c.cmd("C5 RENAME INBOX Old")
            .await
            .starts_with("C5 NO [CANNOT]")
    );
    assert!(
        c.cmd("C6 RENAME Nope X")
            .await
            .starts_with("C6 NO [NONEXISTENT]")
    );
    assert!(c.cmd("C7 DELETE Work").await.contains("C7 OK"));
}
