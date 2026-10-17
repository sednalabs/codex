use clap::Parser;
use codex_config::CloudConfigBundleLoader;
use codex_config::ConfigLayerMetadata;
use codex_config::ConfigLayerSource;
use codex_config::ConfigLoadOptions;
use codex_config::LoaderOverrides;
use codex_config::NoopThreadConfigLoader;
use codex_config::RequirementSource;
use codex_config::loader::load_config_layers_state;
use codex_exec_server::LOCAL_FS;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_cli::CliConfigOverrides;
use serde_json::Value as JsonValue;
use serde_json::json;
use std::collections::HashMap;
use std::io::Read;
use std::io::Write;
use std::path::PathBuf;
use toml::Value as TomlValue;

const MAX_CONTENT_BYTES: usize = 32_768;
const MAX_INPUT_BYTES: usize = MAX_CONTENT_BYTES + 1;
const MAX_REPORT_BYTES: usize = 131_072;

#[derive(Debug, Parser)]
pub(crate) struct DebugInstructionsCommand {
    /// Include projected inline configuration text after heuristic redaction.
    #[arg(long)]
    show_content: bool,

    /// Read caller-selected UTF-8 text from stdin (origin is unverified).
    /// Its content is printed only when --show-content is also set. Never
    /// supply credentials or hidden/live instructions.
    #[arg(long)]
    client_instructions_stdin: bool,
}

pub(crate) async fn run(
    command: DebugInstructionsCommand,
    overrides: CliConfigOverrides,
    cwd: Option<PathBuf>,
    loader_overrides: LoaderOverrides,
) -> anyhow::Result<()> {
    let parsed_overrides = overrides
        .parse_overrides()
        .map_err(|_| anyhow::anyhow!("unable to load instruction configuration"))?;
    let codex_home = codex_core::config::find_codex_home()
        .map_err(|_| anyhow::anyhow!("unable to load instruction configuration"))?;
    let cwd = match cwd {
        Some(path) => AbsolutePathBuf::relative_to_current_dir(path),
        None => AbsolutePathBuf::current_dir(),
    }
    .map_err(|_| anyhow::anyhow!("unable to load instruction configuration"))?;
    let options = ConfigLoadOptions {
        loader_overrides,
        strict_config: false,
        cloud_config_bundle: CloudConfigBundleLoader::default(),
    };
    let layers = load_config_layers_state(
        LOCAL_FS.as_ref(),
        &codex_home,
        Some(cwd),
        &parsed_overrides,
        options,
        &NoopThreadConfigLoader,
    )
    .await
    .map_err(|_| anyhow::anyhow!("unable to load instruction configuration"))?;

    let stdin_text =
        read_optional_input(command.client_instructions_stdin, std::io::stdin().lock())?;

    let report = build_report(&layers, command.show_content, stdin_text.as_deref())?;
    write_bounded_json(&report, &mut std::io::stdout().lock())
}

fn build_report(
    layers: &codex_config::ConfigLayerStack,
    show_content: bool,
    stdin_text: Option<&str>,
) -> anyhow::Result<JsonValue> {
    let effective = layers.effective_config();
    let origins = layers.origins_with_path_filter(|path| is_instruction_path(path));
    let fields = project_inline_config(&effective, &origins, layers, show_content)?;

    let requirements = layers.requirements();
    let managed_present = layers
        .requirements_toml()
        .additional_developer_instructions
        .is_some();
    let managed_source = requirements
        .additional_developer_instructions
        .as_ref()
        .map(|sourced| requirement_source_metadata(&sourced.source));
    let managed = json!({
        "present": managed_present,
        "source": managed_source,
        "content": "excluded; local-managed instruction content is never displayed",
    });

    let client_text = stdin_text.map(|text| {
        let (redacted, count) = redact(text);
        (redacted, count)
    });
    let caller_supplied = if let Some((redacted, count)) = client_text {
        let mut section = json!({"present": true, "origin": "caller-supplied; unverified", "redacted_lines": count});
        if show_content {
            section["content"] = json!(redacted);
        }
        section
    } else {
        json!({"present": false, "origin": "not read"})
    };

    // These boundaries are intentionally explicit: this is a local config
    // projection, not a complete or authoritative session-prompt inspection.
    Ok(json!({
        "scope": "selected local merged configuration; not a complete effective session prompt",
        "inline_configuration": fields,
        "local_managed_additional_developer_instructions": managed,
        "caller_supplied_text": caller_supplied,
        "unknown_or_not_inspected": [
            "cloud-managed additions (cloud callback is no-op)",
            "stored thread history, current model template, runtime conversation and provider-hidden instructions",
            "AGENTS.md and user/provider instruction discovery",
            "model_instructions_file target contents and final baseline fallback",
        ],
        "semantics": [
            "configuration instructions may override stored history, which may override the model base",
            "compiled cadence transforms are conditional and were not evaluated",
        ],
        "publication_warning": "heuristic redaction only; unfamiliar, split-line, multiline, encoded, or unrecognized secrets may remain; absence of credentials is not guaranteed",
    }))
}

