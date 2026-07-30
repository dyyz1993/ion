// Clippy policy: same rationale as ion/src/lib.rs — allow the specific
// historical rules currently triggered so CI's strict -D warnings passes,
// while new lints still surface. Remove a line once its occurrences are
// cleaned up.
#![allow(
    clippy::collapsible_if,
    clippy::doc_overindented_list_items,
    clippy::large_enum_variant,
    clippy::let_unit_value,
    clippy::manual_strip,
    clippy::new_without_default,
    clippy::type_complexity,
    clippy::unnecessary_filter_map,
    clippy::unnecessary_lazy_evaluations,
    clippy::useless_format,
    clippy::while_let_loop
)]

pub mod auth;
pub mod env_keys;
pub mod error;
pub mod event_stream;
pub mod faux;
pub mod paths;
pub mod provider;
pub mod record;
pub mod registry;
pub mod replay;
pub mod transform_messages;
pub mod types;

pub use auth::*;
pub use error::*;
pub use event_stream::*;
pub use faux::*;
pub use provider::*;
pub use registry::*;
pub use types::*;
