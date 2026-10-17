use pretty_assertions::assert_eq;

use super::*;

#[tokio::test]
async fn inter_agent_communication_waits_for_confirmed_delivery_persistence() {
    let (mut session, turn_context) = make_session_and_context().await;
    let turn_context = Arc::new(turn_context);
    let rollout_path = attach_thread_persistence(&mut session).await;
    let session = Arc::new(session);
    let checkpoint = thread_settings::acquire_persistence_lock(&session).await;
    let message = codex_history::RetainedUserMessage {
        origin: codex_history::UserInputOrigin::User,
        turn_id: turn_context.sub_id.clone(),
        message_id: Some("send".to_owned()),
        text: "May I publish?".to_owned(),
        complete: true,
        phase: None,
    };
    let (recording, _) = session.record_delivered_assistant_message(message.clone());
    let communication = InterAgentCommunication::new(
        AgentPath::root().join("worker").expect("worker path"),
        AgentPath::root(),
        Vec::new(),
        "child done".to_owned(),
        /*trigger_turn*/ false,
    );
    let communication_session = Arc::clone(&session);
    let replay_turn = Arc::clone(&turn_context);
    let mut communication_task = tokio::spawn(async move {
        communication_session
            .record_inter_agent_communication(
                &turn_context,
                turn_context.model_info(),
                communication,
            )
            .await
    });
    assert!(
        tokio::time::timeout(
            std::time::Duration::from_millis(/*millis*/ 100),
            &mut communication_task,
        )
        .await
        .is_err()
    );
    // Confirm another send while communication waits for the earlier delivery.
    let (later, _) =
        session.record_delivered_assistant_message(codex_history::RetainedUserMessage {
            message_id: Some("later".to_owned()),
            text: "Actually, wait.".to_owned(),
            ..message.clone()
        });
    drop(checkpoint);
    recording.await.expect("confirmed delivery persisted");
    communication_task
        .await
        .expect("communication task completed")
        .expect("communication persisted after delivery");
    later.await.expect("later delivery persisted");
    session.flush_rollout().await.expect("rollout flushed");
    let InitialHistory::Resumed(resumed) = RolloutRecorder::get_rollout_history(&rollout_path)
        .await
        .expect("read rollout history")
    else {
        panic!("expected resumed rollout history");
    };
    let boundaries = resumed
        .history
        .iter()
        .filter_map(|item| match item {
            RolloutItem::RetainedContext(
                codex_history::RetainedContextEvent::DeliveredAssistantMessage {
                    acceptance_order,
                    ..
                },
            ) => Some(("delivery", Some(*acceptance_order))),
            RolloutItem::ResponseItem(ResponseItemEnvelope {
                item: ResponseItem::AgentMessage { .. },
                metadata: Some(metadata),
            }) => Some(("communication", metadata.user_input_order)),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        boundaries,
        vec![
            ("delivery", Some(0)),
            ("communication", Some(1)),
            ("delivery", Some(2)),
        ]
    );
    let live = session.clone_history().await;
    let communication = live
        .annotated_items()
        .iter()
        .find(|item| matches!(item.item, ResponseItem::AgentMessage { .. }))
        .expect("live communication");
    assert_eq!(
        communication.metadata.as_ref().unwrap().user_input_order,
        Some(1)
    );
    let mut compacted: CompactedItem =
        serde_json::from_value(json!({"message": "compacted"})).unwrap();
    compacted.replacement_history = Some(live.annotated_items().to_vec());
    compacted.retained_context = Some(live.retained_context().clone());
    let mut history = resumed.history.as_ref().clone();
    history.extend([
        RolloutItem::Compacted(compacted),
        RolloutItem::EventMsg(EventMsg::ThreadRolledBack(
            codex_protocol::protocol::ThreadRolledBackEvent { num_turns: 1 },
        )),
    ]);
    let replayed = session
        .reconstruct_history_from_rollout(&replay_turn, &history)
        .await;
    let messages = replayed
        .retained_context
        .ordered_entries()
        .filter_map(|(_, entry)| {
            if let codex_history::RetainedContextEntry::AssistantMessage(message) = entry {
                Some(message.clone())
            } else {
                None
            }
        })
        .collect::<Vec<_>>();
    assert_eq!(messages, vec![message]);
}
async fn make_explicit_test_session(
    codex_home: &Path,
    thread_store: Arc<dyn codex_thread_store::ThreadStore>,
) -> (
    Arc<Session>,
    async_channel::Receiver<codex_protocol::protocol::Event>,
    PathBuf,
) {
    let (session, rx_event, _) = make_session_with_usage_state_and_agent_control_and_rx(
        InitialHistory::New,
        SessionSource::Exec,
        LocalAgentControl::default(),
        codex_home,
        /*enable_usage_state*/ true,
        /*parent_thread_id*/ None,
        Some(thread_store),
        /*enable_multi_agent_v2*/ true,
    )
    .await
    .expect("create explicit inter-agent test session");
    assert_eq!(
        session.multi_agent_version(),
        Some(MultiAgentVersion::V2),
        "explicit inter-agent tests must use the version selected by config"
    );
    let rollout_path = session
        .current_rollout_path()
        .await
        .expect("load explicit inter-agent rollout path")
        .expect("persistent explicit inter-agent session has a rollout path");
    session
        .flush_rollout()
        .await
        .expect("new explicit inter-agent session should flush");
    (session, rx_event, rollout_path)
}

fn explicit_test_communication(update_id: &str, content: &str) -> InterAgentCommunication {
    let mut communication = InterAgentCommunication::new(
        AgentPath::try_from("/root/worker").expect("worker path"),
        AgentPath::root(),
        Vec::new(),
        content.to_string(),
        /*trigger_turn*/ false,
    );
    communication.id = Some(ResponseItemId::new(update_id));
    communication
}

fn explicit_test_admission(
    root_thread_id: ThreadId,
    communication: &InterAgentCommunication,
) -> InterAgentCommunicationAdmission {
    let update_id = communication
        .id
        .clone()
        .expect("explicit test communication has an update ID");
    InterAgentCommunicationAdmission {
        update_id: update_id.clone(),
        idempotency_key: format!("test-key-{update_id}"),
        intent: InterAgentCommunicationIntent::Progress,
        original_communication: communication.clone(),
        root_thread_id,
        sender_thread_id: ThreadId::new(),
        recipient_thread_id: root_thread_id,
        sender_path: communication.author.clone(),
        recipient_path: communication.recipient.clone(),
        supersedes: Vec::new(),
        represented_response_id: None,
    }
}

async fn persist_explicit_test_admission(
    session: &Session,
    admission: &InterAgentCommunicationAdmission,
) {
    session
        .live_thread()
        .expect("explicit test session has a live thread")
        .append_items(&[RolloutItem::InterAgentCommunicationMetadata {
            trigger_turn: false,
            admission: Some(admission.clone()),
        }])
        .await
        .expect("persist explicit test admission");
    session.flush_rollout().await.expect("flush test admission");
}

fn drain_raw_response_items(
    rx_event: &async_channel::Receiver<codex_protocol::protocol::Event>,
) -> Vec<ResponseItem> {
    let mut items = Vec::new();
    while let Ok(event) = rx_event.try_recv() {
        if let codex_protocol::protocol::EventMsg::RawResponseItem(event) = event.msg {
            items.push(event.item);
        }
    }
    items
}

#[tokio::test]
async fn explicit_communication_reconciles_post_append_metadata_failure_once() {
    let codex_home = tempfile::tempdir().expect("create failure fixture home");
    let config = build_test_config(codex_home.path()).await;
    let store = Arc::new(FailOnceThreadStore::new(
        codex_thread_store::LocalThreadStore::new(
            codex_thread_store::LocalThreadStoreConfig::from_config(&config),
            /*state_db*/ None,
        ),
    ));
    let store_handle: Arc<dyn codex_thread_store::ThreadStore> = store.clone();
    let (session, rx_event, rollout_path) =
        make_explicit_test_session(codex_home.path(), store_handle).await;
    let communication = explicit_test_communication(
        "metadata-failure-update",
        "canonical response survives metadata failure",
    );
    let admission = explicit_test_admission(session.thread_id, &communication);
    persist_explicit_test_admission(&session, &admission).await;
    let turn_context = session.new_default_turn().await;
    let _ = drain_raw_response_items(&rx_event);

    // ThreadMetadataSync coalesces updated-at touches for five seconds. Age that
    // checkpoint so the next append reaches the injected post-canonical metadata error.
    tokio::time::sleep(Duration::from_secs(6)).await;
    store.fail_next_metadata_update();
    session
        .record_inter_agent_communication(
            &turn_context,
            turn_context.model_info(),
            communication.clone(),
        )
        .await
        .expect("canonical response readback should complete delivery");
    assert_eq!(store.metadata_failures.load(Ordering::SeqCst), 1);

    let update_id = communication.id.as_ref().expect("update ID");
    let active_items = raw_history_items(&session.clone_history().await);
    assert_eq!(
        active_items
            .iter()
            .filter(|item| item.id() == Some(update_id))
            .count(),
        1,
        "the durable response is restored to active history exactly once"
    );
    let first_raw_events = drain_raw_response_items(&rx_event);
    assert_eq!(first_raw_events.len(), 1);
    assert_eq!(first_raw_events[0].id(), Some(update_id));

    let (items, stored_thread_id, parse_errors) =
        RolloutRecorder::load_rollout_items(&rollout_path)
            .await
            .expect("read canonical rollout");
    assert_eq!(stored_thread_id, Some(session.thread_id));
    assert_eq!(parse_errors, 0);
    assert_eq!(
        items
            .iter()
            .filter(|item| matches!(
                item,
                RolloutItem::ResponseItem(response)
                    if response.item.id() == Some(update_id)
                        && response
                            .metadata
                            .as_ref()
                            .and_then(|metadata| metadata.inter_agent_update_id.as_ref())
                            == Some(update_id)
            ))
            .count(),
        1,
        "the failed metadata update must not cause a duplicate canonical append"
    );

    let retry_turn = session.new_default_turn().await;
    session
        .record_inter_agent_communication(
            &retry_turn,
            retry_turn.model_info(),
            communication.clone(),
        )
        .await
        .expect("a later retry should reconcile the committed response");
    assert_eq!(
        raw_history_items(&session.clone_history().await)
            .iter()
            .filter(|item| item.id() == Some(update_id))
            .count(),
        1,
        "retry keeps exactly one active copy of the explicit response"
    );
    assert!(
        drain_raw_response_items(&rx_event).is_empty(),
        "the retry must not emit a duplicate raw response event"
    );
}

#[tokio::test]
async fn explicit_batch_retry_keeps_failed_message_before_suffix() {
    let codex_home = tempfile::tempdir().expect("create batch retry fixture home");
    let config = build_test_config(codex_home.path()).await;
    let store = Arc::new(FailOnceThreadStore::new(
        codex_thread_store::LocalThreadStore::new(
            codex_thread_store::LocalThreadStoreConfig::from_config(&config),
            /*state_db*/ None,
        ),
    ));
    let store_handle: Arc<dyn codex_thread_store::ThreadStore> = store.clone();
    let (session, rx_event, rollout_path) =
        make_explicit_test_session(codex_home.path(), store_handle).await;
    let first = explicit_test_communication("batch-first-update", "first queued update");
    let second = explicit_test_communication("batch-second-update", "second queued update");
    let first_admission = explicit_test_admission(session.thread_id, &first);
    let second_admission = explicit_test_admission(session.thread_id, &second);
    persist_explicit_test_admission(&session, &first_admission).await;
    persist_explicit_test_admission(&session, &second_admission).await;
    let first_id = first.id.as_ref().expect("first update ID").clone();
    let second_id = second.id.as_ref().expect("second update ID").clone();
    {
        let mut delivered = session
            .delivered_explicit_inter_agent_updates
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        delivered.insert(first_id.clone());
        delivered.insert(second_id.clone());
    }
    *session.active_turn.lock().await = Some(ActiveTurn::default());
    let turn_context = session.new_default_turn().await;
    let _ = drain_raw_response_items(&rx_event);
    let batch = vec![
        TurnInput::InterAgentCommunication(first.clone()),
        TurnInput::InterAgentCommunication(second.clone()),
    ];

    store.fail_next_append();
    let first_attempt = crate::session::turn::run_hooks_and_record_inputs(
        &session,
        &turn_context,
        turn_context.model_info(),
        &batch,
        PersistContext::Standard,
    )
    .await;
    assert!(
        first_attempt.is_err(),
        "the first queued append should fail once"
    );
    assert_eq!(store.append_failures.load(Ordering::SeqCst), 1);

    let turn_state = session
        .active_turn
        .lock()
        .await
        .as_ref()
        .map(|active_turn| Arc::clone(&active_turn.turn_state))
        .expect("the failed batch should retain its active turn");
    let retry_batch = session
        .input_queue
        .take_pending_input_for_turn_state(&turn_state)
        .await;
    assert_eq!(
        retry_batch
            .iter()
            .filter_map(|input| match input {
                TurnInput::InterAgentCommunication(communication) => communication.id.clone(),
                _ => None,
            })
            .collect::<Vec<_>>(),
        vec![first_id.clone(), second_id.clone()],
        "the failed explicit item and its suffix must share one ordered retry path"
    );

    crate::session::turn::run_hooks_and_record_inputs(
        &session,
        &turn_context,
        turn_context.model_info(),
        &retry_batch,
        PersistContext::Standard,
    )
    .await
    .expect("the one-shot failure should recover in original order");

    let (items, stored_thread_id, parse_errors) =
        RolloutRecorder::load_rollout_items(&rollout_path)
            .await
            .expect("read retry rollout");
    assert_eq!(stored_thread_id, Some(session.thread_id));
    assert_eq!(parse_errors, 0);
    assert_eq!(
        items
            .iter()
            .filter_map(|item| match item {
                RolloutItem::ResponseItem(response) => response
                    .metadata
                    .as_ref()
                    .and_then(|metadata| metadata.inter_agent_update_id.clone()),
                _ => None,
            })
            .collect::<Vec<_>>(),
        vec![first_id.clone(), second_id.clone()],
        "retry must persist the failed message before its original suffix"
    );
    assert_eq!(
        drain_raw_response_items(&rx_event)
            .iter()
            .filter_map(|item| item.id().cloned())
            .collect::<Vec<_>>(),
        vec![first_id, second_id],
        "retry must emit the explicit messages in the same order"
    );
}

#[tokio::test]
async fn explicit_retry_restores_persisted_envelope_not_regenerated_metadata() {
    let codex_home = tempfile::tempdir().expect("create retry fixture home");
    let config = build_test_config(codex_home.path()).await;
    let store: Arc<dyn codex_thread_store::ThreadStore> =
        Arc::new(codex_thread_store::LocalThreadStore::new(
            codex_thread_store::LocalThreadStoreConfig::from_config(&config),
            /*state_db*/ None,
        ));
    let (session, rx_event, rollout_path) =
        make_explicit_test_session(codex_home.path(), store).await;
    let mut communication = explicit_test_communication(
        "retry-persisted-envelope-update",
        "recover the canonical envelope",
    );
    communication.internal_chat_message_metadata_passthrough =
        Some(InternalChatMessageMetadataPassthrough {
            turn_id: Some("persisted-turn-id".to_string()),
            ..Default::default()
        });
    let admission = explicit_test_admission(session.thread_id, &communication);
    persist_explicit_test_admission(&session, &admission).await;
    let update_id = communication.id.as_ref().expect("update ID").clone();
    let persisted_envelope = ResponseItemEnvelope {
        item: communication.to_model_input_item(),
        metadata: Some(CodexHarnessMetadata {
            user_input_order: Some(7),
            inter_agent_update_id: Some(update_id.clone()),
            ..Default::default()
        }),
    };
    session
        .live_thread()
        .expect("explicit test session has a live thread")
        .append_items(&[RolloutItem::ResponseItem(persisted_envelope.clone())])
        .await
        .expect("persist canonical explicit response");
    session
        .flush_rollout()
        .await
        .expect("flush canonical response");
    let turn_context = session.new_default_turn().await;
    assert_ne!(turn_context.sub_id, "persisted-turn-id");
    let _ = drain_raw_response_items(&rx_event);

    session
        .record_inter_agent_communication(
            &turn_context,
            turn_context.model_info(),
            communication.clone(),
        )
        .await
        .expect("retry should reconstruct from the persisted response");

    let active_history = session.clone_history().await;
    let active_envelopes = active_history
        .annotated_items()
        .iter()
        .filter(|envelope| envelope.item.id() == Some(&update_id))
        .collect::<Vec<_>>();
    assert_eq!(active_envelopes.len(), 1);
    assert_eq!(active_envelopes[0], &persisted_envelope);
    let raw_events = drain_raw_response_items(&rx_event);
    assert_eq!(raw_events.len(), 1);
    assert_eq!(raw_events[0].id(), Some(&update_id));

    let duplicate_turn = session.new_default_turn().await;
    session
        .record_inter_agent_communication(
            &duplicate_turn,
            duplicate_turn.model_info(),
            communication,
        )
        .await
        .expect("repeated retry should be idempotent");
    assert_eq!(
        session
            .clone_history()
            .await
            .annotated_items()
            .iter()
            .filter(|envelope| envelope.item.id() == Some(&update_id))
            .count(),
        1
    );
    assert!(drain_raw_response_items(&rx_event).is_empty());
    let (items, _, _) = RolloutRecorder::load_rollout_items(&rollout_path)
        .await
        .expect("read canonical rollout after retry");
    assert_eq!(
        items
            .iter()
            .filter(|item| matches!(
                item,
                RolloutItem::ResponseItem(response)
                    if response.item.id() == Some(&update_id)
            ))
            .count(),
        1
    );
}

#[tokio::test]
async fn explicit_retry_does_not_resurrect_rolled_back_compaction_history() {
    let codex_home = tempfile::tempdir().expect("create rollback fixture home");
    let config = build_test_config(codex_home.path()).await;
    let store: Arc<dyn codex_thread_store::ThreadStore> =
        Arc::new(codex_thread_store::LocalThreadStore::new(
            codex_thread_store::LocalThreadStoreConfig::from_config(&config),
            /*state_db*/ None,
        ));
    let (session, rx_event, _rollout_path) =
        make_explicit_test_session(codex_home.path(), store).await;
    let communication = explicit_test_communication(
        "rolled-back-explicit-update",
        "do not reinsert this rolled-back response",
    );
    let admission = explicit_test_admission(session.thread_id, &communication);
    persist_explicit_test_admission(&session, &admission).await;
    let update_id = communication.id.as_ref().expect("update ID").clone();
    let persisted_envelope = ResponseItemEnvelope {
        item: communication.to_model_input_item(),
        metadata: Some(CodexHarnessMetadata {
            user_input_order: Some(4),
            inter_agent_update_id: Some(update_id.clone()),
            ..Default::default()
        }),
    };
    let user_boundary = ResponseItemEnvelope::new(ResponseItem::Message {
        id: Some(ResponseItemId::new("rolled-back-user-boundary")),
        role: "user".to_string(),
        content: vec![ContentItem::InputText {
            text: "a turn that is later rolled back".to_string(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    });
    let mut compacted: CompactedItem =
        serde_json::from_value(json!({"message": "compacted checkpoint"}))
            .expect("create compaction fixture");
    compacted.window_number = Some(1);
    compacted.replacement_history = Some(vec![user_boundary, persisted_envelope.clone()]);
    session
        .live_thread()
        .expect("explicit test session has a live thread")
        .append_items(&[
            RolloutItem::ResponseItem(persisted_envelope),
            RolloutItem::Compacted(compacted),
            RolloutItem::EventMsg(codex_protocol::protocol::EventMsg::ThreadRolledBack(
                codex_protocol::protocol::ThreadRolledBackEvent { num_turns: 1 },
            )),
        ])
        .await
        .expect("persist compacted and rolled-back representation");
    session
        .flush_rollout()
        .await
        .expect("flush rollback fixture");
    let mut turn_context = session.new_default_turn().await;
    Arc::get_mut(&mut turn_context)
        .expect("turn context should be uniquely owned")
        .history_mode = codex_protocol::protocol::ThreadHistoryMode::Paginated;
    let _ = drain_raw_response_items(&rx_event);

    session
        .record_inter_agent_communication(&turn_context, turn_context.model_info(), communication)
        .await
        .expect("inactive persisted representation remains idempotent");

    assert!(
        session
            .clone_history()
            .await
            .raw_items()
            .all(|item| item.id() != Some(&update_id)),
        "rollback keeps the represented response outside active history"
    );
    assert!(
        drain_raw_response_items(&rx_event).is_empty(),
        "a rolled-back representation must not be emitted as a new raw response"
    );
}

#[tokio::test]
async fn explicit_retry_rejects_hidden_representation_with_changed_payload() {
    let codex_home = tempfile::tempdir().expect("create hidden mismatch fixture home");
    let config = build_test_config(codex_home.path()).await;
    let store: Arc<dyn codex_thread_store::ThreadStore> =
        Arc::new(codex_thread_store::LocalThreadStore::new(
            codex_thread_store::LocalThreadStoreConfig::from_config(&config),
            /*state_db*/ None,
        ));
    let (session, rx_event, _) = make_explicit_test_session(codex_home.path(), store).await;
    let communication =
        explicit_test_communication("rolled-back-tampered-update", "original admitted content");
    let admission = explicit_test_admission(session.thread_id, &communication);
    persist_explicit_test_admission(&session, &admission).await;
    let update_id = communication.id.as_ref().expect("update ID").clone();
    let persisted_envelope = ResponseItemEnvelope {
        item: communication.to_model_input_item(),
        metadata: Some(CodexHarnessMetadata {
            user_input_order: Some(5),
            inter_agent_update_id: Some(update_id.clone()),
            ..Default::default()
        }),
    };
    let mut altered_snapshot = persisted_envelope.clone();
    let ResponseItem::AgentMessage { content, .. } = &mut altered_snapshot.item else {
        panic!("explicit communication should produce an agent message");
    };
    let Some(AgentMessageInputContent::InputText { text }) = content.first_mut() else {
        panic!("explicit fixture should have text content");
    };
    *text = "altered content hidden by rollback".to_string();
    let user_boundary = ResponseItemEnvelope::new(ResponseItem::Message {
        id: Some(ResponseItemId::new("tampered-rollback-user-boundary")),
        role: "user".to_string(),
        content: vec![ContentItem::InputText {
            text: "a turn that is later rolled back".to_string(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    });
    let mut compacted: CompactedItem =
        serde_json::from_value(json!({"message": "compacted checkpoint"}))
            .expect("create compaction fixture");
    compacted.window_number = Some(1);
    compacted.replacement_history = Some(vec![user_boundary, altered_snapshot]);
    session
        .live_thread()
        .expect("explicit test session has a live thread")
        .append_items(&[
            RolloutItem::ResponseItem(persisted_envelope),
            RolloutItem::Compacted(compacted),
            RolloutItem::EventMsg(codex_protocol::protocol::EventMsg::ThreadRolledBack(
                codex_protocol::protocol::ThreadRolledBackEvent { num_turns: 1 },
            )),
        ])
        .await
        .expect("persist hidden mismatched representation");
    session
        .flush_rollout()
        .await
        .expect("flush mismatched rollback fixture");
    let mut turn_context = session.new_default_turn().await;
    Arc::get_mut(&mut turn_context)
        .expect("turn context should be uniquely owned")
        .history_mode = codex_protocol::protocol::ThreadHistoryMode::Paginated;
    let _ = drain_raw_response_items(&rx_event);

    let result = session
        .record_inter_agent_communication(&turn_context, turn_context.model_info(), communication)
        .await;

    assert!(result.is_err(), "hidden payload mismatch must fail closed");
    assert!(
        session
            .clone_history()
            .await
            .raw_items()
            .all(|item| item.id() != Some(&update_id)),
        "mismatched persisted content must not enter active history"
    );
    assert!(
        drain_raw_response_items(&rx_event).is_empty(),
        "mismatched persisted content must not be emitted as a raw response"
    );
}

#[tokio::test]
async fn explicit_retry_rejects_duplicate_active_representations() {
    let codex_home = tempfile::tempdir().expect("create duplicate fixture home");
    let config = build_test_config(codex_home.path()).await;
    let store: Arc<dyn codex_thread_store::ThreadStore> =
        Arc::new(codex_thread_store::LocalThreadStore::new(
            codex_thread_store::LocalThreadStoreConfig::from_config(&config),
            /*state_db*/ None,
        ));
    let (session, rx_event, _) = make_explicit_test_session(codex_home.path(), store).await;
    let communication =
        explicit_test_communication("duplicate-active-update", "one logical response only");
    let admission = explicit_test_admission(session.thread_id, &communication);
    persist_explicit_test_admission(&session, &admission).await;
    let update_id = communication.id.as_ref().expect("update ID").clone();
    let response = ResponseItemEnvelope {
        item: communication.to_model_input_item(),
        metadata: Some(CodexHarnessMetadata {
            inter_agent_update_id: Some(update_id.clone()),
            ..Default::default()
        }),
    };
    session
        .live_thread()
        .expect("explicit test session has a live thread")
        .append_items(&[
            RolloutItem::ResponseItem(response.clone()),
            RolloutItem::ResponseItem(response),
        ])
        .await
        .expect("persist duplicate active representations");
    session
        .flush_rollout()
        .await
        .expect("flush duplicate active representations");
    let turn_context = session.new_default_turn().await;
    let _ = drain_raw_response_items(&rx_event);

    let result = session
        .record_inter_agent_communication(&turn_context, turn_context.model_info(), communication)
        .await;

    assert!(result.is_err(), "two active copies must fail closed");
    assert_eq!(
        session
            .clone_history()
            .await
            .raw_items()
            .filter(|item| item.id() == Some(&update_id))
            .count(),
        0,
        "duplicate active persisted items must not be newly recorded"
    );
    assert!(
        drain_raw_response_items(&rx_event).is_empty(),
        "duplicate active persisted items must not be emitted"
    );
}

#[tokio::test]
async fn explicit_retry_rejects_receipt_with_unrelated_response_id() {
    let codex_home = tempfile::tempdir().expect("create receipt fixture home");
    let config = build_test_config(codex_home.path()).await;
    let store: Arc<dyn codex_thread_store::ThreadStore> =
        Arc::new(codex_thread_store::LocalThreadStore::new(
            codex_thread_store::LocalThreadStoreConfig::from_config(&config),
            /*state_db*/ None,
        ));
    let (session, rx_event, _) = make_explicit_test_session(codex_home.path(), store).await;
    let communication = explicit_test_communication(
        "receipt-response-id-update",
        "the receipt must identify this response",
    );
    let mut admission = explicit_test_admission(session.thread_id, &communication);
    admission.represented_response_id = Some(ResponseItemId::new("unrelated-response"));
    persist_explicit_test_admission(&session, &admission).await;
    let update_id = communication.id.as_ref().expect("update ID").clone();
    let turn_context = session.new_default_turn().await;
    let _ = drain_raw_response_items(&rx_event);

    let result = session
        .record_inter_agent_communication(&turn_context, turn_context.model_info(), communication)
        .await;

    assert!(result.is_err(), "unrelated receipt ID must fail closed");
    assert!(
        session
            .clone_history()
            .await
            .raw_items()
            .all(|item| item.id() != Some(&update_id)),
        "unverified receipt must not enter active history"
    );
    assert!(
        drain_raw_response_items(&rx_event).is_empty(),
        "unverified receipt must not be emitted"
    );
}

#[tokio::test]
async fn record_inter_agent_communication_sets_turn_id_in_rollout_and_resume() {
    let (mut session, turn_context) = make_session_and_context().await;
    let rollout_path = attach_thread_persistence(&mut session).await;
    let communication = InterAgentCommunication::new(
        AgentPath::root().join("worker").expect("worker path"),
        AgentPath::root(),
        Vec::new(),
        "child done".to_string(),
        /*trigger_turn*/ false,
    );
    let mut expected_item = communication.to_model_input_item();
    expected_item.set_turn_id_if_missing(&turn_context.sub_id);

    session
        .record_inter_agent_communication(&turn_context, turn_context.model_info(), communication)
        .await
        .expect("communication should be recorded");

    let recorded_history = session.clone_history().await;
    let recorded_items = raw_history_items(&recorded_history);
    let create_time = recorded_items[0]
        .executed_tool_call_metadata()
        .and_then(|metadata| metadata.create_time.clone())
        .expect("locally authored agent message should receive a creation timestamp");
    assert!(create_time.as_f64().is_some_and(|seconds| seconds > 0.0));
    expected_item.set_create_time_if_missing(create_time);

    assert_eq!(
        strip_response_item_ids(&recorded_items),
        strip_response_item_ids(std::slice::from_ref(&expected_item))
    );

    session.flush_rollout().await.expect("rollout should flush");
    let InitialHistory::Resumed(resumed) = RolloutRecorder::get_rollout_history(&rollout_path)
        .await
        .expect("read rollout history")
    else {
        panic!("expected resumed rollout history");
    };
    let persisted_items = resumed
        .history
        .iter()
        .filter(|item| {
            matches!(
                item,
                RolloutItem::ResponseItem(_)
                    | RolloutItem::InterAgentCommunication(_)
                    | RolloutItem::InterAgentCommunicationMetadata { .. }
            )
        })
        .cloned()
        .collect::<Vec<_>>();
    let expected_persisted_items = vec![
        RolloutItem::InterAgentCommunicationMetadata {
            trigger_turn: false,
            admission: None,
        },
        RolloutItem::ResponseItem(ResponseItemEnvelope {
            item: expected_item.clone(),
            metadata: Some(CodexHarnessMetadata {
                user_input_order: Some(0),
                ..Default::default()
            }),
        }),
    ];
    assert_eq!(
        strip_response_item_ids_from_json(serde_json::to_value(persisted_items).unwrap()),
        strip_response_item_ids_from_json(serde_json::to_value(expected_persisted_items).unwrap())
    );

    let (resumed_session, _resumed_turn_context) = make_session_and_context().await;
    resumed_session
        .record_initial_history(InitialHistory::Resumed(resumed))
        .await;
    assert_eq!(
        strip_response_item_ids(&raw_history_items(&resumed_session.clone_history().await)),
        strip_response_item_ids(std::slice::from_ref(&expected_item))
    );
    assert_eq!(resumed_session.reserve_user_input_order().await, 1);
}

#[tokio::test]
async fn record_inter_agent_communication_preserves_item_id_in_rollout_and_resume() {
    let (mut session, turn_context, _rx) = make_session_and_context_with_auth_and_config_and_rx(
        CodexAuth::from_api_key("Test API Key"),
        Vec::new(),
        |_| {},
    )
    .await;
    let rollout_path =
        attach_thread_persistence(Arc::get_mut(&mut session).expect("unique session")).await;
    let mut communication = InterAgentCommunication::new(
        AgentPath::root().join("worker").expect("worker path"),
        AgentPath::root(),
        Vec::new(),
        "child done".to_string(),
        /*trigger_turn*/ false,
    );
    communication.id = Some(ResponseItemId::from_server(
        "legacy-public-message-id".to_string(),
    ));

    session
        .record_inter_agent_communication(&turn_context, turn_context.model_info(), communication)
        .await
        .expect("communication should be recorded");

    let live_history = session.clone_history().await;
    let live_items = raw_history_items(&live_history);
    let [live_item] = live_items.as_slice() else {
        panic!("expected exactly one live history item");
    };
    let live_item_id = live_item
        .id()
        .expect("live agent message should have an item id")
        .to_string();
    assert_eq!(live_item_id, "legacy-public-message-id");

    session.flush_rollout().await.expect("rollout should flush");
    let InitialHistory::Resumed(resumed) = RolloutRecorder::get_rollout_history(&rollout_path)
        .await
        .expect("read rollout history")
    else {
        panic!("expected resumed rollout history");
    };
    let persisted_item_id = resumed.history.iter().find_map(|item| match item {
        RolloutItem::ResponseItem(item)
            if matches!(&item.item, ResponseItem::AgentMessage { .. }) =>
        {
            item.id()
        }
        _ => None,
    });
    assert_eq!(
        persisted_item_id.map(ResponseItemId::as_str),
        Some(live_item_id.as_str())
    );
    assert!(
        resumed
            .history
            .iter()
            .filter_map(|item| match item {
                RolloutItem::ResponseItem(item)
                    if matches!(&item.item, ResponseItem::AgentMessage { .. }) =>
                {
                    Some(
                        item.metadata
                            .as_ref()
                            .and_then(|metadata| metadata.inter_agent_update_id.as_ref()),
                    )
                }
                _ => None,
            })
            .all(|update_id| update_id.is_none())
    );

    let (resumed_session, _resumed_turn_context, _rx) =
        make_session_and_context_with_auth_and_config_and_rx(
            CodexAuth::from_api_key("Test API Key"),
            Vec::new(),
            |_| {},
        )
        .await;
    resumed_session
        .record_initial_history(InitialHistory::Resumed(resumed))
        .await;
    let resumed_history = resumed_session.clone_history().await;
    let resumed_items = raw_history_items(&resumed_history);
    let [resumed_item] = resumed_items.as_slice() else {
        panic!("expected exactly one resumed history item");
    };
    assert_eq!(
        resumed_item.id().map(ResponseItemId::as_str),
        Some(live_item_id.as_str())
    );
}
