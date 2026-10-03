//! Session 机制：多轮会话、多会话管理、审批挂起与恢复。
//!
//! 组件关系：`Agent` → `SessionManager` → 每个 session 一个常驻 `ReactLoop`。

pub mod manager;
pub mod models;

pub use manager::{InMemorySessionManager, SessionManager, SessionRuntimeConfig};
pub use models::{PendingCall, Session, SessionSummary, state_keys};
