//! Email/query и Email/queryChanges (RFC 8621 §4.4–4.5).
//!
//! Нетекстовые условия (папка, даты, размер, ключевые слова, вложения, цепочки) проверяются по метаданным
//! писем (`MessageStore`), текстовые (`text`, `from`, `to`, `cc`, `bcc`, `subject`, `body`, `header`) — запросом
//! к `SearchIndex` на каждое условие; результаты пересекаются (SRS 4.6). Сортировка — по метаданным.

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::str::FromStr;

use serde::Deserialize;
use serde_json::{Map, Value, json};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use tp_contracts::meta::{Address, Message};
use tp_contracts::search::{SearchSort, Snippet, TextField, TextQuery};
use tp_contracts::{Error, MailboxId, MessageId};

use crate::core::error::{MethodError, MethodResult, changes_error};
use crate::core::request::{
    Comparator, QueryChangesArgs, QueryWindow, apply_window, map_changes, object, parse_args,
    parse_state, query_changes_response, state_string,
};
use crate::core::{Ctx, thread_id};
use crate::email_get::{has_keyword, threads};

/// Поддерживаемые свойства сортировки (`emailQuerySortOptions`).
pub(crate) const SORT_OPTIONS: &[&str] = &[
    "receivedAt",
    "sentAt",
    "size",
    "from",
    "to",
    "subject",
    "hasKeyword",
    "allInThreadHaveKeyword",
    "someInThreadHaveKeyword",
];

/// Условие фильтра (RFC 8621 §4.4.1).
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Condition {
    in_mailbox: Option<String>,
    in_mailbox_other_than: Option<Vec<String>>,
    before: Option<String>,
    after: Option<String>,
    min_size: Option<u64>,
    max_size: Option<u64>,
    all_in_thread_have_keyword: Option<String>,
    some_in_thread_have_keyword: Option<String>,
    none_in_thread_have_keyword: Option<String>,
    has_keyword: Option<String>,
    not_keyword: Option<String>,
    has_attachment: Option<bool>,
    text: Option<String>,
    from: Option<String>,
    to: Option<String>,
    cc: Option<String>,
    bcc: Option<String>,
    subject: Option<String>,
    body: Option<String>,
    header: Option<Vec<String>>,
}

/// Разобранное условие: даты и текстовые термы.
#[derive(Debug)]
struct Cond {
    c: Condition,
    before: Option<OffsetDateTime>,
    after: Option<OffsetDateTime>,
    terms: Vec<(TextField, String)>,
}

impl Cond {
    fn uses_threads(&self) -> bool {
        self.c.all_in_thread_have_keyword.is_some()
            || self.c.some_in_thread_have_keyword.is_some()
            || self.c.none_in_thread_have_keyword.is_some()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    And,
    Or,
    Not,
}

#[derive(Debug)]
enum Filter {
    Op(Op, Vec<Filter>),
    Cond(Box<Cond>),
}

fn unsupported(why: impl Into<String>) -> MethodError {
    MethodError::with("unsupportedFilter", why)
}

fn utc(s: &str) -> MethodResult<OffsetDateTime> {
    OffsetDateTime::parse(s, &Rfc3339)
        .map_err(|_| MethodError::invalid(format!("bad UTCDate: {s}")))
}

fn parse_filter(v: &Value) -> MethodResult<Filter> {
    let Value::Object(m) = v else {
        return Err(unsupported("filter must be an object"));
    };
    if let Some(op) = m.get("operator") {
        let op = match op.as_str() {
            Some("AND") => Op::And,
            Some("OR") => Op::Or,
            Some("NOT") => Op::Not,
            _ => return Err(unsupported("operator")),
        };
        let conds = m
            .get("conditions")
            .and_then(Value::as_array)
            .ok_or_else(|| unsupported("conditions"))?;
        return Ok(Filter::Op(
            op,
            conds
                .iter()
                .map(parse_filter)
                .collect::<MethodResult<_>>()?,
        ));
    }
    let c: Condition = serde_json::from_value(v.clone()).map_err(|e| unsupported(e.to_string()))?;
    let before = c.before.as_deref().map(utc).transpose()?;
    let after = c.after.as_deref().map(utc).transpose()?;
    let mut terms = Vec::new();
    for (field, text) in [
        (TextField::Any, &c.text),
        (TextField::From, &c.from),
        (TextField::To, &c.to),
        (TextField::Cc, &c.cc),
        (TextField::Bcc, &c.bcc),
        (TextField::Subject, &c.subject),
        (TextField::Body, &c.body),
    ] {
        if let Some(t) = text {
            terms.push((field, t.clone()));
        }
    }
    match c.header.as_deref() {
        None => {}
        Some([name]) => terms.push((TextField::Header(name.clone()), String::new())),
        Some([name, value]) => terms.push((TextField::Header(name.clone()), value.clone())),
        Some(_) => return Err(MethodError::invalid("header: [name] or [name, value]")),
    }
    Ok(Filter::Cond(Box::new(Cond {
        c,
        before,
        after,
        terms,
    })))
}

fn walk<'f>(f: &'f Filter, out: &mut Vec<&'f Cond>) {
    match f {
        Filter::Op(_, fs) => fs.iter().for_each(|f| walk(f, out)),
        Filter::Cond(c) => out.push(c),
    }
}

