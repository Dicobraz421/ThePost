//! Операции с папками (RFC 9051, 6.3): `LIST` (LIST-EXTENDED, SPECIAL-USE, LIST-STATUS, CHILDREN), `LSUB`,
//! `SELECT`/`EXAMINE`, `UNSELECT`, `STATUS` (с `SIZE`, `DELETED`), `CREATE` (с `USE`), `DELETE`, `RENAME`,
//! `SUBSCRIBE`/`UNSUBSCRIBE`.
//!
//! Иерархия — `parent` папок хранилища, разделитель `/`; полное имя строит протокол, `INBOX` — папка
//! с назначением «Входящие» (имя без учёта регистра). Имена — в UTF-8 после `ENABLE IMAP4rev2`, иначе
//! в модифицированном UTF-7 (IMAP4rev1). `CREATE` и `RENAME` создают недостающие промежуточные папки.
//! Удаление папки с вложенными — `NO [HASCHILDREN]` (хранилище удаляет поддерево только по подтверждению,
//! которого в IMAP нет); системные папки не удаляются (`NO [CANNOT]`).

use std::num::NonZeroU64;

use tp_contracts::meta::{Flag, Mailbox, MessageQuery, NewMailbox, SpecialUse};
use tp_contracts::{Error, MailboxId, Result};

use crate::proto::types::core::{Atom, QuotedChar, Tag};
use crate::proto::types::flag::{Flag as ImapFlag, FlagNameAttribute, FlagPerm};
use crate::proto::types::mailbox::Mailbox as ImapMailbox;
use crate::proto::types::response::{Code, Data, StatusKind};
use crate::proto::{self, ListCommand, StatusItem};
use crate::session::{Selected, Session, Transport};

/// Разделитель иерархии.
pub(crate) const DELIMITER: char = '/';

/// Папка с полным именем.
#[derive(Debug, Clone)]
pub(crate) struct Node {
    pub mailbox: Mailbox,
    /// Полное имя (UTF-8), `INBOX` — в верхнем регистре.
    pub path: String,
    pub children: bool,
}

/// Папки учётной записи с полными именами.
#[derive(Debug, Clone)]
pub(crate) struct Tree(pub Vec<Node>);

impl Tree {
    fn new(mut boxes: Vec<Mailbox>) -> Self {
        boxes.sort_by_key(|m| m.id);
        let path_of = |m: &Mailbox| -> String {
            let mut parts = Vec::new();
            let mut cur = Some(m);
            // Защита от циклов: глубина не больше числа папок.
            for _ in 0..=boxes.len() {
                let Some(b) = cur else { break };
                parts.push(if b.role == Some(SpecialUse::Inbox) && b.parent.is_none() {
                    "INBOX".to_owned()
                } else {
                    b.name.clone()
                });
                cur = b.parent.and_then(|p| boxes.iter().find(|x| x.id == p));
            }
            parts.reverse();
            parts.join(&DELIMITER.to_string())
        };
        let mut nodes: Vec<Node> = boxes
            .iter()
            .map(|m| Node {
                mailbox: m.clone(),
                path: path_of(m),
                children: boxes.iter().any(|c| c.parent == Some(m.id)),
            })
            .collect();
        nodes.sort_by(|a, b| a.path.cmp(&b.path));
        Self(nodes)
    }

    pub fn find(&self, path: &str) -> Option<&Node> {
        self.0.iter().find(|n| n.path == path)
    }
}

/// `INBOX` в начале имени — в верхнем регистре (RFC 9051, 5.1).
fn canonical(path: &str) -> String {
    let (head, tail) = match path.split_once(DELIMITER) {
        Some((h, t)) => (h, Some(t)),
        None => (path, None),
    };
    if head.eq_ignore_ascii_case("INBOX") {
        match tail {
            Some(t) => format!("INBOX{DELIMITER}{t}"),
            None => "INBOX".to_owned(),
        }
    } else {
        path.to_owned()
    }
}

/// Сопоставление с шаблоном LIST: `*` — любые символы, `%` — любые, кроме разделителя.
fn matches(pattern: &[char], name: &[char]) -> bool {
    match pattern.split_first() {
        None => name.is_empty(),
        Some(('*', rest)) => (0..=name.len()).any(|i| matches(rest, &name[i..])),
        Some(('%', rest)) => (0..=name.len())
            .take_while(|&i| i == 0 || name[i - 1] != DELIMITER)
            .any(|i| matches(rest, &name[i..])),
        Some((c, rest)) => name.first() == Some(c) && matches(rest, &name[1..]),
    }
}

