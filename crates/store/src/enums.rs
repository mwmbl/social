//! String-backed enums stored in TEXT columns.

use std::fmt;
use std::str::FromStr;

use rusqlite::types::{FromSql, FromSqlError, FromSqlResult, ToSql, ToSqlOutput, ValueRef};

use crate::StoreError;

macro_rules! string_enum {
    ($(#[$meta:meta])* $name:ident, $kind:literal, { $($(#[$vmeta:meta])* $variant:ident => $text:literal),+ $(,)? }) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub enum $name {
            $($(#[$vmeta])* $variant),+
        }

        impl $name {
            /// The string stored in the database.
            #[must_use]
            pub const fn as_str(self) -> &'static str {
                match self { $(Self::$variant => $text),+ }
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(self.as_str())
            }
        }

        impl FromStr for $name {
            type Err = StoreError;

            fn from_str(s: &str) -> Result<Self, StoreError> {
                match s {
                    $($text => Ok(Self::$variant),)+
                    _ => Err(StoreError::UnknownValue { kind: $kind, value: s.to_owned() }),
                }
            }
        }

        impl ToSql for $name {
            fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
                Ok(ToSqlOutput::from(self.as_str()))
            }
        }

        impl FromSql for $name {
            fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
                let text = value.as_str()?;
                text.parse().map_err(|e| FromSqlError::Other(Box::new(e)))
            }
        }
    };
}

string_enum!(
    /// Lifecycle of a FASP registration.
    RegistrationStatus, "registration status", {
        Pending => "pending",
        Active => "active",
        Revoked => "revoked",
    }
);

string_enum!(
    /// Data-sharing subscription category.
    Category, "category", {
        Content => "content",
        Account => "account",
    }
);

string_enum!(
    /// Data-sharing subscription type.
    SubscriptionType, "subscription type", {
        Lifecycle => "lifecycle",
        Trends => "trends",
    }
);

string_enum!(
    /// State of a backfill request.
    BackfillState, "backfill state", {
        Active => "active",
        Done => "done",
        Failed => "failed",
    }
);

string_enum!(
    /// What a queued URI is expected to be.
    FetchKind, "fetch kind", {
        Object => "object",
        Actor => "actor",
        Unknown => "unknown",
    }
);

string_enum!(
    /// Why a URI is queued. Determines its priority.
    FetchReason, "fetch reason", {
        Announce => "announce",
        Backfill => "backfill",
        Revalidate => "revalidate",
    }
);

impl FetchReason {
    /// Queue priority: lower is more urgent. Announce 0, backfill 1, revalidate 2.
    #[must_use]
    pub const fn priority(self) -> i64 {
        match self {
            Self::Announce => 0,
            Self::Backfill => 1,
            Self::Revalidate => 2,
        }
    }
}

string_enum!(
    /// HTTP signature scheme that a host accepted.
    SignatureScheme, "signature scheme", {
        Rfc9421 => "rfc9421",
        Cavage => "cavage",
    }
);

string_enum!(
    /// Whether a tombstone covers one object or everything by one author.
    TombstoneKind, "tombstone kind", {
        Object => "object",
        Author => "author",
    }
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_and_rejects_unknown() {
        for s in [
            RegistrationStatus::Pending,
            RegistrationStatus::Active,
            RegistrationStatus::Revoked,
        ] {
            assert_eq!(s.as_str().parse::<RegistrationStatus>().ok(), Some(s));
            assert_eq!(s.to_string(), s.as_str());
        }
        assert!(matches!(
            "bogus".parse::<Category>(),
            Err(StoreError::UnknownValue {
                kind: "category",
                ..
            })
        ));
        assert_eq!(FetchReason::Announce.priority(), 0);
        assert_eq!(FetchReason::Backfill.priority(), 1);
        assert_eq!(FetchReason::Revalidate.priority(), 2);
    }
}