fn serialize_bounded_json(report: &JsonValue) -> anyhow::Result<Vec<u8>> {
    let mut output = serde_json::to_vec(report)
        .map_err(|_| anyhow::anyhow!("unable to serialize instruction report"))?;
    output.push(b'\n');
    if output.len() > MAX_REPORT_BYTES {
        anyhow::bail!("instruction report exceeds the output size limit");
    }
    Ok(output)
}

fn write_bounded_json<W: Write>(report: &JsonValue, writer: &mut W) -> anyhow::Result<()> {
    let output = serialize_bounded_json(report)?;
    writer
        .write_all(&output)
        .map_err(|_| anyhow::anyhow!("unable to write instruction report"))?;
    Ok(())
}

const INSTRUCTION_PATHS: &[&[&str]] = &[
    &["instructions"],
    &["developer_instructions"],
    &["model_instructions_file"],
    &[
        "features",
        "multi_agent_v2",
        "subagent_developer_instructions",
    ],
    &["features", "multi_agent_v2", "multi_agent_mode_hint_text"],
    &["features", "multi_agent_v2", "usage_hint_text"],
    &["features", "multi_agent_v2", "root_agent_usage_hint_text"],
    &["features", "multi_agent_v2", "subagent_usage_hint_text"],
];
const MODEL_INSTRUCTIONS_PATH: &[&str] = &["model_instructions_file"];

fn is_instruction_path(path: &[String]) -> bool {
    INSTRUCTION_PATHS.iter().any(|expected| {
        expected.len() == path.len()
            && expected
                .iter()
                .zip(path)
                .all(|(left, right)| *left == right)
    })
}

fn get_path<'a>(value: &'a TomlValue, path: &[&str]) -> Option<&'a TomlValue> {
    path.iter().try_fold(value, |value, key| value.get(*key))
}

fn project_inline_config(
    effective: &TomlValue,
    origins: &HashMap<String, ConfigLayerMetadata>,
    stack: &codex_config::ConfigLayerStack,
    show_content: bool,
) -> anyhow::Result<Vec<JsonValue>> {
    let mut fields = Vec::new();
    let mut content_bytes = 0usize;
    for path in INSTRUCTION_PATHS {
        let value = get_path(effective, path);
        let origin = origins.get(&path.join("."));
        let mut field = json!({
            "key": path.join("."),
            "present": value.is_some(),
            "origin": origin.map(|metadata| source_metadata(&metadata.name, stack)),
        });
        if show_content && *path != MODEL_INSTRUCTIONS_PATH {
            if let Some(value) = value.and_then(TomlValue::as_str) {
                content_bytes = content_bytes.saturating_add(value.len());
                if content_bytes > MAX_CONTENT_BYTES {
                    anyhow::bail!("instruction content exceeds the size limit");
                }
                let (redacted, count) = redact(value);
                field["content"] = json!(redacted);
                field["redacted_lines"] = json!(count);
            }
        }
        if *path == MODEL_INSTRUCTIONS_PATH {
            if let Some(path) = value.and_then(TomlValue::as_str) {
                field["configured_path"] = json!(path);
                field["content_status"] = json!("not_opened; configured path only");
            }
        }
        fields.push(field);
    }
    Ok(fields)
}

