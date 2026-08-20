use std::{
    collections::HashMap,
    sync::{Arc, LazyLock, Mutex},
};
use tokio::sync::Notify;

static COMPLETION_RESERVATION_GATES: LazyLock<
    Mutex<HashMap<String, Arc<CompletionReservationTestGate>>>,
> = LazyLock::new(|| Mutex::new(HashMap::new()));

pub(crate) struct CompletionReservationTestGate {
    reached: Notify,
    release: Notify,
}

impl CompletionReservationTestGate {
    pub(crate) async fn wait_until_reached(&self) {
        self.reached.notified().await;
    }

    pub(crate) fn release(&self) {
        self.release.notify_one();
    }
}

pub(crate) fn install_completion_reservation_test_gate(
    runtime_job_id: &str,
) -> Arc<CompletionReservationTestGate> {
    let gate = Arc::new(CompletionReservationTestGate {
        reached: Notify::new(),
        release: Notify::new(),
    });
    let previous = COMPLETION_RESERVATION_GATES
        .lock()
        .unwrap()
        .insert(runtime_job_id.to_string(), Arc::clone(&gate));
    assert!(
        previous.is_none(),
        "completion reservation test gate already installed"
    );
    gate
}

pub(super) async fn pause_after_completion_reservation(runtime_job_id: &str) {
    let gate = COMPLETION_RESERVATION_GATES
        .lock()
        .unwrap()
        .remove(runtime_job_id);
    if let Some(gate) = gate {
        gate.reached.notify_one();
        gate.release.notified().await;
    }
}
