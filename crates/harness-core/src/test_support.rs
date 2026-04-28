use std::sync::{Mutex, MutexGuard};

static PROCESS_ENV_LOCK: Mutex<()> = Mutex::new(());

pub(crate) fn process_env_lock() -> MutexGuard<'static, ()> {
    PROCESS_ENV_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}
