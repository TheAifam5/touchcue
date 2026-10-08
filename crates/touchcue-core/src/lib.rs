//! Domain model, request state machine, prompt templates and configuration, with no OS dependencies.

pub mod assuan;
pub mod config;
pub mod ctaphid;
pub mod helper;
pub mod limit;
pub mod machine;
pub mod model;
pub mod placeholders;
pub mod skip;
pub mod template;
pub mod text;

#[cfg(test)]
mod test_error;

pub use config::{Config, ConfigError, Hook, HookEvent, Lifetime, OnChange, Rendered, StopSignal};
pub use limit::RateLimit;
pub use machine::{EndReason, Event, Machine, MachineConfig, Request, RequestId, RequestState};
pub use model::{
    Device, DeviceId, DeviceKind, Method, Op, Outcome, Signal, SignalClass, SignalKind, Source,
    Transport,
};
pub use placeholders::{AppInfo, Confidence, ProcessInfo, Requester};
pub use template::{RenderError, Template, TemplateError};