/// Данные для проверки условий: результаты текстовых запросов и цепочки.
struct Eval<'a> {
    text: HashMap<(TextField, String), HashSet<MessageId>>,
    threads: HashMap<String, Vec<&'a Message>>,
}

impl Eval<'_> {
    fn thread(&self, m: &Message) -> &[&Message] {
        self.threads.get(&thread_id(m)).map_or(&[], Vec::as_slice)
    }

    fn matches(&self, f: &Filter, m: &Message) -> bool {
        match f {
            Filter::Op(Op::And, fs) => fs.iter().all(|f| self.matches(f, m)),
            Filter::Op(Op::Or, fs) => fs.iter().any(|f| self.matches(f, m)),
            Filter::Op(Op::Not, fs) => !fs.iter().any(|f| self.matches(f, m)),
            Filter::Cond(c) => self.cond(c, m),
        }
    }

    fn cond(&self, k: &Cond, m: &Message) -> bool {
        let c = &k.c;
        let mailbox = m.mailbox.to_string();
        let kw = |w: &Option<String>, f: &dyn Fn(&str) -> bool| w.as_deref().is_none_or(f);
        c.in_mailbox.as_ref().is_none_or(|id| *id == mailbox)
            && c.in_mailbox_other_than
                .as_ref()
                .is_none_or(|ids| !ids.contains(&mailbox))
            && k.before.is_none_or(|t| m.received_at < t)
            && k.after.is_none_or(|t| m.received_at >= t)
            && c.min_size.is_none_or(|s| m.size >= s)
            && c.max_size.is_none_or(|s| m.size < s)
            && kw(&c.has_keyword, &|w| has_keyword(m, w))
            && kw(&c.not_keyword, &|w| !has_keyword(m, w))
            && kw(&c.all_in_thread_have_keyword, &|w| {
                self.thread(m).iter().all(|x| has_keyword(x, w))
            })
            && kw(&c.some_in_thread_have_keyword, &|w| {
                self.thread(m).iter().any(|x| has_keyword(x, w))
            })
            && kw(&c.none_in_thread_have_keyword, &|w| {
                !self.thread(m).iter().any(|x| has_keyword(x, w))
            })
            && c.has_attachment.is_none_or(|a| m.meta.has_attachment == a)
            && k.terms
                .iter()
                .all(|t| self.text.get(t).is_some_and(|ids| ids.contains(&m.id)))
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct QueryArgs {
    account_id: String,
    filter: Option<Value>,
    sort: Option<Vec<Comparator>>,
    #[serde(default)]
    collapse_threads: bool,
    #[serde(flatten)]
    window: QueryWindow,
}

/// Строка сортировки адреса: имя или адрес первого получателя, без учёта регистра.
fn addr_key(list: &[Address]) -> String {
    list.first()
        .map(|a| {
            a.name
                .clone()
                .filter(|n| !n.is_empty())
                .unwrap_or_else(|| a.email.clone())
        })
        .unwrap_or_default()
        .to_lowercase()
}

/// Базовая тема (RFC 5256 §2.1, упрощённо): без `Re:`/`Fwd:`/`Fw:` в начале, без учёта регистра.
fn base_subject(s: Option<&str>) -> String {
    let mut s = s.unwrap_or_default().trim().to_lowercase();
    loop {
        let stripped = ["re:", "fwd:", "fw:"]
            .iter()
            .find_map(|p| s.strip_prefix(p))
            .map(|r| r.trim_start().to_owned());
        match stripped {
            Some(r) => s = r,
            None => return s,
        }
    }
}

fn compare(c: &Comparator, x: &Message, y: &Message, ev: &Eval<'_>) -> Ordering {
    let kw = c.keyword.as_deref().unwrap_or_default();
    let in_thread = |m: &Message, all: bool| {
        let t = ev.thread(m);
        if all {
            t.iter().all(|x| has_keyword(x, kw))
        } else {
            t.iter().any(|x| has_keyword(x, kw))
        }
    };
    let (ex, ey) = (&x.meta.envelope, &y.meta.envelope);
    match c.property.as_str() {
        "receivedAt" => x.received_at.cmp(&y.received_at),
        "sentAt" => ex.date.cmp(&ey.date),
        "size" => x.size.cmp(&y.size),
        "from" => addr_key(&ex.from).cmp(&addr_key(&ey.from)),
        "to" => addr_key(&ex.to).cmp(&addr_key(&ey.to)),
        "subject" => base_subject(ex.subject.as_deref()).cmp(&base_subject(ey.subject.as_deref())),
        "hasKeyword" => has_keyword(x, kw).cmp(&has_keyword(y, kw)),
        "allInThreadHaveKeyword" => in_thread(x, true).cmp(&in_thread(y, true)),
        "someInThreadHaveKeyword" => in_thread(x, false).cmp(&in_thread(y, false)),
        _ => Ordering::Equal,
    }
}

/// Результаты запроса: идентификаторы писем по порядку; `threads` — условия или сортировка по цепочкам.
async fn run(ctx: &Ctx<'_>, a: &QueryArgs) -> MethodResult<(Vec<String>, bool)> {
    let filter = a.filter.as_ref().map(parse_filter).transpose()?;
    let sort = a.sort.clone().unwrap_or_else(|| {
        vec![Comparator {
            property: "receivedAt".into(),
            is_ascending: false,
            collation: None,
            keyword: None,
        }]
    });
    for c in &sort {
        if !SORT_OPTIONS.contains(&c.property.as_str()) {
            return Err(MethodError::with("unsupportedSort", c.property.clone()));
        }
        if c.property.contains("Keyword") && c.keyword.is_none() {
            return Err(MethodError::invalid("keyword required"));
        }
    }
    let mut conds = Vec::new();
    if let Some(f) = &filter {
        walk(f, &mut conds);
    }
    let thread_sort = sort.iter().any(|c| c.property.contains("InThread"));
    let uses_threads = thread_sort || conds.iter().any(|c| c.uses_threads());
    // Одна папка, если её задаёт условие верхнего уровня, а цепочки не нужны.
    let only: Option<MailboxId> = match &filter {
        Some(Filter::Cond(c)) if !uses_threads => {
            c.c.in_mailbox.as_deref().and_then(|s| s.parse().ok())
        }
        _ => None,
    };
    let msgs = ctx.jmap.messages_in(ctx.scope, only).await?;
    let mut text = HashMap::new();
    for (field, value) in conds.iter().flat_map(|c| c.terms.iter()) {
        let key = (field.clone(), value.clone());
        if text.contains_key(&key) {
            continue;
        }
        let q = TextQuery::term(field.clone(), value.clone());
        let limit = ctx.jmap.cfg.max_search_results as usize;
        let hits = ctx
            .jmap
            .d
            .search
            .query(ctx.scope, &[], &q, SearchSort::NewestFirst, limit)
            .await?;
        text.insert(key, hits.into_iter().collect::<HashSet<_>>());
    }
    let ev = Eval {
        text,
        threads: if uses_threads || a.collapse_threads {
            threads(&msgs)
        } else {
            HashMap::new()
        },
    };
    let mut hits: Vec<&Message> = msgs
        .iter()
        .filter(|m| filter.as_ref().is_none_or(|f| ev.matches(f, m)))
        .collect();
    hits.sort_by(|x, y| {
        for c in &sort {
            let o = compare(c, x, y, &ev);
            let o = if c.is_ascending { o } else { o.reverse() };
            if o != Ordering::Equal {
                return o;
            }
        }
        x.id.cmp(&y.id)
    });
    let mut seen = HashSet::new();
    let ids = hits
        .into_iter()
        .filter(|m| !a.collapse_threads || seen.insert(thread_id(m)))
        .map(|m| m.id.to_string())
        .collect();
    Ok((ids, uses_threads))
}

/// `Email/query`.
pub(crate) async fn query(
    ctx: &mut Ctx<'_>,
    args: Map<String, Value>,
) -> MethodResult<Map<String, Value>> {
    let a: QueryArgs = parse_args(args)?;
    ctx.check_account(&a.account_id)?;
    let state = ctx.state().await?;
    let (ids, uses_threads) = run(ctx, &a).await?;
    let mut out = object(json!({
        "accountId": a.account_id,
        "queryState": state_string(state),
        "canCalculateChanges": !a.collapse_threads && !uses_threads,
        "collapseThreads": a.collapse_threads,
    }));
    apply_window(&ids, &a.window, ctx.jmap.cfg.max_query_limit, &mut out)?;
    Ok(out)
}

/// `Email/queryChanges` по журналу изменений писем. С `collapseThreads` и условиями по цепочкам позиция
/// письма зависит от других писем — `cannotCalculateChanges`.
pub(crate) async fn query_changes(
    ctx: &mut Ctx<'_>,
    args: Map<String, Value>,
) -> MethodResult<Map<String, Value>> {
    let qc: QueryChangesArgs = parse_args(args.clone())?;
    ctx.check_account(&qc.account_id)?;
    let mut q = args;
    q.retain(|k, _| {
        matches!(
            k.as_str(),
            "accountId" | "filter" | "sort" | "collapseThreads"
        )
    });
    let a: QueryArgs = parse_args(q)?;
    if a.collapse_threads {
        return Err(MethodError::new("cannotCalculateChanges"));
    }
    let since = parse_state(&qc.since_query_state)?;
    let limit = qc.limit(ctx.jmap.cfg.max_changes)?;
    let c = ctx
        .jmap
        .d
        .changes
        .message_changes(ctx.scope, None, since, limit.saturating_add(1))
        .await
        .map_err(changes_error)?;
    let c = map_changes(c, |m| m.id.to_string());
    let (ids, uses_threads) = run(ctx, &a).await?;
    if uses_threads {
        return Err(MethodError::new("cannotCalculateChanges"));
    }
    query_changes_response(&qc, &ids, &c, limit)
}

/// Текстовая часть фильтра: термы условий в дереве операторов; нетекстовые условия отбрасываются.
fn text_of(f: &Filter) -> Option<TextQuery> {
    match f {
        Filter::Cond(c) => {
            let mut terms: Vec<TextQuery> = c
                .terms
                .iter()
                .map(|(field, text)| TextQuery::term(field.clone(), text.clone()))
                .collect();
            match terms.len() {
                0 => None,
                1 => terms.pop(),
                _ => Some(TextQuery::And(terms)),
            }
        }
        Filter::Op(op, fs) => {
            let qs: Vec<TextQuery> = fs.iter().filter_map(text_of).collect();
            if qs.is_empty() {
                return None;
            }
            Some(match op {
                Op::And => TextQuery::And(qs),
                Op::Or => TextQuery::Or(qs),
                Op::Not => TextQuery::Not(Box::new(TextQuery::Or(qs))),
            })
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SnippetArgs {
    account_id: String,
    filter: Option<Value>,
    email_ids: Vec<String>,
}

/// `SearchSnippet/get` (RFC 8621 §5): фрагменты с `<mark>` от `SearchIndex::snippet` по текстовой части фильтра.
pub(crate) async fn snippet_get(
    ctx: &mut Ctx<'_>,
    args: Map<String, Value>,
) -> MethodResult<Map<String, Value>> {
    let a: SnippetArgs = parse_args(args)?;
    ctx.check_account(&a.account_id)?;
    if a.email_ids.len() > ctx.jmap.cfg.max_objects_in_get as usize {
        return Err(MethodError::new("requestTooLarge"));
    }
    let q = a
        .filter
        .as_ref()
        .map(parse_filter)
        .transpose()?
        .as_ref()
        .and_then(text_of);
    let d = &ctx.jmap.d;
    let (mut list, mut not_found) = (Vec::new(), Vec::new());
    for id in a.email_ids {
        let msg = match MessageId::from_str(ctx.resolve_id(&id)) {
            Ok(mid) => d.messages.get(ctx.scope, mid).await?,
            Err(_) => None,
        };
        let Some(m) = msg else {
            not_found.push(id);
            continue;
        };
        let s = match &q {
            Some(q) => match d.search.snippet(ctx.scope, m.id, q).await {
                Ok(s) => s,
                // Письмо ещё не проиндексировано.
                Err(Error::NotFound) => Snippet::default(),
                Err(e) => return Err(e.into()),
            },
            None => Snippet::default(),
        };
        list.push(json!({"emailId": id, "subject": s.subject, "preview": s.preview}));
    }
    Ok(object(json!({
        "accountId": a.account_id,
        "list": list,
        "notFound": if not_found.is_empty() { Value::Null } else { json!(not_found) },
    })))
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use time::{Duration, OffsetDateTime};
    use tp_contracts::mailstore::MailStore;
    use tp_contracts::meta::{Flag, FlagOp, SpecialUse};

    use crate::testenv::{Env, mail};

    /// Четыре письма: две цепочки, разные даты, размеры, флаги, вложение.
    async fn fixture(env: &Env) -> Vec<String> {
        let inbox = env.mailbox(SpecialUse::Inbox);
        let junk = env.mailbox(SpecialUse::Junk);
        let t0 = OffsetDateTime::now_utc() - Duration::days(10);
        let att = "MIME-Version: 1.0\r\nContent-Type: multipart/mixed; boundary=b\r\n";
        let att_body = "--b\r\nContent-Type: text/plain\r\n\r\nсчёт во вложении\r\n--b\r\n\
                        Content-Type: application/pdf; name=a.pdf\r\nContent-Disposition: attachment; filename=a.pdf\r\n\
                        Content-Transfer-Encoding: base64\r\n\r\nJVBERi0=\r\n--b--";
        let mails = [
            (
                inbox,
                mail(
                    "Отчёт за квартал",
                    "Boris <boris@x.org>",
                    "Message-ID: <q1@x>\r\n",
                    "цифры за квартал",
                ),
                vec![Flag::Seen],
                0,
            ),
            (
                inbox,
                mail(
                    "Re: Отчёт за квартал",
                    "anna@x.org",
                    "References: <q1@x>\r\n",
                    "спасибо, отчёт получен",
                ),
                vec![],
                1,
            ),
            (
                junk,
                mail(
                    "Акция",
                    "shop@spam.example",
                    "List-Id: <promo.spam.example>\r\n",
                    "скидки",
                ),
                vec![Flag::Flagged],
                2,
            ),
            (
                inbox,
                mail("Счёт", "billing@x.org", att, att_body),
                vec![Flag::Seen, Flag::Flagged],
                3,
            ),
        ];
        let mut ids = Vec::new();
        for (mb, raw, flags, day) in mails {
            let id = env
                .deliver_at(
                    mb,
                    &raw,
                    flags.into_iter().collect(),
                    t0 + Duration::days(day),
                )
                .await;
            ids.push(id.to_string());
        }
        ids
    }

    async fn ids(env: &Env, args: serde_json::Value) -> serde_json::Value {
        let mut a = args;
        a["accountId"] = json!(env.account());
        env.call("Email/query", a, "Email/query").await["ids"].clone()
    }

    #[tokio::test]
    async fn filters_and_search() {
        let env = Env::new().await;
        let m = fixture(&env).await;
        let inbox = env.mailbox(SpecialUse::Inbox).to_string();
        let t0 = OffsetDateTime::now_utc() - time::Duration::days(10);
        let day2 = crate::email_get::utc_date(t0 + time::Duration::days(2));
        // По умолчанию — новые сверху.
        assert_eq!(ids(&env, json!({})).await, json!([m[3], m[2], m[1], m[0]]));
        assert_eq!(
            ids(&env, json!({"filter": {"inMailbox": inbox}})).await,
            json!([m[3], m[1], m[0]])
        );
        assert_eq!(
            ids(&env, json!({"filter": {"inMailboxOtherThan": [inbox]}})).await,
            json!([m[2]])
        );
        assert_eq!(
            ids(&env, json!({"filter": {"before": day2}})).await,
            json!([m[1], m[0]])
        );
        assert_eq!(
            ids(&env, json!({"filter": {"after": day2}})).await,
            json!([m[3], m[2]])
        );
        assert_eq!(
            ids(
                &env,
                json!({"filter": {"hasKeyword": "$flagged", "notKeyword": "$seen"}})
            )
            .await,
            json!([m[2]])
        );
        assert_eq!(
            ids(&env, json!({"filter": {"hasAttachment": true}})).await,
            json!([m[3]])
        );
        let sizes = env
            .call(
                "Email/get",
                json!({"accountId": env.account(), "ids": m, "properties": ["size"]}),
                "Email/get",
            )
            .await;
        let size = |i: usize| sizes["list"][i]["size"].as_u64().expect("size");
        let big = (0..4).max_by_key(|i| size(*i)).expect("max");
        assert_eq!(
            ids(&env, json!({"filter": {"minSize": size(big)}})).await,
            json!([m[big]])
        );
        assert_eq!(
            ids(&env, json!({"filter": {"maxSize": size(big)}}))
                .await
                .as_array()
                .map(Vec::len),
            Some(3)
        );
        // Текстовые условия — через SearchIndex (подстрока без учёта регистра в FakeSearch).
        assert_eq!(
            ids(&env, json!({"filter": {"text": "квартал"}})).await,
            json!([m[1], m[0]])
        );
        assert_eq!(
            ids(&env, json!({"filter": {"from": "boris"}})).await,
            json!([m[0]])
        );
        assert_eq!(
            ids(&env, json!({"filter": {"subject": "акция"}})).await,
            json!([m[2]])
        );
        assert_eq!(
            ids(&env, json!({"filter": {"body": "вложении"}})).await,
            json!([m[3]])
        );
        assert_eq!(
            ids(&env, json!({"filter": {"header": ["List-Id"]}})).await,
            json!([m[2]])
        );
        assert_eq!(
            ids(&env, json!({"filter": {"header": ["List-Id", "promo"]}})).await,
            json!([m[2]])
        );
        assert_eq!(
            ids(
                &env,
                json!({"filter": {"operator": "AND", "conditions": [
                {"text": "отчёт"}, {"operator": "NOT", "conditions": [{"hasKeyword": "$seen"}]}]}})
            )
            .await,
            json!([m[1]])
        );
        assert_eq!(
            ids(&env, json!({"filter": {"operator": "OR", "conditions": [{"from": "shop"}, {"hasAttachment": true}]}})).await,
            json!([m[3], m[2]])
        );
        // Цепочки: в цепочке m0+m1 не все письма прочитаны.
        assert_eq!(
            ids(
                &env,
                json!({"filter": {"someInThreadHaveKeyword": "$seen", "inMailbox": inbox}})
            )
            .await,
            json!([m[3], m[1], m[0]])
        );
        assert_eq!(
            ids(&env, json!({"filter": {"allInThreadHaveKeyword": "$seen"}})).await,
            json!([m[3]])
        );
        assert_eq!(
            ids(
                &env,
                json!({"filter": {"noneInThreadHaveKeyword": "$seen"}})
            )
            .await,
            json!([m[2]])
        );
        let r = env
            .calls(json!([
                ["Email/query", {"accountId": env.account(), "filter": {"color": "red"}}, "a"],
                ["Email/query", {"accountId": env.account(), "sort": [{"property": "color"}]}, "b"],
                ["Email/query", {"accountId": env.account(), "filter": {"before": "yesterday"}}, "c"]
            ]))
            .await;
        assert_eq!(r[0][1]["type"], "unsupportedFilter");
        assert_eq!(r[1][1]["type"], "unsupportedSort");
        assert_eq!(r[2][1]["type"], "invalidArguments");
    }

    #[tokio::test]
    async fn sort_window_collapse() {
        let env = Env::new().await;
        let m = fixture(&env).await;
        let asc = |p: &str| json!({"sort": [{"property": p, "isAscending": true}]});
        assert_eq!(
            ids(&env, asc("receivedAt")).await,
            json!([m[0], m[1], m[2], m[3]])
        );
        // Базовая тема: «Re:» не учитывается, сравнение без регистра; одинаковые — по второму критерию.
        assert_eq!(
            ids(&env, json!({"sort": [{"property": "subject"}, {"property": "receivedAt", "isAscending": false}]})).await,
            json!([m[2], m[1], m[0], m[3]])
        );
        assert_eq!(
            ids(&env, asc("from")).await,
            json!([m[1], m[3], m[0], m[2]])
        );
        assert_eq!(
            ids(&env, json!({"sort": [{"property": "hasKeyword", "keyword": "$flagged", "isAscending": false}, {"property": "size"}]})).await
                .as_array()
                .map(|a| a.len()),
            Some(4)
        );
        let r = env
            .call(
                "Email/query",
                json!({"accountId": env.account(), "collapseThreads": true, "calculateTotal": true, "position": 1, "limit": 2}),
                "Email/query",
            )
            .await;
        assert_eq!(r["ids"], json!([m[2], m[1]]));
        assert_eq!(
            (r["total"].clone(), r["position"].clone()),
            (json!(3), json!(1))
        );
        assert_eq!(r["canCalculateChanges"], false);
        let r = env
            .call(
                "Email/query",
                json!({"accountId": env.account(), "anchor": m[2], "anchorOffset": -1, "limit": 2}),
                "Email/query",
            )
            .await;
        assert_eq!(r["ids"], json!([m[3], m[2]]));
        let r = env
            .call(
                "Email/query",
                json!({"accountId": env.account(), "position": -1}),
                "Email/query",
            )
            .await;
        assert_eq!(
            (r["ids"].clone(), r["position"].clone()),
            (json!([m[0]]), json!(3))
        );
    }

    /// Пример RFC 8621 §4.10: Email/query и Email/get по ссылке на результат.
    #[tokio::test]
    async fn query_then_get_by_reference() {
        let env = Env::new().await;
        let m = fixture(&env).await;
        let acc = env.account();
        let r = env
            .calls(json!([
                ["Email/query", {"accountId": acc, "filter": {"inMailbox": env.mailbox(SpecialUse::Inbox).to_string()},
                    "sort": [{"property": "receivedAt", "isAscending": false}], "collapseThreads": true,
                    "position": 0, "limit": 20, "calculateTotal": true}, "t0"],
                ["Email/get", {"accountId": acc, "#ids": {"resultOf": "t0", "name": "Email/query", "path": "/ids"},
                    "properties": ["threadId"]}, "t1"],
                ["Thread/get", {"accountId": acc, "#ids": {"resultOf": "t1", "name": "Email/get", "path": "/list/*/threadId"}}, "t2"]
            ]))
            .await;
        assert_eq!(r[0][1]["ids"], json!([m[3], m[1]]));
        assert_eq!(r[1][1]["list"].as_array().map(Vec::len), Some(2));
        assert_eq!(r[2][1]["list"][1]["emailIds"], json!([m[0], m[1]]));
    }

    #[tokio::test]
    async fn query_changes() {
        let env = Env::new().await;
        let m = fixture(&env).await;
        let acc = env.account();
        let filter = json!({"hasKeyword": "$seen"});
        let q = env
            .call(
                "Email/query",
                json!({"accountId": acc, "filter": filter}),
                "Email/query",
            )
            .await;
        assert_eq!(q["ids"], json!([m[3], m[0]]));
        assert_eq!(q["canCalculateChanges"], true);
        let id1 = m[1].parse().expect("id");
        env.store
            .set_flags(env.scope, &[id1], FlagOp::Add([Flag::Seen].into()), None)
            .await
            .expect("flags");
        env.deliver(
            env.mailbox(SpecialUse::Inbox),
            &mail("new", "a@x.org", "", "x"),
            [Flag::Seen].into(),
        )
        .await;
        let qc = env
            .call(
                "Email/queryChanges",
                json!({"accountId": acc, "filter": filter, "sinceQueryState": q["queryState"], "calculateTotal": true}),
                "Email/queryChanges",
            )
            .await;
        assert_eq!(qc["removed"], json!([m[1]]));
        assert_eq!(qc["added"].as_array().map(Vec::len), Some(2));
        assert_eq!(qc["added"][1], json!({"id": m[1], "index": 2}));
        assert_eq!(qc["total"], 4);
        assert_eq!(qc["oldQueryState"], q["queryState"]);
        let r = env
            .calls(json!([
                ["Email/queryChanges", {"accountId": acc, "sinceQueryState": q["queryState"], "collapseThreads": true}, "a"],
                ["Email/queryChanges", {"accountId": acc, "sinceQueryState": q["queryState"], "maxChanges": 1}, "b"],
                ["Email/queryChanges", {"accountId": acc, "sinceQueryState": q["queryState"],
                    "filter": {"allInThreadHaveKeyword": "$seen"}}, "c"]
            ]))
            .await;
        assert_eq!(r[0][1]["type"], "cannotCalculateChanges");
        assert_eq!(r[1][1]["type"], "tooManyChanges");
        assert_eq!(r[2][1]["type"], "cannotCalculateChanges");
    }
}
