//! NAMESPACE (RFC 2342), QUOTA (RFC 9208), ACL (RFC 4314) и общие папки (SRS 4.2, 4.5).
//!
//! - `NAMESPACE`: личное пространство `""`; при подключённых ACL — папки других пользователей
//!   `<namespace.other_users>/<имя входа владельца>/<папка>` (папки, открытые субъекту по ACL с правом `l`:
//!   [`AclStore::shared_mailboxes`]). Общая папка выбирается `SELECT`/`EXAMINE` при праве `r` и только для
//!   чтения: `FETCH`, `SEARCH`, `STATUS`, `IDLE`; изменение писем и копирование из неё — не в этой волне.
//! - `GETQUOTAROOT`/`GETQUOTA`: один корень `""` — учётная запись ([`AccountStore::quota`]), ресурс `STORAGE`
//!   в КиБ; без лимита корней нет. `SETQUOTA` — `NO [NOPERM]`: квоты задаёт администратор (политики).
//! - `GETACL`, `SETACL` (`+`/`-` — добавить/убрать права; устаревшие `c` и `d` — как `k` и `xte`, RFC 4314,
//!   2.1.1), `DELETEACL`, `LISTRIGHTS` — с правом `a`; `MYRIGHTS` — права субъекта. Права считает единый
//!   слой авторизации ([`Authorizer`]); владелец имеет все права, его права не меняются.
//!
//! Зависимости расширений необязательны ([`Extensions`], `Server::with_extensions`): без хранилища учётных
//! записей нет QUOTA, без ACL и авторизации — ACL и общих папок; возможности не объявляются.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;

use tp_contracts::acl::{ANYONE, AclStore, validate_identifier};
use tp_contracts::auth::{Authorizer, Principal, Resource, Rights};
use tp_contracts::meta::{AccountScope, AccountStore};
use tp_contracts::{Error, Result};

use crate::mailbox::{DELIMITER, Node, Tree, attr, canonical, matches};
use crate::proto::types::core::{AString, IString, QuotedChar, Tag, Vec1};
use crate::proto::types::extensions::namespace::Namespace;
use crate::proto::types::extensions::quota::{QuotaGet, Resource as QuotaResource};
use crate::proto::types::mailbox::Mailbox as ImapMailbox;
use crate::proto::types::response::Data;
use crate::proto::{self, AclCommand};
use crate::session::{Session, Transport};

/// Необязательные зависимости расширений.
#[derive(Clone, Default)]
pub struct Extensions {
    /// Учётные записи: квота (QUOTA) и имена входа владельцев общих папок.
    pub accounts: Option<Arc<dyn AccountStore>>,
    /// ACL папок (`GETACL`, `SETACL`, общие папки).
    pub acl: Option<Arc<dyn AclStore>>,
    /// Единый слой авторизации (`MYRIGHTS`, права на общие папки).
    pub authz: Option<Arc<dyn Authorizer>>,
}

impl fmt::Debug for Extensions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Extensions")
            .field("quota", &self.accounts.is_some())
            .field("acl", &self.acl_enabled())
            .finish()
    }
}

impl Extensions {
    /// ACL и общие папки доступны.
    pub(crate) fn acl_enabled(&self) -> bool {
        self.acl.is_some() && self.authz.is_some() && self.accounts.is_some()
    }

    /// Возможности расширений для `CAPABILITY`.
    pub(crate) fn capabilities(&self) -> Vec<&'static str> {
        let mut caps = Vec::new();
        if self.accounts.is_some() {
            caps.extend(["QUOTA", "QUOTA=RES-STORAGE"]);
        }
        if self.acl_enabled() {
            caps.extend(["ACL", "RIGHTS=texk"]);
        }
        caps
    }
}

/// Идентификаторы субъекта в ACL: имя входа и группы, допустимые в ACL, без `anyone`.
fn identifiers(p: &Principal) -> Vec<String> {
    std::iter::once(&p.login)
        .chain(&p.groups)
        .filter(|id| !id.eq_ignore_ascii_case(ANYONE) && validate_identifier(id, false).is_ok())
        .cloned()
        .collect()
}

