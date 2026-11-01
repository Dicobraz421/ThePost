//! Протокол IMAP4rev2 (RFC 9051): разбор команд клиента и сериализация ответов сервера.
//!
//! Основа — `imap-codec`: грамматика IMAP4rev1 с расширениями (LITERAL+, IDLE, ENABLE, ID, MOVE, BINARY,
//! UIDPLUS, CONDSTORE/QRESYNC, QUOTA, NAMESPACE, COMPRESS) и нарезка потока на строки и литералы
//! (`fragmentizer`): границы сообщения известны и при битом разборе, литерал не исполняется как команда.
//! Чего нет в imap-codec из IMAP4rev2 — расширенный `LIST`, `CREATE … (USE …)`, `STATUS … SIZE` — разбирает
//! [`rev2`]; ответы с `SIZE` и `CHILDINFO` дописываются к сериализации imap-codec ([`status_line`],
//! [`list_line`]).
//!
//! [`Reader`] собирает сообщения из байтов соединения: синхронизирующий литерал — [`Input::Literal`]
//! (сессия отвечает `+` или отказывает), полное сообщение — [`Input::Command`] и т. п.

use std::num::NonZeroU32;

use imap_codec::encode::Encoder;
use imap_codec::fragmentizer::{DecodeMessageError, FragmentInfo, Fragmentizer, LineEnding};
use imap_codec::{
    AuthenticateDataCodec, CommandCodec, GreetingCodec, IdleDoneCodec, ResponseCodec,
};

pub use imap_codec::imap_types as types;

use types::IntoStatic;
use types::auth::AuthenticateData;
use types::command::{Command as ImapCommand, CommandBody};
use types::core::{AString, LiteralMode, Tag, Text};
use types::mailbox::{ListMailbox, Mailbox};
use types::response::{Code, CodeOther, Data, Greeting, Response, Status};
use types::status::StatusDataItemName;

mod rev2;
#[cfg(test)]
mod tests;
mod utf7;

pub use utf7::{decode_utf7, encode_utf7};

/// Команда клиента.
#[derive(Debug, Clone, PartialEq)]
pub struct Command {
    /// Тег.
    pub tag: Tag<'static>,
    /// Команда.
    pub body: Body,
}

/// Тело команды. `LIST`/`LSUB`, `CREATE`, `STATUS` приводятся к общему виду с расширениями IMAP4rev2;
/// остальное — как разобрал imap-codec.
#[derive(Debug, Clone, PartialEq)]
pub enum Body {
    /// Команда из грамматики imap-codec.
    Std(CommandBody<'static>),
    /// `LIST` (RFC 9051, RFC 5258, 5819, 6154) или `LSUB`.
    List(ListCommand),
    /// `CREATE` с атрибутами назначения (RFC 6154 `USE`).
    Create {
        /// Имя папки как передал клиент.
        mailbox: Vec<u8>,
        /// Атрибуты назначения (`\SENT`, ...) в верхнем регистре.
        uses: Vec<String>,
    },
    /// `STATUS`.
    Status {
        /// Имя папки как передал клиент.
        mailbox: Vec<u8>,
        /// Запрошенные элементы.
        items: Vec<StatusItem>,
    },
}

/// `LIST`/`LSUB` с опциями расширенного `LIST`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ListCommand {
    /// `LSUB` (IMAP4rev1).
    pub lsub: bool,
    /// Расширенная форма (RFC 5258): ответ содержит `\Subscribed` и т. п. только по опциям.
    pub extended: bool,
    /// Ссылка (reference) как передал клиент.
    pub reference: Vec<u8>,
    /// Шаблоны с `*` и `%`.
    pub patterns: Vec<Vec<u8>>,
    /// Опция выбора `SUBSCRIBED`.
    pub select_subscribed: bool,
    /// Опция выбора `SPECIAL-USE` (RFC 6154).
    pub select_special_use: bool,
    /// Опция выбора `RECURSIVEMATCH`.
    pub recursive: bool,
    /// `RETURN (SUBSCRIBED)`.
    pub return_subscribed: bool,
    /// `RETURN (CHILDREN)`.
    pub return_children: bool,
    /// `RETURN (SPECIAL-USE)`.
    pub return_special_use: bool,
    /// `RETURN (STATUS (...))` (RFC 5819).
    pub return_status: Option<Vec<StatusItem>>,
}