fn source_metadata(
    source: &ConfigLayerSource,
    stack: &codex_config::ConfigLayerStack,
) -> JsonValue {
    let mut metadata = match source {
        ConfigLayerSource::PackagedDefaults { file } => file_source("packaged-defaults", file),
        ConfigLayerSource::System { file } => file_source("system", file),
        ConfigLayerSource::User { file, profile } => {
            let mut value = file_source("user", file);
            if let Some(profile) = profile {
                value["profile"] = json!(profile);
            }
            value
        }
        ConfigLayerSource::Project { dot_codex_folder } => {
            json!({"kind": "project", "file": dot_codex_folder.join("config.toml")})
        }
        ConfigLayerSource::SessionFlags => json!({"kind": "session-flags"}),
        ConfigLayerSource::EnterpriseManaged { .. } => json!({"kind": "enterprise-managed"}),
        ConfigLayerSource::Mdm { .. } => json!({"kind": "mdm"}),
        ConfigLayerSource::LegacyManagedConfigTomlFromFile { file } => {
            file_source("legacy-managed-file", file)
        }
        ConfigLayerSource::LegacyManagedConfigTomlFromMdm => {
            json!({"kind": "legacy-managed-mdm"})
        }
    };
    let disabled = stack
        .layers_high_to_low()
        .find(|layer| &layer.name == source)
        .and_then(|layer| layer.disabled_reason.as_ref())
        .is_some();
    metadata["disabled_or_untrusted"] = json!(disabled);
    metadata["precedence"] = json!(source.precedence());
    metadata
}

fn read_optional_input<R: Read>(enabled: bool, reader: R) -> anyhow::Result<Option<String>> {
    if !enabled {
        return Ok(None);
    }
    let mut bytes = Vec::new();
    reader
        .take(MAX_INPUT_BYTES as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| anyhow::anyhow!("unable to read caller-supplied instruction text"))?;
    if bytes.len() > MAX_CONTENT_BYTES {
        anyhow::bail!("caller-supplied instruction text exceeds the size limit");
    }
    String::from_utf8(bytes)
        .map(Some)
        .map_err(|_| anyhow::anyhow!("caller-supplied instruction text must be UTF-8"))
}

fn file_source(kind: &str, file: &AbsolutePathBuf) -> JsonValue {
    json!({"kind": kind, "file": file.as_path()})
}

fn requirement_source_metadata(source: &RequirementSource) -> JsonValue {
    match source {
        RequirementSource::SystemRequirementsToml { file } => {
            file_source("system-requirements", file)
        }
        RequirementSource::LegacyManagedConfigTomlFromFile { file } => {
            file_source("legacy-managed-file", file)
        }
        RequirementSource::MdmManagedPreferences { .. } => json!({"kind": "mdm"}),
        RequirementSource::EnterpriseManaged { .. } => json!({"kind": "enterprise-managed"}),
        RequirementSource::LegacyManagedConfigTomlFromMdm => {
            json!({"kind": "legacy-managed-mdm"})
        }
        RequirementSource::Composite { .. } => json!({"kind": "composite"}),
        RequirementSource::Unknown => json!({"kind": "unknown"}),
    }
}

