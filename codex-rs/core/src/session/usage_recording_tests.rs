use super::*;

#[tokio::test]
async fn failed_optional_usage_projection_does_not_fail_response_recording() {
    let sqlite_home = tempfile::tempdir().expect("create isolated sqlite home");
    let sqlite = codex_state::SqliteConfig::new_for_testing(sqlite_home.path().abs());
    let state_db = codex_state::StateRuntime::init(sqlite, "openai".to_string())
        .await
        .expect("initialize state runtime");
    let (mut session, turn_context, _events) = make_session_and_context_with_rx().await;
    let source = SessionSource::Cli;
    let thread_source = ThreadSource::User;
    let usage_logger = codex_state::UsageLogger::try_new(
        &state_db,
        session.thread_id(),
        &source,
        Some(&thread_source),
        /*parent_thread_id*/ None,
        /*fork_parent_thread_id*/ None,
    )
    .await
    .expect("create usage logger")
    .expect("usage database should be available");
    let usage_pool = state_db
        .usage_ledger_pool()
        .expect("usage database should remain available");
    // Closing the optional pool makes its next projection fail without adding a
    // database driver dependency to core's test target.
    usage_pool.close().await;
    Arc::get_mut(&mut session)
        .expect("test session should have a single owner")
        .services
        .usage_logger = Some(usage_logger);

    let usage = TokenUsage {
        input_tokens: 12,
        cached_input_tokens: 4,
        output_tokens: 3,
        total_tokens: 15,
        ..TokenUsage::default()
    };
    session
        .record_response_token_usage(
            turn_context.as_ref(),
            "response-with-ledger-error",
            Some(&usage),
            Some(turn_context.model_info().slug.as_str()),
            codex_state::UsageCallStatus::Completed,
        )
        .await;

    pretty_assertions::assert_eq!(
        session
            .state
            .lock()
            .await
            .latest_token_usage_record
            .as_ref()
            .map(|record| record.response_id.as_str()),
        Some("response-with-ledger-error")
    );
    state_db.close().await;
}
