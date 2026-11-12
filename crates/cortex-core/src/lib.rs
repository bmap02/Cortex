//! The Cortex core: the agent loop, model backends, tools, config, the calculator and sessions,
//! shared by the Cortex front-ends.
//!
//! `ui` and `markdown` are terminal rendering. They live here only because the agent loop still
//! prints as it runs; they move to the CLI with the event-driven agent loop
//! (`.scratch/cortex-roadmap/issues/02-event-driven-agent-loop.md`).

pub mod agent;
pub mod backend;
pub mod calc;
pub mod config;
pub mod markdown;
pub mod tools;
pub mod ui;
