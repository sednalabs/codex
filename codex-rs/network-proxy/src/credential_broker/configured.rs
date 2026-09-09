use super::CredentialAuthMethod;
use super::CredentialProviderConfig;
use super::destination::CredentialDestination;
use super::env_value;
use super::providers;
use super::providers::CredentialHostBinding;
use anyhow::Context;
use anyhow::Result;
use anyhow::ensure;
use base64::Engine as _;
use rama_http::HeaderMap;
use rama_http::HeaderName;
use rama_http::HeaderValue;
use rama_http::header::AUTHORIZATION;
use rand::Rng as _;
use regex::RegexBuilder;
use regex_syntax::hir::Class;
use regex_syntax::hir::ClassBytes;
use regex_syntax::hir::ClassBytesRange;
use regex_syntax::hir::ClassUnicode;
use regex_syntax::hir::ClassUnicodeRange;
use regex_syntax::hir::Hir;
use regex_syntax::hir::HirKind;
use regex_syntax::hir::Look;
use std::collections::HashMap;

const MAX_REGEX_BYTES: usize = 2048;
const MAX_REGEX_REPEAT: u32 = 64;
const MAX_DUMMY_ATTEMPTS: usize = 64;
const MAX_DUMMY_VALUE_BYTES: usize = 2048;

pub(super) struct ConfiguredCredentialProvider {
    pub(super) id: String,
    pub(super) config: CredentialProviderConfig,
    patterns: Vec<ConfiguredCredentialPattern>,
    header: Option<HeaderName>,
}

struct ConfiguredCredentialPattern {
    full_matcher: regex::Regex,
    ascii_generator: Option<rand_regex::Regex>,
    generator: rand_regex::Regex,
}

impl ConfiguredCredentialPattern {
    fn candidates(&self) -> impl Iterator<Item = String> + '_ {
        self.ascii_generator
            .iter()
            .chain(std::iter::once(&self.generator))
            .flat_map(|generator| {
                (0..MAX_DUMMY_ATTEMPTS).map(move |_| rand::rng().sample::<String, _>(generator))
            })
            .filter(move |candidate| {
                candidate.len() <= MAX_DUMMY_VALUE_BYTES
                    && !candidate.contains('\0')
                    && self.full_matcher.is_match(candidate)
            })
    }
}

#[derive(Clone, Copy)]
enum DummyAlphabet {
    Ascii,
    Unicode,
}

impl ConfiguredCredentialProvider {
    pub(super) fn compile(id: &str, config: &CredentialProviderConfig) -> Result<Self> {
        ensure!(!id.is_empty(), "credential provider name must not be empty");
        ensure!(
            !config.env.is_empty(),
            "credential provider `{id}` has no environment keys"
        );
        ensure!(
            !config.patterns.is_empty(),
            "credential provider `{id}` has no credential patterns"
        );
        ensure!(
            !config.url_prefixes.is_empty() || config.url_prefix_from_env.is_some(),
            "credential provider `{id}` has no destination URL prefixes"
        );
        ensure!(
            config.env.iter().all(|key| valid_environment_key(key)),
            "credential provider `{id}` has an invalid environment key"
        );
        ensure!(
            !config
                .env
                .iter()
                .any(|key| super::is_credential_broker_provider_env_key(key)),
            "credential provider `{id}` overlaps a built-in credential source"
        );
        if let Some(key) = config.url_prefix_from_env.as_deref() {
            ensure!(
                valid_environment_key(key)
                    && !config.env.iter().any(|source| {
                        source.as_str() == key || cfg!(windows) && source.eq_ignore_ascii_case(key)
                    }),
                "credential provider `{id}` has an invalid host environment key"
            );
        }
        for destination in &config.url_prefixes {
            CredentialDestination::parse(destination)
                .with_context(|| format!("invalid destination for credential provider `{id}`"))?;
        }
        let methods = auth_methods(config);
        let header = config
            .header
            .as_deref()
            .map(|header| HeaderName::from_bytes(header.as_bytes()))
            .transpose()
            .with_context(|| format!("invalid header for credential provider `{id}`"))?;
        ensure!(
            methods.contains(&CredentialAuthMethod::Header) == header.is_some(),
            "credential provider `{id}` requires a header name exactly when using header authentication"
        );
        ensure!(
            config.prefix.is_none() || header.is_some(),
            "credential provider `{id}` requires header authentication to use a header prefix"
        );
        let patterns = config
            .patterns
            .iter()
            .map(|pattern| {
                ensure!(
                    pattern.len() <= MAX_REGEX_BYTES,
                    "credential pattern exceeds {MAX_REGEX_BYTES} bytes"
                );
                let parsed = regex_syntax::parse(pattern)?;
                let ascii_generator = rand_regex::Regex::with_hir(
                    dummy_credential_pattern(parsed.clone(), DummyAlphabet::Ascii),
                    MAX_REGEX_REPEAT,
                )
                .ok();
                let generator = rand_regex::Regex::with_hir(
                    dummy_credential_pattern(parsed.clone(), DummyAlphabet::Unicode),
                    MAX_REGEX_REPEAT,
                )?;
                let full_pattern =
                    Hir::concat(vec![Hir::look(Look::Start), parsed, Hir::look(Look::End)])
                        .to_string();
                let full_matcher = RegexBuilder::new(&full_pattern)
                    .size_limit(1 << 20)
                    .dfa_size_limit(1 << 20)
                    .build()?;
                ensure!(
                    !full_matcher.is_match(""),
                    "credential pattern must not match an empty value"
                );
                ensure!(
                    generator.is_utf8() && generator.capacity() <= MAX_REGEX_BYTES,
                    "credential pattern must generate bounded UTF-8 values"
                );
                let compiled = ConfiguredCredentialPattern {
                    full_matcher,
                    ascii_generator,
                    generator,
                };
                ensure!(
                    compiled.candidates().next().is_some(),
                    "credential pattern could not independently generate a matching dummy"
                );
                Ok(compiled)
            })
            .collect::<Result<Vec<_>>>()
            .with_context(|| format!("invalid credential pattern for provider `{id}`"))?;

        Ok(Self {
            id: id.to_string(),
            config: config.clone(),
            patterns,
            header,
        })
    }

