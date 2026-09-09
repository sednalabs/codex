use super::CredentialBrokerState;
use super::MIN_EMBEDDED_CREDENTIAL_LENGTH;
use super::brokered_credential_dummy_env_keys;
use super::env_key_matches;
use super::env_value;
use super::prioritized_credentials;
use super::providers;
use std::collections::HashMap;
use std::path::Path;
use url::Position;
use url::Url;

pub(super) fn virtualize_text(
    state: &CredentialBrokerState,
    text: &mut String,
    env: &HashMap<String, String>,
) -> bool {
    if !state.enabled {
        return true;
    }

    let allowed_keys = brokered_credential_dummy_env_keys(env);
    let credentials = prioritized_credentials(state, env);
    let mut allowed = true;
    for credential in &credentials {
        let contains_real = text.as_str() == credential.real_value
            || credential.real_value.len() >= MIN_EMBEDDED_CREDENTIAL_LENGTH
                && text.contains(&credential.real_value);
        let contains_dummy = text.as_str() == credential.dummy_value
            || credential.dummy_value.len() >= MIN_EMBEDDED_CREDENTIAL_LENGTH
                && text.contains(&credential.dummy_value);
        if !contains_real && !contains_dummy {
            continue;
        }

        let replacement = credentials.iter().copied().find(|candidate| {
            std::ptr::eq(candidate.provider, credential.provider)
                && candidate.real_value == credential.real_value
                && (allowed_keys.iter().any(|key| {
                    env_key_matches(key, &candidate.env_var)
                        && env_value(env, key) == Some(candidate.dummy_value.as_str())
                }) || state.credential_aliases.iter().any(|alias| {
                    env_value(env, &alias.env_var) == Some(alias.dummy_value.as_str())
                        && (alias.dummy_value == candidate.dummy_value
                            || candidate.dummy_value.len() >= MIN_EMBEDDED_CREDENTIAL_LENGTH
                                && alias.dummy_value.contains(&candidate.dummy_value))
                        && !state.credentials.iter().any(|other| {
                            other.dummy_value != candidate.dummy_value
                                && (alias.dummy_value == other.dummy_value
                                    || other.dummy_value.len() >= MIN_EMBEDDED_CREDENTIAL_LENGTH
                                        && alias.dummy_value.contains(&other.dummy_value))
                        })
                }))
        });
        if replacement.is_none() {
            allowed = false;
        }
        let replacement = replacement.map_or("", |candidate| candidate.dummy_value.as_str());
        if contains_real {
            *text = text.replace(&credential.real_value, replacement);
        }
        if contains_dummy && credential.dummy_value != replacement {
            *text = text.replace(&credential.dummy_value, replacement);
        }
    }

    // Startup can copy a supported credential before unsetting its source variable.
    for provider in providers::credential_providers() {
        for prefix in provider.credential_prefixes {
            let mut offset = 0;
            while let Some(position) = text[offset..].find(prefix) {
                let start = offset + position;
                if let Some(length) = state
                    .credentials
                    .iter()
                    .flat_map(|credential| [&credential.real_value, &credential.dummy_value])
                    .filter(|credential| credential.len() >= MIN_EMBEDDED_CREDENTIAL_LENGTH)
                    .filter(|credential| text[start..].starts_with(credential.as_str()))
                    .map(String::len)
                    .max()
                {
                    offset = start + length;
                    continue;
                }
                let candidate = builtin_credential_candidate(provider, text, start);
                let length = candidate.len();
                if provider
                    .ignored_credential_prefixes
                    .iter()
                    .any(|prefix| candidate.starts_with(prefix))
                    && provider
                        .credential_watermark
                        .is_none_or(|watermark| !candidate.contains(watermark))
                {
                    let known_length = env
                        .values()
                        .filter(|known| {
                            known.len() >= provider.minimum_credential_len
                                && candidate
                                    .strip_prefix(known.as_str())
                                    .is_some_and(|suffix| {
                                        provider.credential_prefixes.iter().any(|prefix| {
                                            suffix.match_indices(prefix).any(|(offset, _)| {
                                                suffix.len() - offset
                                                    >= provider.minimum_credential_len
                                            })
                                        })
                                    })
                        })
                        .map(String::len)
                        .min();
                    let embedded_supported = provider
                        .credential_prefixes
                        .iter()
                        .flat_map(|prefix| candidate.match_indices(prefix))
                        .filter_map(|(offset, _)| {
                            (offset > 0
                                && candidate.len() - offset >= provider.minimum_credential_len
                                && !provider
                                    .ignored_credential_prefixes
                                    .iter()
                                    .any(|ignored| candidate[offset..].starts_with(ignored)))
                            .then_some(offset)
                        })
                        .min();
                    offset = start
                        + known_length
                            .into_iter()
                            .chain(embedded_supported)
                            .min()
                            .unwrap_or(length);
                    continue;
                }
                let end = start + length;
                let credential = &text[start..end];
                let ignored_credential_match =
                    ignored_credential_match(provider, text, start, credential);
                if length >= provider.minimum_credential_len && !ignored_credential_match {
                    text.replace_range(start..end, "");
                    allowed = false;
                } else {
                    offset = start
                        + if ignored_credential_match {
                            prefix.len()
                        } else {
                            length
                        };
                }
            }
        }
    }

    allowed
}

