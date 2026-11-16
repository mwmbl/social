//! FASP (Fediverse Auxiliary Service Provider) `general` and `data_sharing`, in both directions.
//!
//! - [`registration`]: discover a server's FASP base URL and register with it (we initiate).
//! - [`http`]: the axum router serving our FASP endpoints (`provider_info`, capability
//!   activation, `data_sharing` announcements), with request authentication and response signing.
//! - [`client`]: a signed client for one registration's FASP API. It refuses to call a
//!   capability's API unless the store says that capability is enabled (CLAUDE.md principle 4).
//! - [`data_sharing`]: the orchestration the binary runs when `data_sharing` is switched on or
//!   off and when backfill announcements arrive.
//!
//! The crate never reads a clock or a database directly: the binary supplies both through
//! [`StoreAccess`] and [`http::FaspState`]. Nothing here logs object content, only registration
//! ids, hosts and counts.

#![deny(
    clippy::exit,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::unwrap_used,
    clippy::panic,
    clippy::string_slice,
    clippy::unimplemented,
    clippy::unreachable,
    clippy::todo,
    clippy::pedantic
)]
// As in mwmbl_core: every public fn here returns Result, and the error types say what failed.
#![allow(clippy::missing_errors_doc)]

mod access;
pub mod announcement;
pub mod client;
pub mod data_sharing;
mod error;
pub mod http;
pub mod registration;
mod types;

pub use access::StoreAccess;
pub use error::{ClientError, NotEnabled, RegistrationError};
pub use types::{
    ACCOUNT_SEARCH_ID, ACCOUNT_SEARCH_VERSION, Capability, DATA_SHARING_ID, DATA_SHARING_VERSION,
    PrivacyPolicy, ProviderInfoConfig, system_now,
};
