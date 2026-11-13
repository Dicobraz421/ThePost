//! Раскрытие динамических групп и контактов (SMTP-26) на in-memory адресной книге `MemAddressBook`.

use std::collections::BTreeMap;

use tp_contracts::addressbook::{
    ContactStore, DynamicGroup, DynamicGroupStore, MailContact, MemberFilter, OrgAttributes,
    OrgStore,
};
use tp_contracts::groups::{DistributionGroup, GroupMember, GroupStore, SenderRestriction};
use tp_contracts::{ContactId, GroupId};
use tp_testkit::MemAddressBook;

use super::*;
use crate::groups::Groups;

struct Book {
    env: Env,
    book: MemAddressBook,
}

impl Book {
    async fn new() -> Self {
        Self {
            env: Env::new(DecisionHooks::default()).await,
            book: MemAddressBook::new(),
        }
    }

    fn delivery(&self) -> Delivery {
        self.env.delivery(true).with_groups(
            Groups::new(
                Arc::new(self.env.groups.clone()),
                Arc::new(self.env.blobs.clone()),
                Arc::new(self.env.index.clone()),
            )
            .with_address_book(Arc::new(self.book.clone()), Arc::new(self.book.clone())),
        )
    }

    async fn group(
        &self,
        address: &str,
        members: Vec<GroupMember>,
        senders: SenderRestriction,
    ) -> GroupId {
        let g = DistributionGroup {
            id: GroupId::new(),
            tenant: TenantId::DEFAULT,
            name: address.into(),
            address: Some(address.into()),
            external_id: None,
            members,
            senders,
            moderators: Vec::new(),
            hidden_from_gal: false,
        };
        self.env.groups.put(g.clone()).await.expect("put group");
        g.id
    }

    async fn account(&self, login: &str) -> GroupMember {
        GroupMember::Account(self.env.scope(login).await.account)
    }

    /// Атрибут `office` учётной записи.
    async fn office(&self, login: &str, office: &str) {
        let account = self.env.scope(login).await.account;
        let org = OrgAttributes {
            attributes: BTreeMap::from([("office".to_owned(), office.to_owned())]),
            ..OrgAttributes::default()
        };
        self.book
            .set_org(TenantId::DEFAULT, account, org)
            .await
            .expect("set org");
    }

    /// Группа с фильтром `office = office`.
    async fn dynamic(&self, group: GroupId, office: &str, contacts: bool) {
        self.book
            .put_dynamic(DynamicGroup {
                group,
                tenant: TenantId::DEFAULT,
                filter: MemberFilter::Equals {
                    attribute: "office".into(),
                    value: office.into(),
                },
                contacts,
            })
            .await
            .expect("put dynamic");
    }

    async fn contact(&self, email: &str, office: &str, groups: Vec<GroupId>) {
        self.book
            .put_contact(MailContact {
                id: ContactId::new(),
                tenant: TenantId::DEFAULT,
                display_name: email.into(),
                email: email.into(),
                external_id: None,
                attributes: BTreeMap::from([("office".to_owned(), office.to_owned())]),
                groups,
                hidden_from_gal: false,
            })
            .await
            .expect("put contact");
    }

    fn queued(&self) -> Vec<Vec<String>> {
        let q = self.env.queue.0.lock().expect("lock");
        q.iter().map(|(_, rcpts, _)| rcpts.clone()).collect()
    }
}

#[tokio::test]
async fn dynamic_group_expands_by_filter_at_send_time() {
    let b = Book::new().await;
    // Статический член динамической группы при раскрытии не используется.
    let carol = b.account("carol@example.org").await;
    let g = b
        .group("msk@example.org", vec![carol], SenderRestriction::Anyone)
        .await;
    b.dynamic(g, "msk", false).await;
    b.office("alice@example.org", "msk").await;
    b.office("bob@example.org", "spb").await;
    let d = b.delivery();
    let e = envelope(Mode::Inbound, &["msk@example.org"]);
    assert_eq!(d.rcpt(&e, "msk@example.org").await.code, 250);
    assert_eq!(d.data(&e, Bytes::from_static(MAIL)).await.code, 250);
    assert_eq!(b.env.inbox("alice@example.org").await.len(), 1);
    assert!(b.env.inbox("bob@example.org").await.is_empty());
    assert!(b.env.inbox("carol@example.org").await.is_empty());
    // Атрибут изменился — следующее письмо получает новый состав.
    b.office("bob@example.org", "msk").await;
    b.office("alice@example.org", "spb").await;
    assert_eq!(d.data(&e, Bytes::from_static(MAIL)).await.code, 250);
    assert_eq!(b.env.inbox("alice@example.org").await.len(), 1);
    assert_eq!(b.env.inbox("bob@example.org").await.len(), 1);
}

