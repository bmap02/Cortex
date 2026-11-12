//! Stopping a turn: one handle per session, shared by the front-end (Ctrl-C, a Stop button) and
//! everything the turn runs (the model stream, tool processes).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Clone, Default)]
pub struct CancelHandle(Arc<AtomicBool>);

impl CancelHandle {
    pub fn new() -> CancelHandle { CancelHandle::default() }
    /// Stop the turn in progress: the reply stream ends and a running tool process is killed.
    pub fn cancel(&self) { self.0.store(true, Ordering::SeqCst); }
    pub fn is_cancelled(&self) -> bool { self.0.load(Ordering::SeqCst) }
    /// Clear a stop left over from earlier, before starting new work.
    pub fn reset(&self) { self.0.store(false, Ordering::SeqCst); }
}
