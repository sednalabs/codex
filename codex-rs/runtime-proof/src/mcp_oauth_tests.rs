use super::McpOAuthPolicy;
use super::TokenEndpointResponse;
use super::build_client_credentials_request;
use super::validate_token_response;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use pretty_assertions::assert_eq;
use serde_json::Value;
use uuid::Uuid;

const PRINCIPAL: &str = "22222222-2222-4222-8222-222222222222";
const PROJECT_ID: u64 = 17;
const NOW: i64 = 1_800_000_000;

fn policy() -> McpOAuthPolicy {
    McpOAuthPolicy {
        token_endpoint: "https://issuer.example/oauth/token".to_string(),
        expected_issuer: "https://issuer.example/".to_string(),
        client_id: "codex-work-claim".to_string(),
        audience: "https://mcp.example/".to_string(),
        requested_scope: "ops:write".to_string(),
    }
}

fn access_token(claims: Value) -> String {
    let header = serde_json::json!({"alg": "RS256", "kid": "issuer-key-1", "typ": "JWT"});
    format!(
        "{}.{}.{}",
        URL_SAFE_NO_PAD.encode(serde_json::to_vec(&header).unwrap()),
        URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).unwrap()),
        URL_SAFE_NO_PAD.encode(b"synthetic-signature")
    )
}

fn claims() -> Value {
    serde_json::json!({
        "iss": "https://issuer.example/",
        "sub": PRINCIPAL,
        "azp": "codex-work-claim",
        "aud": ["https://mcp.example/"],
        "exp": NOW + 600,
        "scope": "ops:write",
        "project_id": PROJECT_ID
    })
}

fn response(claims: Value) -> TokenEndpointResponse {
    TokenEndpointResponse {
        access_token: crate::provider_auth::SecretString(access_token(claims)),
        token_type: "Bearer".to_string(),
        expires_in: Some(120),
        scope: Some("ops:write".to_string()),
    }
}

