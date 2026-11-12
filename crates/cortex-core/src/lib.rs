//! The Cortex core: the agent loop, model backends, tools, config, the calculator and sessions,
//! shared by the Cortex front-ends. It never prints or reads stdin during a turn: a turn reports
//! what happens as events (see [`agent`]) and front-ends do the presenting.

pub mod agent;
pub mod backend;
pub mod calc;
pub mod cancel;
pub mod config;
pub mod tools;
