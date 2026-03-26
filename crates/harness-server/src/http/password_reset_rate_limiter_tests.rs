use super::PasswordResetRateLimiter;

#[test]
fn allows_requests_within_limit() {
    let limiter = PasswordResetRateLimiter::new(3);
    assert!(limiter.check_and_increment("user@example.com"));
    assert!(limiter.check_and_increment("user@example.com"));
    assert!(limiter.check_and_increment("user@example.com"));
}

#[test]
fn blocks_after_limit_exceeded() {
    let limiter = PasswordResetRateLimiter::new(2);
    assert!(limiter.check_and_increment("a@example.com"));
    assert!(limiter.check_and_increment("a@example.com"));
    assert!(!limiter.check_and_increment("a@example.com"));
}

#[test]
fn limits_are_per_identifier() {
    let limiter = PasswordResetRateLimiter::new(1);
    assert!(limiter.check_and_increment("alice@example.com"));
    assert!(!limiter.check_and_increment("alice@example.com"));
    // Different email is independent
    assert!(limiter.check_and_increment("bob@example.com"));
}

#[test]
fn rejects_new_identifiers_when_key_cap_reached() {
    // Cap at 2 tracked identifiers to test the memory-DoS guard.
    let limiter = PasswordResetRateLimiter::new_with_cap(10, 2);
    assert!(limiter.check_and_increment("a@example.com"));
    assert!(limiter.check_and_increment("b@example.com"));
    // Third unique identifier is rejected because cap is reached.
    assert!(!limiter.check_and_increment("c@example.com"));
    // Already-tracked identifiers still work.
    assert!(limiter.check_and_increment("a@example.com"));
}
