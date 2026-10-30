//! Идентификаторы (CONTRACTS.md#Ids, SRS 3.2, 3.8).

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use uuid::Uuid;

use crate::error::{Error, Result};

macro_rules! uuid_id {
    ($(#[$m:meta])* $name:ident) => {
        $(#[$m])*
        #[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(Uuid);

        impl $name {
            /// Новый идентификатор (UUID v7: упорядочен по времени создания).
            pub fn new() -> Self {
                Self(Uuid::now_v7())
            }

            /// Из готового UUID (чтение из БД, разбор).
            pub const fn from_uuid(id: Uuid) -> Self {
                Self(id)
            }

            /// Значение UUID.
            pub const fn as_uuid(&self) -> &Uuid {
                &self.0
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                fmt::Display::fmt(&self.0.hyphenated(), f)
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, concat!(stringify!($name), "({})"), self.0.hyphenated())
            }
        }

        impl FromStr for $name {
            type Err = Error;

            fn from_str(s: &str) -> Result<Self> {
                Uuid::parse_str(s)
                    .map(Self)
                    .map_err(|e| Error::Invalid(format!(concat!(stringify!($name), ": {}"), e)))
            }
        }
    };
}

uuid_id!(
    /// Организация (tenant, SRS 3.8). Входит в ключи всех таблиц и объектов BLOB.
    TenantId
);
uuid_id!(
    /// Почтовый домен организации.
    DomainId
);
uuid_id!(
    /// Учётная запись (почтовый ящик пользователя, общий ящик).
    AccountId
);
uuid_id!(
    /// Папка (IMAP mailbox / JMAP Mailbox) внутри учётной записи.
    MailboxId
);
uuid_id!(
    /// Письмо в папке учётной записи.
    MessageId
);

impl TenantId {
    /// Организация по умолчанию — единственная в базовой поставке (SRS 3.8).
    /// Фиксированное значение `00000000-0000-0000-0000-000000000001` (не nil, чтобы не путать с «не задано»).
    pub const DEFAULT: TenantId = TenantId(Uuid::from_u128(1));
}

macro_rules! num_id {
    ($(#[$m:meta])* $name:ident($t:ty)) => {
        $(#[$m])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub $t);

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                fmt::Display::fmt(&self.0, f)
            }
        }
    };
}

num_id!(
    /// IMAP UID письма в папке: ≥ 1, строго возрастает в пределах `UidValidity`, не переиспользуется.
    Uid(u32)
);
num_id!(
    /// IMAP UIDVALIDITY папки: ≥ 1; меняется, только если UID папки больше не действительны.
    UidValidity(u32)
);
num_id!(
    /// Номер изменения (IMAP CONDSTORE MODSEQ, состояние JMAP), см. инварианты в `meta`.
    ModSeq(u64)
);
num_id!(
    /// Поколение объекта BLOB с данным хэшем (SRS 3.4): растёт, если запись была `deleting`.
    Gen(u32)
);

impl Uid {
    /// Наименьший допустимый UID.
    pub const MIN: Uid = Uid(1);

    /// Следующий UID.
    pub fn next(self) -> Option<Uid> {
        self.0.checked_add(1).map(Uid)
    }
}

impl ModSeq {
    /// Начальное значение: изменений ещё не было.
    pub const ZERO: ModSeq = ModSeq(0);

    /// Следующий номер изменения.
    pub fn next(self) -> ModSeq {
        ModSeq(self.0 + 1)
    }
}

impl Gen {
    /// Первое поколение объекта.
    pub const INITIAL: Gen = Gen(0);

    /// Следующее поколение (после `deleting`).
    pub fn next(self) -> Gen {
        Gen(self.0 + 1)
    }
}

/// SHA-256 содержимого объекта BLOB. `Display`, `FromStr` и serde — 64 символа hex в нижнем регистре.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct BlobHash(pub [u8; 32]);

impl BlobHash {
    /// Байты хэша.
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl fmt::Display for BlobHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut buf = [0u8; 64];
        for (i, b) in self.0.iter().enumerate() {
            buf[2 * i] = HEX[usize::from(b >> 4)];
            buf[2 * i + 1] = HEX[usize::from(b & 0x0f)];
        }
        // Только ASCII-символы hex.
        f.write_str(std::str::from_utf8(&buf).map_err(|_| fmt::Error)?)
    }
}

impl fmt::Debug for BlobHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "BlobHash({self})")
    }
}

impl FromStr for BlobHash {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        fn nibble(c: u8) -> Option<u8> {
            match c {
                b'0'..=b'9' => Some(c - b'0'),
                b'a'..=b'f' => Some(c - b'a' + 10),
                b'A'..=b'F' => Some(c - b'A' + 10),
                _ => None,
            }
        }
        let s = s.as_bytes();
        if s.len() != 64 {
            return Err(Error::invalid("BlobHash: expected 64 hex chars"));
        }
        let mut out = [0u8; 32];
        for (i, pair) in s.chunks_exact(2).enumerate() {
            match (nibble(pair[0]), nibble(pair[1])) {
                (Some(h), Some(l)) => out[i] = (h << 4) | l,
                _ => return Err(Error::invalid("BlobHash: not hex")),
            }
        }
        Ok(BlobHash(out))
    }
}