fn role_attr(role: SpecialUse) -> Option<&'static str> {
    Some(match role {
        SpecialUse::Inbox => return None,
        SpecialUse::Archive => "Archive",
        SpecialUse::Drafts => "Drafts",
        SpecialUse::Junk => "Junk",
        SpecialUse::Sent => "Sent",
        SpecialUse::Trash => "Trash",
    })
}

fn role_from_attr(attr: &str) -> Option<SpecialUse> {
    let attr = attr.strip_prefix('\\')?;
    [
        SpecialUse::Archive,
        SpecialUse::Drafts,
        SpecialUse::Junk,
        SpecialUse::Sent,
        SpecialUse::Trash,
    ]
    .into_iter()
    .find(|r| role_attr(*r).is_some_and(|a| a.eq_ignore_ascii_case(attr)))
}

fn attr(name: &'static str) -> FlagNameAttribute<'static> {
    FlagNameAttribute::from(Atom::unvalidated(name))
}

/// Системные флаги: в `FLAGS` и `PERMANENTFLAGS`.
fn system_flags() -> Vec<ImapFlag<'static>> {
    vec![
        ImapFlag::Answered,
        ImapFlag::Flagged,
        ImapFlag::Deleted,
        ImapFlag::Seen,
        ImapFlag::Draft,
    ]
}

impl<T: Transport> Session<T> {
    /// Имя от клиента: UTF-8 (IMAP4rev2) или модифицированный UTF-7.
    fn decode_name(&self, raw: &[u8]) -> Option<String> {
        let name = if self.rev2 {
            String::from_utf8(raw.to_vec()).ok()?
        } else {
            proto::decode_utf7(raw)?
        };
        (!name.chars().any(char::is_control)).then(|| canonical(&name))
    }

    /// Имя для клиента.
    fn encode_name(&self, path: &str) -> Vec<u8> {
        if self.rev2 {
            path.as_bytes().to_vec()
        } else {
            proto::encode_utf7(path).into_bytes()
        }
    }

