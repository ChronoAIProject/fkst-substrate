//! Shared types between fkst-supervisor and fkst-framework.

pub mod built_in_provider;
pub mod config;
pub mod durable_layout;
pub mod error;
pub mod event;
pub mod runtime_layout;
pub mod validation;

pub use config::Config;
pub use durable_layout::{DurableLayout, DURABLE_ROOT_ENV};
pub use error::FkstError;
pub use event::Event;
pub use runtime_layout::{
    runtime_key_file, RuntimeKind, RuntimeLayout, RUNTIME_LOCK_LEAF, RUNTIME_MARK_LEAF,
    RUNTIME_VALUE_LEAF,
};
pub use validation::validate_runtime_key;