/// Элемент `STATUS`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StatusItem {
    /// `MESSAGES`.
    Messages,
    /// `RECENT` (только IMAP4rev1, всегда 0).
    Recent,
    /// `UIDNEXT`.
    UidNext,
    /// `UIDVALIDITY`.
    UidValidity,
    /// `UNSEEN`.
    Unseen,
    /// `DELETED` (RFC 9051).
    Deleted,
    /// `SIZE` (RFC 8438).
    Size,
    /// `HIGHESTMODSEQ` (RFC 7162).
    HighestModSeq,
}

impl StatusItem {
    /// Имя элемента в ответе.
    pub fn name(self) -> &'static str {
        match self {
            StatusItem::Messages => "MESSAGES",
            StatusItem::Recent => "RECENT",
            StatusItem::UidNext => "UIDNEXT",
            StatusItem::UidValidity => "UIDVALIDITY",
            StatusItem::Unseen => "UNSEEN",
            StatusItem::Deleted => "DELETED",
            StatusItem::Size => "SIZE",
            StatusItem::HighestModSeq => "HIGHESTMODSEQ",
        }
    }

    fn parse(name: &str) -> Option<Self> {
        [
            StatusItem::Messages,
            StatusItem::Recent,
            StatusItem::UidNext,
            StatusItem::UidValidity,
            StatusItem::Unseen,
            StatusItem::Deleted,
            StatusItem::Size,
            StatusItem::HighestModSeq,
        ]
        .into_iter()
        .find(|i| i.name().eq_ignore_ascii_case(name))
    }

    fn from_codec(name: &StatusDataItemName) -> Option<Self> {
        Some(match name {
            StatusDataItemName::Messages => StatusItem::Messages,
            StatusDataItemName::Recent => StatusItem::Recent,
            StatusDataItemName::UidNext => StatusItem::UidNext,
            StatusDataItemName::UidValidity => StatusItem::UidValidity,
            StatusDataItemName::Unseen => StatusItem::Unseen,
            StatusDataItemName::Deleted => StatusItem::Deleted,
            StatusDataItemName::HighestModSeq => StatusItem::HighestModSeq,
            StatusDataItemName::DeletedStorage => return None,
        })
    }
}

/// Имя папки из команды как байты (`INBOX` — в верхнем регистре).
pub fn mailbox_bytes(mailbox: &Mailbox<'_>) -> Vec<u8> {
    match mailbox {
        Mailbox::Inbox => b"INBOX".to_vec(),
        Mailbox::Other(other) => other.inner().as_ref().to_vec(),
    }
}

fn list_mailbox_bytes(m: &ListMailbox<'_>) -> Vec<u8> {
    match m {
        ListMailbox::Token(t) => t.as_ref().to_vec(),
        ListMailbox::String(s) => s.as_ref().to_vec(),
    }
}

impl Command {
    fn from_codec(cmd: ImapCommand<'static>) -> Option<Self> {
        let body = match cmd.body {
            CommandBody::List {
                reference,
                mailbox_wildcard,
            } => Body::List(ListCommand {
                reference: mailbox_bytes(&reference),
                patterns: vec![list_mailbox_bytes(&mailbox_wildcard)],
                ..ListCommand::default()
            }),
            CommandBody::Lsub {
                reference,
                mailbox_wildcard,
            } => Body::List(ListCommand {
                lsub: true,
                reference: mailbox_bytes(&reference),
                patterns: vec![list_mailbox_bytes(&mailbox_wildcard)],
                ..ListCommand::default()
            }),
            CommandBody::Create { mailbox } => Body::Create {
                mailbox: mailbox_bytes(&mailbox),
                uses: Vec::new(),
            },
            CommandBody::Status {
                mailbox,
                item_names,
            } => Body::Status {
                mailbox: mailbox_bytes(&mailbox),
                items: item_names
                    .iter()
                    .map(StatusItem::from_codec)
                    .collect::<Option<_>>()?,
            },
            body => Body::Std(body),
        };
        Some(Command { tag: cmd.tag, body })
    }
}

