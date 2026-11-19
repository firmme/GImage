use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// Cooperative cancellation shared between the UI, scanners and batch workers.
///
/// Workers must poll `is_cancelled()` between items and abort long loops early;
/// nothing is force-killed, so temporary files can always be cleaned up.
#[derive(Clone, Debug, Default)]
pub struct CancellationToken(Arc<AtomicBool>);

impl CancellationToken {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }

    /// Reset for reuse (e.g. "retry failed items").
    pub fn reset(&self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clones_share_state() {
        let token = CancellationToken::new();
        let other = token.clone();
        other.cancel();
        assert!(token.is_cancelled());
        token.reset();
        assert!(!other.is_cancelled());
    }
}