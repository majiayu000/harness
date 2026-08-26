use std::sync::atomic::{AtomicU8, Ordering};

const EXECUTION_PENDING: u8 = 0;
const EXECUTION_ADMITTED: u8 = 1;
const EXECUTION_CANCELLED: u8 = 2;

#[derive(Debug, Default)]
pub(super) struct RuntimeExecutionAdmission {
    state: AtomicU8,
}

impl RuntimeExecutionAdmission {
    pub(super) fn try_admit(&self) -> bool {
        self.state
            .compare_exchange(
                EXECUTION_PENDING,
                EXECUTION_ADMITTED,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
    }

    pub(super) fn cancel(&self) -> bool {
        self.state
            .compare_exchange(
                EXECUTION_PENDING,
                EXECUTION_CANCELLED,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Barrier};

    #[test]
    fn cancellation_and_execution_admission_have_one_linearized_winner() {
        for _ in 0..1_000 {
            let admission = Arc::new(RuntimeExecutionAdmission::default());
            let barrier = Arc::new(Barrier::new(2));
            let cancel_admission = admission.clone();
            let cancel_barrier = barrier.clone();
            let cancellation = std::thread::spawn(move || {
                cancel_barrier.wait();
                cancel_admission.cancel()
            });

            barrier.wait();
            let execution_admitted = admission.try_admit();
            let cancellation_won = cancellation.join().expect("join cancellation racer");

            assert_ne!(execution_admitted, cancellation_won);
            assert!(!admission.try_admit());
        }
    }

    #[test]
    fn prefired_cancellation_fences_execution_admission() {
        let admission = RuntimeExecutionAdmission::default();

        assert!(admission.cancel());
        assert!(!admission.try_admit());
    }
}
