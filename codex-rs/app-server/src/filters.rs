use codex_app_server_protocol::ThreadSourceKind;
use codex_core::INTERACTIVE_SESSION_SOURCES;
use codex_protocol::protocol::SessionSource as CoreSessionSource;
use codex_protocol::protocol::SubAgentSource as CoreSubAgentSource;
use codex_protocol::protocol::ThreadSource as CoreThreadSource;

pub(crate) fn compute_source_filters(
    source_kinds: Option<Vec<ThreadSourceKind>>,
) -> (Vec<CoreSessionSource>, Option<Vec<ThreadSourceKind>>) {
    let Some(source_kinds) = source_kinds else {
        return (INTERACTIVE_SESSION_SOURCES.to_vec(), None);
    };

    if source_kinds.is_empty() {
        return (INTERACTIVE_SESSION_SOURCES.to_vec(), None);
    }

    let requires_post_filter = source_kinds.iter().any(|kind| {
        matches!(
            kind,
            ThreadSourceKind::Exec
                | ThreadSourceKind::AppServer
                | ThreadSourceKind::SubAgent
                | ThreadSourceKind::SubAgentReview
                | ThreadSourceKind::SubAgentCompact
                | ThreadSourceKind::SubAgentThreadSpawn
                | ThreadSourceKind::SubAgentOther
                | ThreadSourceKind::Unknown
        )
    });

    if requires_post_filter {
        (Vec::new(), Some(source_kinds))
    } else {
        let interactive_sources = source_kinds
            .iter()
            .filter_map(|kind| match kind {
                ThreadSourceKind::Cli => Some(CoreSessionSource::Cli),
                ThreadSourceKind::VsCode => Some(CoreSessionSource::VSCode),
                ThreadSourceKind::Exec
                | ThreadSourceKind::AppServer
                | ThreadSourceKind::SubAgent
                | ThreadSourceKind::SubAgentReview
                | ThreadSourceKind::SubAgentCompact
                | ThreadSourceKind::SubAgentThreadSpawn
                | ThreadSourceKind::SubAgentOther
                | ThreadSourceKind::Unknown => None,
            })
            .collect::<Vec<_>>();
        (interactive_sources, Some(source_kinds))
    }
}

pub(crate) fn source_kind_matches(source: &CoreSessionSource, filter: &[ThreadSourceKind]) -> bool {
    filter.iter().any(|kind| match kind {
        ThreadSourceKind::Cli => matches!(source, CoreSessionSource::Cli),
        ThreadSourceKind::VsCode => matches!(source, CoreSessionSource::VSCode),
        ThreadSourceKind::Exec => matches!(source, CoreSessionSource::Exec),
        ThreadSourceKind::AppServer => matches!(source, CoreSessionSource::Mcp),
        ThreadSourceKind::SubAgent => matches!(source, CoreSessionSource::SubAgent(_)),
        ThreadSourceKind::SubAgentReview => {
            matches!(
                source,
                CoreSessionSource::SubAgent(CoreSubAgentSource::Review)
            )
        }
        ThreadSourceKind::SubAgentCompact => {
            matches!(
                source,
                CoreSessionSource::SubAgent(CoreSubAgentSource::Compact)
            )
        }
        ThreadSourceKind::SubAgentThreadSpawn => matches!(
            source,
            CoreSessionSource::SubAgent(CoreSubAgentSource::ThreadSpawn { .. })
        ),
        ThreadSourceKind::SubAgentOther => matches!(
            source,
            CoreSessionSource::SubAgent(CoreSubAgentSource::Other(_))
        ),
        ThreadSourceKind::Unknown => matches!(source, CoreSessionSource::Unknown),
    })
}