/// Что ожидает сессия следующим сообщением.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Expect {
    /// Команду.
    Command,
    /// Ответ клиента в `AUTHENTICATE`.
    AuthData,
    /// `DONE` после `IDLE`.
    IdleDone,
}

/// Результат разбора входящих байтов.
#[derive(Debug, Clone, PartialEq)]
pub enum Input {
    /// Команда.
    Command(Command),
    /// Ответ клиента в `AUTHENTICATE` (байты после base64); `None` — отмена (`*`).
    AuthData(Option<Vec<u8>>),
    /// `DONE`.
    IdleDone,
    /// Объявлен литерал (до его байтов): синхронизирующий требует `+` или отказа
    /// ([`Reader::reject_literal`]); `length` больше лимита — сессия отказывает.
    Literal {
        /// Тег команды, если разобрался.
        tag: Option<Tag<'static>>,
        /// Длина литерала.
        length: u32,
        /// Синхронизирующий (`{n}`), иначе LITERAL+ (`{n+}`).
        sync: bool,
    },
    /// Сообщение не разобрано и отброшено.
    Bad {
        /// Тег, если разобрался.
        tag: Option<Tag<'static>>,
        /// Причина для ответа `BAD`.
        reason: &'static str,
    },
    /// Сообщение длиннее лимита и отброшено.
    TooLong {
        /// Тег, если разобрался.
        tag: Option<Tag<'static>>,
    },
}

/// Сборка сообщений клиента из потока байтов.
#[derive(Debug)]
pub struct Reader {
    frag: Fragmentizer,
    bad_line_ending: bool,
}

impl Reader {
    /// Читатель с лимитом размера сообщения (строки с литералами), байт.
    pub fn new(max_message: u32) -> Self {
        Self {
            frag: Fragmentizer::new(max_message),
            bad_line_ending: false,
        }
    }

    /// Добавляет прочитанные байты.
    pub fn push(&mut self, data: &[u8]) {
        self.frag.enqueue_bytes(data);
    }

    /// Отказ в синхронизирующем литерале: клиент его не пришлёт, сообщение отбрасывается.
    pub fn reject_literal(&mut self) {
        self.frag.skip_message();
        self.bad_line_ending = false;
    }

    /// Следующее событие; `None` — нужны ещё байты.
    pub fn next(&mut self, expect: Expect) -> Option<Input> {
        loop {
            match self.frag.progress()? {
                FragmentInfo::Literal { .. } => continue,
                FragmentInfo::Line {
                    announcement,
                    ending,
                    ..
                } => {
                    // RFC 9051, 2.2: строки завершаются CRLF.
                    if ending == LineEnding::Lf {
                        self.frag.poison_message();
                        self.bad_line_ending = true;
                    }
                    if let Some(a) = announcement {
                        return Some(Input::Literal {
                            tag: self.tag(),
                            length: a.length,
                            sync: a.mode == LiteralMode::Sync,
                        });
                    }
                    let input = self.decode(expect);
                    self.bad_line_ending = false;
                    return Some(input);
                }
            }
        }
    }

    fn tag(&self) -> Option<Tag<'static>> {
        self.frag.decode_tag().map(IntoStatic::into_static)
    }

    fn decode(&self, expect: Expect) -> Input {
        let bad = |reason| Input::Bad {
            tag: self.tag(),
            reason,
        };
        match expect {
            Expect::Command => match self.frag.decode_message(&CommandCodec::default()) {
                Ok(cmd) => Command::from_codec(cmd.into_static())
                    .map_or_else(|| bad("Unsupported item"), Input::Command),
                Err(DecodeMessageError::MessageTooLong { .. }) => {
                    Input::TooLong { tag: self.tag() }
                }
                Err(DecodeMessageError::MessagePoisoned { .. }) => bad("Expected CRLF"),
                Err(_) => rev2::parse(self.frag.message_bytes())
                    .map_or_else(|| bad("Syntax error"), Input::Command),
            },
            Expect::AuthData => match self.frag.decode_message(&AuthenticateDataCodec::default()) {
                Ok(AuthenticateData::Continue(data)) => {
                    Input::AuthData(Some(data.declassify().to_vec()))
                }
                Ok(AuthenticateData::Cancel) => Input::AuthData(None),
                Err(_) if self.bad_line_ending => bad("Expected CRLF"),
                Err(_) => bad("Invalid base64"),
            },
            Expect::IdleDone => match self.frag.decode_message(&IdleDoneCodec::default()) {
                Ok(_) => Input::IdleDone,
                Err(_) => bad("Expected DONE"),
            },
        }
    }
}