fn redact(input: &str) -> (String, usize) {
    let suspicious = [
        "api_key",
        "api-key",
        "apikey",
        "access_token",
        "refresh_token",
        "password",
        "authorization",
        "bearer",
        "sk-",
        "ghp_",
        "github_pat_",
        "xoxb-",
        "xoxp-",
        "begin private key",
        "end private key",
        "begin rsa private key",
        "end rsa private key",
        "begin ec private key",
        "end ec private key",
        "begin openssh private key",
        "end openssh private key",
    ];
    let mut result = String::with_capacity(input.len());
    let mut redacted_lines = 0;
    let mut private_key_end: Option<&str> = None;
    for line in input.split_inclusive('\n') {
        let lower = line.to_ascii_lowercase();
        for (begin, end) in [
            ("begin private key", "end private key"),
            ("begin rsa private key", "end rsa private key"),
            ("begin ec private key", "end ec private key"),
            ("begin openssh private key", "end openssh private key"),
        ] {
            if lower.contains(begin) {
                private_key_end = Some(end);
                break;
            }
        }
        let closes_key_span = private_key_end.is_some_and(|end| lower.contains(end));
        let suspicious_line =
            private_key_end.is_some() || suspicious.iter().any(|marker| lower.contains(marker));
        if suspicious_line {
            let newline = line.ends_with('\n');
            result.push_str("[REDACTED]");
            if newline {
                result.push('\n');
            }
            redacted_lines += 1;
        } else {
            result.push_str(line);
        }
        if closes_key_span {
            private_key_end = None;
        }
    }
    (result, redacted_lines)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use tempfile::tempdir;

    #[test]
    fn redacts_markers_and_private_key_spans_preserving_lines() {
        for secret in [
            "api_key=x",
            "api-key: x",
            "apikey x",
            "access_token=x",
            "refresh_token=x",
            "password=x",
            "authorization: x",
            "Bearer x",
            "sk-test",
            "ghp_test",
            "github_pat_test",
            "xoxb-test",
            "xoxp-test",
        ] {
            assert_eq!(redact(secret).0, "[REDACTED]");
        }
        let (output, lines) = redact(
            "safe\n-----BEGIN OPENSSH PRIVATE KEY-----\na\nb\n-----END OPENSSH PRIVATE KEY-----\ntail\n",
        );
        assert_eq!(
            output,
            "safe\n[REDACTED]\n[REDACTED]\n[REDACTED]\n[REDACTED]\ntail\n"
        );
        assert_eq!(lines, 4);
    }

    #[test]
    fn instruction_projection_never_selects_unrelated_fields() {
        let expected: &[&[&str]] = &[
            &["instructions"],
            &["developer_instructions"],
            &["model_instructions_file"],
            &[
                "features",
                "multi_agent_v2",
                "subagent_developer_instructions",
            ],
            &["features", "multi_agent_v2", "multi_agent_mode_hint_text"],
            &["features", "multi_agent_v2", "usage_hint_text"],
            &["features", "multi_agent_v2", "root_agent_usage_hint_text"],
            &["features", "multi_agent_v2", "subagent_usage_hint_text"],
        ];
        let actual = INSTRUCTION_PATHS
            .iter()
            .map(|path| path.join("."))
            .collect::<Vec<_>>();
        let expected = expected
            .iter()
            .map(|path| path.join("."))
            .collect::<Vec<_>>();
        assert_eq!(actual, expected);
        assert!(!is_instruction_path(&["api_key".to_string()]));
    }

    #[test]
    fn stdin_is_not_consumed_without_the_explicit_flag() {
        struct PanicReader;
        impl Read for PanicReader {
            fn read(&mut self, _buf: &mut [u8]) -> std::io::Result<usize> {
                panic!("stdin must not be read unless explicitly requested")
            }
        }
        assert!(
            read_optional_input(false, PanicReader)
                .expect("disabled input")
                .is_none()
        );
        assert_eq!(
            read_optional_input(true, Cursor::new(b"caller text".to_vec()))
                .expect("input")
                .as_deref(),
            Some("caller text")
        );
    }

    #[test]
    fn stdin_rejects_oversize_and_non_utf8_before_reporting() {
        assert!(read_optional_input(true, Cursor::new(vec![b'a'; MAX_INPUT_BYTES])).is_err());
        assert!(read_optional_input(true, Cursor::new(vec![0xff])).is_err());
    }

    #[test]
    fn oversized_encoded_report_writes_no_partial_output() {
        let worst_case_text = "\0".repeat(MAX_CONTENT_BYTES);
        let report = build_report(
            &codex_config::ConfigLayerStack::default(),
            true,
            Some(&worst_case_text),
        )
        .expect("complete report assembly");
        let mut output = Vec::new();
        assert!(write_bounded_json(&report, &mut output).is_err());
        assert!(
            output.is_empty(),
            "oversized report must not write a prefix"
        );
    }

    #[tokio::test]
    async fn fixture_loader_honors_profile_trusted_project_cwd_and_cli_precedence() {
        let temp = tempdir().expect("fixture directory");
        let home = temp.path().join("home");
        std::fs::create_dir_all(&home).expect("fixture home");
        let project = temp.path().join("project");
        let cwd = project.join("nested");
        std::fs::create_dir_all(project.join(".git")).expect("fixture git root");
        std::fs::create_dir_all(project.join(".codex")).expect("fixture project config dir");
        std::fs::write(project.join(".git/HEAD"), "ref: refs/heads/main\n")
            .expect("fixture git head");
        std::fs::create_dir_all(&cwd).expect("fixture cwd");
        let project_key =
            toml::Value::String(codex_config::loader::project_trust_key(&project)).to_string();
        std::fs::write(
            home.join("config.toml"),
            format!(
                "instructions = \"base instruction\"\napi_key = \"UNRELATED_API_KEY_MUST_NOT_APPEAR\"\naccess_token = \"UNRELATED_ACCESS_TOKEN_MUST_NOT_APPEAR\"\nprovider = \"UNRELATED_PROVIDER_MUST_NOT_APPEAR\"\nenvironment_secret = \"UNRELATED_ENV_SECRET_MUST_NOT_APPEAR\"\nauth_token = \"UNRELATED_AUTH_TOKEN_MUST_NOT_APPEAR\"\n[projects.{project_key}]\ntrust_level = \"trusted\"\n"
            ),
        )
        .expect("write base user config fixture");
        let profile: codex_utils_cli::ProfileV2Name = "inspection".parse().expect("profile name");
        let mut loader_overrides =
            super::super::loader_overrides_for_profile_at_codex_home(Some(&profile), &home);
        let profile_path = codex_core::config::resolve_profile_v2_config_path(&home, &profile);
        std::fs::write(
            profile_path,
            r#"
developer_instructions = "profile developer text"
model_instructions_file = "/forbidden/not-opened.md"
features.multi_agent_v2.subagent_developer_instructions = "subagent text"
features.multi_agent_v2.multi_agent_mode_hint_text = "profile mode hint"
features.multi_agent_v2.usage_hint_text = "profile usage hint"
features.multi_agent_v2.root_agent_usage_hint_text = "root usage hint"
features.multi_agent_v2.subagent_usage_hint_text = "subagent usage hint"
"#,
        )
        .expect("write profile fixture");
        std::fs::write(
            project.join(".codex/config.toml"),
            r#"instructions = "project instruction"
features.multi_agent_v2.multi_agent_mode_hint_text = "project mode hint""#,
        )
        .expect("write project config fixture");
        let requirements_path = home.join("requirements.toml");
        std::fs::write(
            &requirements_path,
            r#"additional_developer_instructions = "MANAGED_CONTENT_MUST_NEVER_LEAK""#,
        )
        .expect("write requirements fixture");

        loader_overrides.managed_config_path = Some(home.join("managed_config.toml"));
        loader_overrides.system_config_path = Some(home.join("system.toml"));
        loader_overrides.system_requirements_path = Some(requirements_path.clone());
        #[cfg(target_os = "macos")]
        {
            loader_overrides.managed_preferences_base64 = Some(String::new());
            loader_overrides.macos_managed_config_requirements_base64 = Some(String::new());
        }
        let layers = load_config_layers_state(
            LOCAL_FS.as_ref(),
            &home,
            Some(AbsolutePathBuf::from_absolute_path(&cwd).expect("absolute cwd")),
            &[(
                "features.multi_agent_v2.usage_hint_text".to_string(),
                TomlValue::String("CLI usage hint".to_string()),
            )],
            ConfigLoadOptions {
                loader_overrides,
                strict_config: false,
                cloud_config_bundle: CloudConfigBundleLoader::default(),
            },
            &NoopThreadConfigLoader,
        )
        .await
        .expect("load fixture-only config");
        let origins = layers.origins_with_path_filter(|path| is_instruction_path(path));
        assert_eq!(origins.len(), INSTRUCTION_PATHS.len());
        assert!(
            origins
                .values()
                .any(|origin| matches!(origin.name, ConfigLayerSource::SessionFlags))
        );
        assert!(matches!(
            origins["instructions"].name,
            ConfigLayerSource::Project { .. }
        ));
        assert!(matches!(
            &origins["developer_instructions"].name,
            ConfigLayerSource::User {
                profile: Some(name), ..
            } if name == "inspection"
        ));
        assert!(matches!(
            origins["features.multi_agent_v2.usage_hint_text"].name,
            ConfigLayerSource::SessionFlags
        ));
        assert!(matches!(
            origins["features.multi_agent_v2.multi_agent_mode_hint_text"].name,
            ConfigLayerSource::Project { .. }
        ));

        let default_report = build_report(&layers, false, None).expect("default report");
        let default_json = serde_json::to_string(&default_report).expect("default JSON");
        assert!(!default_json.contains("project instruction"));
        assert!(!default_json.contains("profile developer text"));
        assert!(!default_json.contains("UNRELATED_API_KEY"));
        assert!(!default_json.contains("UNRELATED_ACCESS_TOKEN"));
        assert!(!default_json.contains("UNRELATED_PROVIDER"));
        assert!(!default_json.contains("UNRELATED_ENV_SECRET"));
        assert!(!default_json.contains("UNRELATED_AUTH_TOKEN"));
        assert!(default_json.contains("/forbidden/not-opened.md"));
        let default_managed_report =
            &default_report["local_managed_additional_developer_instructions"];
        assert!(
            default_managed_report["present"]
                .as_bool()
                .expect("presence boolean")
        );
        assert!(
            default_managed_report
                .to_string()
                .contains("requirements.toml")
        );
        assert!(!default_json.contains("MANAGED_CONTENT_MUST_NEVER_LEAK"));
        let stdin_hidden_report = build_report(&layers, false, Some("caller-supplied text"))
            .expect("hidden stdin report");
        assert!(
            stdin_hidden_report["caller_supplied_text"]["present"]
                .as_bool()
                .expect("stdin presence")
        );
        assert!(
            stdin_hidden_report["caller_supplied_text"]
                .get("content")
                .is_none()
        );

        let content_report =
            build_report(&layers, true, Some("caller-supplied text")).expect("content report");
        let content_json = serde_json::to_string(&content_report).expect("content JSON");
        assert!(content_json.contains("project instruction"));
        assert!(content_json.contains("profile developer text"));
        assert!(content_json.contains("CLI usage hint"));
        assert_eq!(
            content_report["caller_supplied_text"]["origin"],
            "caller-supplied; unverified"
        );
        assert!(!content_json.contains("UNRELATED_API_KEY"));
        assert!(!content_json.contains("UNRELATED_ACCESS_TOKEN"));
        assert!(!content_json.contains("UNRELATED_PROVIDER"));
        assert!(!content_json.contains("UNRELATED_ENV_SECRET"));
        assert!(!content_json.contains("UNRELATED_AUTH_TOKEN"));
        assert!(!content_json.contains("MANAGED_CONTENT_MUST_NEVER_LEAK"));

        let managed_report = &content_report["local_managed_additional_developer_instructions"];
        assert!(
            managed_report["present"]
                .as_bool()
                .expect("presence boolean")
        );
        assert!(managed_report.to_string().contains("requirements.toml"));
        assert!(
            !managed_report
                .to_string()
                .contains("MANAGED_CONTENT_MUST_NEVER_LEAK")
        );
    }
}
