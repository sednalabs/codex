use super::StartupDiagnostic;
use super::StartupSite;
use pretty_assertions::assert_eq;
use std::io::Write;

#[test]
fn startup_site_witness_preserves_results_and_rejects_payloads() {
    let sites = [
        (StartupSite::TimeProvider, "time_provider"),
        (StartupSite::ThreadPersistence, "thread_persistence"),
        (StartupSite::LocalRolloutPath, "local_rollout_path"),
        (StartupSite::AgentsMdRefresh, "agents_md_refresh"),
        (StartupSite::NetworkProxy, "network_proxy"),
        (StartupSite::HooksNew, "hooks_new"),
        (StartupSite::McpInitialInstall, "mcp_initial_install"),
        (
            StartupSite::ReferencedRolloutMaterialization,
            "referenced_rollout_materialization",
        ),
    ];
    let private_error = String::from("private-error/path/token-canary");
    for case in ["restricted", "project_docs"] {
        for (site, label) in sites {
            let mut bytes = Vec::new();
            let result: Result<(), &String> = StartupDiagnostic::from_case(Some(case)).result_to(
                site,
                Err(&private_error),
                &mut bytes,
            );
            assert!(std::ptr::eq(result.unwrap_err(), &private_error));
            assert_eq!(
                String::from_utf8(bytes).unwrap(),
                format!("codex-core-runtime-diagnostic-startup-v1 case={case} site={label}\n"),
            );
        }
    }

    for case in [None, Some("unknown"), Some(private_error.as_str())] {
        let mut bytes = Vec::new();
        let result: Result<(), &String> = StartupDiagnostic::from_case(case).result_to(
            StartupSite::AgentsMdRefresh,
            Err(&private_error),
            &mut bytes,
        );
        assert!(std::ptr::eq(result.unwrap_err(), &private_error));
        assert!(bytes.is_empty());
    }

    let mut bytes = Vec::new();
    let result: Result<&String, ()> = StartupDiagnostic::from_case(Some("restricted")).result_to(
        StartupSite::AgentsMdRefresh,
        Ok(&private_error),
        &mut bytes,
    );
    assert!(std::ptr::eq(result.unwrap(), &private_error));
    assert!(bytes.is_empty());
    let result: Result<(), &String> = StartupDiagnostic::from_case(Some("restricted")).result_to(
        StartupSite::AgentsMdRefresh,
        Err(&private_error),
        &mut FailingWriter,
    );
    assert!(std::ptr::eq(result.unwrap_err(), &private_error));
}

struct FailingWriter;

impl Write for FailingWriter {
    fn write(&mut self, _bytes: &[u8]) -> std::io::Result<usize> {
        Err(std::io::Error::other("private diagnostic sink failure"))
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