// ---------- Ответы ----------

/// Сериализует ответ.
pub fn encode(response: &Response<'_>) -> Vec<u8> {
    ResponseCodec::default().encode(response).dump()
}

/// Сериализует приветствие.
pub fn encode_greeting(greeting: &Greeting<'_>) -> Vec<u8> {
    GreetingCodec::default().encode(greeting).dump()
}

/// Текст ответа; недопустимые символы (CR, LF) заменяются пробелами.
pub fn text(s: impl Into<String>) -> Text<'static> {
    let s: String = s.into();
    let s: String = s
        .chars()
        .map(|c| if c == '\r' || c == '\n' { ' ' } else { c })
        .collect();
    let s = if s.trim().is_empty() {
        "-".to_owned()
    } else {
        s
    };
    Text::try_from(s).unwrap_or_else(|_| Text::unvalidated("-"))
}

/// Код ответа, которого нет в imap-types (RFC 5530: `AUTHENTICATIONFAILED`, `NONEXISTENT`, ...).
pub fn code(name: &'static str) -> Code<'static> {
    Code::Other(CodeOther::unvalidated(name.as_bytes()))
}

/// Ответ о завершении команды.
pub fn tagged(
    tag: &Tag<'static>,
    kind: types::response::StatusKind,
    code: Option<Code<'static>>,
    msg: impl Into<String>,
) -> Response<'static> {
    let body = types::response::StatusBody {
        kind,
        code,
        text: text(msg),
    };
    Response::Status(Status::Tagged(types::response::Tagged {
        tag: tag.clone(),
        body,
    }))
}

/// Ответ `* STATUS` с элементами, которых нет в imap-types (`SIZE`).
pub fn status_line(mailbox: AString<'static>, items: &[(StatusItem, u64)]) -> Vec<u8> {
    let data = Data::Status {
        mailbox: Mailbox::from(mailbox),
        items: Vec::new().into(),
    };
    let mut line = encode(&Response::Data(data));
    // `* STATUS <mailbox> ()\r\n` — элементы вписываются в скобки.
    line.truncate(line.len().saturating_sub(3));
    let items: Vec<String> = items
        .iter()
        .map(|(i, v)| format!("{} {v}", i.name()))
        .collect();
    line.extend_from_slice(items.join(" ").as_bytes());
    line.extend_from_slice(b")\r\n");
    line
}

/// Ответ `* n FETCH (…)`. imap-codec пишет `MODSEQ n` без скобок (RFC 7162, 3.1.4.1: `MODSEQ (n)`) и не умеет
/// `BINARY[…]<origin>` (RFC 3516, 4.3): такие ответы собираются из элементов, сериализованных по одному.
/// `origins` — номер элемента `BINARY` и начало его диапазона.
pub fn fetch_line(
    seq: NonZeroU32,
    items: types::core::Vec1<types::fetch::MessageDataItem<'static>>,
    origins: &[(usize, u32)],
) -> Vec<u8> {
    use types::fetch::MessageDataItem;
    let modseq = items
        .as_ref()
        .iter()
        .any(|i| matches!(i, MessageDataItem::ModSeq(_)));
    if origins.is_empty() && !modseq {
        return encode(&Response::Data(Data::Fetch { seq, items }));
    }
    let head = format!("* {seq} FETCH (");
    let mut out = head.clone().into_bytes();
    for (i, item) in items.into_iter().enumerate() {
        if i > 0 {
            out.push(b' ');
        }
        if let MessageDataItem::ModSeq(m) = item {
            out.extend_from_slice(format!("MODSEQ ({m})").as_bytes());
            continue;
        }
        let line = encode(&Response::Data(Data::Fetch {
            seq,
            items: types::core::Vec1::from(item),
        }));
        // `* n FETCH (` элемент `)\r\n`.
        let body = line
            .get(head.len()..line.len().saturating_sub(3))
            .unwrap_or_default();
        let origin = origins.iter().find(|(j, _)| *j == i).map(|(_, o)| *o);
        match (origin, body.iter().position(|&b| b == b']')) {
            (Some(origin), Some(end)) => {
                out.extend_from_slice(&body[..=end]);
                out.extend_from_slice(format!("<{origin}>").as_bytes());
                out.extend_from_slice(&body[end + 1..]);
            }
            _ => out.extend_from_slice(body),
        }
    }
    out.extend_from_slice(b")\r\n");
    out
}

