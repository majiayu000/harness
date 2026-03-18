use harness_core::TaskId;

#[test]
fn task_id_new_is_unique() {
    let a = TaskId::new();
    let b = TaskId::new();
    assert_ne!(a, b);
}

#[test]
fn task_id_serde_roundtrip() {
    let id = TaskId::new();
    let json = serde_json::to_string(&id).unwrap();
    let back: TaskId = serde_json::from_str(&json).unwrap();
    assert_eq!(id, back);
}

#[test]
fn task_id_from_str_roundtrip() {
    let id = TaskId::new();
    let s = id.as_str().to_owned();
    let back = TaskId::from_str(&s);
    assert_eq!(id, back);
}

#[test]
fn task_id_display() {
    let id = TaskId::from_str("test-task-123");
    assert_eq!(id.to_string(), "test-task-123");
}
