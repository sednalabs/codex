use anyhow::Result;
use app_test_support::MockResponsesConfig;
use app_test_support::TestAppServer;
use app_test_support::create_fake_parented_rollout_with_source;
use app_test_support::create_fake_rollout;
use app_test_support::create_mock_responses_server_repeating_assistant;
use codex_app_server_protocol::ThreadLoadedListParams;
use codex_app_server_protocol::ThreadLoadedListResponse;
use codex_app_server_protocol::ThreadResumeParams;
use codex_app_server_protocol::ThreadResumeResponse;
use codex_app_server_protocol::ThreadStartParams;
use codex_app_server_protocol::ThreadStartResponse;
use codex_protocol::ThreadId;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use codex_state::DirectionalThreadSpawnEdgeStatus;
use codex_state::StateRuntime;
use codex_utils_absolute_path::test_support::PathExt;
use pretty_assertions::assert_eq;
use tempfile::TempDir;
use tokio::time::timeout;

const DEFAULT_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

#[tokio::test]
async fn thread_loaded_list_returns_loaded_thread_ids() -> Result<()> {
    let server = create_mock_responses_server_repeating_assistant("Done").await;
    let codex_home = TempDir::new()?;
    MockResponsesConfig::new(&server.uri()).write(codex_home.path())?;

    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .build_initialized_with_timeout(DEFAULT_READ_TIMEOUT)
        .await?;

    let thread_id = start_thread(&mut mcp).await?;

    let list_id = mcp
        .send_thread_loaded_list_request(ThreadLoadedListParams::default())
        .await?;
    let ThreadLoadedListResponse {
        mut data,
        next_cursor,
        ..
    } = timeout(DEFAULT_READ_TIMEOUT, mcp.read_response(list_id)).await??;
    data.sort();
    assert_eq!(data, vec![thread_id]);
    assert_eq!(next_cursor, None);

    Ok(())
}

#[tokio::test]
async fn thread_loaded_list_paginates() -> Result<()> {
    let server = create_mock_responses_server_repeating_assistant("Done").await;
    let codex_home = TempDir::new()?;
    MockResponsesConfig::new(&server.uri()).write(codex_home.path())?;

    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .build_initialized_with_timeout(DEFAULT_READ_TIMEOUT)
        .await?;

    let first = start_thread(&mut mcp).await?;
    let second = start_thread(&mut mcp).await?;

    let mut expected = [first, second];
    expected.sort();

    let list_id = mcp
        .send_thread_loaded_list_request(ThreadLoadedListParams {
            cursor: None,
            limit: Some(1),
            ancestor_thread_id: None,
        })
        .await?;
    let ThreadLoadedListResponse {
        data: first_page,
        next_cursor,
        ..
    } = timeout(DEFAULT_READ_TIMEOUT, mcp.read_response(list_id)).await??;
    assert_eq!(first_page, vec![expected[0].clone()]);
    assert_eq!(next_cursor, Some(expected[0].clone()));

    let list_id = mcp
        .send_thread_loaded_list_request(ThreadLoadedListParams {
            cursor: next_cursor,
            limit: Some(1),
            ancestor_thread_id: None,
        })
        .await?;
    let ThreadLoadedListResponse {
        data: second_page,
        next_cursor,
        ..
    } = timeout(DEFAULT_READ_TIMEOUT, mcp.read_response(list_id)).await??;
    assert_eq!(second_page, vec![expected[1].clone()]);
    assert_eq!(next_cursor, None);

    Ok(())
}