/// Ответ `* LIST` с расширенными данными (`CHILDINFO`, RFC 5258) после имени папки.
pub fn list_line(data: Data<'static>, extended: &str) -> Vec<u8> {
    let mut line = encode(&Response::Data(data));
    if !extended.is_empty() {
        line.truncate(line.len().saturating_sub(2));
        line.push(b' ');
        line.extend_from_slice(extended.as_bytes());
        line.extend_from_slice(b"\r\n");
    }
    line
}

/// Строка в виде `astring` IMAP (атом, строка в кавычках или литерал — выбирает imap-codec).
pub fn astring(bytes: Vec<u8>) -> Option<AString<'static>> {
    AString::try_from(bytes).ok()
}

/// Ненулевое число для кодов ответа (`UIDNEXT`, `UIDVALIDITY`).
pub fn nz(n: u32) -> NonZeroU32 {
    NonZeroU32::new(n).unwrap_or(NonZeroU32::MIN)
}

/// Отрезки подряд идущих чисел (по возрастанию, без повторов, без нуля).
pub fn runs(nums: &[u32]) -> Vec<(u32, u32)> {
    let mut sorted: Vec<u32> = nums.iter().copied().filter(|&n| n > 0).collect();
    sorted.sort_unstable();
    sorted.dedup();
    let mut out: Vec<(u32, u32)> = Vec::new();
    for n in sorted {
        match out.last_mut() {
            Some((_, end)) if end.checked_add(1) == Some(n) => *end = n,
            _ => out.push((n, n)),
        }
    }
    out
}

/// Набор номеров (`1:3,7`) для кодов ответа (`MODIFIED`); пустой — `None`.
pub fn sequence_set(nums: &[u32]) -> Option<types::sequence::SequenceSet> {
    use types::sequence::{SeqOrUid, Sequence, SequenceSet};
    let seqs: Vec<Sequence> = runs(nums)
        .into_iter()
        .map(|(a, b)| {
            let (a, b) = (SeqOrUid::Value(nz(a)), SeqOrUid::Value(nz(b)));
            if a == b {
                Sequence::Single(a)
            } else {
                Sequence::Range(a, b)
            }
        })
        .collect();
    types::core::Vec1::try_from(seqs).ok().map(SequenceSet)
}

/// Набор номеров текстом (`1:3,7`) для ответов, которые собираются вручную (`ESEARCH`).
pub fn sequence_text(nums: &[u32]) -> String {
    runs(nums)
        .into_iter()
        .map(|(a, b)| {
            if a == b {
                a.to_string()
            } else {
                format!("{a}:{b}")
            }
        })
        .collect::<Vec<_>>()
        .join(",")
}

/// Разбор произвольных байтов как потока команд: не должен паниковать (цель fuzz, `fuzz/`).
#[doc(hidden)]
pub fn fuzz_input(data: &[u8]) {
    let mut reader = Reader::new(64 * 1024);
    for chunk in data.chunks(7) {
        reader.push(chunk);
        while let Some(input) = reader.next(Expect::Command) {
            if let Input::Command(cmd) = input {
                // Ответ на разобранную команду сериализуется.
                let _ = encode(&tagged(
                    &cmd.tag,
                    types::response::StatusKind::Ok,
                    None,
                    "done",
                ));
            }
        }
    }
    let _ = rev2::parse(data);
    let _ = decode_utf7(data);
}