    pub(super) fn matches_value(&self, value: &str) -> bool {
        self.patterns
            .iter()
            .any(|pattern| pattern.full_matcher.is_match(value))
    }

    pub(super) fn host_binding(
        &self,
        env: &HashMap<String, String>,
    ) -> Option<CredentialHostBinding> {
        let mut destinations = self
            .config
            .url_prefixes
            .iter()
            .filter_map(|destination| CredentialDestination::parse(destination).ok())
            .collect::<Vec<_>>();
        if let Some(destination) = self.dynamic_destination(env)
            && !destinations.contains(&destination)
        {
            destinations.push(destination);
        }
        (!destinations.is_empty()).then_some(CredentialHostBinding::ConfiguredHosts(destinations))
    }

    pub(super) fn dynamic_destination(
        &self,
        env: &HashMap<String, String>,
    ) -> Option<CredentialDestination> {
        let key = self.config.url_prefix_from_env.as_deref()?;
        CredentialDestination::parse(env_value(env, key)?)
            .ok()
            .filter(|destination| !destination.is_wildcard())
    }

    pub(super) fn dummy_value(&self, real_value: &str) -> Option<String> {
        let pattern = self
            .patterns
            .iter()
            .find(|pattern| pattern.full_matcher.is_match(real_value))?;
        pattern.candidates().find(|candidate| {
            candidate != real_value && self.preserves_usable_auth_methods(real_value, candidate)
        })
    }

    pub(super) fn request_header_value(&self, value: &str) -> Option<HeaderValue> {
        auth_methods(&self.config)
            .iter()
            .find_map(|method| self.request_header_value_for_method(*method, value))
    }

    fn preserves_usable_auth_methods(&self, real_value: &str, dummy_value: &str) -> bool {
        auth_methods(&self.config).iter().all(|method| {
            // Preserve whether clients serialize a whole Basic pair or one component.
            if *method == CredentialAuthMethod::Basic
                && real_value.contains(':') != dummy_value.contains(':')
            {
                return false;
            }
            let Some(real_header) = self.request_header_value_for_method(*method, real_value)
            else {
                return true;
            };
            let Some(dummy_header) = self.request_header_value_for_method(*method, dummy_value)
            else {
                return false;
            };
            if dummy_header.as_bytes().trim_ascii() != dummy_header.as_bytes() {
                return false;
            }
            let header = match method {
                CredentialAuthMethod::Bearer
                | CredentialAuthMethod::Token
                | CredentialAuthMethod::Basic => AUTHORIZATION,
                CredentialAuthMethod::Header => {
                    let Some(header) = self.header.as_ref() else {
                        return false;
                    };
                    header.clone()
                }
            };
            self.translate_request_headers(
                &HeaderMap::from_iter([(header.clone(), dummy_header)]),
                dummy_value,
                real_value,
            )
            .contains(&(header, real_header))
        })
    }

    fn request_header_value_for_method(
        &self,
        method: CredentialAuthMethod,
        value: &str,
    ) -> Option<HeaderValue> {
        let value = match method {
            CredentialAuthMethod::Bearer => format!("Bearer {value}"),
            CredentialAuthMethod::Token => format!("token {value}"),
            CredentialAuthMethod::Basic => format!(
                "Basic {}",
                base64::engine::general_purpose::STANDARD.encode(value)
            ),
            CredentialAuthMethod::Header => {
                format!(
                    "{}{value}",
                    self.config.prefix.as_deref().unwrap_or_default()
                )
            }
        };
        HeaderValue::from_str(&value)
            .ok()
            .filter(|header| header.to_str().is_ok())
    }

