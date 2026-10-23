use super::SafeHttpResponseDiagnostics;
use super::StreamableHttpClientAdapterError;
use super::safe_http_response_diagnostics;
use super::validate_cloudflare_ray;
use codex_exec_server::HttpHeader;

#[test]
fn http_failure_displays_only_validated_transport_hints() {
    let headers = vec![
        header("cf-ray", "0123456789abcdef-SYD"),
        header("via", "untrusted gateway value"),
        header("server", "internal-host.example"),
        header("x-request-id", "attacker supplied request id"),
        header("authorization", "Bearer secret-value"),
        header("cookie", "session=secret-value"),
        header("set-cookie", "session=secret-value"),
        header("location", "https://internal.example/path?token=secret"),
        header("www-authenticate", "Bearer realm=internal"),
        header("cf-ray", "poisoned-value.internal.example"),
    ];
    let error = StreamableHttpClientAdapterError::HttpStatus {
        status: 502,
        body: "existing bounded body preview".into(),
        retry_after: None,
        diagnostics: safe_http_response_diagnostics(&headers),
    };

    let rendered = error.to_string();
    assert!(rendered.starts_with("HTTP 502: existing bounded body preview"));
    assert!(rendered.contains("provider=cloudflare cf-ray=0123456789abcdef-SYD"));
    assert!(rendered.contains("via-present"));
    for forbidden in [
        "untrusted gateway value",
        "internal-host.example",
        "attacker supplied request id",
        "secret-value",
        "internal.example",
        "realm=internal",
        "poisoned-value",
    ] {
        assert!(!rendered.contains(forbidden), "unexpected {forbidden:?}");
    }
}

#[test]
fn invalid_provider_hints_are_omitted_and_empty_hints_render_nothing() {
    for invalid in [
        "0123456789abcde-SYD",
        "0123456789abcdef-syd",
        "0123456789abcdef-SYD\nforged",
        "0123456789abcdef-TOOLONG",
        "0123456789abcdef-SYD.internal.example",
    ] {
        assert_eq!(validate_cloudflare_ray(invalid), None);
    }
    assert_eq!(
        validate_cloudflare_ray("0123456789abcdef-SYD").as_deref(),
        Some("0123456789abcdef-SYD")
    );
    assert_eq!(SafeHttpResponseDiagnostics::default().to_string(), "");
    assert_eq!(
        safe_http_response_diagnostics(&[header("via", "arbitrary")]).to_string(),
        " [transport: via-present]"
    );
}

fn header(name: &str, value: &str) -> HttpHeader {
    HttpHeader {
        name: name.into(),
        value: value.into(),
    }
}
