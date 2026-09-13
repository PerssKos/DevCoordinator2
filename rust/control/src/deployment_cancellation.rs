//! Cancellation ownership for explicitly finite deployment operations.
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

#[derive(Clone, Default)]
pub(crate) struct FiniteOperations(Arc<Mutex<BTreeMap<String, Arc<Operation>>>>);

struct Operation {
    // active, cancellation requested, or sealed for successful commit
    phase: AtomicU8,
    flag: Arc<AtomicBool>,
    finished: Mutex<bool>,
    wake: Condvar,
}

pub(crate) struct FiniteGuard {
    id: String,
    registry: FiniteOperations,
    operation: Arc<Operation>,
}

impl FiniteOperations {
    pub(crate) fn begin(&self, id: &str) -> FiniteGuard {
        let operation = Arc::new(Operation {
            phase: AtomicU8::new(0),
            flag: Arc::new(AtomicBool::new(false)),
            finished: Mutex::new(false),
            wake: Condvar::new(),
        });
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(id.into(), operation.clone());
        FiniteGuard {
            id: id.into(),
            registry: self.clone(),
            operation,
        }
    }

    pub(crate) fn flag(&self, id: &str) -> Option<Arc<AtomicBool>> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(id)
            .map(|op| op.flag.clone())
    }

    pub(crate) fn cancelled(&self, id: &str) -> bool {
        self.flag(id)
            .is_some_and(|flag| flag.load(Ordering::Acquire))
    }

    pub(crate) fn seal(&self, id: &str) -> bool {
        let operation = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(id)
            .cloned();
        operation.is_none_or(|op| {
            op.phase
                .compare_exchange(0, 2, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
        })
    }

    // None: no owned finite operation. Some(true): cleanup settled. False: the
    // caller must continue observing; it must not claim cancellation completed.
    pub(crate) fn cancel_and_wait(&self, id: &str, timeout: Duration) -> Option<(bool, bool)> {
        let operation = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(id)
            .cloned()?;
        if operation
            .phase
            .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            operation.flag.store(true, Ordering::Release);
        }
        let requested = operation.phase.load(Ordering::Acquire) == 1;
        let finished = operation
            .finished
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (finished, _) = operation
            .wake
            .wait_timeout_while(finished, timeout, |done| !*done)
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Some((requested, *finished))
    }
}

impl Drop for FiniteGuard {
    fn drop(&mut self) {
        *self
            .operation
            .finished
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = true;
        self.operation.wake.notify_all();
        let mut registry = self
            .registry
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if registry
            .get(&self.id)
            .is_some_and(|op| Arc::ptr_eq(op, &self.operation))
        {
            registry.remove(&self.id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cancellation_is_exact_and_does_not_claim_unfinished_cleanup() {
        let registry = FiniteOperations::default();
        let guard = registry.begin("one");
        let other = registry.begin("two");
        assert_eq!(registry.cancel_and_wait("missing", Duration::ZERO), None);
        assert_eq!(
            registry.cancel_and_wait("one", Duration::ZERO),
            Some((true, false))
        );
        assert!(registry.cancelled("one"));
        assert!(!registry.cancelled("two"));
        assert!(!registry.seal("one"));
        drop(guard);
        assert!(registry.flag("one").is_none());
        assert!(registry.seal("two"));
        assert_eq!(
            registry.cancel_and_wait("two", Duration::ZERO),
            Some((false, false))
        );
        assert!(!registry.cancelled("two"));
        drop(other);
    }
}
