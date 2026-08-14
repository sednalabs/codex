#![allow(warnings, clippy::all)]

use super::*;
use crate::config::RolloutConfig;
use crate::mutation_authority::MutationAdmissionError;
use chrono::TimeZone;
use codex_protocol::SessionId;
use codex_protocol::ThreadId;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::AgentMessageEvent;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::HistoryPosition;
use codex_protocol::protocol::RolloutItem;
use codex_protocol::protocol::RolloutLine;
use codex_protocol::protocol::SandboxPolicy;
use codex_protocol::protocol::SessionMeta;
use codex_protocol::protocol::SessionMetaLine;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_protocol::protocol::ThreadSource;
use codex_protocol::protocol::TurnContextItem;
use codex_protocol::protocol::UserMessageEvent;
use codex_utils_absolute_path::test_support::PathExt;
use pretty_assertions::assert_eq;
use std::fs;
use std::fs::File;
use std::future::Future;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::pin::Pin;
use std::task::Poll;
use std::time::Duration;
use tempfile::TempDir;
use tokio::sync::Barrier;
use uuid::Uuid;

fn tracked_writer(future: impl Future<Output = ()> + Send + 'static) -> Arc<RolloutWriterTask> {
    let writer_task = Arc::new(RolloutWriterTask::new());
    writer_task.set_handle(tokio::spawn(future));
    writer_task
}

async fn assert_pending_once<F: Future>(mut future: Pin<&mut F>) {
    std::future::poll_fn(|cx| match future.as_mut().poll(cx) {
        Poll::Pending => Poll::Ready(()),
        Poll::Ready(_) => panic!("terminal command completed before its barrier opened"),
    })
    .await;
}

#[tokio::test]
async fn terminal_reservation_preserves_order_and_post_commit_cancel_fails_closed() {
    let (tx, mut rx) = mpsc::channel(1);
    tx.send(RolloutCmd::AddItems(Vec::new())).await.unwrap();
    let reader_gate = Arc::new(Barrier::new(2));
    let ack_gate = Arc::new(Barrier::new(2));
    let exit_gate = Arc::new(Barrier::new(2));
    let writer_task = tracked_writer({
        let reader_gate = Arc::clone(&reader_gate);
        let ack_gate = Arc::clone(&ack_gate);
        let exit_gate = Arc::clone(&exit_gate);
        async move {
            reader_gate.wait().await;
            assert!(matches!(rx.recv().await, Some(RolloutCmd::AddItems(_))));
            let Some(RolloutCmd::Shutdown { ack, .. }) = rx.recv().await else {
                panic!("shutdown must follow the command that consumed bounded capacity");
            };
            ack.send(Ok(())).unwrap();
            ack_gate.wait().await;
            exit_gate.wait().await;
        }
    });

    let mut attempt = Box::pin(writer_task.reserve_shutdown(&tx));
    assert_pending_once(attempt.as_mut()).await;
    reader_gate.wait().await;
    tokio::select! {
        _ = &mut attempt => panic!("shutdown completed before writer exit"),
        _ = ack_gate.wait() => {}
    }
    assert_pending_once(attempt.as_mut()).await;
    assert!(writer_task.command_admission.is_terminal());
    drop(attempt);
    assert!(matches!(
        writer_task.reserve_shutdown(&tx).await,
        TerminalCommandOutcome::Terminated(Err(_))
    ));
    exit_gate.wait().await;
}

#[tokio::test]
async fn cancelling_capacity_wait_leaves_admission_and_writer_active() {
    let (tx, mut rx) = mpsc::channel(1);
    tx.send(RolloutCmd::AddItems(Vec::new())).await.unwrap();
    let reader_gate = Arc::new(Barrier::new(2));
    let writer_task = tracked_writer({
        let reader_gate = Arc::clone(&reader_gate);
        async move {
            reader_gate.wait().await;
            assert!(matches!(rx.recv().await, Some(RolloutCmd::AddItems(_))));
        }
    });
    let mut attempt = Box::pin(writer_task.reserve_shutdown(&tx));
    assert_pending_once(attempt.as_mut()).await;
    drop(attempt);
    assert!(!writer_task.command_admission.is_terminal());
    reader_gate.wait().await;
    writer_task.take_handle().unwrap().wait().await.unwrap();
}

