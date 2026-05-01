use std::path::Path;

use serde::Deserialize;

pub const ANDROID_RUNTIME_CONFIG_FILES: [&str; 3] = [
    "android-computer-use.json",
    "android-dynamic-tools.json",
    "solarlab-android-dynamic-tools.json",
];
pub const ANDROID_MCP_URL_ENV_VARS: [&str; 2] =
    ["CODEX_ANDROID_MCP_URL", "SOLARLAB_ANDROID_MCP_URL"];
pub const ANDROID_MCP_HOSTNAME_ENV_VARS: [&str; 2] = [
    "CODEX_ANDROID_MCP_HOSTNAME",
    "SOLARLAB_ANDROID_MCP_HOSTNAME",
];
pub const ANDROID_MCP_CF_ACCESS_CLIENT_ID_ENV_VARS: [&str; 2] = [
    "CODEX_ANDROID_MCP_CF_ACCESS_CLIENT_ID",
    "SOLARLAB_ANDROID_MCP_CF_ACCESS_CLIENT_ID",
];
pub const ANDROID_MCP_CF_ACCESS_CLIENT_SECRET_ENV_VARS: [&str; 2] = [
    "CODEX_ANDROID_MCP_CF_ACCESS_CLIENT_SECRET",
    "SOLARLAB_ANDROID_MCP_CF_ACCESS_CLIENT_SECRET",
];

const DEFAULT_MCP_URL_PATH: &str = "/mcp";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AndroidRuntimeConfig {
    pub mcp_url: String,
    pub cf_access_client_id: Option<String>,
    pub cf_access_client_secret: Option<String>,
}

#[derive(Deserialize)]
struct AndroidRuntimeConfigFile {
    mcp_url: Option<String>,
}

pub fn load_android_runtime_config(codex_home: &Path) -> Option<AndroidRuntimeConfig> {
    load_android_runtime_config_with_env(codex_home, |name| std::env::var(name).ok())
}

pub fn load_android_runtime_config_with_env<F>(
    codex_home: &Path,
    env_var: F,
) -> Option<AndroidRuntimeConfig>
where
    F: Fn(&str) -> Option<String>,
{
    let mcp_url = configured_android_provider_url(codex_home, &env_var)?;
    Some(AndroidRuntimeConfig {
        mcp_url,
        cf_access_client_id: first_value(&ANDROID_MCP_CF_ACCESS_CLIENT_ID_ENV_VARS, &env_var),
        cf_access_client_secret: first_value(
            &ANDROID_MCP_CF_ACCESS_CLIENT_SECRET_ENV_VARS,
            &env_var,
        ),
    })
}

fn configured_android_provider_url<F>(codex_home: &Path, env_var: &F) -> Option<String>
where
    F: Fn(&str) -> Option<String>,
{
    configured_android_provider_url_from_env(env_var)
        .or_else(|| configured_android_provider_url_from_file(codex_home))
}

fn configured_android_provider_url_from_env<F>(env_var: &F) -> Option<String>
where
    F: Fn(&str) -> Option<String>,
{
    first_value(&ANDROID_MCP_URL_ENV_VARS, env_var).or_else(|| {
        first_value(&ANDROID_MCP_HOSTNAME_ENV_VARS, env_var).map(|host| {
            let host = host.trim_end_matches('/');
            if host.starts_with("http://") || host.starts_with("https://") {
                format!("{host}{DEFAULT_MCP_URL_PATH}")
            } else {
                format!("https://{host}{DEFAULT_MCP_URL_PATH}")
            }
        })
    })
}

fn configured_android_provider_url_from_file(codex_home: &Path) -> Option<String> {
    ANDROID_RUNTIME_CONFIG_FILES
        .into_iter()
        .filter_map(|file_name| {
            let contents = std::fs::read_to_string(codex_home.join(file_name)).ok()?;
            serde_json::from_str::<AndroidRuntimeConfigFile>(&contents).ok()
        })
        .find_map(|config| {
            config
                .mcp_url
                .map(|mcp_url| mcp_url.trim().to_string())
                .filter(|mcp_url| !mcp_url.is_empty())
        })
}

