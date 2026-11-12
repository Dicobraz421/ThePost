//! In-memory реализации контрактов и тестовый корпус писем для тестов полос.
//!
//! Реализации разделяют состояние между клонами (`Arc` внутри) и берут время из [`Clock`]:
//! тесты таймаутов используют [`ManualClock`]. Фейки Ф1 для протокольных полос — [`fakes`]. Общие наборы тестов контрактов — [`suite`].

pub mod abuse;
pub mod acl;
pub mod addressbook;
pub mod admin_tokens;
pub mod app_passwords;
pub mod audit;
pub mod blob;
mod busy;
pub mod corpus;
pub mod dav;
pub mod delivery;
pub mod fakes;
pub mod groups;
pub mod identity;
pub mod infra;
pub mod load;
pub mod membership;
pub mod meta;
pub mod outbound;
pub mod policies;
pub mod rpc;
pub mod schedule;
pub mod secrets;
pub mod senders;
pub mod sieve;
pub mod suite;
pub mod transport;

pub use abuse::MemAbuseBlocks;
pub use acl::MemAcl;
pub use addressbook::{MemAddressBook, MemSyncGate};
pub use admin_tokens::MemLocalAdminTokens;
pub use app_passwords::MemAppPasswords;
pub use audit::MemMailboxAudit;
pub use blob::{BlobOp, MemBlobIndex, MemBlobStore};
pub use corpus::{CorpusCard, LoadModel, SyntheticMail, corpus, pg_schema, pg_url, vcards};
pub use dav::MemDav;
pub use delivery::MemDeliveryLog;
pub use fakes::{
    FakeAuthenticator, FakeDirectory, FakeSasl, FakeSearch, HookCall, MemIndexManifests, MemQueue,
    MemQuota, MemTrash, RecordingDecisionHook, SearchDoc, principal,
};
pub use groups::{MemGroups, MemModeration, ModerationDecision};
pub use identity::{MemIdentities, MemSubmissions};
pub use infra::{
    Clock, ManualClock, MemEventBus, MemEventLog, MemLease, MemPartitionAssigner,
    MemShardedEventLog, SystemClock,
};
pub use load::MemLoadMonitor;
pub use membership::MemMembership;
pub use meta::MemMetadata;
pub use outbound::{MemOutbound, Queued, Scheduled};
pub use policies::{MemPolicies, PolicyLevel};
pub use rpc::{MemRpc, MemRpcNet};
pub use schedule::{FakeItip, MemItipSender};
pub use secrets::MemSecretStore;
pub use senders::MemSenderLists;
pub use sieve::{FakeRules, MemSieve, MemVacationLog, RulesCall};
pub use transport::MemTransport;