/// Права `SETACL`: `+`/`-` в начале — добавить/убрать; `c` — `k`, `d` — `xte` (RFC 4314, 2.1.1).
fn parse_rights(raw: &str) -> Option<(char, Rights)> {
    let (mode, letters) = match raw.as_bytes().first() {
        Some(b'+') => ('+', &raw[1..]),
        Some(b'-') => ('-', &raw[1..]),
        _ => ('=', raw),
    };
    let letters: String = letters
        .chars()
        .map(|c| match c {
            'c' => "k".to_owned(),
            'd' => "xte".to_owned(),
            c => c.to_string(),
        })
        .collect();
    letters.parse().ok().map(|r| (mode, r))
}

/// Общая папка: путь без учёта регистра имени входа и `INBOX`.
fn shared_key(path: &str) -> String {
    let mut parts = path.splitn(3, DELIMITER);
    let root = parts.next().unwrap_or_default();
    let login = parts.next().unwrap_or_default().to_lowercase();
    match parts.next() {
        Some(rest) => format!("{root}{DELIMITER}{login}{DELIMITER}{}", canonical(rest)),
        None => format!("{root}{DELIMITER}{login}"),
    }
}

impl<T: Transport> Session<T> {
    fn root(&self) -> &str {
        &self.ctx.config.namespace.other_users
    }

    // ---------- Общие папки ----------

    /// Общие папки других учётных записей, доступные субъекту (с правом `l`), по возрастанию пути.
    async fn shared_nodes(&self) -> Result<Vec<Node>> {
        let (Some(acl), Some(accounts), Some(p)) = (
            self.ext.acl.as_ref(),
            self.ext.accounts.as_ref(),
            self.principal.as_ref(),
        ) else {
            return Ok(Vec::new());
        };
        if !self.ext.acl_enabled() {
            return Ok(Vec::new());
        }
        let mut by_owner: BTreeMap<_, Vec<_>> = BTreeMap::new();
        for s in acl.shared_mailboxes(p.tenant, &identifiers(p)).await? {
            if s.account != p.account && s.rights.contains(Rights::LOOKUP) {
                by_owner.entry(s.account).or_default().push(s.mailbox);
            }
        }
        let mut out = Vec::new();
        for (account, mailboxes) in by_owner {
            let Some(owner) = accounts.get(p.tenant, account).await? else {
                continue;
            };
            let scope = AccountScope {
                tenant: p.tenant,
                account,
            };
            let tree = Tree::new(self.ctx.deps.mailboxes.list(scope).await?);
            let mut nodes: Vec<Node> = tree
                .0
                .into_iter()
                .filter(|n| mailboxes.contains(&n.mailbox.id))
                .map(|mut n| {
                    n.path = format!(
                        "{}{DELIMITER}{}{DELIMITER}{}",
                        self.root(),
                        owner.login,
                        n.path
                    );
                    n.owner = Some(scope);
                    n
                })
                .collect();
            // Вложенные — только видимые субъекту.
            let paths: Vec<String> = nodes.iter().map(|n| format!("{}/", n.path)).collect();
            for n in &mut nodes {
                let prefix = format!("{}/", n.path);
                n.children = paths.iter().any(|p| p != &prefix && p.starts_with(&prefix));
            }
            out.extend(nodes);
        }
        out.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(out)
    }

    /// Общая папка по полному имени (`<other_users>/<имя входа>/<папка>`).
    pub(crate) async fn shared_node(&self, name: &str) -> Result<Option<Node>> {
        if !self.ext.acl_enabled() || !name.starts_with(&format!("{}{DELIMITER}", self.root())) {
            return Ok(None);
        }
        let key = shared_key(name);
        Ok(self
            .shared_nodes()
            .await?
            .into_iter()
            .find(|n| shared_key(&n.path) == key))
    }