pub(crate) fn thread_source_matches(
    thread_source: Option<CoreThreadSource>,
    filter: Option<&[CoreThreadSource]>,
) -> bool {
    match filter {
        Some(filter) if !filter.is_empty() => {
            thread_source.is_some_and(|thread_source| filter.contains(&thread_source))
        }
        Some(_) | None => !matches!(thread_source, Some(CoreThreadSource::Side)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_protocol::ThreadId;
    use pretty_assertions::assert_eq;
    use uuid::Uuid;

    #[test]
    fn compute_source_filters_defaults_to_interactive_sources() {
        let (allowed_sources, filter) = compute_source_filters(/*source_kinds*/ None);

        assert_eq!(allowed_sources, INTERACTIVE_SESSION_SOURCES.to_vec());
        assert_eq!(filter, None);
    }

    #[test]
    fn compute_source_filters_empty_means_interactive_sources() {
        let (allowed_sources, filter) = compute_source_filters(Some(Vec::new()));

        assert_eq!(allowed_sources, INTERACTIVE_SESSION_SOURCES.to_vec());
        assert_eq!(filter, None);
    }

    #[test]
    fn compute_source_filters_interactive_only_skips_post_filtering() {
        let source_kinds = vec![ThreadSourceKind::Cli, ThreadSourceKind::VsCode];
        let (allowed_sources, filter) = compute_source_filters(Some(source_kinds.clone()));

        assert_eq!(
            allowed_sources,
            vec![CoreSessionSource::Cli, CoreSessionSource::VSCode]
        );
        assert_eq!(filter, Some(source_kinds));
    }

    #[test]
    fn compute_source_filters_subagent_variant_requires_post_filtering() {
        let source_kinds = vec![ThreadSourceKind::SubAgentReview];
        let (allowed_sources, filter) = compute_source_filters(Some(source_kinds.clone()));

        assert_eq!(allowed_sources, Vec::new());
        assert_eq!(filter, Some(source_kinds));
    }

    #[test]
    fn source_kind_matches_distinguishes_subagent_variants() {
        let parent_thread_id =
            ThreadId::from_string(&Uuid::new_v4().to_string()).expect("valid thread id");
        let review = CoreSessionSource::SubAgent(CoreSubAgentSource::Review);
        let spawn = CoreSessionSource::SubAgent(CoreSubAgentSource::ThreadSpawn {
            parent_thread_id,
            depth: 1,
            agent_path: None,
            agent_nickname: None,
            agent_role: None,
        });

        assert!(source_kind_matches(
            &review,
            &[ThreadSourceKind::SubAgentReview]
        ));
        assert!(!source_kind_matches(
            &review,
            &[ThreadSourceKind::SubAgentThreadSpawn]
        ));
        assert!(source_kind_matches(
            &spawn,
            &[ThreadSourceKind::SubAgentThreadSpawn]
        ));
        assert!(!source_kind_matches(
            &spawn,
            &[ThreadSourceKind::SubAgentReview]
        ));
    }

    #[test]
    fn thread_source_filter_excludes_side_by_default() {
        let no_thread_source = Option::<CoreThreadSource>::None;
        let no_filter = Option::<&[CoreThreadSource]>::None;

        assert!(thread_source_matches(no_thread_source, no_filter));
        assert!(thread_source_matches(
            Some(CoreThreadSource::User),
            no_filter
        ));
        assert!(!thread_source_matches(
            Some(CoreThreadSource::Side),
            no_filter
        ));
        assert!(!thread_source_matches(
            Some(CoreThreadSource::Side),
            Some(&[])
        ));
    }

    #[test]
    fn thread_source_filter_matches_side_only_when_requested() {
        let filter = [CoreThreadSource::Side];
        let no_thread_source = Option::<CoreThreadSource>::None;

        assert!(thread_source_matches(
            Some(CoreThreadSource::Side),
            Some(&filter)
        ));
        assert!(!thread_source_matches(
            Some(CoreThreadSource::User),
            Some(&filter)
        ));
        assert!(!thread_source_matches(no_thread_source, Some(&filter)));
    }
}