#[tokio::test]
async fn contacts_go_to_outbound_queue() {
    let b = Book::new().await;
    let bob = b.account("bob@example.org").await;
    let staff = b
        .group("staff@example.org", vec![bob], SenderRestriction::Anyone)
        .await;
    b.contact("partner@partner.example", "spb", vec![staff])
        .await;
    let msk = b
        .group("msk@example.org", Vec::new(), SenderRestriction::Anyone)
        .await;
    b.dynamic(msk, "msk", true).await;
    b.contact("agent@agency.example", "msk", Vec::new()).await;
    b.office("carol@example.org", "msk").await;
    let d = b.delivery();
    // Отправка вне сессии SMTP: участники — в ящики, контакты — в очередь внешней доставки.
    let sent = d
        .submit(
            TenantId::DEFAULT,
            None,
            "alice@example.org",
            &rcpts(&["staff@example.org", "msk@example.org"]),
            Bytes::from_static(MAIL),
        )
        .await
        .expect("submit");
    assert!(
        sent.rcpts
            .iter()
            .all(|(_, s)| *s == SubmitStatus::Delivered)
    );
    assert_eq!(b.env.inbox("bob@example.org").await.len(), 1);
    assert_eq!(b.env.inbox("carol@example.org").await.len(), 1);
    assert_eq!(
        b.queued(),
        [["partner@partner.example", "agent@agency.example"]]
    );
    // Динамическая группа без `contacts`: контакт под фильтр не попадает.
    b.dynamic(msk, "msk", false).await;
    let e = envelope(Mode::Submission, &["msk@example.org"]);
    assert_eq!(d.data(&e, Bytes::from_static(MAIL)).await.code, 250);
    assert_eq!(b.queued().len(), 1);
    assert_eq!(b.env.inbox("carol@example.org").await.len(), 2);
}

#[tokio::test]
async fn nested_dynamic_group_and_contacts_of_nested_groups() {
    let b = Book::new().await;
    let msk = b
        .group("msk@example.org", Vec::new(), SenderRestriction::Anyone)
        .await;
    b.dynamic(msk, "msk", false).await;
    b.office("carol@example.org", "msk").await;
    let bob = b.account("bob@example.org").await;
    let inner = b
        .group("inner@example.org", vec![bob], SenderRestriction::Anyone)
        .await;
    b.contact("partner@partner.example", "spb", vec![inner])
        .await;
    // Цикл вложенности не зацикливает раскрытие.
    let all = b
        .group(
            "everyone@example.org",
            vec![GroupMember::Group(msk), GroupMember::Group(inner)],
            SenderRestriction::Anyone,
        )
        .await;
    let mut g = b
        .env
        .groups
        .get(TenantId::DEFAULT, inner)
        .await
        .expect("get")
        .expect("inner");
    g.members.push(GroupMember::Group(all));
    b.env.groups.put(g).await.expect("put");
    let d = b.delivery();
    let e = envelope(Mode::Submission, &["everyone@example.org"]);
    assert_eq!(d.data(&e, Bytes::from_static(MAIL)).await.code, 250);
    assert_eq!(b.env.inbox("bob@example.org").await.len(), 1);
    assert_eq!(b.env.inbox("carol@example.org").await.len(), 1);
    assert_eq!(b.queued(), [["partner@partner.example"]]);
}

#[tokio::test]
async fn sender_restriction_of_dynamic_group_and_by_dynamic_group() {
    let b = Book::new().await;
    let msk = b
        .group("msk@example.org", Vec::new(), SenderRestriction::Internal)
        .await;
    b.dynamic(msk, "msk", false).await;
    b.office("alice@example.org", "msk").await;
    // Писать в закрытую группу могут только участники динамической группы.
    let bob = b.account("bob@example.org").await;
    b.group(
        "board@example.org",
        vec![bob],
        SenderRestriction::Only {
            accounts: Vec::new(),
            groups: vec![msk],
        },
    )
    .await;
    let d = b.delivery();
    // Динамическая группа с ограничением «только свои»: внешний отправитель — отказ, как у группы каталога.
    let outside = envelope(Mode::Inbound, &[]);
    let r = d.rcpt(&outside, "msk@example.org").await;
    assert_eq!((r.code, r.esc), (550, [5, 7, 1]));
    let list = rcpts(&["board@example.org"]);
    let sent = d
        .submit(
            TenantId::DEFAULT,
            None,
            "alice@example.org",
            &list,
            Bytes::from_static(MAIL),
        )
        .await
        .expect("submit");
    assert_eq!(sent.rcpts[0].1, SubmitStatus::Delivered);
    assert_eq!(b.env.inbox("bob@example.org").await.len(), 1);
    let sent = d
        .submit(
            TenantId::DEFAULT,
            None,
            "carol@example.org",
            &list,
            Bytes::from_static(MAIL),
        )
        .await
        .expect("submit");
    assert!(matches!(&sent.rcpts[0].1, SubmitStatus::Failed(r) if r.starts_with("550 5.7.1")));
    assert_eq!(b.env.inbox("bob@example.org").await.len(), 1);
}