    /// Действующие права субъекта на папку (своя — все права).
    pub(crate) async fn shared_rights(&self, node: &Node) -> Result<Rights> {
        let p = self.principal.as_ref().ok_or(Error::Forbidden)?;
        let owner = node.owner.map_or(p.account, |o| o.account);
        match self.ext.authz.as_ref() {
            Some(authz) => {
                let resource = Resource::Mailbox {
                    account: owner,
                    mailbox: node.mailbox.id,
                };
                authz.rights(p, &resource).await
            }
            None if owner == p.account => Ok(Rights::ALL),
            None => Ok(Rights::NONE),
        }
    }

    /// Общие папки и промежуточные уровни (`\NoSelect`) для `LIST` по шаблонам.
    pub(crate) async fn list_shared(&mut self, patterns: &[Vec<char>]) -> Result<()> {
        if !self.ext.acl_enabled() {
            return Ok(());
        }
        let nodes = self.shared_nodes().await?;
        if nodes.is_empty() {
            return Ok(());
        }
        let root = self.root().to_owned();
        // (путь, атрибуты): корень, владельцы, папки.
        let mut entries: Vec<(String, Vec<&'static str>)> =
            vec![(root.clone(), vec!["NoSelect", "HasChildren"])];
        for n in &nodes {
            let owner_path = n
                .path
                .splitn(3, DELIMITER)
                .take(2)
                .collect::<Vec<_>>()
                .join("/");
            if !entries.iter().any(|(p, _)| *p == owner_path) {
                entries.push((owner_path, vec!["NoSelect", "HasChildren"]));
            }
            let children = if n.children {
                "HasChildren"
            } else {
                "HasNoChildren"
            };
            entries.push((n.path.clone(), vec![children]));
        }
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        for (path, attrs) in entries {
            let name: Vec<char> = path.chars().collect();
            if !patterns.iter().any(|p| matches(p, &name)) {
                continue;
            }
            let Some(mailbox) = proto::astring(self.encode_name(&path)).map(ImapMailbox::from)
            else {
                continue;
            };
            self.data(Data::List {
                items: attrs.into_iter().map(attr).collect(),
                delimiter: QuotedChar::try_from(DELIMITER).ok(),
                mailbox,
            });
            self.maybe_flush().await?;
        }
        Ok(())
    }

    // ---------- NAMESPACE ----------

    pub(crate) fn namespace(&mut self, tag: &Tag<'static>) {
        let ns = |prefix: &str| -> Option<Namespace<'static>> {
            let prefix = IString::try_from(prefix.to_owned()).ok()?;
            Some(Namespace::new(prefix, QuotedChar::try_from(DELIMITER).ok()))
        };
        let personal = ns("").into_iter().collect();
        let other = if self.ext.acl_enabled() {
            let prefix =
                String::from_utf8_lossy(&self.encode_name(&format!("{}{DELIMITER}", self.root())))
                    .into_owned();
            ns(&prefix).into_iter().collect()
        } else {
            Vec::new()
        };
        self.data(Data::Namespace {
            personal,
            other,
            shared: Vec::new(),
        });
        self.ok(tag, None, "NAMESPACE completed");
    }

    // ---------- QUOTA ----------

