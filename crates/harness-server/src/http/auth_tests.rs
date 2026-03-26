use super::percent_decode;

#[test]
fn percent_decode_plain_text_unchanged() {
    assert_eq!(percent_decode("mytoken123"), "mytoken123");
}

#[test]
fn percent_decode_slash_encoded() {
    assert_eq!(percent_decode("tok%2Fwith%2Fslashes"), "tok/with/slashes");
}

#[test]
fn percent_decode_equals_and_plus() {
    assert_eq!(percent_decode("base64%3D%3D"), "base64==");
    assert_eq!(percent_decode("a%2Bb"), "a+b");
}

#[test]
fn percent_decode_percent_encoded_percent() {
    assert_eq!(percent_decode("100%25"), "100%");
}

#[test]
fn percent_decode_incomplete_sequence_passed_through() {
    // Trailing % with fewer than 2 hex digits must not panic.
    assert_eq!(percent_decode("abc%2"), "abc%2");
    assert_eq!(percent_decode("abc%"), "abc%");
}

#[test]
fn percent_decode_invalid_hex_passed_through() {
    // Non-hex digits after % must be passed through unchanged.
    assert_eq!(percent_decode("abc%ZZdef"), "abc%ZZdef");
}
