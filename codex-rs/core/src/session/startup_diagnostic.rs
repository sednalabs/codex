use std::io::Write;

/// Payload-free observation for the two fixed hosted startup discriminators.
/// Release builds and ordinary sessions do not enable this observer.
#[derive(Clone, Copy)]
pub(super) struct StartupDiagnostic(Option<&'static str>);

#[derive(Clone, Copy)]
pub(super) enum StartupSite {
    TimeProvider,
    ThreadPersistence,
    LocalRolloutPath,
    AgentsMdRefresh,
    NetworkProxy,
    HooksNew,
    McpInitialInstall,
    ReferencedRolloutMaterialization,
}

impl StartupSite {
    fn label(self) -> &'static str {
        match self {
            Self::TimeProvider => "time_provider",
            Self::ThreadPersistence => "thread_persistence",
            Self::LocalRolloutPath => "local_rollout_path",
            Self::AgentsMdRefresh => "agents_md_refresh",
            Self::NetworkProxy => "network_proxy",
            Self::HooksNew => "hooks_new",
            Self::McpInitialInstall => "mcp_initial_install",
            Self::ReferencedRolloutMaterialization => "referenced_rollout_materialization",
        }
    }
}

impl StartupDiagnostic {
    pub(super) fn disabled() -> Self {
        Self(None)
    }

    pub(super) fn from_process() -> Self {
        #[cfg(all(debug_assertions, target_os = "linux"))]
        {
            Self::from_case(
                std::env::var("CODEX_CORE_RUNTIME_DIAGNOSTIC_CASE")
                    .ok()
                    .as_deref(),
            )
        }
        #[cfg(not(all(debug_assertions, target_os = "linux")))]
        {
            Self::disabled()
        }
    }

    #[cfg(any(test, all(debug_assertions, target_os = "linux")))]
    fn from_case(case: Option<&str>) -> Self {
        Self(match case {
            Some("restricted") => Some("restricted"),
            Some("project_docs") => Some("project_docs"),
            _ => None,
        })
    }

    pub(super) fn result<T, E>(self, site: StartupSite, result: Result<T, E>) -> Result<T, E> {
        if self.0.is_some() && result.is_err() {
            self.result_to(site, result, &mut std::io::stderr().lock())
        } else {
            result
        }
    }

    fn result_to<T, E>(
        self,
        site: StartupSite,
        result: Result<T, E>,
        writer: &mut impl Write,
    ) -> Result<T, E> {
        if let Some(case) = self.0
            && result.is_err()
        {
            // Never format the error, configuration, paths, or any other payload.
            // A failed diagnostic write must not replace the original Result.
            let _ = writeln!(
                writer,
                "codex-core-runtime-diagnostic-startup-v1 case={case} site={}",
                site.label(),
            );
        }
        result
    }
}

#[cfg(test)]
#[path = "startup_diagnostic_tests.rs"]
mod tests;