#[tokio::test]
async fn thread_loaded_list_omitted_limit_uses_bounded_page_and_continuation() -> Result<()> {
    const DEFAULT_PAGE_SIZE: usize = 100;

    let server = create_mock_responses_server_repeating_assistant("Done").await;
    let codex_home = TempDir::new()?;
    MockResponsesConfig::new(&server.uri()).write(codex_home.path())?;

    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .build_initialized_with_timeout(DEFAULT_READ_TIMEOUT)
        .await?;

    let mut expected = Vec::with_capacity(DEFAULT_PAGE_SIZE + 1);
    for _ in 0..=DEFAULT_PAGE_SIZE {
        expected.push(start_thread(&mut mcp).await?);
    }
    expected.sort();

    // Omitting `limit` intentionally uses the protocol's bounded 100-id default rather than
    // the historical unbounded form. The continuation makes the remaining loaded id reachable.
    let list_id = mcp
        .send_thread_loaded_list_request(ThreadLoadedListParams::default())
        .await?;
    let ThreadLoadedListResponse {
        data: first_page,
        next_cursor,
        ..
    } = timeout(DEFAULT_READ_TIMEOUT, mcp.read_response(list_id)).await??;
    assert_eq!(first_page, expected[..DEFAULT_PAGE_SIZE].to_vec());
    assert_eq!(next_cursor, Some(expected[DEFAULT_PAGE_SIZE - 1].clone()));

    let list_id = mcp
        .send_thread_loaded_list_request(ThreadLoadedListParams {
            cursor: next_cursor,
            limit: None,
            ancestor_thread_id: None,
        })
        .await?;
    let ThreadLoadedListResponse {
        data: second_page,
        next_cursor,
        ..
    } = timeout(DEFAULT_READ_TIMEOUT, mcp.read_response(list_id)).await??;
    assert_eq!(second_page, expected[DEFAULT_PAGE_SIZE..].to_vec());
    assert_eq!(next_cursor, None);

    Ok(())
}

#[tokio::test]
async fn thread_loaded_list_filters_loaded_spawn_descendants() -> Result<()> {
    let server = create_mock_responses_server_repeating_assistant("Done").await;
    let codex_home = TempDir::new()?;
    MockResponsesConfig::new(&server.uri()).write(codex_home.path())?;
    let root_thread_id = create_fake_rollout(
        codex_home.path(),
        "2026-01-07T00-00-00",
        "2026-01-07T00:00:00Z",
        "Saved root message",
        Some("mock_provider"),
        /*git_info*/ None,
    )?;
    let root_thread_uuid = ThreadId::from_string(&root_thread_id)?;
    let child_thread_id = create_fake_parented_rollout_with_source(
        codex_home.path(),
        "2026-01-07T00-00-01",
        "2026-01-07T00:00:01Z",
        "Saved child message",
        Some("mock_provider"),
        /*git_info*/ None,
        SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
            parent_thread_id: root_thread_uuid,
            depth: 1,
            agent_path: None,
            agent_nickname: None,
            agent_role: None,
        }),
        root_thread_uuid.into(),
        root_thread_uuid,
    )?;
    let child_thread_uuid = ThreadId::from_string(&child_thread_id)?;
    let grandchild_thread_id = create_fake_parented_rollout_with_source(
        codex_home.path(),
        "2026-01-07T00-00-02",
        "2026-01-07T00:00:02Z",
        "Saved grandchild message",
        Some("mock_provider"),
        /*git_info*/ None,
        SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
            parent_thread_id: child_thread_uuid,
            depth: 2,
            agent_path: None,
            agent_nickname: None,
            agent_role: None,
        }),
        root_thread_uuid.into(),
        child_thread_uuid,
    )?;
    let unrelated_root_thread_id = create_fake_rollout(
        codex_home.path(),
        "2026-01-07T00-00-03",
        "2026-01-07T00:00:03Z",
        "Saved unrelated root message",
        Some("mock_provider"),
        /*git_info*/ None,
    )?;
    let unrelated_root_thread_uuid = ThreadId::from_string(&unrelated_root_thread_id)?;
    let unrelated_child_thread_id = create_fake_parented_rollout_with_source(
        codex_home.path(),
        "2026-01-07T00-00-04",
        "2026-01-07T00:00:04Z",
        "Saved unrelated child message",
        Some("mock_provider"),
        /*git_info*/ None,
        SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
            parent_thread_id: unrelated_root_thread_uuid,
            depth: 1,
            agent_path: None,
            agent_nickname: None,
            agent_role: None,
        }),
        unrelated_root_thread_uuid.into(),
        unrelated_root_thread_uuid,
    )?;

    let state_db = StateRuntime::init(
        codex_state::SqliteConfig::new_for_testing(codex_home.path().abs()),
        "mock_provider".into(),
    )
    .await?;
    for (parent_thread_id, child_thread_id) in [
        (root_thread_uuid, child_thread_uuid),
        (
            child_thread_uuid,
            ThreadId::from_string(&grandchild_thread_id)?,
        ),
        (
            unrelated_root_thread_uuid,
            ThreadId::from_string(&unrelated_child_thread_id)?,
        ),
    ] {
        state_db
            .upsert_thread_spawn_edge(
                parent_thread_id,
                child_thread_id,
                DirectionalThreadSpawnEdgeStatus::Open,
            )
            .await?;
    }
    // These persisted descendants are intentionally never resumed. A loaded-list request must
    // not materialize this historical subtree merely to find the one loaded nested descendant.
    for historical_suffix in 100..356 {
        let historical_thread_id = ThreadId::from_string(&format!(
            "00000000-0000-0000-0000-{historical_suffix:012}"
        ))?;
        state_db
            .upsert_thread_spawn_edge(
                root_thread_uuid,
                historical_thread_id,
                DirectionalThreadSpawnEdgeStatus::Closed,
            )
            .await?;
    }

    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .build_initialized_with_timeout(DEFAULT_READ_TIMEOUT)
        .await?;
    // The child is deliberately left unloaded. The durable root -> child edge must still
    // establish that the loaded grandchild belongs below root, without returning the unloaded
    // child itself.
    for thread_id in [
        &root_thread_id,
        &grandchild_thread_id,
        &unrelated_root_thread_id,
        &unrelated_child_thread_id,
    ] {
        let resume_id = mcp
            .send_thread_resume_request(ThreadResumeParams {
                thread_id: thread_id.clone(),
                ..Default::default()
            })
            .await?;
        let _: ThreadResumeResponse =
            timeout(DEFAULT_READ_TIMEOUT, mcp.read_response(resume_id)).await??;
    }

    let list_id = mcp
        .send_thread_loaded_list_request(ThreadLoadedListParams {
            cursor: None,
            limit: Some(1),
            ancestor_thread_id: Some(root_thread_id.clone()),
        })
        .await?;
    let ThreadLoadedListResponse {
        data,
        next_cursor,
        ancestor_filter_applied,
    } = timeout(DEFAULT_READ_TIMEOUT, mcp.read_response(list_id)).await??;
    assert_eq!(data, vec![grandchild_thread_id.clone()]);
    assert!(!data.contains(&child_thread_id));
    assert!(!data.contains(&unrelated_root_thread_id));
    assert!(!data.contains(&unrelated_child_thread_id));
    assert_eq!(next_cursor, None);
    assert!(ancestor_filter_applied);

    // An oversized public limit is clamped before it reaches the manager's probe allocation.
    // The same bounded path handles the protocol's omitted-limit compatibility form.
    for limit in [Some(u32::MAX), None] {
        let list_id = mcp
            .send_thread_loaded_list_request(ThreadLoadedListParams {
                cursor: None,
                limit,
                ancestor_thread_id: Some(root_thread_id.clone()),
            })
            .await?;
        let ThreadLoadedListResponse {
            data,
            next_cursor,
            ancestor_filter_applied,
        } = timeout(DEFAULT_READ_TIMEOUT, mcp.read_response(list_id)).await??;
        assert_eq!(data, vec![grandchild_thread_id.clone()]);
        assert_eq!(next_cursor, None);
        assert!(ancestor_filter_applied);
    }

    Ok(())
}