    /// Квота учётной записи (`STORAGE` в КиБ); `None` — лимита нет (корня квоты нет).
    async fn quota(&self) -> Result<Option<QuotaGet<'static>>> {
        let (Some(accounts), Some(scope)) = (self.ext.accounts.as_ref(), self.scope()) else {
            return Ok(None);
        };
        let q = accounts.quota(scope.tenant, scope.account).await?;
        Ok(q.limit_bytes.map(|limit| {
            QuotaGet::new(
                QuotaResource::Storage,
                q.used_bytes.div_ceil(1024),
                limit / 1024,
            )
        }))
    }

    fn quota_data(&mut self, quota: QuotaGet<'static>) {
        if let Ok(root) = AString::try_from("") {
            self.data(Data::Quota {
                root,
                quotas: Vec1::from(quota),
            });
        }
    }

    pub(crate) async fn get_quota_root(
        &mut self,
        tag: &Tag<'static>,
        mailbox: &ImapMailbox<'static>,
    ) -> Result<()> {
        if self.ext.accounts.is_none() {
            self.bad(Some(tag), "Command not supported");
            return Ok(());
        }
        let Some(node) = self
            .resolve_any(tag, &proto::mailbox_bytes(mailbox))
            .await?
        else {
            return Ok(());
        };
        // Общая папка — квота владельца, корень которой субъекту не виден.
        let quota = match node.owner {
            None => self.quota().await?,
            Some(_) => None,
        };
        let roots = match quota {
            Some(_) => AString::try_from("").ok().into_iter().collect(),
            None => Vec::new(),
        };
        self.data(Data::QuotaRoot {
            mailbox: self.imap_mailbox(&node.path),
            roots,
        });
        if let Some(q) = quota {
            self.quota_data(q);
        }
        self.ok(tag, None, "GETQUOTAROOT completed");
        Ok(())
    }

    pub(crate) async fn get_quota(
        &mut self,
        tag: &Tag<'static>,
        root: &AString<'static>,
    ) -> Result<()> {
        if self.ext.accounts.is_none() {
            self.bad(Some(tag), "Command not supported");
            return Ok(());
        }
        let quota = if root.as_ref().is_empty() {
            self.quota().await?
        } else {
            None
        };
        let Some(q) = quota else {
            self.no(tag, Some(proto::code("NONEXISTENT")), "No such quota root");
            return Ok(());
        };
        self.quota_data(q);
        self.ok(tag, None, "GETQUOTA completed");
        Ok(())
    }

    pub(crate) fn set_quota(&mut self, tag: &Tag<'static>) {
        if self.ext.accounts.is_none() {
            self.bad(Some(tag), "Command not supported");
            return;
        }
        self.no(
            tag,
            Some(proto::code("NOPERM")),
            "Quotas are set by the administrator",
        );
    }

    // ---------- ACL ----------

    /// Имя входа владельца папки.
    async fn owner_login(&self, node: &Node) -> Result<String> {
        let p = self.principal.as_ref().ok_or(Error::Forbidden)?;
        let Some(owner) = node.owner else {
            return Ok(p.login.clone());
        };
        let accounts = self.ext.accounts.as_ref().ok_or(Error::NotFound)?;
        Ok(accounts
            .get(owner.tenant, owner.account)
            .await?
            .ok_or(Error::NotFound)?
            .login)
    }

    fn untagged_line(&mut self, words: &[&[u8]]) {
        self.out.extend_from_slice(b"*");
        for w in words {
            self.out.push(b' ');
            self.out.extend_from_slice(w);
        }
        self.out.extend_from_slice(b"\r\n");
    }

    pub(crate) async fn acl_command(&mut self, tag: &Tag<'static>, cmd: AclCommand) -> Result<()> {
        let Some(acl) = self.ext.acl.clone().filter(|_| self.ext.acl_enabled()) else {
            self.bad(Some(tag), "Command not supported");
            return Ok(());
        };
        let raw = match &cmd {
            AclCommand::Set { mailbox, .. }
            | AclCommand::Delete { mailbox, .. }
            | AclCommand::Get { mailbox }
            | AclCommand::ListRights { mailbox, .. }
            | AclCommand::MyRights { mailbox } => mailbox.clone(),
        };
        let Some(node) = self.resolve_any(tag, &raw).await? else {
            return Ok(());
        };
        let rights = self.shared_rights(&node).await?;
        let name = proto::astring_text(&self.encode_name(&node.path));
        if let AclCommand::MyRights { .. } = cmd {
            let r = rights.to_string();
            self.untagged_line(&[b"MYRIGHTS", &name, &proto::astring_text(r.as_bytes())]);
            self.ok(tag, None, "MYRIGHTS completed");
            return Ok(());
        }
        // RFC 4314, 4: остальные команды — с правом `a`.
        if !rights.contains(Rights::ADMIN) {
            self.no(tag, Some(proto::code("NOPERM")), "Permission denied");
            return Ok(());
        }
        let scope = match node.owner {
            Some(owner) => owner,
            None => self.scope().ok_or(Error::Forbidden)?,
        };
        let owner_login = self.owner_login(&node).await?;
        let mailbox = node.mailbox.id;
        match cmd {
            AclCommand::Get { .. } => {
                let entries = acl.acl(scope, mailbox).await?;
                let mut words: Vec<Vec<u8>> = vec![
                    b"ACL".to_vec(),
                    name,
                    proto::astring_text(owner_login.as_bytes()),
                    Rights::ALL.to_string().into_bytes(),
                ];
                for e in entries {
                    words.push(proto::astring_text(e.identifier.as_bytes()));
                    words.push(proto::astring_text(e.rights.to_string().as_bytes()));
                }
                let refs: Vec<&[u8]> = words.iter().map(Vec::as_slice).collect();
                self.untagged_line(&refs);
                self.ok(tag, None, "GETACL completed");
            }
            AclCommand::ListRights { identifier, .. } => {
                let id = proto::astring_text(&identifier);
                if identifier == owner_login.as_bytes() {
                    let all = Rights::ALL.to_string();
                    self.untagged_line(&[b"LISTRIGHTS", &name, &id, all.as_bytes()]);
                } else {
                    let mut words: Vec<Vec<u8>> =
                        vec![b"LISTRIGHTS".to_vec(), name, id, b"\"\"".to_vec()];
                    words.extend(Rights::ALL.to_string().bytes().map(|c| vec![c]));
                    let refs: Vec<&[u8]> = words.iter().map(Vec::as_slice).collect();
                    self.untagged_line(&refs);
                }
                self.ok(tag, None, "LISTRIGHTS completed");
            }
            AclCommand::Set {
                identifier, rights, ..
            } => {
                let (Ok(identifier), Some((mode, rights))) = (
                    String::from_utf8(identifier),
                    std::str::from_utf8(&rights).ok().and_then(parse_rights),
                ) else {
                    self.bad(Some(tag), "Invalid identifier or rights");
                    return Ok(());
                };
                if identifier == owner_login {
                    self.no(
                        tag,
                        Some(proto::code("CANNOT")),
                        "Owner rights cannot be changed",
                    );
                    return Ok(());
                }
                let current = acl
                    .acl(scope, mailbox)
                    .await?
                    .into_iter()
                    .find(|e| e.identifier == identifier)
                    .map_or(Rights::NONE, |e| e.rights);
                let new = match mode {
                    '+' => current | rights,
                    '-' => current - rights,
                    _ => rights,
                };
                acl.set_acl(scope, mailbox, &identifier, new).await?;
                self.ok(tag, None, "SETACL completed");
            }
            AclCommand::Delete { identifier, .. } => {
                let Ok(identifier) = String::from_utf8(identifier) else {
                    self.bad(Some(tag), "Invalid identifier");
                    return Ok(());
                };
                if identifier == owner_login {
                    self.no(
                        tag,
                        Some(proto::code("CANNOT")),
                        "Owner rights cannot be changed",
                    );
                    return Ok(());
                }
                acl.set_acl(scope, mailbox, &identifier, Rights::NONE)
                    .await?;
                self.ok(tag, None, "DELETEACL completed");
            }
            AclCommand::MyRights { .. } => {}
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rights_modes() {
        let (m, r) = parse_rights("+lr").expect("rights");
        assert_eq!((m, r.to_string().as_str()), ('+', "lr"));
        let (m, r) = parse_rights("-w").expect("rights");
        assert_eq!((m, r.to_string().as_str()), ('-', "w"));
        // Устаревшие права RFC 2086.
        let (m, r) = parse_rights("lrcd").expect("rights");
        assert_eq!((m, r.to_string().as_str()), ('=', "lrkxte"));
        assert!(parse_rights("lz").is_none());
    }

    #[test]
    fn shared_key_ignores_case() {
        assert_eq!(
            shared_key("Other Users/Bob@x.test/inbox/Sub"),
            shared_key("Other Users/bob@x.test/INBOX/Sub")
        );
        assert_ne!(
            shared_key("Other Users/bob/Sub"),
            shared_key("Other Users/bob/sub")
        );
    }
}