    pub(super) fn translate_request_headers(
        &self,
        headers: &HeaderMap,
        expected_value: &str,
        replacement_value: &str,
    ) -> Vec<(HeaderName, HeaderValue)> {
        let mut translated = Vec::new();
        for method in auth_methods(&self.config) {
            match method {
                CredentialAuthMethod::Bearer
                | CredentialAuthMethod::Token
                | CredentialAuthMethod::Basic => {
                    let Some(header) = headers
                        .get(AUTHORIZATION)
                        .and_then(|value| value.to_str().ok())
                    else {
                        continue;
                    };
                    let Some((scheme, _)) = header.split_once(' ') else {
                        continue;
                    };
                    let expected_scheme = match method {
                        CredentialAuthMethod::Bearer => "bearer",
                        CredentialAuthMethod::Token => "token",
                        CredentialAuthMethod::Basic => "basic",
                        CredentialAuthMethod::Header => continue,
                    };
                    if !scheme.eq_ignore_ascii_case(expected_scheme) {
                        continue;
                    }
                    if let Some(value) = providers::translate_standard_request_header(
                        headers,
                        expected_value,
                        replacement_value,
                    ) && !translated.iter().any(|(header, _)| header == AUTHORIZATION)
                    {
                        translated.push((AUTHORIZATION, value));
                    }
                }
                CredentialAuthMethod::Header => {
                    let Some(header) = self.header.as_ref() else {
                        continue;
                    };
                    let Some(value) = headers.get(header).and_then(|value| value.to_str().ok())
                    else {
                        continue;
                    };
                    let prefix = self.config.prefix.as_deref().unwrap_or_default();
                    if value == format!("{prefix}{expected_value}")
                        && !translated.iter().any(|(name, _)| name == header)
                        && let Ok(value) =
                            HeaderValue::from_str(&format!("{prefix}{replacement_value}"))
                    {
                        translated.push((header.clone(), value));
                    }
                }
            }
        }
        translated
    }
}

fn dummy_credential_pattern(hir: Hir, alphabet: DummyAlphabet) -> Hir {
    match hir.into_kind() {
        HirKind::Empty => Hir::empty(),
        HirKind::Literal(literal) => {
            if matches!(alphabet, DummyAlphabet::Ascii) && !literal.0.is_ascii() {
                Hir::fail()
            } else {
                Hir::literal(literal.0)
            }
        }
        HirKind::Class(mut class) => {
            if matches!(alphabet, DummyAlphabet::Ascii) {
                match &mut class {
                    Class::Unicode(class) => {
                        class.intersect(&ClassUnicode::new([ClassUnicodeRange::new(' ', '~')]))
                    }
                    Class::Bytes(class) => {
                        class.intersect(&ClassBytes::new([ClassBytesRange::new(b' ', b'~')]))
                    }
                }
            }
            Hir::class(class)
        }
        HirKind::Look(_) => Hir::empty(),
        HirKind::Repetition(mut repetition) => {
            repetition.sub = Box::new(dummy_credential_pattern(*repetition.sub, alphabet));
            Hir::repetition(repetition)
        }
        HirKind::Capture(mut capture) => {
            capture.sub = Box::new(dummy_credential_pattern(*capture.sub, alphabet));
            Hir::capture(capture)
        }
        HirKind::Concat(expressions) => Hir::concat(
            expressions
                .into_iter()
                .map(|expression| dummy_credential_pattern(expression, alphabet))
                .collect(),
        ),
        HirKind::Alternation(expressions) => {
            let mut expressions = expressions
                .into_iter()
                .map(|expression| dummy_credential_pattern(expression, alphabet))
                .filter(|expression| expression.properties().minimum_len().is_some())
                .collect::<Vec<_>>();
            expressions.sort_by_key(|expression| {
                std::cmp::Reverse(expression.properties().maximum_len().unwrap_or(usize::MAX))
            });
            Hir::alternation(expressions)
        }
    }
}

fn auth_methods(config: &CredentialProviderConfig) -> &[CredentialAuthMethod] {
    if config.auth.is_empty() {
        &[CredentialAuthMethod::Bearer]
    } else {
        &config.auth
    }
}

pub(super) fn valid_environment_key(key: &str) -> bool {
    let mut chars = key.chars();
    chars
        .next()
        .is_some_and(|character| character == '_' || character.is_ascii_alphabetic())
        && chars.all(|character| character == '_' || character.is_ascii_alphanumeric())
}