fn first_value<F>(names: &[&str], env_var: &F) -> Option<String>
where
    F: Fn(&str) -> Option<String>,
{
    names
        .iter()
        .filter_map(|name| env_var(name))
        .map(|value| value.trim().to_string())
        .find(|value| !value.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn env_from<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        |name| {
            pairs
                .iter()
                .find_map(|(key, value)| (*key == name).then(|| (*value).to_string()))
        }
    }

    #[test]
    fn loads_direct_url_from_env() {
        let dir = tempfile::tempdir().expect("temp dir");

        let actual = load_android_runtime_config_with_env(
            dir.path(),
            env_from(&[("CODEX_ANDROID_MCP_URL", " https://android.example/mcp ")]),
        );

        assert_eq!(
            actual,
            Some(AndroidRuntimeConfig {
                mcp_url: "https://android.example/mcp".to_string(),
                cf_access_client_id: None,
                cf_access_client_secret: None,
            })
        );
    }

    #[test]
    fn normalizes_hostname_from_env() {
        let dir = tempfile::tempdir().expect("temp dir");

        let actual = load_android_runtime_config_with_env(
            dir.path(),
            env_from(&[("SOLARLAB_ANDROID_MCP_HOSTNAME", "android.example")]),
        );

        assert_eq!(
            actual.map(|config| config.mcp_url),
            Some("https://android.example/mcp".to_string())
        );
    }

    #[test]
    fn preserves_scheme_hostname_from_env() {
        let dir = tempfile::tempdir().expect("temp dir");

        let actual = load_android_runtime_config_with_env(
            dir.path(),
            env_from(&[("CODEX_ANDROID_MCP_HOSTNAME", "http://localhost:8080/")]),
        );

        assert_eq!(
            actual.map(|config| config.mcp_url),
            Some("http://localhost:8080/mcp".to_string())
        );
    }

    #[test]
    fn loads_url_from_codex_home_file() {
        let dir = tempfile::tempdir().expect("temp dir");
        std::fs::write(
            dir.path().join("android-computer-use.json"),
            r#"{"mcp_url":"https://file.example/mcp"}"#,
        )
        .expect("write runtime config");

        let actual = load_android_runtime_config_with_env(dir.path(), env_from(&[]));

        assert_eq!(
            actual.map(|config| config.mcp_url),
            Some("https://file.example/mcp".to_string())
        );
    }

    #[test]
    fn ignores_empty_sources() {
        let dir = tempfile::tempdir().expect("temp dir");
        std::fs::write(
            dir.path().join("android-computer-use.json"),
            r#"{"mcp_url":""}"#,
        )
        .expect("write runtime config");

        let actual = load_android_runtime_config_with_env(
            dir.path(),
            env_from(&[("CODEX_ANDROID_MCP_URL", " ")]),
        );

        assert_eq!(actual, None);
    }

    #[test]
    fn includes_cloudflare_access_env_pair() {
        let dir = tempfile::tempdir().expect("temp dir");

        let actual = load_android_runtime_config_with_env(
            dir.path(),
            env_from(&[
                ("CODEX_ANDROID_MCP_URL", "https://android.example/mcp"),
                ("CODEX_ANDROID_MCP_CF_ACCESS_CLIENT_ID", "id"),
                ("CODEX_ANDROID_MCP_CF_ACCESS_CLIENT_SECRET", "secret"),
            ]),
        );

        assert_eq!(
            actual,
            Some(AndroidRuntimeConfig {
                mcp_url: "https://android.example/mcp".to_string(),
                cf_access_client_id: Some("id".to_string()),
                cf_access_client_secret: Some("secret".to_string()),
            })
        );
    }
}