    fn imap_mailbox(&self, path: &str) -> ImapMailbox<'static> {
        if path == "INBOX" {
            return ImapMailbox::Inbox;
        }
        proto::astring(self.encode_name(path))
            .map(ImapMailbox::from)
            .unwrap_or(ImapMailbox::Inbox)
    }

    pub(crate) async fn tree(&self) -> Result<Tree> {
        let scope = self.scope().ok_or(Error::Forbidden)?;
        Ok(Tree::new(self.ctx.deps.mailboxes.list(scope).await?))
    }

    /// Папка по имени от клиента; иначе — ответ `NO`/`BAD` уже записан.
    async fn resolve(&mut self, tag: &Tag<'static>, raw: &[u8]) -> Result<Option<Node>> {
        let Some(name) = self.decode_name(raw) else {
            self.bad(Some(tag), "Invalid mailbox name");
            return Ok(None);
        };
        let tree = self.tree().await?;
        match tree.find(&name) {
            Some(n) => Ok(Some(n.clone())),
            None => {
                self.no(tag, Some(proto::code("NONEXISTENT")), "No such mailbox");
                Ok(None)
            }
        }
    }

    /// Папка назначения `APPEND`/`COPY`/`MOVE`; нет — `NO [TRYCREATE]` (RFC 9051, 6.3.12) уже записан.
    pub(crate) async fn target(&mut self, tag: &Tag<'static>, raw: &[u8]) -> Result<Option<Node>> {
        let Some(name) = self.decode_name(raw) else {
            self.bad(Some(tag), "Invalid mailbox name");
            return Ok(None);
        };
        let tree = self.tree().await?;
        match tree.find(&name) {
            Some(n) => Ok(Some(n.clone())),
            None => {
                self.no(tag, Some(Code::TryCreate), "No such mailbox");
                Ok(None)
            }
        }
    }

    // ---------- LIST ----------

    pub(crate) async fn list(&mut self, tag: &Tag<'static>, cmd: ListCommand) -> Result<()> {
        let names: Option<Vec<String>> = cmd
            .patterns
            .iter()
            .map(|p| {
                let mut full = cmd.reference.clone();
                full.extend_from_slice(p);
                self.decode_name(&full)
                    .or_else(|| (full.is_empty()).then(String::new))
            })
            .collect();
        let Some(patterns) = names else {
            self.bad(Some(tag), "Invalid mailbox name");
            return Ok(());
        };
        let verb = if cmd.lsub { "LSUB" } else { "LIST" };
        // `LIST "" ""` — разделитель и корень иерархии.
        if !cmd.lsub && patterns.iter().all(String::is_empty) && cmd.patterns.len() == 1 {
            self.data(Data::List {
                items: vec![FlagNameAttribute::Noselect],
                delimiter: QuotedChar::try_from(DELIMITER).ok(),
                mailbox: proto::astring(Vec::new())
                    .map(ImapMailbox::from)
                    .unwrap_or(ImapMailbox::Inbox),
            });
            self.ok(tag, None, "LIST completed");
            return Ok(());
        }
        // `INBOX` в шаблоне — без учёта регистра.
        let patterns: Vec<Vec<char>> = patterns
            .iter()
            .map(|p| {
                let mut chars: Vec<char> = p.chars().collect();
                let head: String = chars.iter().take(5).collect();
                let boundary = matches!(chars.get(5), None | Some('/' | '*' | '%'));
                if boundary && head.eq_ignore_ascii_case("INBOX") {
                    chars[..5].copy_from_slice(&['I', 'N', 'B', 'O', 'X']);
                }
                chars
            })
            .collect();
        let tree = self.tree().await?;
        let hit = |n: &Node| {
            let name: Vec<char> = n.path.chars().collect();
            patterns.iter().any(|p| matches(p, &name))
        };
        let selected_by = |n: &Node| {
            (!(cmd.select_subscribed || cmd.lsub) || n.mailbox.subscribed)
                && (!cmd.select_special_use
                    || n.mailbox.role.is_some_and(|r| role_attr(r).is_some()))
        };
        let extended_select = cmd.select_subscribed || cmd.select_special_use;
        let mut listed: Vec<(Node, bool, bool)> = Vec::new();
        for n in tree.0.iter().filter(|n| hit(n)) {
            let own = selected_by(n);
            // RECURSIVEMATCH: потомок отвечает выбору, но не шаблону (RFC 5258, 3.5).
            let child_info = cmd.recursive && {
                let prefix = format!("{}{DELIMITER}", n.path);
                tree.0
                    .iter()
                    .any(|d| d.path.starts_with(&prefix) && selected_by(d) && !hit(d))
            };
            if own || child_info {
                listed.push((n.clone(), own, child_info));
            }
        }
        for (n, own, child_info) in listed {
            let mut items = Vec::new();
            if !cmd.lsub {
                items.push(attr(if n.children {
                    "HasChildren"
                } else {
                    "HasNoChildren"
                }));
            }
            let subscribed_attr = (cmd.select_subscribed && own)
                || (cmd.extended && cmd.return_subscribed && n.mailbox.subscribed);
            if subscribed_attr {
                items.push(attr("Subscribed"));
            }
            if let Some(a) = n.mailbox.role.and_then(role_attr) {
                items.push(attr(a));
            }
            let mailbox = self.imap_mailbox(&n.path);
            let delimiter = QuotedChar::try_from(DELIMITER).ok();
            let line = if cmd.lsub {
                proto::encode(&crate::proto::types::response::Response::Data(Data::Lsub {
                    items,
                    delimiter,
                    mailbox,
                }))
            } else {
                let extended = if child_info {
                    let what = if cmd.select_subscribed {
                        "SUBSCRIBED"
                    } else {
                        "SPECIAL-USE"
                    };
                    format!("(\"CHILDINFO\" (\"{what}\"))")
                } else {
                    String::new()
                };
                proto::list_line(
                    Data::List {
                        items,
                        delimiter,
                        mailbox,
                    },
                    &extended,
                )
            };
            self.out.extend(line);
            if let Some(status) = &cmd.return_status
                && (own || !extended_select)
            {
                self.status_response(&n, status).await?;
            }
            self.maybe_flush().await?;
        }
        self.ok(tag, None, &format!("{verb} completed"));
        Ok(())
    }

    // ---------- STATUS ----------

    async fn status_response(&mut self, node: &Node, items: &[StatusItem]) -> Result<()> {
        let scope = self.scope().ok_or(Error::Forbidden)?;
        let id = node.mailbox.id;
        let st = self.ctx.deps.mailboxes.status(scope, id).await?;
        let mut deleted = None;
        let mut values = Vec::new();
        for &item in items {
            let v = match item {
                StatusItem::Messages => u64::from(st.messages),
                StatusItem::Recent => 0,
                StatusItem::UidNext => u64::from(st.uid_next.0.max(1)),
                StatusItem::UidValidity => u64::from(st.uid_validity.0.max(1)),
                StatusItem::Unseen => u64::from(st.unseen),
                StatusItem::Size => st.size,
                StatusItem::HighestModSeq => st.highest_modseq.0,
                StatusItem::Deleted => match deleted {
                    Some(d) => d,
                    None => {
                        let limit = self.ctx.config.limits.max_mailbox_messages;
                        let list = self
                            .ctx
                            .deps
                            .messages
                            .list(scope, id, MessageQuery::All, limit)
                            .await?;
                        let d = list
                            .iter()
                            .filter(|m| m.flags.contains(&Flag::Deleted))
                            .count() as u64;
                        deleted = Some(d);
                        d
                    }
                },
            };
            if !values.iter().any(|(i, _)| *i == item) {
                values.push((item, v));
            }
        }
        let name = proto::astring(self.encode_name(&node.path)).ok_or(Error::NotFound)?;
        self.out.extend(proto::status_line(name, &values));
        Ok(())
    }

    pub(crate) async fn status(
        &mut self,
        tag: &Tag<'static>,
        mailbox: &[u8],
        items: &[StatusItem],
    ) -> Result<()> {
        let Some(node) = self.resolve(tag, mailbox).await? else {
            return Ok(());
        };
        self.status_response(&node, items).await?;
        self.ok(tag, None, "STATUS completed");
        Ok(())
    }

    // ---------- SELECT / EXAMINE ----------

    pub(crate) async fn select(
        &mut self,
        tag: &Tag<'static>,
        mailbox: &[u8],
        read_only: bool,
    ) -> Result<()> {
        // Неуспешный SELECT оставляет сессию без выбранной папки (RFC 9051, 6.3.2).
        if self.selected.take().is_some() {
            self.untagged(
                StatusKind::Ok,
                Some(Code::Closed),
                "Previous mailbox is now closed",
            );
        }
        let Some(node) = self.resolve(tag, mailbox).await? else {
            return Ok(());
        };
        let scope = self.scope().ok_or(Error::Forbidden)?;
        let id = node.mailbox.id;
        let limit = self.ctx.config.limits.max_mailbox_messages;
        let list = self
            .ctx
            .deps
            .messages
            .list(scope, id, MessageQuery::All, limit.saturating_add(1))
            .await?;
        if list.len() > limit {
            self.no(tag, Some(proto::code("LIMIT")), "Mailbox is too large");
            return Ok(());
        }
        let st = self.ctx.deps.mailboxes.status(scope, id).await?;
        let first_unseen = list.iter().position(|m| !m.flags.contains(&Flag::Seen));
        self.data(Data::Flags(system_flags()));
        let permanent = if read_only {
            Vec::new()
        } else {
            let mut p: Vec<FlagPerm<'static>> =
                system_flags().into_iter().map(FlagPerm::Flag).collect();
            p.push(FlagPerm::Asterisk);
            p
        };
        self.untagged(
            StatusKind::Ok,
            Some(Code::PermanentFlags(permanent)),
            "Flags permitted",
        );
        self.data(Data::Exists(u32::try_from(list.len()).unwrap_or(u32::MAX)));
        if !self.rev2 {
            self.data(Data::Recent(0));
            if let Some(i) = first_unseen {
                let seq = u32::try_from(i + 1).unwrap_or(u32::MAX);
                self.untagged(
                    StatusKind::Ok,
                    Some(Code::Unseen(proto::nz(seq))),
                    "First unseen",
                );
            }
        }
        self.untagged(
            StatusKind::Ok,
            Some(Code::UidValidity(proto::nz(st.uid_validity.0))),
            "UIDs valid",
        );
        self.untagged(
            StatusKind::Ok,
            Some(Code::UidNext(proto::nz(st.uid_next.0))),
            "Predicted next UID",
        );
        if self.condstore {
            let highest = NonZeroU64::new(st.highest_modseq.0).unwrap_or(NonZeroU64::MIN);
            self.untagged(
                StatusKind::Ok,
                Some(Code::HighestModSeq(highest)),
                "Highest",
            );
        }
        if self.rev2 {
            let mailbox = self.imap_mailbox(&node.path);
            self.data(Data::List {
                items: Vec::new(),
                delimiter: QuotedChar::try_from(DELIMITER).ok(),
                mailbox,
            });
        }
        self.selected = Some(Selected {
            mailbox: id,
            read_only,
            uid_validity: st.uid_validity,
            modseq: st.highest_modseq,
            messages: list.iter().map(|m| (m.uid, m.id)).collect(),
        });
        let (code, verb) = if read_only {
            (Code::ReadOnly, "EXAMINE")
        } else {
            (Code::ReadWrite, "SELECT")
        };
        self.ok(tag, Some(code), &format!("{verb} completed"));
        Ok(())
    }

    pub(crate) fn unselect(&mut self, tag: &Tag<'static>) {
        if self.selected.take().is_none() {
            self.bad(Some(tag), "No mailbox selected");
            return;
        }
        self.ok(tag, None, "UNSELECT completed");
    }

    // ---------- CREATE / DELETE / RENAME ----------

    /// Создаёт недостающие папки пути `parts` (кроме последней) и возвращает родителя последней.
    async fn ensure_parents(&mut self, tree: &Tree, parts: &[&str]) -> Result<Option<MailboxId>> {
        let scope = self.scope().ok_or(Error::Forbidden)?;
        let mut parent: Option<MailboxId> = None;
        let mut path = String::new();
        for part in parts {
            if !path.is_empty() {
                path.push(DELIMITER);
            }
            path.push_str(part);
            parent = Some(match tree.find(&path) {
                Some(n) => n.mailbox.id,
                None => {
                    let data = NewMailbox {
                        parent,
                        name: (*part).to_owned(),
                        role: None,
                    };
                    self.ctx.deps.mailboxes.create(scope, data).await?.id
                }
            });
        }
        Ok(parent)
    }

    /// Имя папки от клиента, разбитое на части; иначе — `BAD` уже записан.
    fn split_name(&mut self, tag: &Tag<'static>, raw: &[u8]) -> Option<Vec<String>> {
        let name = self.decode_name(raw);
        let name = name
            .as_deref()
            .map(|n| n.strip_suffix(DELIMITER).unwrap_or(n));
        let parts: Option<Vec<String>> = name.and_then(|n| {
            let parts: Vec<String> = n.split(DELIMITER).map(str::to_owned).collect();
            parts.iter().all(|p| !p.is_empty()).then_some(parts)
        });
        if parts.is_none() {
            self.bad(Some(tag), "Invalid mailbox name");
        }
        parts
    }

    pub(crate) async fn create(
        &mut self,
        tag: &Tag<'static>,
        mailbox: &[u8],
        uses: &[String],
    ) -> Result<()> {
        let Some(parts) = self.split_name(tag, mailbox) else {
            return Ok(());
        };
        let role = match uses {
            [] => None,
            [one] => match role_from_attr(one) {
                Some(r) => Some(r),
                None => {
                    self.no(tag, Some(proto::code("USEATTR")), "Unsupported special-use");
                    return Ok(());
                }
            },
            _ => {
                self.no(
                    tag,
                    Some(proto::code("USEATTR")),
                    "One special-use per mailbox",
                );
                return Ok(());
            }
        };
        let tree = self.tree().await?;
        let path = parts.join(&DELIMITER.to_string());
        if tree.find(&path).is_some() {
            self.no(
                tag,
                Some(proto::code("ALREADYEXISTS")),
                "Mailbox already exists",
            );
            return Ok(());
        }
        if role.is_some() && tree.0.iter().any(|n| n.mailbox.role == role) {
            self.no(
                tag,
                Some(proto::code("USEATTR")),
                "Special-use already in use",
            );
            return Ok(());
        }
        let refs: Vec<&str> = parts.iter().map(String::as_str).collect();
        let (last, prefix) = refs.split_last().ok_or(Error::NotFound)?;
        let parent = self.ensure_parents(&tree, prefix).await?;
        let scope = self.scope().ok_or(Error::Forbidden)?;
        let data = NewMailbox {
            parent,
            name: (*last).to_owned(),
            role,
        };
        match self.ctx.deps.mailboxes.create(scope, data).await {
            Ok(_) => self.ok(tag, None, "CREATE completed"),
            Err(Error::Conflict) => {
                self.no(
                    tag,
                    Some(proto::code("ALREADYEXISTS")),
                    "Mailbox already exists",
                );
            }
            Err(Error::Invalid(_)) => {
                self.no(tag, Some(proto::code("CANNOT")), "Invalid mailbox name");
            }
            Err(e) => return Err(e),
        }
        Ok(())
    }

    pub(crate) async fn delete(&mut self, tag: &Tag<'static>, mailbox: &[u8]) -> Result<()> {
        let Some(node) = self.resolve(tag, mailbox).await? else {
            return Ok(());
        };
        let scope = self.scope().ok_or(Error::Forbidden)?;
        let id = node.mailbox.id;
        match self.ctx.deps.mailboxes.delete(scope, id, false).await {
            Ok(()) => {
                if self.selected.as_ref().is_some_and(|s| s.mailbox == id) {
                    self.selected = None;
                    self.untagged(StatusKind::Ok, Some(Code::Closed), "Mailbox deleted");
                }
                self.ok(tag, None, "DELETE completed");
            }
            Err(Error::Conflict) => self.no(
                tag,
                Some(proto::code("HASCHILDREN")),
                "Mailbox has children, delete them first",
            ),
            Err(Error::Forbidden) => {
                self.no(
                    tag,
                    Some(proto::code("CANNOT")),
                    "System mailbox can't be deleted",
                );
            }
            Err(e) => return Err(e),
        }
        Ok(())
    }

    pub(crate) async fn rename(
        &mut self,
        tag: &Tag<'static>,
        from: &[u8],
        to: &[u8],
    ) -> Result<()> {
        let Some(node) = self.resolve(tag, from).await? else {
            return Ok(());
        };
        let Some(parts) = self.split_name(tag, to) else {
            return Ok(());
        };
        if node.path == "INBOX" {
            self.no(
                tag,
                Some(proto::code("CANNOT")),
                "Renaming INBOX is not supported",
            );
            return Ok(());
        }
        let tree = self.tree().await?;
        let path = parts.join(&DELIMITER.to_string());
        if tree.find(&path).is_some() {
            self.no(
                tag,
                Some(proto::code("ALREADYEXISTS")),
                "Mailbox already exists",
            );
            return Ok(());
        }
        if path.starts_with(&format!("{}{DELIMITER}", node.path)) {
            self.no(
                tag,
                Some(proto::code("CANNOT")),
                "Can't move mailbox into itself",
            );
            return Ok(());
        }
        let refs: Vec<&str> = parts.iter().map(String::as_str).collect();
        let (last, prefix) = refs.split_last().ok_or(Error::NotFound)?;
        let parent = self.ensure_parents(&tree, prefix).await?;
        let scope = self.scope().ok_or(Error::Forbidden)?;
        match self
            .ctx
            .deps
            .mailboxes
            .rename(scope, node.mailbox.id, parent, last)
            .await
        {
            Ok(()) => self.ok(tag, None, "RENAME completed"),
            Err(Error::Conflict) => {
                self.no(
                    tag,
                    Some(proto::code("ALREADYEXISTS")),
                    "Mailbox already exists",
                );
            }
            Err(Error::Forbidden | Error::Invalid(_)) => {
                self.no(tag, Some(proto::code("CANNOT")), "Mailbox can't be renamed");
            }
            Err(e) => return Err(e),
        }
        Ok(())
    }

    pub(crate) async fn subscribe(
        &mut self,
        tag: &Tag<'static>,
        mailbox: &[u8],
        on: bool,
    ) -> Result<()> {
        let Some(node) = self.resolve(tag, mailbox).await? else {
            return Ok(());
        };
        let scope = self.scope().ok_or(Error::Forbidden)?;
        self.ctx
            .deps
            .mailboxes
            .set_subscribed(scope, node.mailbox.id, on)
            .await?;
        let verb = if on { "SUBSCRIBE" } else { "UNSUBSCRIBE" };
        self.ok(tag, None, &format!("{verb} completed"));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(p: &str, n: &str) -> bool {
        let p: Vec<char> = p.chars().collect();
        let n: Vec<char> = n.chars().collect();
        matches(&p, &n)
    }

    #[test]
    fn wildcards() {
        assert!(m("*", "a/b/c"));
        assert!(m("%", "a") && !m("%", "a/b"));
        assert!(m("a/%", "a/b") && !m("a/%", "a/b/c"));
        assert!(m("a/*", "a/b/c") && !m("a/*", "a"));
        assert!(m("%/b", "a/b") && m("*b", "a/b"));
        assert!(m("Входящие*", "Входящие/Архив"));
        assert!(!m("", "a") && m("", ""));
    }

    #[test]
    fn canonical_inbox() {
        assert_eq!(canonical("inbox"), "INBOX");
        assert_eq!(canonical("InBox/Sub"), "INBOX/Sub");
        assert_eq!(canonical("Inboxes"), "Inboxes");
    }
}
