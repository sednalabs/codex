use super::redact_bytes;

#[test]
fn response_body_redacts_launcher_credentials_and_proof() {
    let mut body = br#"Bearer test-bearer test-certificate test-proof test-bearer"#.to_vec();

    redact_bytes(&mut body, "test-bearer");
    redact_bytes(&mut body, "test-certificate");
    redact_bytes(&mut body, "test-proof");

    assert_eq!(
        String::from_utf8(body).expect("redacted response is UTF-8"),
        "Bearer [runtime secret redacted] [runtime secret redacted] [runtime secret redacted] [runtime secret redacted]"
    );
}