#[tokio::test]
async fn cancelling_committed_shutdown_reopens_after_retryable_writer_error() {
    let (tx, mut rx) = mpsc::channel(1);
    let first_command_received = Arc::new(tokio::sync::Notify::new());
    let release_retryable_error = Arc::new(tokio::sync::Notify::new());
    let writer_task = tracked_writer({
        let first_command_received = Arc::clone(&first_command_received);
        let release_retryable_error = Arc::clone(&release_retryable_error);
        async move {
            let Some(RolloutCmd::Shutdown { ack, continuing }) = rx.recv().await else {
                panic!("first shutdown command must be committed");
            };
            first_command_received.notify_one();
            release_retryable_error.notified().await;
            let _ = ack.send(Err(IoError::other("retryable terminal drain")));
            let _ = continuing.send(());

            let Some(RolloutCmd::Shutdown { ack, .. }) = rx.recv().await else {
                panic!("retry shutdown command must be admitted");
            };
            let _ = ack.send(Ok(()));
        }
    });

    let mut first_shutdown = Box::pin(writer_task.reserve_shutdown(&tx));
    tokio::select! {
        _ = &mut first_shutdown => panic!("shutdown completed before retryable drain result"),
        _ = first_command_received.notified() => {}
    }
    assert!(writer_task.command_admission.is_terminal());
    drop(first_shutdown);
    release_retryable_error.notify_one();

    tokio::time::timeout(Duration::from_secs(5), async {
        while writer_task.command_admission.is_terminal() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("owned terminal continuation should reopen admission");
    let retry = tokio::time::timeout(Duration::from_secs(5), writer_task.reserve_shutdown(&tx))
        .await
        .expect("retry shutdown should complete");
    assert!(matches!(retry, TerminalCommandOutcome::Terminated(Ok(()))));
}

async fn recorder_with_deferred_rollout(home: &Path) -> std::io::Result<RolloutRecorder> {
    RolloutRecorder::new(
        &test_config(home),
        RolloutRecorderParams::new(
            ThreadId::new(),
            /*forked_from_id*/ None,
            /*parent_thread_id*/ None,
            SessionSource::Exec,
            /*thread_source*/ None,
            "test_originator".to_string(),
            BaseInstructions::default(),
            Vec::new(),
        ),
    )
    .await
}

async fn wait_until_authority_is_closed(authority: &RolloutMutationAuthority) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match authority.admit() {
                Err(MutationAdmissionError::AdmissionClosed) => return,
                Err(MutationAdmissionError::CounterOverflow) => {
                    panic!("mutation admission counter overflow")
                }
                Ok(custody) => drop(custody),
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("mutation authority should close");
}

#[tokio::test]
async fn shutdown_waits_for_exact_admitted_write_and_blocks_stale_generation() {
    let home = TempDir::new().expect("temp dir");
    let recorder = recorder_with_deferred_rollout(home.path()).await.unwrap();
    let admitted_path = home.path().join("admitted-write.jsonl");
    File::create(&admitted_path).expect("create admitted-write target");
    let admitted_file = std::fs::OpenOptions::new()
        .append(true)
        .open(&admitted_path)
        .expect("open admitted-write target");
    let gate = JsonlWriteGate {
        entered: Arc::new(tokio::sync::Notify::new()),
        release: Arc::new(tokio::sync::Notify::new()),
    };
    let mut admitted_writer = JsonlWriter::new(
        tokio::fs::File::from_std(admitted_file),
        recorder.mutation_authority.clone(),
    );
    admitted_writer.write_gate = Some(gate.clone());
    let write = tokio::spawn(async move {
        admitted_writer
            .write_rollout_item(&agent_message_item("admitted write"), None)
            .await
    });
    gate.entered.notified().await;
    let shutdown = tokio::spawn({
        let recorder = recorder.clone();
        async move { recorder.shutdown().await }
    });

    wait_until_authority_is_closed(&recorder.mutation_authority).await;
    assert!(
        !shutdown.is_finished(),
        "shutdown completed before its exact admitted write drained"
    );
    write.abort();
    assert!(
        !shutdown.is_finished(),
        "cancelling the outer append released mutation custody"
    );
    gate.release.notify_one();
    shutdown.await.expect("join shutdown").expect("shutdown");
    assert!(
        fs::read_to_string(&admitted_path)
            .expect("read admitted write")
            .contains("admitted write"),
        "owned continuation must finish the admitted write after caller cancellation"
    );
    assert_eq!(
        recorder.mutation_authority.admit().err(),
        Some(MutationAdmissionError::AdmissionClosed)
    );
    let stale_path = home.path().join("stale-generation.jsonl");
    File::create(&stale_path).expect("create stale target");
    let stale_file = std::fs::OpenOptions::new()
        .append(true)
        .open(&stale_path)
        .expect("open stale target");
    let mut stale_writer = JsonlWriter::new(
        tokio::fs::File::from_std(stale_file),
        recorder.mutation_authority.clone(),
    );
    stale_writer
        .write_rollout_item(&agent_message_item("stale write"), None)
        .await
        .expect_err("revoked generation must reject actual filesystem append");
    assert!(fs::read(&stale_path).expect("read stale target").is_empty());
    recorder
        .record_canonical_items(&[agent_message_item("stale command")])
        .await
        .expect_err("terminal recorder must reject stale-generation append");
}

#[tokio::test]
async fn cancelled_shutdown_keeps_revocation_owned_until_admitted_write_drains() {
    let home = TempDir::new().expect("temp dir");
    let recorder = recorder_with_deferred_rollout(home.path()).await.unwrap();
    let custody = recorder
        .mutation_authority
        .admit()
        .expect("admit simulated filesystem write");
    let shutdown = tokio::spawn({
        let recorder = recorder.clone();
        async move { recorder.shutdown().await }
    });
    wait_until_authority_is_closed(&recorder.mutation_authority).await;
    shutdown.abort();
    drop(custody);

    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if recorder.tx.is_closed() {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("writer should finish after admitted write drains");
    assert_eq!(
        recorder.mutation_authority.admit().err(),
        Some(MutationAdmissionError::AdmissionClosed)
    );
}

#[tokio::test]
async fn failed_precommit_open_releases_mutation_custody_for_retry() {
    let home = TempDir::new().expect("temp dir");
    let authority = RolloutMutationAuthority::new();
    let blocker = home.path().join("not-a-directory");
    File::create(&blocker).expect("create parent blocker");
    open_log_file_with_authority(&blocker.join("rollout.jsonl"), authority.clone())
        .expect_err("blocked parent must fail before writer commit");
    drop(authority.admit().expect("failed open must release custody"));
}

#[tokio::test]
async fn failed_async_write_releases_mutation_custody_for_retry() {
    let home = TempDir::new().expect("temp dir");
    let rollout_path = home.path().join("rollout.jsonl");
    File::create(&rollout_path).expect("create rollout");
    let file = std::fs::OpenOptions::new()
        .read(true)
        .open(&rollout_path)
        .expect("open read-only rollout");
    let authority = RolloutMutationAuthority::new();
    let mut writer = JsonlWriter::new(tokio::fs::File::from_std(file), authority.clone());
    writer
        .write_rollout_item(&agent_message_item("write failure"), None)
        .await
        .expect_err("read-only writer must fail");
    drop(
        authority
            .admit()
            .expect("failed write must release custody"),
    );
}

#[tokio::test]
async fn partial_line_failure_rolls_back_before_retry_and_reports_one_commit() {
    assert_scripted_line_failure_is_retried_once(JsonlWriteFault::PartialWriteThenError(17)).await;
}

#[tokio::test]
async fn flush_failure_rolls_back_complete_line_before_retry_and_reports_one_commit() {
    assert_scripted_line_failure_is_retried_once(JsonlWriteFault::FlushErrorAfterWrite).await;
}

async fn assert_scripted_line_failure_is_retried_once(write_fault: JsonlWriteFault) {
    let home = TempDir::new().expect("temp dir");
    let rollout_path = home.path().join("rollout.jsonl");
    let file = std::fs::OpenOptions::new()
        .read(true)
        .append(true)
        .create(true)
        .open(&rollout_path)
        .expect("open rollout");
    let mutation_authority = RolloutMutationAuthority::new();
    let mut writer = JsonlWriter::new(tokio::fs::File::from_std(file), mutation_authority.clone());
    writer.write_fault = Some(write_fault);
    let mut state = RolloutWriterState {
        writer: Some(writer),
        deferred_log_file_info: None,
        pending_items: Vec::new(),
        meta: None,
        cwd: home.path().to_path_buf(),
        rollout_path: rollout_path.clone(),
        ordinal_state: RolloutOrdinalState::Legacy,
        last_logged_error: None,
        repair_offset: None,
        mutation_authority,
    };

    let marker = "transactional-line-marker";
    let (result, committed) = state
        .append_items_committed(vec![agent_message_item(marker)])
        .await;
    result.expect("reopened retry should succeed");
    assert_eq!(committed, 1);
    drop(state);

    let text = fs::read_to_string(rollout_path).expect("read rollout");
    assert_eq!(
        text.matches(marker).count(),
        1,
        "item must not be duplicated"
    );
    assert_eq!(text.lines().count(), 1, "partial JSONL must be removed");
    serde_json::from_str::<serde_json::Value>(&text).expect("line must remain valid JSON");
}

#[tokio::test]
async fn failed_shutdown_drain_keeps_authority_open_until_successful_retry() {
    let home = TempDir::new().expect("temp dir");
    let blocker = home.path().join("not-a-directory");
    File::create(&blocker).expect("create parent blocker");
    let rollout_path = blocker.join("rollout.jsonl");
    let read_only_path = home.path().join("read-only.jsonl");
    File::create(&read_only_path).expect("create read-only rollout");
    let read_only_file = std::fs::OpenOptions::new()
        .read(true)
        .open(&read_only_path)
        .expect("open read-only rollout");
    let mutation_authority = RolloutMutationAuthority::new();
    let mut state = RolloutWriterState {
        writer: Some(JsonlWriter::new(
            tokio::fs::File::from_std(read_only_file),
            mutation_authority.clone(),
        )),
        deferred_log_file_info: None,
        pending_items: vec![agent_message_item("retry terminal drain")],
        meta: None,
        cwd: home.path().to_path_buf(),
        rollout_path: rollout_path.clone(),
        ordinal_state: RolloutOrdinalState::Legacy,
        last_logged_error: None,
        repair_offset: None,
        mutation_authority: mutation_authority.clone(),
    };

    state
        .shutdown()
        .await
        .expect_err("write and reopen failures must leave shutdown retryable");
    drop(
        mutation_authority
            .admit()
            .expect("failed terminal drain must keep authority open"),
    );

    fs::remove_file(&blocker).expect("remove parent blocker");
    fs::create_dir(&blocker).expect("replace blocker with directory");
    state.shutdown().await.expect("retry terminal drain");
    assert_eq!(
        mutation_authority.admit().err(),
        Some(MutationAdmissionError::AdmissionClosed)
    );
    assert!(
        fs::read_to_string(&rollout_path)
            .expect("read retried terminal drain")
            .contains("retry terminal drain")
    );
}

fn test_config(codex_home: &Path) -> RolloutConfig {
    RolloutConfig {
        codex_home: codex_home.to_path_buf(),
        sqlite: codex_state::SqliteConfig::new_for_testing(codex_home.abs()),
        cwd: codex_home.to_path_buf(),
        model_provider_id: "test-provider".to_string(),
        generate_memories: true,
    }
}

fn paginated_session_meta_item(thread_id: ThreadId, cwd: &Path) -> RolloutItem {
    RolloutItem::SessionMeta(SessionMetaLine {
        meta: SessionMeta {
            session_id: thread_id.into(),
            id: thread_id,
            timestamp: "2026-07-09T00:00:00Z".to_string(),
            cwd: cwd.to_path_buf(),
            originator: "test".to_string(),
            cli_version: "test".to_string(),
            source: SessionSource::Exec,
            history_mode: ThreadHistoryMode::Paginated,
            ..SessionMeta::default()
        },
        git: None,
    })
}

fn agent_message_item(message: &str) -> RolloutItem {
    RolloutItem::EventMsg(EventMsg::AgentMessage(AgentMessageEvent {
        message: message.to_string(),
        phase: None,
        memory_citation: None,
    }))
}

fn write_paginated_rollout(
    path: &Path,
    thread_id: ThreadId,
    subsequent_ordinals: &[u64],
) -> std::io::Result<()> {
    let mut records = vec![RolloutLine {
        timestamp: "2026-07-09T00:00:00Z".to_string(),
        ordinal: Some(0),
        item: paginated_session_meta_item(thread_id, path.parent().unwrap_or(path)),
    }];
    records.extend(
        subsequent_ordinals
            .iter()
            .enumerate()
            .map(|(index, ordinal)| RolloutLine {
                timestamp: format!("2026-07-09T00:00:{:02}Z", index + 1),
                ordinal: Some(*ordinal),
                item: agent_message_item(format!("message-{index}").as_str()),
            }),
    );
    let jsonl = records
        .iter()
        .map(serde_json::to_string)
        .collect::<Result<Vec<_>, _>>()?
        .join("\n");
    fs::write(path, format!("{jsonl}\n"))
}

fn read_rollout_lines(path: &Path) -> std::io::Result<Vec<RolloutLine>> {
    fs::read_to_string(path)?
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).map_err(std::io::Error::other))
        .collect()
}

fn write_session_file(root: &Path, ts: &str, uuid: Uuid) -> std::io::Result<PathBuf> {
    let day_dir = root.join("sessions/2025/01/03");
    fs::create_dir_all(&day_dir)?;
    let path = day_dir.join(format!("rollout-{ts}-{uuid}.jsonl"));
    let mut file = File::create(&path)?;
    let meta = serde_json::json!({
        "timestamp": ts,
        "type": "session_meta",
        "payload": {
            "session_id": uuid,
            "id": uuid,
            "timestamp": ts,
            "cwd": ".",
            "originator": "test_originator",
            "cli_version": "test_version",
            "source": "cli",
            "model_provider": "test-provider",
        },
    });
    writeln!(file, "{meta}")?;
    let user_event = serde_json::json!({
        "timestamp": ts,
        "type": "event_msg",
        "payload": {
            "type": "user_message",
            "message": "Hello from user",
            "kind": "plain",
        },
    });
    writeln!(file, "{user_event}")?;
    Ok(path)
}

#[test]
fn append_repair_terminates_nonempty_rollout_tail() -> std::io::Result<()> {
    let home = TempDir::new().expect("temp dir");
    let rollout_path = home.path().join("rollout.jsonl");
    fs::write(&rollout_path, b"{\"type\":\"event_msg\"}")?;
    drop(open_log_file(&rollout_path)?);
    drop(open_log_file(&rollout_path)?);

    assert_eq!(fs::read(&rollout_path)?, b"{\"type\":\"event_msg\"}\n");
    Ok(())
}

#[tokio::test]
async fn opening_existing_rollout_preserves_modified_time() -> std::io::Result<()> {
    let home = TempDir::new().expect("temp dir");
    let rollout_path = home.path().join("rollout.jsonl");
    write_paginated_rollout(&rollout_path, ThreadId::default(), &[])?;
    let modified = std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    File::options()
        .write(true)
        .open(&rollout_path)?
        .set_times(std::fs::FileTimes::new().set_modified(modified))?;

    drop(open_log_file(&rollout_path)?);
    assert_eq!(fs::metadata(&rollout_path)?.modified()?, modified);

    drop(
        open_rollout_for_append_with_authority(&rollout_path, RolloutMutationAuthority::new())
            .await?,
    );
    assert_eq!(fs::metadata(&rollout_path)?.modified()?, modified);
    Ok(())
}

#[tokio::test]
async fn state_db_init_backfills_before_returning() -> anyhow::Result<()> {
    let home = TempDir::new().expect("temp dir");
    let uuid = Uuid::new_v4();
    let thread_id = ThreadId::from_string(&uuid.to_string())?;
    let rollout_path = home.path().join(format!(
        "sessions/2026/01/27/rollout-2026-01-27T12-34-56-{uuid}.jsonl"
    ));
    let parent = rollout_path
        .parent()
        .expect("rollout path should have parent");
    fs::create_dir_all(parent)?;

    let session_meta_line = SessionMetaLine {
        meta: SessionMeta {
            session_id: thread_id.into(),
            id: thread_id,
            forked_from_id: None,
            parent_thread_id: None,
            timestamp: "2026-01-27T12:34:56Z".to_string(),
            cwd: home.path().to_path_buf(),
            originator: "test".to_string(),
            cli_version: "test".to_string(),
            source: SessionSource::Cli,
            thread_source: None,
            agent_path: None,
            agent_nickname: None,
            agent_role: None,
            model_provider: None,
            base_instructions: None,
            dynamic_tools: None,
            selected_capability_roots: Vec::new(),
            memory_mode: None,
            history_mode: Default::default(),
            history_base: None,
            subagent_history_start_ordinal: None,
            multi_agent_version: None,
            context_window: None,
        },
        git: None,
    };
    let lines = [
        RolloutLine {
            timestamp: "2026-01-27T12:34:56Z".to_string(),
            ordinal: None,
            item: RolloutItem::SessionMeta(session_meta_line),
        },
        RolloutLine {
            timestamp: "2026-01-27T12:34:57Z".to_string(),
            ordinal: None,
            item: RolloutItem::EventMsg(EventMsg::UserMessage(UserMessageEvent {
                client_id: None,
                message: "hello from startup backfill".to_string(),
                images: None,
                local_images: Vec::new(),
                text_elements: Vec::new(),
                ..Default::default()
            })),
        },
    ];
    let jsonl = lines
        .iter()
        .map(serde_json::to_string)
        .collect::<Result<Vec<_>, _>>()?
        .join("\n");
    fs::write(&rollout_path, format!("{jsonl}\n"))?;

    let runtime = crate::state_db::init(&test_config(home.path()))
        .await
        .expect("state db should initialize");

    let metadata = runtime
        .get_thread(thread_id)
        .await?
        .expect("thread should be backfilled before init returns");
    assert_eq!(metadata.rollout_path, rollout_path);
    assert_eq!(
        runtime.get_backfill_state().await?.status,
        codex_state::BackfillStatus::Complete
    );

    Ok(())
}

#[tokio::test]
async fn load_rollout_items_defaults_legacy_session_id() -> std::io::Result<()> {
    let home = TempDir::new().expect("temp dir");
    let rollout_path = home.path().join("rollout.jsonl");
    let mut file = File::create(&rollout_path)?;
    let thread_id = ThreadId::new();
    let ts = "2025-01-03T12:00:00Z";

    writeln!(
        file,
        "{}",
        serde_json::json!({
            "timestamp": ts,
            "type": "session_meta",
            "payload": {
                "id": thread_id,
                "timestamp": ts,
                "cwd": ".",
                "originator": "test_originator",
                "cli_version": "test_version",
                "source": "cli",
                "model_provider": "test-provider",
            },
        })
    )?;
    writeln!(
        file,
        "{}",
        serde_json::json!({
            "timestamp": ts,
            "type": "response_item",
            "payload": {
                "type": "ghost_snapshot",
                "ghost_commit": {
                    "id": "deadbeef",
                    "preexisting_untracked_dirs": [],
                    "preexisting_untracked_files": [],
                },
            },
        })
    )?;
    writeln!(
        file,
        "{}",
        serde_json::json!({
            "timestamp": ts,
            "type": "response_item",
            "payload": {
                "type": "message",
                "role": "assistant",
                "content": [
                    {
                        "type": "output_text",
                        "text": "hello",
                    }
                ],
            },
        })
    )?;

    let (items, loaded_thread_id, parse_errors) =
        RolloutRecorder::load_rollout_items(&rollout_path).await?;

    assert_eq!(loaded_thread_id, Some(thread_id));
    assert_eq!(parse_errors, 0);
    assert_eq!(items.len(), 2);
    let RolloutItem::SessionMeta(session_meta) = &items[0] else {
        panic!("expected session metadata");
    };
    assert_eq!(session_meta.meta.session_id, SessionId::from(thread_id));
    assert!(matches!(
        items[1],
        RolloutItem::ResponseItem(ResponseItem::Message { .. })
    ));

    Ok(())
}

#[tokio::test]
async fn load_rollout_items_ignores_unknown_fork_source_history_mode() -> std::io::Result<()> {
    let home = TempDir::new().expect("temp dir");
    let uuid = Uuid::new_v4();
    let thread_id = ThreadId::from_string(&uuid.to_string()).expect("thread id");
    let rollout_path = write_session_file(home.path(), "2025-01-03T12-00-00", uuid)?;
    let mut file = fs::OpenOptions::new().append(true).open(&rollout_path)?;
    let source_uuid = Uuid::new_v4();
    writeln!(
        file,
        "{}",
        serde_json::json!({
            "timestamp": "2025-01-03T12:00:01Z",
            "type": "session_meta",
            "payload": {
                "session_id": source_uuid,
                "id": source_uuid,
                "timestamp": "2025-01-03T12:00:01Z",
                "cwd": ".",
                "originator": "test_originator",
                "cli_version": "test_version",
                "source": "cli",
                "model_provider": "test-provider",
                "history_mode": "future",
            },
        })
    )?;

    let (items, loaded_thread_id, parse_errors) =
        RolloutRecorder::load_rollout_items(&rollout_path).await?;

    assert_eq!(loaded_thread_id, Some(thread_id));
    assert_eq!(parse_errors, 1);
    assert_eq!(items.len(), 2);
    Ok(())
}

#[tokio::test]
async fn load_rollout_items_preserves_legacy_guardian_assessment_lines() -> std::io::Result<()> {
    let home = TempDir::new().expect("temp dir");
    let rollout_path = home.path().join("rollout.jsonl");
    let mut file = File::create(&rollout_path)?;
    let thread_id = ThreadId::new();
    let ts = "2025-01-03T12:00:00Z";

    writeln!(
        file,
        "{}",
        serde_json::json!({
            "timestamp": ts,
            "type": "session_meta",
            "payload": {
                "session_id": thread_id,
                "id": thread_id,
                "timestamp": ts,
                "cwd": ".",
                "originator": "test_originator",
                "cli_version": "test_version",
                "source": "cli",
                "model_provider": "test-provider",
            },
        })
    )?;
    writeln!(
        file,
        "{}",
        serde_json::json!({
            "timestamp": ts,
            "type": "event_msg",
            "payload": {
                "type": "guardian_assessment",
                "id": "guardian-1",
                "turn_id": "turn-1",
                "status": "in_progress",
                "action": {
                    "type": "command",
                    "source": "shell",
                    "command": "rm -rf /tmp/guardian",
                    "cwd": if cfg!(windows) { r"C:\tmp" } else { "/tmp" },
                },
            },
        })
    )?;

    let (items, loaded_thread_id, parse_errors) =
        RolloutRecorder::load_rollout_items(&rollout_path).await?;

    assert_eq!(loaded_thread_id, Some(thread_id));
    assert_eq!(parse_errors, 0);
    assert_eq!(items.len(), 2);
    let RolloutItem::EventMsg(EventMsg::GuardianAssessment(assessment)) = &items[1] else {
        panic!("expected guardian assessment rollout item");
    };
    assert_eq!(assessment.id, "guardian-1");
    assert_eq!(assessment.turn_id, "turn-1");
    assert_eq!(assessment.started_at_ms, 0);

    Ok(())
}

#[tokio::test]
async fn load_rollout_items_filters_legacy_ghost_snapshots_from_compaction_history()
-> std::io::Result<()> {
    let home = TempDir::new().expect("temp dir");
    let rollout_path = home.path().join("rollout.jsonl");
    let mut file = File::create(&rollout_path)?;
    let thread_id = ThreadId::new();
    let ts = "2025-01-03T12:00:00Z";

    writeln!(
        file,
        "{}",
        serde_json::json!({
            "timestamp": ts,
            "type": "session_meta",
            "payload": {
                "session_id": thread_id,
                "id": thread_id,
                "timestamp": ts,
                "cwd": ".",
                "originator": "test_originator",
                "cli_version": "test_version",
                "source": "cli",
                "model_provider": "test-provider",
            },
        })
    )?;
    writeln!(
        file,
        "{}",
        serde_json::json!({
            "timestamp": ts,
            "type": "compacted",
            "payload": {
                "message": "summary",
                "replacement_history": [
                    {
                        "type": "message",
                        "role": "assistant",
                        "content": [
                            {
                                "type": "output_text",
                                "text": "kept",
                            }
                        ],
                    },
                    {
                        "type": "ghost_snapshot",
                        "ghost_commit": {
                            "id": "deadbeef",
                            "preexisting_untracked_dirs": [],
                            "preexisting_untracked_files": [],
                        },
                    }
                ],
            },
        })
    )?;

    let (items, loaded_thread_id, parse_errors) =
        RolloutRecorder::load_rollout_items(&rollout_path).await?;

    assert_eq!(loaded_thread_id, Some(thread_id));
    assert_eq!(parse_errors, 0);
    assert_eq!(items.len(), 2);
    let RolloutItem::Compacted(compacted) = &items[1] else {
        panic!("expected compacted rollout item");
    };
    let replacement_history = compacted
        .replacement_history
        .as_ref()
        .expect("replacement history");
    assert_eq!(replacement_history.len(), 1);
    assert!(matches!(
        &replacement_history[0],
        ResponseItem::Message { .. }
    ));

    Ok(())
}

#[tokio::test]
async fn recorder_materializes_on_flush_with_pending_items() -> std::io::Result<()> {
    let home = TempDir::new().expect("temp dir");
    let config = test_config(home.path());
    let session_id = SessionId::default();
    let thread_id = ThreadId::new();
    let initial_window_id = Uuid::now_v7().to_string();
    let recorder = RolloutRecorder::new(
        &config,
        RolloutRecorderParams::new(
            thread_id,
            /*forked_from_id*/ None,
            /*parent_thread_id*/ None,
            SessionSource::Exec,
            /*thread_source*/ None,
            "test_originator".to_string(),
            BaseInstructions::default(),
            Vec::new(),
        )
        .with_session_id(session_id)
        .with_history_mode(ThreadHistoryMode::Paginated)
        .with_initial_window_id(initial_window_id.clone()),
    )
    .await?;

    let rollout_path = recorder.rollout_path().to_path_buf();
    assert!(
        !rollout_path.exists(),
        "rollout file should not exist before the first recordable item"
    );

    recorder
        .record_canonical_items(&[RolloutItem::EventMsg(EventMsg::AgentMessage(
            AgentMessageEvent {
                message: "buffered-event".to_string(),
                phase: None,
                memory_citation: None,
            },
        ))])
        .await?;
    recorder.flush().await?;
    assert!(
        rollout_path.exists(),
        "flush with pending items should materialize the rollout"
    );

    recorder
        .record_canonical_items(&[RolloutItem::EventMsg(EventMsg::UserMessage(
            UserMessageEvent {
                client_id: None,
                message: "first-user-message".to_string(),
                images: None,
                local_images: Vec::new(),
                text_elements: Vec::new(),
                ..Default::default()
            },
        ))])
        .await?;
    recorder.flush().await?;

    recorder.persist().await?;
    // Second call verifies `persist()` is idempotent after materialization.
    recorder.persist().await?;
    assert!(rollout_path.exists(), "rollout file should be materialized");

    let text = std::fs::read_to_string(&rollout_path)?;
    let lines = read_rollout_lines(&rollout_path)?;
    assert_eq!(
        lines.iter().map(|line| line.ordinal).collect::<Vec<_>>(),
        vec![Some(0), Some(1), Some(2)]
    );
    let first_line = text.lines().next().expect("session metadata line");
    let session_meta: RolloutLine = serde_json::from_str(first_line)?;
    let RolloutItem::SessionMeta(session_meta) = session_meta.item else {
        panic!("expected session metadata in rollout");
    };
    assert_eq!(session_meta.meta.session_id, session_id);
    assert_eq!(session_meta.meta.history_mode, ThreadHistoryMode::Paginated);
    assert_eq!(
        session_meta
            .meta
            .context_window
            .map(|window| window.window_id),
        Some(initial_window_id)
    );
    let buffered_idx = text
        .find("buffered-event")
        .expect("buffered event in rollout");
    let user_idx = text
        .find("first-user-message")
        .expect("first user message in rollout");
    assert!(
        buffered_idx < user_idx,
        "buffered items should preserve ordering"
    );
    let text_after_second_persist = std::fs::read_to_string(&rollout_path)?;
    assert_eq!(text_after_second_persist, text);

    recorder.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn referenced_paginated_rollout_starts_at_history_cutoff_and_resumes() -> std::io::Result<()>
{
    let home = TempDir::new().expect("temp dir");
    let config = test_config(home.path());
    let history_base = HistoryPosition {
        thread_id: ThreadId::new(),
        end_ordinal_exclusive: 41,
        end_byte_offset: 1,
    };
    let recorder = RolloutRecorder::new(
        &config,
        RolloutRecorderParams::new(
            ThreadId::new(),
            /*forked_from_id*/ None,
            /*parent_thread_id*/ None,
            SessionSource::Exec,
            /*thread_source*/ None,
            "test_originator".to_string(),
            BaseInstructions::default(),
            Vec::new(),
        )
        .with_history_mode(ThreadHistoryMode::Paginated)
        .with_history_base(Some(history_base)),
    )
    .await?;
    let rollout_path = recorder.rollout_path().to_path_buf();
    recorder.persist().await?;
    recorder.shutdown().await?;

    let resumed =
        RolloutRecorder::new(&config, RolloutRecorderParams::resume(rollout_path.clone())).await?;
    resumed
        .record_canonical_items(&[agent_message_item("first child record")])
        .await?;
    resumed.flush().await?;
    resumed.shutdown().await?;

    let resumed =
        RolloutRecorder::new(&config, RolloutRecorderParams::resume(rollout_path.clone())).await?;
    resumed
        .record_canonical_items(&[agent_message_item("second child record")])
        .await?;
    resumed.flush().await?;
    assert_eq!(
        read_rollout_lines(&rollout_path)?
            .into_iter()
            .map(|line| line.ordinal)
            .collect::<Vec<_>>(),
        vec![Some(41), Some(42), Some(43)]
    );
    resumed.shutdown().await
}

#[tokio::test]
async fn recorder_omits_ordinals_from_legacy_rollouts() -> std::io::Result<()> {
    let home = TempDir::new().expect("temp dir");
    let config = test_config(home.path());
    let recorder = RolloutRecorder::new(
        &config,
        RolloutRecorderParams::new(
            ThreadId::new(),
            /*forked_from_id*/ None,
            /*parent_thread_id*/ None,
            SessionSource::Exec,
            /*thread_source*/ None,
            "test_originator".to_string(),
            BaseInstructions::default(),
            Vec::new(),
        ),
    )
    .await?;
    recorder
        .record_canonical_items(&[agent_message_item("legacy")])
        .await?;
    recorder.flush().await?;

    let text = fs::read_to_string(recorder.rollout_path())?;
    let values = text
        .lines()
        .map(serde_json::from_str::<serde_json::Value>)
        .collect::<Result<Vec<_>, _>>()?;
    assert!(values.iter().all(|value| value.get("ordinal").is_none()));

    recorder.shutdown().await
}

#[tokio::test]
async fn resumed_empty_rollout_omits_ordinals() -> std::io::Result<()> {
    let home = TempDir::new().expect("temp dir");
    let config = test_config(home.path());
    let rollout_path = home.path().join("rollout.jsonl");
    File::create(&rollout_path)?;

    let recorder =
        RolloutRecorder::new(&config, RolloutRecorderParams::resume(rollout_path.clone())).await?;
    recorder
        .record_canonical_items(&[agent_message_item("legacy")])
        .await?;
    recorder.flush().await?;

    let text = fs::read_to_string(rollout_path)?;
    let values = text
        .lines()
        .map(serde_json::from_str::<serde_json::Value>)
        .collect::<Result<Vec<_>, _>>()?;
    assert!(values.iter().all(|value| value.get("ordinal").is_none()));

    recorder.shutdown().await
}

#[tokio::test]
async fn persist_reports_filesystem_error_and_retries_buffered_items() -> std::io::Result<()> {
    let home = TempDir::new().expect("temp dir");
    let config = test_config(home.path());
    let thread_id = ThreadId::new();
    let recorder = RolloutRecorder::new(
        &config,
        RolloutRecorderParams::new(
            thread_id,
            /*forked_from_id*/ None,
            /*parent_thread_id*/ None,
            SessionSource::Exec,
            /*thread_source*/ None,
            "test_originator".to_string(),
            BaseInstructions::default(),
            Vec::new(),
        ),
    )
    .await?;
    let rollout_path = recorder.rollout_path().to_path_buf();

    recorder
        .record_canonical_items(&[RolloutItem::EventMsg(EventMsg::AgentMessage(
            AgentMessageEvent {
                message: "buffered-before-persist".to_string(),
                phase: None,
                memory_citation: None,
            },
        ))])
        .await?;
    let sessions_blocker_path = home.path().join("sessions");
    File::create(&sessions_blocker_path)?;

    let err = recorder
        .persist()
        .await
        .expect_err("blocked sessions directory should fail persist");
    assert_ne!(err.kind(), std::io::ErrorKind::Interrupted);
    assert!(
        !rollout_path.exists(),
        "failed persist should keep the rollout deferred"
    );

    fs::remove_file(sessions_blocker_path)?;
    recorder.flush().await?;
    let text = std::fs::read_to_string(&rollout_path)?;
    assert!(
        text.contains("buffered-before-persist"),
        "retry should preserve items buffered before the failed persist"
    );

    recorder.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn writer_state_retries_write_error_before_reporting_flush_success() -> std::io::Result<()> {
    let home = TempDir::new().expect("temp dir");
    let rollout_path = home.path().join("rollout.jsonl");
    File::create(&rollout_path)?;
    let read_only_file = std::fs::OpenOptions::new().read(true).open(&rollout_path)?;
    let mutation_authority = RolloutMutationAuthority::new();
    let mut state = RolloutWriterState {
        writer: Some(JsonlWriter::new(
            tokio::fs::File::from_std(read_only_file),
            mutation_authority.clone(),
        )),
        deferred_log_file_info: None,
        pending_items: Vec::new(),
        meta: None,
        cwd: home.path().to_path_buf(),
        rollout_path: rollout_path.clone(),
        ordinal_state: RolloutOrdinalState::Legacy,
        last_logged_error: None,
        repair_offset: None,
        mutation_authority,
    };
    state.add_items(vec![RolloutItem::EventMsg(EventMsg::AgentMessage(
        AgentMessageEvent {
            message: "queued-after-writer-error".to_string(),
            phase: None,
            memory_citation: None,
        },
    ))]);

    state.flush().await?;
    let text_after_retry = std::fs::read_to_string(&rollout_path)?;
    assert!(
        text_after_retry.contains("queued-after-writer-error"),
        "flush should retry after reopening and write buffered items"
    );
    Ok(())
}

#[tokio::test]
async fn resumed_paginated_rollout_continues_after_ordinal_gap() -> std::io::Result<()> {
    let home = TempDir::new().expect("temp dir");
    let config = test_config(home.path());
    let rollout_path = home.path().join("rollout.jsonl");
    write_paginated_rollout(&rollout_path, ThreadId::new(), &[4])?;

    let recorder =
        RolloutRecorder::new(&config, RolloutRecorderParams::resume(rollout_path.clone())).await?;
    recorder
        .record_canonical_items(&[agent_message_item("after-resume")])
        .await?;
    recorder.flush().await?;

    let lines = read_rollout_lines(&rollout_path)?;
    assert_eq!(
        lines.iter().map(|line| line.ordinal).collect::<Vec<_>>(),
        vec![Some(0), Some(4), Some(5)]
    );
    recorder.shutdown().await
}

#[tokio::test]
async fn resumed_paginated_rollout_repairs_unsafe_tail() -> std::io::Result<()> {
    let valid_unterminated = serde_json::to_string(&RolloutLine {
        timestamp: "2026-07-09T00:00:05Z".to_string(),
        ordinal: Some(5),
        item: agent_message_item("valid unterminated"),
    })?;
    for (name, tail, expected_ordinals) in [
        (
            "valid unterminated",
            valid_unterminated,
            vec![Some(0), Some(4), Some(5), Some(6)],
        ),
        (
            "invalid unterminated",
            "{\"timestamp\":\"unterminated\"".to_string(),
            vec![Some(0), Some(4), Some(5)],
        ),
    ] {
        let home = TempDir::new().expect("temp dir");
        let config = test_config(home.path());
        let rollout_path = home.path().join("rollout.jsonl");
        write_paginated_rollout(&rollout_path, ThreadId::new(), &[4])?;
        let mut file = fs::OpenOptions::new().append(true).open(&rollout_path)?;
        write!(file, "{tail}")?;
        drop(file);

        let recorder =
            RolloutRecorder::new(&config, RolloutRecorderParams::resume(rollout_path.clone()))
                .await?;
        recorder
            .record_canonical_items(&[agent_message_item("after-tail-repair")])
            .await?;
        recorder.flush().await?;

        let contents = fs::read_to_string(&rollout_path)?;
        assert!(contents.ends_with('\n'), "{name} tail should be terminated");
        let ordinals = contents
            .lines()
            .filter_map(|line| serde_json::from_str::<RolloutLine>(line).ok())
            .map(|line| line.ordinal)
            .collect::<Vec<_>>();
        assert_eq!(
            ordinals, expected_ordinals,
            "unexpected ordinals after repairing {name} tail"
        );
        recorder.shutdown().await?;
    }
    Ok(())
}

#[tokio::test]
async fn paginated_ordinal_overflow_fails_without_appending() -> std::io::Result<()> {
    let home = TempDir::new().expect("temp dir");
    let config = test_config(home.path());
    let rollout_path = home.path().join("rollout.jsonl");
    write_paginated_rollout(&rollout_path, ThreadId::new(), &[u64::MAX])?;
    let before = fs::read(&rollout_path)?;

    let recorder =
        RolloutRecorder::new(&config, RolloutRecorderParams::resume(rollout_path.clone())).await?;
    recorder
        .record_canonical_items(&[agent_message_item("overflow")])
        .await?;
    let err = recorder
        .flush()
        .await
        .expect_err("ordinal overflow should fail the append");
    assert!(err.to_string().contains("overflow"));
    assert_eq!(fs::read(&rollout_path)?, before);
    Ok(())
}

#[tokio::test]
async fn resumed_paginated_subagent_rollout_rejects_incomplete_prefix() -> std::io::Result<()> {
    let home = TempDir::new().expect("temp dir");
    let config = test_config(home.path());
    let rollout_path = home.path().join("rollout.jsonl");
    let thread_id = ThreadId::new();
    let mut session_meta = paginated_session_meta_item(thread_id, home.path());
    let RolloutItem::SessionMeta(meta_line) = &mut session_meta else {
        panic!("fixture should be session metadata");
    };
    meta_line.meta.subagent_history_start_ordinal = Some(3);
    let lines = [
        RolloutLine {
            timestamp: "2026-07-09T00:00:00Z".to_string(),
            ordinal: Some(0),
            item: session_meta,
        },
        RolloutLine {
            timestamp: "2026-07-09T00:00:01Z".to_string(),
            ordinal: Some(1),
            item: agent_message_item("partial inherited prefix"),
        },
    ];
    let jsonl = lines
        .iter()
        .map(serde_json::to_string)
        .collect::<Result<Vec<_>, _>>()?
        .join("\n");
    fs::write(&rollout_path, format!("{jsonl}\n"))?;

    let err = match RolloutRecorder::new(&config, RolloutRecorderParams::resume(rollout_path)).await
    {
        Ok(_) => panic!("incomplete prefix should fail resume"),
        Err(err) => err,
    };

    assert!(err.to_string().contains("incomplete"));
    Ok(())
}

#[tokio::test]
async fn append_rollout_item_to_path_assigns_next_paginated_ordinal() -> std::io::Result<()> {
    let home = TempDir::new().expect("temp dir");
    let rollout_path = home.path().join("rollout.jsonl");
    write_paginated_rollout(&rollout_path, ThreadId::new(), &[4])?;

    append_rollout_item_to_path(&rollout_path, &agent_message_item("offline")).await?;

    let lines = read_rollout_lines(&rollout_path)?;
    assert_eq!(lines.last().and_then(|line| line.ordinal), Some(5));
    Ok(())
}

#[tokio::test]
async fn list_threads_db_disabled_does_not_skip_paginated_items() -> std::io::Result<()> {
    let home = TempDir::new().expect("temp dir");
    let config = test_config(home.path());

    let newest = write_session_file(home.path(), "2025-01-03T12-00-00", Uuid::from_u128(9001))?;
    let middle = write_session_file(home.path(), "2025-01-02T12-00-00", Uuid::from_u128(9002))?;
    let _oldest = write_session_file(home.path(), "2025-01-01T12-00-00", Uuid::from_u128(9003))?;

    let default_provider = config.model_provider_id.clone();
    let page1 = RolloutRecorder::list_threads(
        /*state_db_ctx*/ None,
        &config,
        /*page_size*/ 1,
        /*cursor*/ None,
        ThreadSortKey::CreatedAt,
        SortDirection::Desc,
        &[],
        /*model_providers*/ None,
        /*cwd_filters*/ None,
        default_provider.as_str(),
        /*search_term*/ None,
    )
    .await?;
    assert_eq!(page1.items.len(), 1);
    assert_eq!(page1.items[0].path, newest);
    let cursor = page1.next_cursor.clone().expect("cursor should be present");

    let page2 = RolloutRecorder::list_threads(
        /*state_db_ctx*/ None,
        &config,
        /*page_size*/ 1,
        Some(&cursor),
        ThreadSortKey::CreatedAt,
        SortDirection::Desc,
        &[],
        /*model_providers*/ None,
        /*cwd_filters*/ None,
        default_provider.as_str(),
        /*search_term*/ None,
    )
    .await?;
    assert_eq!(page2.items.len(), 1);
    assert_eq!(page2.items[0].path, middle);
    Ok(())
}

#[tokio::test]
async fn list_threads_db_enabled_drops_missing_rollout_paths() -> std::io::Result<()> {
    let home = TempDir::new().expect("temp dir");
    let config = test_config(home.path());

    let uuid = Uuid::from_u128(9010);
    let thread_id = ThreadId::from_string(&uuid.to_string()).expect("valid thread id");
    let stale_path = home.path().join(format!(
        "sessions/2099/01/01/rollout-2099-01-01T00-00-00-{uuid}.jsonl"
    ));

    let runtime = codex_state::StateRuntime::init(
        codex_state::SqliteConfig::new_for_testing(home.path().abs()),
        config.model_provider_id.clone(),
    )
    .await
    .expect("state db should initialize");
    runtime
        .mark_backfill_complete(/*last_watermark*/ None)
        .await
        .expect("backfill should be complete");
    let created_at = chrono::Utc
        .with_ymd_and_hms(2025, 1, 3, 13, 0, 0)
        .single()
        .expect("valid datetime");
    let mut builder = codex_state::ThreadMetadataBuilder::new(
        thread_id,
        stale_path,
        created_at,
        SessionSource::Cli,
    );
    builder.model_provider = Some(config.model_provider_id.clone());
    builder.cwd = home.path().to_path_buf();
    let mut metadata = builder.build(config.model_provider_id.as_str());
    metadata.first_user_message = Some("Hello from user".to_string());
    metadata.preview = metadata.first_user_message.clone();
    runtime
        .upsert_thread(&metadata)
        .await
        .expect("state db upsert should succeed");

    let default_provider = config.model_provider_id.clone();
    let page = RolloutRecorder::list_threads(
        Some(runtime.clone()),
        &config,
        /*page_size*/ 10,
        /*cursor*/ None,
        ThreadSortKey::CreatedAt,
        SortDirection::Desc,
        &[],
        /*model_providers*/ None,
        /*cwd_filters*/ None,
        default_provider.as_str(),
        /*search_term*/ None,
    )
    .await?;
    assert_eq!(page.items.len(), 0);
    let stored_path = runtime
        .find_rollout_path_by_id(thread_id, Some(false))
        .await
        .expect("state db lookup should succeed");
    assert_eq!(stored_path, None);
    Ok(())
}

#[tokio::test]
async fn list_threads_db_enabled_repairs_stale_rollout_paths() -> std::io::Result<()> {
    let home = TempDir::new().expect("temp dir");
    let config = test_config(home.path());

    let uuid = Uuid::from_u128(9011);
    let thread_id = ThreadId::from_string(&uuid.to_string()).expect("valid thread id");
    let real_path = write_session_file(home.path(), "2025-01-03T13-00-00", uuid)?;
    let stale_path = home.path().join(format!(
        "sessions/2099/01/01/rollout-2099-01-01T00-00-00-{uuid}.jsonl"
    ));

    let runtime = codex_state::StateRuntime::init(
        codex_state::SqliteConfig::new_for_testing(home.path().abs()),
        config.model_provider_id.clone(),
    )
    .await
    .expect("state db should initialize");
    runtime
        .mark_backfill_complete(/*last_watermark*/ None)
        .await
        .expect("backfill should be complete");
    let created_at = chrono::Utc
        .with_ymd_and_hms(2025, 1, 3, 13, 0, 0)
        .single()
        .expect("valid datetime");
    let mut builder = codex_state::ThreadMetadataBuilder::new(
        thread_id,
        stale_path,
        created_at,
        SessionSource::Cli,
    );
    builder.model_provider = Some(config.model_provider_id.clone());
    builder.cwd = home.path().to_path_buf();
    let mut metadata = builder.build(config.model_provider_id.as_str());
    metadata.first_user_message = Some("Hello from user".to_string());
    metadata.preview = metadata.first_user_message.clone();
    runtime
        .upsert_thread(&metadata)
        .await
        .expect("state db upsert should succeed");

    let default_provider = config.model_provider_id.clone();
    let page = RolloutRecorder::list_threads(
        Some(runtime.clone()),
        &config,
        /*page_size*/ 1,
        /*cursor*/ None,
        ThreadSortKey::CreatedAt,
        SortDirection::Desc,
        &[],
        /*model_providers*/ None,
        /*cwd_filters*/ None,
        default_provider.as_str(),
        /*search_term*/ None,
    )
    .await?;
    assert_eq!(page.items.len(), 1);
    assert_eq!(page.items[0].path, real_path);

    let repaired_path = runtime
        .find_rollout_path_by_id(thread_id, Some(false))
        .await
        .expect("state db lookup should succeed");
    assert_eq!(repaired_path, Some(real_path));
    Ok(())
}

#[tokio::test]
async fn list_threads_state_db_only_skips_jsonl_repair_scan() -> std::io::Result<()> {
    let home = TempDir::new().expect("temp dir");
    let config = test_config(home.path());

    let runtime = codex_state::StateRuntime::init(
        codex_state::SqliteConfig::new_for_testing(home.path().abs()),
        config.model_provider_id.clone(),
    )
    .await
    .expect("state db should initialize");
    runtime
        .mark_backfill_complete(/*last_watermark*/ None)
        .await
        .expect("backfill should be complete");

    let uuid = Uuid::from_u128(9012);
    let ts = "2025-01-03T14-00-00";
    let day_dir = home.path().join("sessions/2025/01/03");
    fs::create_dir_all(&day_dir)?;
    let path = day_dir.join(format!("rollout-{ts}-{uuid}.jsonl"));
    let mut file = File::create(&path)?;
    let meta = serde_json::json!({
        "timestamp": ts,
        "type": "session_meta",
        "payload": {
            "session_id": uuid,
            "id": uuid,
            "timestamp": ts,
            "cwd": home.path().display().to_string(),
            "originator": "test_originator",
            "cli_version": "test_version",
            "source": "cli",
            "model_provider": "test-provider",
        },
    });
    writeln!(file, "{meta}")?;
    let user_event = serde_json::json!({
        "timestamp": ts,
        "type": "event_msg",
        "payload": {
            "type": "user_message",
            "message": "Hello from user",
            "kind": "plain",
        },
    });
    writeln!(file, "{user_event}")?;

    let cwd_filters = [home.path().to_path_buf()];
    let state_db_only_page = RolloutRecorder::list_threads_from_state_db(
        Some(runtime.clone()),
        &config,
        /*page_size*/ 10,
        /*cursor*/ None,
        ThreadSortKey::CreatedAt,
        SortDirection::Desc,
        &[],
        /*model_providers*/ None,
        /*cwd_filters*/ Some(cwd_filters.as_slice()),
        config.model_provider_id.as_str(),
        /*search_term*/ None,
    )
    .await?;
    assert_eq!(state_db_only_page.items.len(), 0);

    let repaired_page = RolloutRecorder::list_threads(
        Some(runtime.clone()),
        &config,
        /*page_size*/ 10,
        /*cursor*/ None,
        ThreadSortKey::CreatedAt,
        SortDirection::Desc,
        &[],
        /*model_providers*/ None,
        /*cwd_filters*/ Some(cwd_filters.as_slice()),
        config.model_provider_id.as_str(),
        /*search_term*/ None,
    )
    .await?;
    assert_eq!(repaired_page.items.len(), 1);

    let repaired_state_db_only_page = RolloutRecorder::list_threads_from_state_db(
        Some(runtime.clone()),
        &config,
        /*page_size*/ 10,
        /*cursor*/ None,
        ThreadSortKey::CreatedAt,
        SortDirection::Desc,
        &[],
        /*model_providers*/ None,
        /*cwd_filters*/ Some(cwd_filters.as_slice()),
        config.model_provider_id.as_str(),
        /*search_term*/ None,
    )
    .await?;
    assert_eq!(repaired_state_db_only_page.items.len(), 1);
    Ok(())
}

#[tokio::test]
async fn list_threads_default_filter_returns_filesystem_scan_results() -> std::io::Result<()> {
    let home = TempDir::new().expect("temp dir");
    let config = test_config(home.path());

    let uuid = Uuid::from_u128(9013);
    let thread_id = ThreadId::from_string(&uuid.to_string()).expect("valid thread id");
    let real_path = write_session_file(home.path(), "2025-01-03T13-00-00", uuid)?;
    let stale_cwd = home.path().join("stale-cwd");

    let runtime = codex_state::StateRuntime::init(
        codex_state::SqliteConfig::new_for_testing(home.path().abs()),
        config.model_provider_id.clone(),
    )
    .await
    .expect("state db should initialize");
    runtime
        .mark_backfill_complete(/*last_watermark*/ None)
        .await
        .expect("backfill should be complete");
    let created_at = chrono::Utc
        .with_ymd_and_hms(2025, 1, 3, 13, 0, 0)
        .single()
        .expect("valid datetime");
    let mut builder = codex_state::ThreadMetadataBuilder::new(
        thread_id,
        real_path,
        created_at,
        SessionSource::Cli,
    );
    builder.model_provider = Some(config.model_provider_id.clone());
    builder.cwd = stale_cwd.clone();
    let mut metadata = builder.build(config.model_provider_id.as_str());
    metadata.first_user_message = Some("Hello from user".to_string());
    metadata.preview = metadata.first_user_message.clone();
    runtime
        .upsert_thread(&metadata)
        .await
        .expect("state db upsert should succeed");

    let cwd_filters = [stale_cwd];
    let state_db_only_page = RolloutRecorder::list_threads_from_state_db(
        Some(runtime.clone()),
        &config,
        /*page_size*/ 10,
        /*cursor*/ None,
        ThreadSortKey::CreatedAt,
        SortDirection::Desc,
        &[],
        /*model_providers*/ None,
        /*cwd_filters*/ Some(cwd_filters.as_slice()),
        config.model_provider_id.as_str(),
        /*search_term*/ None,
    )
    .await?;
    assert_eq!(state_db_only_page.items.len(), 1);

    let scanned_page = RolloutRecorder::list_threads(
        Some(runtime.clone()),
        &config,
        /*page_size*/ 10,
        /*cursor*/ None,
        ThreadSortKey::CreatedAt,
        SortDirection::Desc,
        &[],
        /*model_providers*/ None,
        /*cwd_filters*/ Some(cwd_filters.as_slice()),
        config.model_provider_id.as_str(),
        /*search_term*/ None,
    )
    .await?;
    assert_eq!(scanned_page.items.len(), 0);

    let repaired_state_db_only_page = RolloutRecorder::list_threads_from_state_db(
        Some(runtime.clone()),
        &config,
        /*page_size*/ 10,
        /*cursor*/ None,
        ThreadSortKey::CreatedAt,
        SortDirection::Desc,
        &[],
        /*model_providers*/ None,
        /*cwd_filters*/ Some(cwd_filters.as_slice()),
        config.model_provider_id.as_str(),
        /*search_term*/ None,
    )
    .await?;
    assert_eq!(repaired_state_db_only_page.items.len(), 0);
    Ok(())
}

#[tokio::test]
async fn list_threads_metadata_filter_overlays_state_db_list_metadata() -> std::io::Result<()> {
    let home = TempDir::new().expect("temp dir");
    let config = test_config(home.path());

    let uuid = Uuid::from_u128(9015);
    let thread_id = ThreadId::from_string(&uuid.to_string()).expect("valid thread id");
    let rollout_path = write_session_file(home.path(), "2025-01-03T16-00-00", uuid)?;

    let runtime = codex_state::StateRuntime::init(
        codex_state::SqliteConfig::new_for_testing(home.path().abs()),
        config.model_provider_id.clone(),
    )
    .await
    .expect("state db should initialize");
    runtime
        .mark_backfill_complete(/*last_watermark*/ None)
        .await
        .expect("backfill should be complete");
    let created_at = chrono::Utc
        .with_ymd_and_hms(2025, 1, 3, 16, 0, 0)
        .single()
        .expect("valid datetime");
    let mut builder = codex_state::ThreadMetadataBuilder::new(
        thread_id,
        rollout_path,
        created_at,
        SessionSource::Cli,
    );
    builder.model_provider = Some(config.model_provider_id.clone());
    builder.cwd = home.path().to_path_buf();
    builder.git_branch = Some("sqlite-branch".to_string());
    builder.git_sha = Some("sqlite-sha".to_string());
    builder.git_origin_url = Some("https://example.com/repo.git".to_string());
    let mut metadata = builder.build(config.model_provider_id.as_str());
    metadata.first_user_message = Some("Hello from user".to_string());
    metadata.preview = metadata.first_user_message.clone();
    runtime
        .upsert_thread(&metadata)
        .await
        .expect("state db upsert should succeed");

    let page = RolloutRecorder::list_threads(
        Some(runtime.clone()),
        &config,
        /*page_size*/ 10,
        /*cursor*/ None,
        ThreadSortKey::CreatedAt,
        SortDirection::Desc,
        &[SessionSource::Cli],
        /*model_providers*/ None,
        /*cwd_filters*/ None,
        config.model_provider_id.as_str(),
        /*search_term*/ None,
    )
    .await?;

    assert_eq!(page.items.len(), 1);
    assert_eq!(page.items[0].git_branch.as_deref(), Some("sqlite-branch"));
    assert_eq!(page.items[0].git_sha.as_deref(), Some("sqlite-sha"));
    assert_eq!(
        page.items[0].git_origin_url.as_deref(),
        Some("https://example.com/repo.git")
    );
    Ok(())
}

#[test]
fn fill_missing_thread_item_metadata_preserves_identity_and_prefers_state_git_fields() {
    let filesystem_thread_id = ThreadId::new();
    let state_thread_id = ThreadId::new();
    let filesystem_path = PathBuf::from("/tmp/filesystem-rollout.jsonl");
    let state_path = PathBuf::from("/tmp/state-rollout.jsonl");
    let mut item = ThreadItem {
        path: filesystem_path.clone(),
        thread_id: Some(filesystem_thread_id),
        first_user_message: Some("filesystem message".to_string()),
        preview: Some("filesystem preview".to_string()),
        is_pinned: false,
        cwd: None,
        git_branch: Some("filesystem-branch".to_string()),
        git_sha: Some("filesystem-sha".to_string()),
        git_origin_url: Some("https://example.com/filesystem.git".to_string()),
        source: None,
        thread_source: None,
        history_mode: Default::default(),
        parent_thread_id: None,
        agent_nickname: None,
        agent_role: None,
        model_provider: None,
        cli_version: None,
        created_at: None,
        recency_at: Some("2025-01-03T15:59:00.000Z".to_string()),
        updated_at: None,
    };
    let state_item = ThreadItem {
        path: state_path,
        thread_id: Some(state_thread_id),
        first_user_message: Some("state message".to_string()),
        preview: Some("state preview".to_string()),
        is_pinned: true,
        cwd: Some(PathBuf::from("/tmp/state-cwd")),
        git_branch: Some("state-branch".to_string()),
        git_sha: Some("state-sha".to_string()),
        git_origin_url: Some("https://example.com/state.git".to_string()),
        source: Some(SessionSource::Exec),
        thread_source: Some(ThreadSource::Side),
        history_mode: Default::default(),
        parent_thread_id: None,
        agent_nickname: Some("state-agent".to_string()),
        agent_role: Some("state-role".to_string()),
        model_provider: Some("state-provider".to_string()),
        cli_version: Some("state-version".to_string()),
        created_at: Some("2025-01-03T16:00:00Z".to_string()),
        recency_at: Some("2025-01-03T16:00:30.001Z".to_string()),
        updated_at: Some("2025-01-03T16:01:02.003Z".to_string()),
    };

    fill_missing_thread_item_metadata(&mut item, state_item);

    assert_eq!(item.path, filesystem_path);
    assert_eq!(item.thread_id, Some(filesystem_thread_id));
    assert!(item.is_pinned);
    assert_eq!(
        item.first_user_message.as_deref(),
        Some("filesystem message")
    );
    assert_eq!(item.preview.as_deref(), Some("filesystem preview"));
    assert_eq!(item.cwd.as_deref(), Some(Path::new("/tmp/state-cwd")));
    assert_eq!(item.git_branch.as_deref(), Some("state-branch"));
    assert_eq!(item.git_sha.as_deref(), Some("state-sha"));
    assert_eq!(
        item.git_origin_url.as_deref(),
        Some("https://example.com/state.git")
    );
    assert_eq!(item.source, Some(SessionSource::Exec));
    assert_eq!(item.thread_source, Some(ThreadSource::Side));
    assert_eq!(item.agent_nickname.as_deref(), Some("state-agent"));
    assert_eq!(item.agent_role.as_deref(), Some("state-role"));
    assert_eq!(item.model_provider.as_deref(), Some("state-provider"));
    assert_eq!(item.cli_version.as_deref(), Some("state-version"));
    assert_eq!(item.created_at.as_deref(), Some("2025-01-03T16:00:00Z"));
    assert_eq!(item.recency_at.as_deref(), Some("2025-01-03T16:00:30.001Z"));
    assert_eq!(item.updated_at.as_deref(), Some("2025-01-03T16:01:02.003Z"));
}

#[tokio::test]
async fn list_threads_search_repairs_stale_state_db_hits_before_returning() -> std::io::Result<()> {
    let home = TempDir::new().expect("temp dir");
    let config = test_config(home.path());

    let uuid = Uuid::from_u128(9014);
    let thread_id = ThreadId::from_string(&uuid.to_string()).expect("valid thread id");
    let real_path = write_session_file(home.path(), "2025-01-03T15-00-00", uuid)?;

    let runtime = codex_state::StateRuntime::init(
        codex_state::SqliteConfig::new_for_testing(home.path().abs()),
        config.model_provider_id.clone(),
    )
    .await
    .expect("state db should initialize");
    runtime
        .mark_backfill_complete(/*last_watermark*/ None)
        .await
        .expect("backfill should be complete");
    let created_at = chrono::Utc
        .with_ymd_and_hms(2025, 1, 3, 15, 0, 0)
        .single()
        .expect("valid datetime");
    let mut builder = codex_state::ThreadMetadataBuilder::new(
        thread_id,
        real_path,
        created_at,
        SessionSource::Cli,
    );
    builder.model_provider = Some(config.model_provider_id.clone());
    builder.cwd = home.path().to_path_buf();
    let mut metadata = builder.build(config.model_provider_id.as_str());
    metadata.title = "needle stale first user".to_string();
    metadata.first_user_message = Some(metadata.title.clone());
    metadata.preview = metadata.first_user_message.clone();
    runtime
        .upsert_thread(&metadata)
        .await
        .expect("state db upsert should succeed");

    let stale_state_db_only_page = RolloutRecorder::list_threads_from_state_db(
        Some(runtime.clone()),
        &config,
        /*page_size*/ 10,
        /*cursor*/ None,
        ThreadSortKey::CreatedAt,
        SortDirection::Desc,
        &[],
        /*model_providers*/ None,
        /*cwd_filters*/ None,
        config.model_provider_id.as_str(),
        Some("needle"),
    )
    .await?;
    assert_eq!(stale_state_db_only_page.items.len(), 1);

    let scanned_page = RolloutRecorder::list_threads(
        Some(runtime.clone()),
        &config,
        /*page_size*/ 10,
        /*cursor*/ None,
        ThreadSortKey::CreatedAt,
        SortDirection::Desc,
        &[],
        /*model_providers*/ None,
        /*cwd_filters*/ None,
        config.model_provider_id.as_str(),
        Some("needle"),
    )
    .await?;
    assert_eq!(scanned_page.items.len(), 0);

    let repaired_state_db_only_page = RolloutRecorder::list_threads_from_state_db(
        Some(runtime.clone()),
        &config,
        /*page_size*/ 10,
        /*cursor*/ None,
        ThreadSortKey::CreatedAt,
        SortDirection::Desc,
        &[],
        /*model_providers*/ None,
        /*cwd_filters*/ None,
        config.model_provider_id.as_str(),
        Some("needle"),
    )
    .await?;
    assert_eq!(repaired_state_db_only_page.items.len(), 0);
    Ok(())
}

#[tokio::test]
async fn resume_candidate_matches_cwd_reads_latest_turn_context() -> std::io::Result<()> {
    let home = TempDir::new().expect("temp dir");
    let stale_cwd = home.path().join("stale");
    let latest_cwd = home.path().join("latest");
    fs::create_dir_all(&stale_cwd)?;
    fs::create_dir_all(&latest_cwd)?;

    let path = write_session_file(home.path(), "2025-01-03T13-00-00", Uuid::from_u128(9012))?;
    let mut file = std::fs::OpenOptions::new().append(true).open(&path)?;
    let turn_context = RolloutLine {
        timestamp: "2025-01-03T13:00:01Z".to_string(),
        ordinal: None,
        item: RolloutItem::TurnContext(TurnContextItem {
            turn_id: Some("turn-1".to_string()),
            cwd: serde_json::from_value(serde_json::json!(&latest_cwd))
                .expect("absolute latest cwd"),
            workspace_roots: None,
            current_date: None,
            timezone: None,
            approval_policy: AskForApproval::Never,
            approvals_reviewer: None,
            sandbox_policy: SandboxPolicy::new_read_only_policy(),
            permission_profile: None,
            network: None,
            file_system_sandbox_policy: None,
            model: "test-model".to_string(),
            comp_hash: None,
            personality: None,
            collaboration_mode: None,
            multi_agent_version: None,
            multi_agent_mode: None,
            realtime_active: None,
            effort: None,
            summary: codex_protocol::config_types::ReasoningSummary::Auto,
        }),
    };
    writeln!(file, "{}", serde_json::to_string(&turn_context)?)?;

    assert!(
        resume_candidate_matches_cwd(
            path.as_path(),
            Some(stale_cwd.as_path()),
            latest_cwd.as_path(),
            "test-provider",
        )
        .await
    );
    Ok(())
}