pub(super) fn is_operational_path_match(text: &str, start: usize, end: usize) -> bool {
    let is_value_boundary = |character: char| {
        character.is_ascii_whitespace() || matches!(character, '"' | '\'' | '=' | '`')
    };
    let value_start = text[..start]
        .rfind(is_value_boundary)
        .map_or(0, |index| index + 1);
    let value_end = text[end..]
        .find(is_value_boundary)
        .map_or(text.len(), |index| end + index);
    let value = &text[value_start..value_end];
    if let Ok(url) = Url::parse(value)
        && url.has_host()
    {
        let relative_start = start - value_start;
        let relative_end = end - value_start;
        return relative_start >= url[..Position::BeforeHost].len()
            && relative_end <= url[..Position::AfterPort].len();
    }

    let relative_start = start - value_start;
    let relative_end = end - value_start;
    if !value[..relative_start].contains(['/', '\\'])
        && !value[relative_end..].contains(['/', '\\'])
    {
        return false;
    }

    let path = Path::new(value);
    path.has_root() || path.components().count() > 1 || value.contains('\\')
}

fn ignored_credential_match(
    provider: &providers::CredentialProvider,
    text: &str,
    start: usize,
    credential: &str,
) -> bool {
    provider
        .ignored_credential_prefixes
        .iter()
        .any(|prefix| credential.starts_with(prefix))
        && provider
            .credential_watermark
            .is_none_or(|watermark| !credential.contains(watermark))
        || credential.starts_with("sk-")
            && provider
                .credential_watermark
                .is_none_or(|watermark| !credential.contains(watermark))
            && credential[3..].split(['-', '_']).any(|segment| {
                segment.len() == 64
                    && segment.bytes().all(|byte| byte.is_ascii_hexdigit())
                    && credential
                        .find(segment)
                        .is_some_and(|offset| offset < provider.minimum_credential_len)
            })
            && text[..start]
                .rsplit(|character: char| {
                    character.is_ascii() && !character.is_ascii_alphanumeric()
                })
                .next()
                .is_some_and(|word| {
                    ((1..=3).contains(&word.len())
                        || word
                            .get(word.len().saturating_sub(2)..)
                            .is_some_and(|suffix| {
                                suffix.eq_ignore_ascii_case("di")
                                    || suffix.eq_ignore_ascii_case("ta")
                                        && !word.eq_ignore_ascii_case("data")
                            })
                        || credential.strip_prefix("sk-").is_some_and(|hash| {
                            hash.len() == 64 && hash.bytes().all(|byte| byte.is_ascii_hexdigit())
                        }))
                        && word.bytes().all(|byte| byte.is_ascii_alphabetic())
                        && text[..start]
                            .rsplit_once(['/', '\\'])
                            .is_some_and(|(_, component)| {
                                component.chars().all(|character| {
                                    !character.is_ascii()
                                        || character.is_ascii_alphanumeric()
                                        || matches!(character, '_' | '-')
                                })
                            })
                })
}

pub(super) fn recognized_credential_match<'a>(
    provider: &providers::CredentialProvider,
    value: &'a str,
    virtualized: &str,
    start: usize,
) -> Option<&'a str> {
    let credential = builtin_credential_candidate(provider, value, start);
    let length = credential.len();
    let enclosing_start = value.as_bytes()[..start]
        .iter()
        .rposition(|byte| !byte.is_ascii_alphanumeric() && !matches!(byte, b'_' | b'-'))
        .map_or(0, |offset| offset + 1);
    let enclosing = &value[enclosing_start..start + length];
    (length >= provider.minimum_credential_len || !virtualized.contains(credential))
        .then_some(credential)
        .filter(|credential| !ignored_credential_match(provider, value, start, credential))
        .filter(|_| {
            !provider.ignored_credential_prefixes.iter().any(|prefix| {
                enclosing.match_indices(prefix).any(|(offset, _)| {
                    let ignored = &enclosing[offset..];
                    offset <= start - enclosing_start
                        && ignored_credential_match(
                            provider,
                            value,
                            enclosing_start + offset,
                            ignored,
                        )
                        && virtualized.contains(ignored)
                })
            })
        })
}

pub(super) fn builtin_credential_candidate<'a>(
    provider: &providers::CredentialProvider,
    value: &'a str,
    start: usize,
) -> &'a str {
    let mut length = value.as_bytes()[start..]
        .iter()
        .take_while(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
        .count();
    let candidate = &value[start..start + length];
    let current_prefix_len = provider
        .credential_prefixes
        .iter()
        .filter(|prefix| candidate.starts_with(**prefix))
        .map(|prefix| prefix.len())
        .max()
        .unwrap_or(0);
    if let Some(separator) = providers::credential_providers()
        .flat_map(|candidate_provider| {
            candidate_provider
                .credential_prefixes
                .iter()
                .map(move |prefix| (prefix, candidate_provider.minimum_credential_len))
        })
        .filter_map(|(candidate_prefix, minimum_length)| {
            candidate[current_prefix_len..]
                .match_indices(*candidate_prefix)
                .find_map(|(offset, _)| {
                    let offset = current_prefix_len + offset;
                    (matches!(candidate.as_bytes()[offset - 1], b'_' | b'-')
                        && offset > provider.minimum_credential_len
                        && candidate.len() - offset >= minimum_length)
                        .then_some(offset - 1)
                })
        })
        .min()
    {
        length = separator;
    }
    &value[start..start + length]
}