#[tokio::test]
async fn thread_loaded_list_falls_back_to_live_spawn_descendants_when_graph_query_fails()
-> Result<()> {
    let server = create_mock_responses_server_repeating_assistant("Done").await;
    let codex_home = TempDir::new()?;
    MockResponsesConfig::new(&server.uri()).write(codex_home.path())?;
    let root_thread_id = create_fake_rollout(
        codex_home.path(),
        "2026-01-08T00-00-00",
        "2026-01-08T00:00:00Z",
        "Saved root message",
        Some("mock_provider"),
        /*git_info*/ None,
    )?;
    let root_thread_uuid = ThreadId::from_string(&root_thread_id)?;
    let child_thread_id = create_fake_parented_rollout_with_source(
        codex_home.path(),
        "2026-01-08T00-00-01",
        "2026-01-08T00:00:01Z",
        "Saved child message",
        Some("mock_provider"),
        /*git_info*/ None,
        SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
            parent_thread_id: root_thread_uuid,
            depth: 1,
            agent_path: None,
            agent_nickname: None,
            agent_role: None,
        }),
        root_thread_uuid.into(),
        root_thread_uuid,
    )?;
    let child_thread_uuid = ThreadId::from_string(&child_thread_id)?;
    let grandchild_thread_id = create_fake_parented_rollout_with_source(
        codex_home.path(),
        "2026-01-08T00-00-02",
        "2026-01-08T00:00:02Z",
        "Saved grandchild message",
        Some("mock_provider"),
        /*git_info*/ None,
        SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
            parent_thread_id: child_thread_uuid,
            depth: 2,
            agent_path: None,
            agent_nickname: None,
            agent_role: None,
        }),
        root_thread_uuid.into(),
        child_thread_uuid,
    )?;
    let unrelated_root_thread_id = create_fake_rollout(
        codex_home.path(),
        "2026-01-08T00-00-03",
        "2026-01-08T00:00:03Z",
        "Saved unrelated root message",
        Some("mock_provider"),
        /*git_info*/ None,
    )?;
    let unrelated_root_thread_uuid = ThreadId::from_string(&unrelated_root_thread_id)?;
    let unrelated_child_thread_id = create_fake_parented_rollout_with_source(
        codex_home.path(),
        "2026-01-08T00-00-04",
        "2026-01-08T00:00:04Z",
        "Saved unrelated child message",
        Some("mock_provider"),
        /*git_info*/ None,
        SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
            parent_thread_id: unrelated_root_thread_uuid,
            depth: 1,
            agent_path: None,
            agent_nickname: None,
            agent_role: None,
        }),
        unrelated_root_thread_uuid.into(),
        unrelated_root_thread_uuid,
    )?;

    let sqlite = codex_state::SqliteConfig::new_for_testing(codex_home.path().abs());
    let state_db = StateRuntime::init(sqlite.clone(), "mock_provider".into()).await?;
    drop(state_db);

    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .build_initialized_with_timeout(DEFAULT_READ_TIMEOUT)
        .await?;
    for thread_id in [
        &root_thread_id,
        &child_thread_id,
        &grandchild_thread_id,
        &unrelated_root_thread_id,
        &unrelated_child_thread_id,
    ] {
        let resume_id = mcp
            .send_thread_resume_request(ThreadResumeParams {
                thread_id: thread_id.clone(),
                ..Default::default()
            })
            .await?;
        let _: ThreadResumeResponse =
            timeout(DEFAULT_READ_TIMEOUT, mcp.read_response(resume_id)).await??;
    }

    // Inject a persisted-graph query failure after the live registry has been populated. The
    // ancestor-filtered RPC must still use the loaded direct and nested ThreadSpawn edges.
    let fault_injection_db = sqlite.open_read_write_pool(&sqlite.state_db_path()).await?;
    sqlx::query("DROP TABLE thread_spawn_edges")
        .execute(&fault_injection_db)
        .await?;
    fault_injection_db.close().await;

    let mut expected = vec![child_thread_id, grandchild_thread_id];
    expected.sort();

    let list_id = mcp
        .send_thread_loaded_list_request(ThreadLoadedListParams {
            cursor: None,
            limit: Some(1),
            ancestor_thread_id: Some(root_thread_id),
        })
        .await?;
    let ThreadLoadedListResponse {
        data: first_page,
        next_cursor,
        ..
    } = timeout(DEFAULT_READ_TIMEOUT, mcp.read_response(list_id)).await??;
    assert_eq!(first_page, vec![expected[0].clone()]);
    assert_eq!(next_cursor, Some(expected[0].clone()));

    let list_id = mcp
        .send_thread_loaded_list_request(ThreadLoadedListParams {
            cursor: next_cursor,
            limit: Some(1),
            ancestor_thread_id: Some(root_thread_id),
        })
        .await?;
    let ThreadLoadedListResponse {
        data: second_page,
        next_cursor,
        ..
    } = timeout(DEFAULT_READ_TIMEOUT, mcp.read_response(list_id)).await??;
    assert_eq!(second_page, vec![expected[1].clone()]);
    assert_eq!(next_cursor, None);
    assert!(!first_page.contains(&unrelated_root_thread_id));
    assert!(!first_page.contains(&unrelated_child_thread_id));
    assert!(!second_page.contains(&unrelated_root_thread_id));
    assert!(!second_page.contains(&unrelated_child_thread_id));

    Ok(())
}

async fn start_thread(mcp: &mut TestAppServer) -> Result<String> {
    let req_id = mcp
        .send_thread_start_request_with_auto_env(ThreadStartParams {
            model: Some("gpt-5.2".to_string()),
            ..Default::default()
        })
        .await?;
    let ThreadStartResponse { thread, .. } =
        timeout(DEFAULT_READ_TIMEOUT, mcp.read_response(req_id)).await??;
    Ok(thread.id)
}
