//! Flash agent runtime: orchestrates provider streaming, tool dispatch, and
//! session persistence. This module is a thin facade re-exporting the public
//! API surface; all logic lives in focused submodules.

mod artifact;
mod context;
mod control;
mod error;
mod finalize;
mod hooks;
mod provider_attempt;
mod runtime;
#[cfg(any(test, feature = "smoke"))]
mod smoke;
mod tool_scheduler;
mod turn;

#[cfg(test)]
mod test_support;

pub use crate::error::{AgentError, AgentRun};
pub use crate::hooks::{ApprovalController, ApprovalRequest, EventObserver};
pub use crate::runtime::{AgentOptions, AgentRuntime};

#[cfg(any(test, feature = "smoke"))]
pub use crate::smoke::SmokeProvider;