#[test]
fn request_uses_fixed_client_credentials_scope_and_audience_without_secret_in_body() {
    let policy = policy();
    policy.validate().expect("fixed HTTPS policy is admitted");
    let secret = crate::provider_auth::SecretString("synthetic-client-secret".to_string());
    let client = reqwest::blocking::Client::builder()
        .https_only(true)
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .build()
        .expect("test client builds");
    let request = build_client_credentials_request(&client, &policy, &secret)
        .expect("fixed request builds");

    assert_eq!(request.method(), reqwest::Method::POST);
    assert_eq!(request.url().as_str(), policy.token_endpoint);
    let authorization = request
        .headers()
        .get(reqwest::header::AUTHORIZATION)
        .expect("OAuth client authentication is explicit")
        .to_str()
        .expect("authorization header is ASCII");
    let encoded_credentials = authorization
        .strip_prefix("Basic ")
        .expect("client secret uses HTTP Basic auth");
    assert_eq!(
        STANDARD.decode(encoded_credentials).expect("base64 credentials"),
        b"codex-work-claim:synthetic-client-secret"
    );
    let body = request
        .body()
        .and_then(reqwest::blocking::Body::as_bytes)
        .expect("fixed form body");
    let form = url::form_urlencoded::parse(body)
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect::<Vec<_>>();
    assert_eq!(
        form,
        vec![
            ("grant_type".to_string(), "client_credentials".to_string()),
            ("audience".to_string(), "https://mcp.example/".to_string()),
            ("scope".to_string(), "ops:write".to_string()),
        ]
    );
    assert!(!String::from_utf8_lossy(body).contains(&secret.0));

    let special_secret = crate::provider_auth::SecretString("secret:+/=".to_string());
    let special_request = build_client_credentials_request(&client, &policy, &special_secret)
        .expect("special OAuth secret is encoded per the client-secret-basic contract");
    let encoded_credentials = special_request
        .headers()
        .get(reqwest::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Basic "))
        .and_then(|value| STANDARD.decode(value).ok())
        .expect("encoded client authentication");
    assert_eq!(
        encoded_credentials,
        b"codex-work-claim:secret%3A%2B%2F%3D"
    );
}

#[test]
fn policy_rejects_non_https_or_dynamic_scope_inputs() {
    let mut policy = policy();
    policy.token_endpoint = "http://issuer.example/oauth/token".to_string();
    assert!(policy.validate().is_err());

    let mut policy = policy();
    policy.expected_issuer = "https://user:password@issuer.example/".to_string();
    assert!(policy.validate().is_err());

    let mut policy = policy();
    policy.token_endpoint = "https://issuer.example/oauth/token#fragment".to_string();
    assert!(policy.validate().is_err());

    let mut policy = policy();
    policy.token_endpoint = "https://issuer.example/oauth/token?access_token=private".to_string();
    assert!(policy.validate().is_err());

    let mut policy = policy();
    policy.requested_scope = "ops:write ops:admin".to_string();
    assert!(policy.validate().is_err());
}

#[test]
fn response_and_jwt_claims_must_bind_to_the_fixed_identity_contract() {
    let policy = policy();
    let valid = validate_token_response(
        response(claims()),
        &policy,
        Uuid::parse_str(PRINCIPAL).unwrap(),
        PROJECT_ID,
        NOW,
    )
    .expect("synthetic token claims match the launcher preflight contract");
    assert_eq!(valid.bearer.0, access_token(claims()));
    assert_eq!(valid.expires_at, NOW + 120);

    let mut standard_response = response(claims());
    standard_response.expires_in = None;
    standard_response.scope = None;
    let valid = validate_token_response(
        standard_response,
        &policy,
        Uuid::parse_str(PRINCIPAL).unwrap(),
        PROJECT_ID,
        NOW,
    )
    .expect("the signed token claims still bind scope and expiry when optional fields are absent");
    assert_eq!(valid.expires_at, NOW + 600);

    for (claim, value) in [
        ("iss", serde_json::json!("https://other.example/")),
        ("sub", serde_json::json!("33333333-3333-4333-8333-333333333333")),
        ("azp", serde_json::json!("other-client")),
        ("client_id", serde_json::json!("other-client")),
        ("aud", serde_json::json!(["https://mcp.example/", "https://other.example/"])),
        ("aud", serde_json::json!(["https://mcp.example/", 17])),
        ("scope", serde_json::json!("ops:write ops:admin")),
        ("scope", serde_json::json!(["ops:write", 17])),
        ("project_id", serde_json::json!(18)),
        ("exp", serde_json::json!(NOW)),
    ] {
        let mut invalid_claims = claims();
        invalid_claims[claim] = value;
        assert!(
            validate_token_response(
                response(invalid_claims),
                &policy,
                Uuid::parse_str(PRINCIPAL).unwrap(),
                PROJECT_ID,
                NOW,
            )
            .is_err(),
            "claim {claim} must remain bound to policy"
        );
    }

    let mut bad_scope = response(claims());
    bad_scope.scope = Some("ops:write ops:admin".to_string());
    assert!(
        validate_token_response(
            bad_scope,
            &policy,
            Uuid::parse_str(PRINCIPAL).unwrap(),
            PROJECT_ID,
            NOW,
        )
        .is_err()
    );
}

#[test]
fn token_type_lifetime_header_and_signed_jwt_shape_fail_closed() {
    let policy = policy();
    let principal = Uuid::parse_str(PRINCIPAL).unwrap();
    let mut invalid = response(claims());
    invalid.token_type = "DPoP".to_string();
    assert!(validate_token_response(invalid, &policy, principal, PROJECT_ID, NOW).is_err());

    let mut invalid = response(claims());
    invalid.expires_in = Some(0);
    assert!(validate_token_response(invalid, &policy, principal, PROJECT_ID, NOW).is_err());

    let mut invalid = response(claims());
    invalid.access_token = crate::provider_auth::SecretString("opaque-token".to_string());
    assert!(validate_token_response(invalid, &policy, principal, PROJECT_ID, NOW).is_err());

    let mut invalid = response(claims());
    let bad_header = serde_json::json!({"alg": "none", "kid": "issuer-key-1"});
    let payload = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims()).unwrap());
    invalid.access_token = crate::provider_auth::SecretString(format!(
        "{}.{}.{}",
        URL_SAFE_NO_PAD.encode(serde_json::to_vec(&bad_header).unwrap()),
        payload,
        URL_SAFE_NO_PAD.encode(b"synthetic-signature")
    ));
    assert!(validate_token_response(invalid, &policy, principal, PROJECT_ID, NOW).is_err());
}
