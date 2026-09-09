use super::CredentialBrokerState;
use super::CredentialRecord;
use super::env_entry;
use rama_http::HeaderMap;
use std::collections::HashMap;

pub(super) fn prioritized_credentials<'a>(
    state: &'a CredentialBrokerState,
    env: &HashMap<String, String>,
) -> Vec<&'a CredentialRecord> {
    let mut credentials = state.credentials.iter().collect::<Vec<_>>();
    credentials.sort_unstable_by_key(|credential| {
        let active = env_entry(env, &credential.env_var)
            .is_some_and(|(_, value)| value == credential.dummy_value);
        let has_matching_host_binding = credential.provider.sources().iter().any(|source| {
            !source.binding_env_vars.is_empty()
                && (source.host_binding)(env, state.openai_api_host.as_deref())
                    .is_some_and(|binding| binding == credential.host_binding)
        });
        (
            std::cmp::Reverse(credential.real_value.len()),
            std::cmp::Reverse(active),
            std::cmp::Reverse(has_matching_host_binding),
        )
    });
    credentials
}

pub(super) fn select_credential<'a>(
    headers: &HeaderMap,
    host: &str,
    credentials: &'a [CredentialRecord],
) -> Option<(&'a CredentialRecord, rama_http::HeaderValue)> {
    let mut translated_matches = credentials
        .iter()
        .filter(|credential| credential.matches_host(host))
        .filter_map(|credential| {
            credentials
                .iter()
                .filter(|candidate| {
                    std::ptr::eq(candidate.provider, credential.provider)
                        && candidate.real_value == credential.real_value
                        && candidate.matches_host(host)
                })
                .find_map(|candidate| {
                    credential.provider.translate_request_header(
                        headers,
                        &candidate.dummy_value,
                        &credential.real_value,
                    )
                })
                .map(|header_value| (credential, header_value))
        });
    let matched = translated_matches.next()?;
    translated_matches
        .all(|(credential, _)| {
            std::ptr::eq(credential.provider, matched.0.provider)
                && credential.real_value == matched.0.real_value
        })
        .then_some(matched)
}