impl Serialize for BlobHash {
    fn serialize<S: Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for BlobHash {
    fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        let s = <std::borrow::Cow<'de, str>>::deserialize(d)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

/// Ключ объекта BLOB: организация + хэш + поколение (SRS 3.2, 3.4, 3.8).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct BlobKey {
    /// Организация — дедупликация только в её пределах.
    pub tenant: TenantId,
    /// SHA-256 содержимого.
    pub hash: BlobHash,
    /// Поколение.
    pub generation: Gen,
}

impl BlobKey {
    /// Путь объекта в хранилище: `"{tenant}/{hash[0..2]}/{hash}/{gen}"`.
    pub fn path(&self) -> String {
        self.to_string()
    }

    /// Префикс всех объектов организации (для `BlobStore::list`).
    pub fn tenant_prefix(tenant: TenantId) -> String {
        format!("{tenant}/")
    }

    /// Разбор пути, выданного `path()`; чужие пути (сегменты индексов и т. п.) — `Invalid`.
    pub fn parse_path(path: &str) -> Result<BlobKey> {
        let bad = || Error::Invalid(format!("not a blob key path: {path}"));
        let mut it = path.split('/');
        let (Some(tenant), Some(fan), Some(hash), Some(generation), None) =
            (it.next(), it.next(), it.next(), it.next(), it.next())
        else {
            return Err(bad());
        };
        if hash.get(..2) != Some(fan) {
            return Err(bad());
        }
        Ok(BlobKey {
            tenant: tenant.parse().map_err(|_| bad())?,
            hash: hash.parse().map_err(|_| bad())?,
            generation: Gen(generation.parse().map_err(|_| bad())?),
        })
    }
}

impl fmt::Display for BlobKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let hash = self.hash.to_string();
        write!(f, "{}/{}/{}/{}", self.tenant, &hash[..2], hash, self.generation)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // Временно: serde_json не подключён к tp-contracts (dev-зависимость — полоса deps), берём реэкспорт schemars.
    use schemars::_private::serde_json;

    fn roundtrip<T>(v: &T) -> String
    where
        T: Serialize + for<'de> Deserialize<'de> + PartialEq + fmt::Debug,
    {
        let s = serde_json::to_string(v).expect("serialize");
        let back: T = serde_json::from_str(&s).expect("deserialize");
        assert_eq!(&back, v);
        s
    }

    fn hash() -> BlobHash {
        let mut b = [0u8; 32];
        for (i, x) in b.iter_mut().enumerate() {
            *x = (i as u8).wrapping_mul(37) ^ 0xa5;
        }
        BlobHash(b)
    }

    #[test]
    fn uuid_ids_serde_and_str() {
        let id = AccountId::new();
        assert_eq!(roundtrip(&id), format!("\"{id}\""));
        assert_eq!(id.to_string().parse::<AccountId>(), Ok(id));
        assert!("nope".parse::<MailboxId>().is_err());
        roundtrip(&TenantId::DEFAULT);
        assert_eq!(
            TenantId::DEFAULT.to_string(),
            "00000000-0000-0000-0000-000000000001"
        );
        assert!(MessageId::new() != MessageId::new());
    }

    #[test]
    fn numeric_ids_serde() {
        assert_eq!(roundtrip(&Uid(42)), "42");
        assert_eq!(roundtrip(&ModSeq(u64::MAX)), u64::MAX.to_string());
        roundtrip(&UidValidity(7));
        roundtrip(&Gen(3));
        assert_eq!(Uid::MIN.next(), Some(Uid(2)));
        assert_eq!(Uid(u32::MAX).next(), None);
        assert_eq!(Gen::INITIAL.next(), Gen(1));
    }

    #[test]
    fn blob_hash_hex() {
        let h = hash();
        let s = h.to_string();
        assert_eq!(s.len(), 64);
        assert!(s.bytes().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
        assert_eq!(s.parse::<BlobHash>(), Ok(h));
        assert_eq!(s.to_uppercase().parse::<BlobHash>(), Ok(h));
        assert_eq!(roundtrip(&h), format!("\"{s}\""));
        assert!(s[..63].parse::<BlobHash>().is_err());
        assert!(format!("{}g", &s[..63]).parse::<BlobHash>().is_err());
    }

    #[test]
    fn blob_key_path() {
        let key = BlobKey {
            tenant: TenantId::DEFAULT,
            hash: hash(),
            generation: Gen(2),
        };
        let h = key.hash.to_string();
        let path = key.path();
        assert_eq!(
            path,
            format!("00000000-0000-0000-0000-000000000001/{}/{h}/2", &h[..2])
        );
        assert!(path.starts_with(&BlobKey::tenant_prefix(TenantId::DEFAULT)));
        assert_eq!(BlobKey::parse_path(&path), Ok(key));
        roundtrip(&key);
        for bad in [
            "",
            "idx/x/y",
            &format!("{}/zz/{h}/2", TenantId::DEFAULT),
            &format!("{path}/extra"),
            &format!("{}/{}/{h}/x", TenantId::DEFAULT, &h[..2]),
        ] {
            assert!(BlobKey::parse_path(bad).is_err(), "{bad}");
        }
    }
}
