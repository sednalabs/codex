#[tokio::test]
async fn remote_compaction_v2_retains_only_the_selected_step(first_attempt: FirstAttempt) {
    let server = responses::start_mock_server().await;
    let (session, turn, events) = make_remote_compaction_session(&server.uri()).await;
    session
        .record_conversation_items(
            &turn,
            turn.model_info(),
            &[user_message("before compaction")],
        )
        .await;
    session
        .spawn_task(
            Arc::clone(&turn),
            Vec::new(),
            NeverEndingTask {
                kind: TaskKind::Regular,
                listen_to_cancellation_token: true,
            },
        )
        .await;
    let primary_turn = Arc::new(
        turn.with_model("gpt-5.4".to_string(), &session.services.models_manager)
            .await,
    );
    let primary = session
        .capture_step_context(primary_turn, &CancellationToken::new())
        .await
        .expect("capture primary step");
    let fallback = session
        .capture_speculative_step_context(turn, &CancellationToken::new())
        .await
        .expect("capture speculative fallback");
    let state = {
        let active = session.active_turn.lock().await;
        Arc::clone(&active.as_ref().expect("active turn").turn_state)
    };
    assert_eq!(
        state
            .lock()
            .await
            .last_known_step_context
            .as_ref()
            .map(Arc::as_ptr),
        Some(Arc::as_ptr(&primary)),
    );

    let success = ResponseTemplate::new(/*status*/ 200)
        .insert_header("content-type", "text/event-stream")
        .set_body_string(responses::sse(vec![
            json!({
                "type": "response.output_item.done",
                "item": { "type": "compaction", "encrypted_content": "summary" },
            }),
            responses::ev_completed("compact-response"),
        ]));
    let replies = match first_attempt {
        FirstAttempt::Succeeds => vec![success],
        FirstAttempt::Retries => vec![
            ResponseTemplate::new(/*status*/ 400)
                .set_body_json(json!({ "detail": "previous model unavailable" })),
            success,
        ],
    };
    let requests = responses::mount_response_sequence(&server, replies).await;
    let mut client_session = session.services.model_client.new_session();
    crate::compact_remote_v2::run_inline_remote_auto_compact_task(
        Arc::clone(&session),
        Arc::clone(&primary),
        Arc::clone(&fallback),
        &mut client_session,
        Arc::new(
            session
                .build_world_state_for_step(&fallback, /*new_window*/ true)
                .await
                .unwrap(),
        ),
        CompactionReason::ModelDownshift,
        CompactionPhase::PreTurn,
        /*candidate*/ None,
        crate::compact_remote_v2::CompactionFallback::Foreground,
    )
    .await
    .expect("compaction succeeds");

    let (expected, models) = match first_attempt {
        FirstAttempt::Succeeds => (&primary, vec![json!("gpt-5.4")]),
        FirstAttempt::Retries => (&fallback, vec![json!("gpt-5.4"), json!("gpt-5.2")]),
    };
    assert_eq!(
        state
            .lock()
            .await
            .last_known_step_context
            .as_ref()
            .map(Arc::as_ptr),
        Some(Arc::as_ptr(expected)),
    );
    assert_eq!(
        requests
            .requests()
            .iter()
            .map(|request| request.body_json()["model"].clone())
            .collect::<Vec<_>>(),
        models,
    );
    assert!(
        requests
            .requests()
            .iter()
            .all(|request| request.inputs_of_type("compaction_trigger").len() == 1),
    );
    session.abort_all_tasks(TurnAbortReason::Interrupted).await;
    recv_terminal_event(&events, TerminalEventKind::TurnAborted).await;
}

#[tokio::test]
async fn interrupting_compaction_fallback_retains_last_known_step_context() {
    let (release_primary, primary_gate) = tokio::sync::oneshot::channel();
    let (release_fallback, fallback_gate) = tokio::sync::oneshot::channel();
    let (server, _) = start_streaming_sse_server(vec![
        vec![StreamingSseChunk {
            gate: Some(primary_gate),
            body: responses::sse_failed(
                "primary",
                "context_length_exceeded",
                "compact with the current model",
            ),
        }],
        vec![StreamingSseChunk {
            gate: Some(fallback_gate),
            body: responses::sse_completed("fallback"),
        }],
    ])
    .await;
    let (session, mut turn, events) = make_remote_compaction_session(server.uri()).await;
    update_turn_settings_for_test(
        Arc::get_mut(&mut turn).expect("unshared turn"),
        |settings| {
            Arc::make_mut(&mut settings.model_info).comp_hash = Some("new".to_string());
        },
    );
    session
        .set_previous_turn_settings(Some(PreviousTurnSettings {
            model: "gpt-5.4".to_string(),
            comp_hash: Some("old".to_string()),
            cyber_access_program: None,
            realtime_active: Some(turn.realtime_active),
        }))
        .await;
    session
        .record_conversation_items(
            &turn,
            turn.model_info(),
            &[user_message("before compaction")],
        )
        .await;
    session
        .spawn_task(turn, Vec::new(), crate::tasks::RegularTask::new())
        .await;
    let state = {
        let active = session.active_turn.lock().await;
        Arc::clone(&active.as_ref().expect("active turn").turn_state)
    };

    // The real turn loop has prepared both contexts before sending its first compact request.
    timeout(
        Duration::from_secs(/*secs*/ 10),
        server.wait_for_request_count(/*count*/ 1),
    )
    .await
    .expect("primary compaction request");
    let primary = state
        .lock()
        .await
        .last_known_step_context
        .clone()
        .expect("primary step");
    assert_eq!(primary.settings.model_info.slug, "gpt-5.4");

    release_primary.send(()).expect("release primary failure");
    timeout(
        Duration::from_secs(/*secs*/ 10),
        server.wait_for_request_count(/*count*/ 2),
    )
    .await
    .expect("fallback compaction request");
    let fallback = state
        .lock()
        .await
        .last_known_step_context
        .clone()
        .expect("fallback step");
    assert_eq!(fallback.settings.model_info.slug, "gpt-5.2");

    session.abort_all_tasks(TurnAbortReason::Interrupted).await;
    recv_terminal_event(&events, TerminalEventKind::TurnAborted).await;
    assert_eq!(
        state
            .lock()
            .await
            .last_known_step_context
            .as_ref()
            .map(Arc::as_ptr),
        Some(Arc::as_ptr(&fallback)),
    );
    drop(release_fallback);
    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn extension_interrupt_emits_thread_idle() {
    struct ThreadIdleRecorder(async_channel::Sender<()>);

    impl codex_extension_api::ThreadLifecycleContributor<crate::config::Config> for ThreadIdleRecorder {
        fn on_thread_idle<'a>(
            &'a self,
            _input: codex_extension_api::ThreadIdleInput<'a>,
        ) -> codex_extension_api::ExtensionFuture<'a, ()> {
            Box::pin(async move {
                self.0.send(()).await.expect("idle receiver open");
            })
        }
    }

    let (mut session, turn_context) = make_session_and_context().await;
    let (idle_tx, idle_rx) = async_channel::bounded(1);
    let mut builder = codex_extension_api::ExtensionRegistryBuilder::<crate::config::Config>::new();
    builder.thread_lifecycle_contributor(Arc::new(ThreadIdleRecorder(idle_tx)));
    session.services.extensions = Arc::new(builder.build());

    Arc::new(session)
        .spawn_task(Arc::new(turn_context), Vec::new(), ExtensionInterruptedTask)
        .await;

    timeout(StdDuration::from_secs(5), idle_rx.recv())
        .await
        .expect("extension interrupt should emit thread idle lifecycle")
        .expect("idle receiver open");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn extension_interrupt_survives_the_calling_runtime() {
    let (sess, tc, rx) = make_session_and_context_with_rx().await;
    let input = vec![TurnInput::UserInput {
        metadata: Default::default(),
        content: vec![UserInput::Text {
            text: "keep turn active for extension interruption".to_string(),
            text_elements: Vec::new(),
        }],
        client_id: None,
    }];
    sess.spawn_task(
        Arc::clone(&tc),
        input,
        NeverEndingTask {
            kind: TaskKind::Regular,
            listen_to_cancellation_token: true,
        },
    )
    .await;

    let session_for_review = Arc::clone(&sess);
    let review_thread = std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("helper review runtime");
        runtime.block_on(async move {
            session_for_review
                .interrupt_turn_with_warning(
                    &tc.sub_id,
                    EventMsg::Warning(codex_protocol::protocol::WarningEvent {
                        message: "extension interrupted this turn".into(),
                    }),
                    /*error*/ None,
                )
                .await;
        });
    });
    review_thread.join().expect("helper review thread");

    let mut observed = Vec::new();
    let aborted = timeout(StdDuration::from_secs(5), async {
        loop {
            let event = rx.recv().await.expect("event");
            if let EventMsg::TurnAborted(event) = &event.msg {
                let event = event.clone();
                observed.push(EventMsg::TurnAborted(event.clone()));
                break event;
            }
            observed.push(event.msg);
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!("extension should interrupt the turn; observed events: {observed:?}")
    });
    assert_eq!(aborted.reason, TurnAbortReason::Interrupted);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn turn_complete_flushes_terminal_event_after_delivery() {
    let (mut sess, tc, rx) = make_session_and_context_with_rx().await;
    let store = attach_in_memory_thread_store(
        Arc::get_mut(&mut sess).expect("session should be uniquely owned"),
    )
    .await;

    let input = vec![TurnInput::UserInput {
        metadata: Default::default(),
        content: vec![UserInput::Text {
            text: "complete normally".to_string(),
            text_elements: Vec::new(),
        }],
        client_id: None,
    }];
    sess.spawn_task(Arc::clone(&tc), input, CompletingTask)
        .await;

    let event = recv_terminal_event(&rx, TerminalEventKind::TurnComplete).await;
    assert!(matches!(event.msg, EventMsg::TurnComplete(_)));
    // Expected flushes:
    // 1. Task-runner flush after the task body finishes, before TurnComplete is emitted.
    // 2. Terminal-event flush after TurnComplete is appended.
    let calls = wait_for_flush_count(&store, /*expected_flushes*/ 2).await;
    assert_eq!(2, calls.flush_thread);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn turn_aborted_flushes_terminal_event_after_delivery() {
    let (mut sess, tc, rx) = make_session_and_context_with_rx().await;
    let store = attach_in_memory_thread_store(
        Arc::get_mut(&mut sess).expect("session should be uniquely owned"),
    )
    .await;

    let input = vec![TurnInput::UserInput {
        metadata: Default::default(),
        content: vec![UserInput::Text {
            text: "interrupt me".to_string(),
            text_elements: Vec::new(),
        }],
        client_id: None,
    }];
    sess.spawn_task(
        Arc::clone(&tc),
        input,
        NeverEndingTask {
            kind: TaskKind::Regular,
            listen_to_cancellation_token: true,
        },
    )
    .await;

    let abort_task = tokio::spawn({
        let sess = Arc::clone(&sess);
        async move {
            sess.abort_all_tasks(TurnAbortReason::Interrupted).await;
        }
    });

    let event = recv_terminal_event(&rx, TerminalEventKind::TurnAborted).await;
    match event.msg {
        EventMsg::TurnAborted(e) => assert_eq!(TurnAbortReason::Interrupted, e.reason),
        other => panic!("unexpected event: {other:?}"),
    }
    abort_task.await.expect("abort task should finish");
    // Expected flushes:
    // 1. Task-runner flush after the task body observes cancellation.
    // 2. Interrupted-marker flush before TurnAborted so abort observers can reread it.
    // 3. Terminal-event flush after TurnAborted is appended.
    let calls = wait_for_flush_count(&store, /*expected_flushes*/ 3).await;
    assert_eq!(3, calls.flush_thread);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[test_log::test]
async fn abort_regular_task_emits_marker_before_turn_aborted() {
    let (sess, tc, rx) = make_session_and_context_with_rx().await;
    let input = vec![TurnInput::UserInput {
        metadata: Default::default(),
        content: vec![UserInput::Text {
            text: "hello".to_string(),
            text_elements: Vec::new(),
        }],
        client_id: None,
    }];
    sess.spawn_task(
        Arc::clone(&tc),
        input,
        NeverEndingTask {
            kind: TaskKind::Regular,
            listen_to_cancellation_token: false,
        },
    )
    .await;

    sess.abort_all_tasks(TurnAbortReason::Interrupted).await;

    // Interrupts surface the model-visible `<turn_aborted>` marker before the abort event.
    let marker_evt = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
        .await
        .expect("timeout waiting for marker event")
        .expect("event");
    assert!(matches!(marker_evt.msg, EventMsg::RawResponseItem(_)));

    let evt = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
        .await
        .expect("timeout waiting for event")
        .expect("event");
    match evt.msg {
        EventMsg::TurnAborted(e) => assert_eq!(TurnAbortReason::Interrupted, e.reason),
        other => panic!("unexpected event: {other:?}"),
    }
    // No extra events should be emitted after an abort.
    assert!(rx.try_recv().is_err());
}

#[tokio::test]
async fn abort_gracefully_emits_marker_before_turn_aborted() {
    let (sess, tc, rx) = make_session_and_context_with_rx().await;
    let input = vec![TurnInput::UserInput {
        metadata: Default::default(),
        content: vec![UserInput::Text {
            text: "hello".to_string(),
            text_elements: Vec::new(),
        }],
        client_id: None,
    }];
    sess.spawn_task(
        Arc::clone(&tc),
        input,
        NeverEndingTask {
            kind: TaskKind::Regular,
            listen_to_cancellation_token: true,
        },
    )
    .await;

    sess.abort_all_tasks(TurnAbortReason::Interrupted).await;

    // Gracefully cancelled tasks surface the model-visible marker before the abort event too.
    let marker_evt = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
        .await
        .expect("timeout waiting for marker event")
        .expect("event");
    assert!(matches!(marker_evt.msg, EventMsg::RawResponseItem(_)));

    let evt = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
        .await
        .expect("timeout waiting for event")
        .expect("event");
    match evt.msg {
        EventMsg::TurnAborted(e) => assert_eq!(TurnAbortReason::Interrupted, e.reason),
        other => panic!("unexpected event: {other:?}"),
    }
    // No extra events should be emitted after an abort.
    assert!(rx.try_recv().is_err());
}

async fn submit_steer_only(
    sess: &Arc<Session>,
    input: Vec<UserInput>,
    expected_turn_id: &str,
) -> TurnInputSubmission {
    super::turn_input::handle(
        sess,
        TurnInputRequest::new(SubmittedTurnInput::UserInput {
            content: input,
            client_id: None,
        }),
        TurnInputMode::Steer {
            expected_turn_id: expected_turn_id.to_string(),
        },
        "test-submission".to_string(),
    )
    .await
    .expect("steer-only submission should be valid")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn task_finish_emits_turn_item_lifecycle_for_leftover_pending_user_input() {
    let (sess, mut tc, rx) = make_session_and_context_with_auth_and_config_and_rx(
        CodexAuth::from_api_key("Test API Key"),
        Vec::new(),
        |config| {
            config.features.enable(Feature::UnifiedImageBudget).unwrap();
        },
    )
    .await;
    update_turn_settings_for_test(
        Arc::get_mut(&mut tc).expect("unshared context"),
        |settings| {
            let model = Arc::make_mut(&mut settings.model_info);
            model.use_responses_lite = false;
            model.supports_image_detail_original = false;
        },
    );
    let input = vec![TurnInput::UserInput {
        metadata: Default::default(),
        content: vec![UserInput::Text {
            text: "hello".to_string(),
            text_elements: Vec::new(),
        }],
        client_id: None,
    }];
    sess.spawn_task(
        Arc::clone(&tc),
        input,
        NeverEndingTask {
            kind: TaskKind::Regular,
            listen_to_cancellation_token: false,
        },
    )
    .await;

    while rx.try_recv().is_ok() {}

    let mut encoded = std::io::Cursor::new(Vec::new());
    image::DynamicImage::new_rgba8(/*w*/ 8, /*h*/ 8)
        .write_to(&mut encoded, image::ImageFormat::Png)
        .expect("encode image");
    let image_url = codex_utils_image::data_url_from_bytes("image/png", &encoded.into_inner());
    let text_element = codex_protocol::user_input::TextElement::new(
        codex_protocol::user_input::ByteRange { start: 5, end: 12 },
        Some("pending marker".to_string()),
    );
    let pending_user_input = vec![
        UserInput::Text {
            text: "late pending input".to_string(),
            text_elements: vec![text_element.clone()],
        },
        UserInput::Image {
            image: ImageReference::Inline {
                image_url: image_url.clone(),
            },
            detail: Some(ImageDetail::High),
        },
    ];
    let submission = submit_steer_only(&sess, pending_user_input.clone(), &tc.sub_id).await;
    assert!(matches!(submission, TurnInputSubmission::Steered { .. }));

    let mut current = tc.initial_settings.as_ref().clone();
    Arc::make_mut(&mut current.model_info).supports_image_detail_original = true;
    tc.next_step_settings.store(Arc::new(current));

    sess.on_task_finished(Arc::clone(&tc), /*task_result*/ Ok(None))
        .await;

    let history = sess.clone_history().await;
    let expected = ResponseItem::Message {
        status: None,
        encrypted_content: None,
        id: None,
        role: "user".to_string(),
        content: vec![
            ContentItem::InputText {
                text: "late pending input".to_string(),
            },
            ContentItem::InputImage {
                image: ImageReference::Inline {
                    image_url: image_url.clone(),
                },
                detail: Some(ImageDetail::Original),
            },
        ],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    };
    assert!(
        strip_response_item_ids(&strip_metadata_from_items(&raw_history_items(&history)))
            .contains(&expected),
        "expected pending input to use the current model's image rules on turn completion"
    );

    let first = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
        .await
        .expect("expected raw response item event")
        .expect("channel open");
    assert!(matches!(first.msg, EventMsg::RawResponseItem(_)));

    let second = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
        .await
        .expect("expected item started event")
        .expect("channel open");
    assert!(matches!(
        second.msg,
        EventMsg::ItemStarted(ItemStartedEvent {
            item: TurnItem::UserMessage(UserMessageItem { content, .. }),
            ..
        }) if content == pending_user_input
    ));

    let third = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
        .await
        .expect("expected item completed event")
        .expect("channel open");
    assert!(matches!(
        third.msg,
        EventMsg::ItemCompleted(ItemCompletedEvent {
            item: TurnItem::UserMessage(UserMessageItem { content, .. }),
            ..
        }) if content == pending_user_input
    ));

    let fourth = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
        .await
        .expect("expected legacy user message event")
        .expect("channel open");
    assert!(matches!(
        fourth.msg,
        EventMsg::UserMessage(UserMessageEvent {
                client_id: None,
            message,
            images,
            text_elements,
            local_images,
            ..
        }) if message == "late pending input"
            && images == Some(vec![image_url])
            && text_elements == vec![text_element]
            && local_images.is_empty()
    ));

    let fifth = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
        .await
        .expect("expected turn complete event")
        .expect("channel open");
    assert!(matches!(
        fifth.msg,
        EventMsg::TurnComplete(TurnCompleteEvent {
            turn_id,
            last_agent_message: None,
            error: None,
            time_to_first_token_ms: None,
            ..
        }) if turn_id == tc.sub_id
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn task_finish_emits_thread_idle_lifecycle_after_active_turn_clears() {
    struct ThreadIdleRecorder {
        calls: Arc<std::sync::atomic::AtomicUsize>,
        idle_tx: async_channel::Sender<()>,
        expected_thread_id: ThreadId,
    }

    impl codex_extension_api::ThreadLifecycleContributor<crate::config::Config> for ThreadIdleRecorder {
        fn on_thread_idle<'a>(
            &'a self,
            input: codex_extension_api::ThreadIdleInput<'a>,
        ) -> codex_extension_api::ExtensionFuture<'a, ()> {
            Box::pin(async move {
                assert_eq!(
                    self.expected_thread_id.to_string(),
                    input.thread_store.level_id()
                );
                self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                self.idle_tx.send(()).await.expect("idle receiver open");
            })
        }
    }

    let (mut session, turn_context) = make_session_and_context().await;
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let (idle_tx, idle_rx) = async_channel::bounded(1);
    let mut builder = codex_extension_api::ExtensionRegistryBuilder::<crate::config::Config>::new();
    builder.thread_lifecycle_contributor(Arc::new(ThreadIdleRecorder {
        calls: Arc::clone(&calls),
        idle_tx,
        expected_thread_id: session.thread_id,
    }));
    session.services.extensions = Arc::new(builder.build());

    let session = Arc::new(session);
    session
        .spawn_task(Arc::new(turn_context), Vec::new(), CompletingTask)
        .await;

    timeout(StdDuration::from_secs(2), idle_rx.recv())
        .await
        .expect("thread idle lifecycle")
        .expect("idle receiver open");
    assert_eq!(1, calls.load(std::sync::atomic::Ordering::SeqCst));
    assert!(session.active_turn.lock().await.is_none());
}

#[tokio::test]
async fn thread_idle_lifecycle_waits_for_trigger_turn_mailbox_work() {
    struct ThreadIdleRecorder {
        calls: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl codex_extension_api::ThreadLifecycleContributor<crate::config::Config> for ThreadIdleRecorder {
        fn on_thread_idle<'a>(
            &'a self,
            _input: codex_extension_api::ThreadIdleInput<'a>,
        ) -> codex_extension_api::ExtensionFuture<'a, ()> {
            Box::pin(async move {
                self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            })
        }
    }

    let (mut session, _turn_context) = make_session_and_context().await;
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut builder = codex_extension_api::ExtensionRegistryBuilder::<crate::config::Config>::new();
    builder.thread_lifecycle_contributor(Arc::new(ThreadIdleRecorder {
        calls: Arc::clone(&calls),
    }));
    session.services.extensions = Arc::new(builder.build());
    session
        .input_queue
        .enqueue_mailbox_communication(
            InterAgentCommunication::new(
                AgentPath::root(),
                AgentPath::root(),
                Vec::new(),
                "pending trigger".to_string(),
                /*trigger_turn*/ true,
            )
            .into(),
            Default::default(),
        )
        .await;

    session
        .emit_thread_idle_lifecycle_if_idle(codex_extension_api::ThreadIdleCause::Completed)
        .await;

    assert_eq!(0, calls.load(std::sync::atomic::Ordering::SeqCst));
}

#[tokio::test]
async fn abort_empty_active_turn_preserves_pending_input() {
    let (sess, _tc, _rx) = make_session_and_context_with_rx().await;
    let pending_item = ResponseItem::Message {
        status: None,
        encrypted_content: None,
        id: None,
        role: "user".to_string(),
        content: vec![ContentItem::InputText {
            text: "late pending input".to_string(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    };
    let turn_state = {
        let mut active = sess.active_turn.lock().await;
        let active_turn = active.get_or_insert_with(ActiveTurn::default);
        Arc::clone(&active_turn.turn_state)
    };
    sess.input_queue
        .extend_pending_input_for_turn_state(
            turn_state.as_ref(),
            vec![TurnInput::ResponseItem(pending_item.clone().into())],
        )
        .await;

    sess.abort_all_tasks(TurnAbortReason::Replaced).await;

    assert!(sess.active_turn.lock().await.is_none());
    assert_eq!(
        sess.input_queue
            .take_pending_input_for_turn_state(turn_state.as_ref())
            .await,
        vec![TurnInput::ResponseItem(pending_item.into())]
    );
}

async fn set_total_token_usage(sess: &Session, total_token_usage: TokenUsage) {
    let mut state = sess.state.lock().await;
    state.set_token_info(Some(TokenUsageInfo {
        total_token_usage,
        last_usage_includes_reasoning: None,
        last_token_usage: TokenUsage::default(),
        model_context_window: None,
    }));
}

#[tokio::test]
async fn queue_only_mailbox_mail_waits_for_next_turn_after_answer_boundary() {
    let (sess, tc, _rx) = make_session_and_context_with_rx().await;
    let communication = InterAgentCommunication::new(
        AgentPath::try_from("/root/worker").expect("worker path should parse"),
        AgentPath::root(),
        Vec::new(),
        "late queue-only update".to_string(),
        /*trigger_turn*/ false,
    );
    sess.spawn_task(
        Arc::clone(&tc),
        Vec::new(),
        NeverEndingTask {
            kind: TaskKind::Regular,
            listen_to_cancellation_token: true,
        },
    )
    .await;

    sess.input_queue
        .defer_mailbox_delivery_to_next_turn(&sess.active_turn, &tc.sub_id)
        .await;
    sess.input_queue
        .enqueue_mailbox_communication(communication.clone().into(), Default::default())
        .await;

    assert!(
        !sess.input_queue.has_pending_input(&sess.active_turn).await,
        "queue-only mailbox mail should stay buffered once the current turn emitted its answer"
    );
    assert_eq!(
        sess.input_queue
            .get_pending_input(&sess.active_turn)
            .await
            .0,
        Vec::new()
    );

    sess.abort_all_tasks(TurnAbortReason::Replaced).await;

    assert_eq!(
        (sess.input_queue.get_pending_input(&sess.active_turn).await).0,
        vec![TurnInput::InterAgentCommunication(communication.into())],
    );
}

#[tokio::test]
async fn trigger_turn_mailbox_mail_waits_for_next_turn_after_answer_boundary() {
    let (sess, tc, _rx) = make_session_and_context_with_rx().await;
    sess.spawn_task(
        Arc::clone(&tc),
        Vec::new(),
        NeverEndingTask {
            kind: TaskKind::Regular,
            listen_to_cancellation_token: true,
        },
    )
    .await;

    sess.input_queue
        .defer_mailbox_delivery_to_next_turn(&sess.active_turn, &tc.sub_id)
        .await;
    sess.input_queue
        .enqueue_mailbox_communication(
            InterAgentCommunication::new(
                AgentPath::try_from("/root/worker").expect("worker path should parse"),
                AgentPath::root(),
                Vec::new(),
                "late trigger update".to_string(),
                /*trigger_turn*/ true,
            )
            .into(),
            Default::default(),
        )
        .await;

    assert!(
        !sess.input_queue.has_pending_input(&sess.active_turn).await,
        "trigger-turn mailbox mail should not extend the current turn after its answer boundary"
    );

    sess.abort_all_tasks(TurnAbortReason::Replaced).await;

    assert!(sess.input_queue.has_trigger_turn_mailbox_items().await);
}

#[test_case(None; "independent root")]
#[test_case(Some("root-a"); "inherited root")]
#[tokio::test]
async fn active_turn_keeps_first_root_when_mail_coalesces(inherited_root: Option<&str>) {
    let (sess, tc, _rx) = make_session_and_context_with_rx().await;
    if let Some(root) = inherited_root {
        tc.turn_metadata_state.set_root_turn_id(root.to_string());
    }
    let first = InterAgentCommunication::new(
        AgentPath::try_from("/root/worker_a").expect("worker path should parse"),
        AgentPath::root(),
        Vec::new(),
        "first".to_string(),
        /*trigger_turn*/ true,
    );
    let second = InterAgentCommunication::new(
        AgentPath::try_from("/root/worker_b").expect("worker path should parse"),
        AgentPath::root(),
        Vec::new(),
        "second".to_string(),
        /*trigger_turn*/ true,
    );
    for (index, (communication, parent_turn_id, root_turn_id)) in [
        (first.clone(), "parent-a", "root-a"),
        (second.clone(), "parent-b", "root-b"),
    ]
    .into_iter()
    .enumerate()
    {
        sess.input_queue
            .enqueue_mailbox_communication(
                communication.into(),
                codex_protocol::turn_input::TurnStartOptions {
                    parent_turn_id: Some(parent_turn_id.to_string()),
                    root_turn_id: Some(root_turn_id.to_string()),
                    ..Default::default()
                },
            )
            .await;
        if index == 0 {
            // The first message is already queued when this independent task
            // starts; the second arrives after its root is established.
            sess.spawn_task(
                Arc::clone(&tc),
                Vec::new(),
                NeverEndingTask {
                    kind: TaskKind::Regular,
                    listen_to_cancellation_token: true,
                },
            )
            .await;
        }
    }

    assert_eq!(
        (sess.input_queue.get_pending_input(&sess.active_turn).await).0,
        vec![
            TurnInput::InterAgentCommunication(first.into()),
            TurnInput::InterAgentCommunication(second.into()),
        ]
    );
    assert_eq!(
        tc.turn_metadata_state.root_turn_id().as_deref(),
        Some(inherited_root.unwrap_or(&tc.sub_id))
    );
    assert!(!sess.input_queue.has_pending_mailbox_items().await);

    sess.abort_all_tasks(TurnAbortReason::Replaced).await;
}

#[tokio::test]
async fn steered_input_reopens_mailbox_delivery_for_current_turn() {
    let (sess, tc, _rx) = make_session_and_context_with_rx().await;
    let communication = InterAgentCommunication::new(
        AgentPath::try_from("/root/worker").expect("worker path should parse"),
        AgentPath::root(),
        Vec::new(),
        "queued child update".to_string(),
        /*trigger_turn*/ false,
    );
    sess.spawn_task(
        Arc::clone(&tc),
        Vec::new(),
        NeverEndingTask {
            kind: TaskKind::Regular,
            listen_to_cancellation_token: true,
        },
    )
    .await;

    sess.input_queue
        .defer_mailbox_delivery_to_next_turn(&sess.active_turn, &tc.sub_id)
        .await;
    sess.input_queue
        .enqueue_mailbox_communication(communication.clone().into(), Default::default())
        .await;
    let submission = submit_steer_only(
        &sess,
        vec![UserInput::Text {
            text: "follow up".to_string(),
            text_elements: Vec::new(),
        }],
        &tc.sub_id,
    )
    .await;
    assert!(matches!(submission, TurnInputSubmission::Steered { .. }));

    assert_eq!(
        (sess.input_queue.get_pending_input(&sess.active_turn).await).0,
        vec![
            TurnInput::UserInput {
                metadata: crate::session::UserInputMetadata {
                    acceptance_order: Some(0),
                    ..Default::default()
                },
                content: vec![UserInput::Text {
                    text: "follow up".to_string(),
                    text_elements: Vec::new(),
                }],
                client_id: None,
            },
            TurnInput::InterAgentCommunication(communication.into()),
        ],
    );
}

#[tokio::test]
async fn stale_defer_mailbox_delivery_does_not_override_steered_input() {
    let (sess, tc, _rx) = make_session_and_context_with_rx().await;
    let communication = InterAgentCommunication::new(
        AgentPath::try_from("/root/worker").expect("worker path should parse"),
        AgentPath::root(),
        Vec::new(),
        "queued child update".to_string(),
        /*trigger_turn*/ false,
    );
    sess.spawn_task(
        Arc::clone(&tc),
        Vec::new(),
        NeverEndingTask {
            kind: TaskKind::Regular,
            listen_to_cancellation_token: true,
        },
    )
    .await;

    sess.input_queue
        .defer_mailbox_delivery_to_next_turn(&sess.active_turn, &tc.sub_id)
        .await;
    sess.input_queue
        .enqueue_mailbox_communication(communication.clone().into(), Default::default())
        .await;
    let submission = submit_steer_only(
        &sess,
        vec![UserInput::Text {
            text: "follow up".to_string(),
            text_elements: Vec::new(),
        }],
        &tc.sub_id,
    )
    .await;
    assert!(matches!(submission, TurnInputSubmission::Steered { .. }));

    sess.input_queue
        .defer_mailbox_delivery_to_next_turn(&sess.active_turn, &tc.sub_id)
        .await;

    assert_eq!(
        (sess.input_queue.get_pending_input(&sess.active_turn).await).0,
        vec![
            TurnInput::UserInput {
                metadata: crate::session::UserInputMetadata {
                    acceptance_order: Some(0),
                    ..Default::default()
                },
                content: vec![UserInput::Text {
                    text: "follow up".to_string(),
                    text_elements: Vec::new(),
                }],
                client_id: None,
            },
            TurnInput::InterAgentCommunication(communication.into()),
        ],
    );
}

#[tokio::test]
async fn tool_calls_reopen_mailbox_delivery_for_current_turn() {
    let (sess, tc, _rx) = make_session_and_context_with_rx().await;
    let communication = InterAgentCommunication::new(
        AgentPath::try_from("/root/worker").expect("worker path should parse"),
        AgentPath::root(),
        Vec::new(),
        "queued child update".to_string(),
        /*trigger_turn*/ false,
    );
    sess.spawn_task(
        Arc::clone(&tc),
        Vec::new(),
        NeverEndingTask {
            kind: TaskKind::Regular,
            listen_to_cancellation_token: true,
        },
    )
    .await;

    sess.input_queue
        .defer_mailbox_delivery_to_next_turn(&sess.active_turn, &tc.sub_id)
        .await;
    sess.input_queue
        .enqueue_mailbox_communication(communication.clone().into(), Default::default())
        .await;

    let item = ResponseItem::FunctionCall {
        status: None,
        encrypted_content: None,
        id: None,
        name: "test_tool".to_string(),
        namespace: None,
        arguments: "{}".to_string(),
        call_id: "call-1".to_string(),
        encrypted_function_args: None,
        internal_chat_message_metadata_passthrough: None,
    };
    let mut ctx = HandleOutputCtx {
        sess: Arc::clone(&sess),
        step_context: StepContext::for_test(Arc::clone(&tc)),
        turn_store: Arc::new(codex_extension_api::ExtensionData::new(tc.sub_id.clone())),
        tool_runtime: test_tool_runtime(Arc::clone(&sess), Arc::clone(&tc)).await,
        defer_tool_dispatch: false,
        cancellation_token: CancellationToken::new(),
    };

    let output = handle_output_item_done(
        &mut ctx, item, /*previously_active_item*/ None, /*moderation*/ None,
    )
    .await
    .expect("tool call should be handled");

    assert!(output.needs_follow_up);
    assert!(output.tool_future.is_some());
    assert_eq!(
        (sess.input_queue.get_pending_input(&sess.active_turn).await).0,
        vec![TurnInput::InterAgentCommunication(communication.into())],
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn abort_review_task_emits_exited_then_aborted_and_records_history() {
    let (sess, tc, rx) = make_session_and_context_with_rx().await;
    let input = vec![TurnInput::UserInput {
        metadata: Default::default(),
        content: vec![UserInput::Text {
            text: "start review".to_string(),
            text_elements: Vec::new(),
        }],
        client_id: None,
    }];
    sess.spawn_task(Arc::clone(&tc), input, ReviewTask::new())
        .await;

    sess.abort_all_tasks(TurnAbortReason::Interrupted).await;

    // Aborting a review task should exit review mode before surfacing the abort to the client.
    // We scan for these events (rather than relying on fixed ordering) since unrelated events
    // may interleave.
    let mut exited_review_mode_idx = None;
    let mut turn_aborted_idx = None;
    let mut idx = 0usize;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(3);
    while tokio::time::Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let evt = tokio::time::timeout(remaining, rx.recv())
            .await
            .expect("timeout waiting for event")
            .expect("event");
        let event_idx = idx;
        idx = idx.saturating_add(1);
        match evt.msg {
            EventMsg::ExitedReviewMode(ev) => {
                assert!(ev.review_output.is_none());
                exited_review_mode_idx = Some(event_idx);
            }
            EventMsg::TurnAborted(ev) => {
                assert_eq!(TurnAbortReason::Interrupted, ev.reason);
                turn_aborted_idx = Some(event_idx);
                break;
            }
            _ => {}
        }
    }
    assert!(
        exited_review_mode_idx.is_some(),
        "expected ExitedReviewMode after abort"
    );
    assert!(
        turn_aborted_idx.is_some(),
        "expected TurnAborted after abort"
    );
    assert!(
        exited_review_mode_idx.unwrap() < turn_aborted_idx.unwrap(),
        "expected ExitedReviewMode before TurnAborted"
    );

    let history = sess.clone_history().await;
    // Verify the `<turn_aborted>` marker is still recorded in history for the model.
    assert!(
        history.raw_items().any(|item| {
            let ResponseItem::Message { role, content, .. } = item else {
                return false;
            };
            if role != "user" {
                return false;
            }
            content.iter().any(|content_item| {
                let ContentItem::InputText { text } = content_item else {
                    return false;
                };
                TurnAborted::matches_text(text)
            })
        }),
        "expected a model-visible turn aborted marker in history after interrupt"
    );
}

#[tokio::test]
async fn fatal_tool_error_stops_turn_and_reports_error() {
    let (session, turn_context, _rx) = make_session_and_context_with_rx().await;
    let step_context = StepContext::for_test(Arc::clone(&turn_context));
    let (registry, hosted_specs) =
        tool_registry_for_test_step(&session, step_context.as_ref()).await;
    let router = ToolRouter::from_registry(
        step_context.turn.as_ref(),
        step_context.turn.model_info(),
        registry,
        hosted_specs,
        &Default::default(),
    );
    let item = ResponseItem::CustomToolCall {
        encrypted_content: None,
        id: None,
        status: None,
        call_id: "call-1".to_string(),
        name: "exec_command".to_string(),
        namespace: None,
        input: "{}".to_string(),
        internal_chat_message_metadata_passthrough: None,
    };

    let call = router
        .build_tool_call(item.clone())
        .expect("build tool call")
        .expect("tool call present");
    let tracker = Arc::new(tokio::sync::Mutex::new(TurnDiffTracker::new()));
    let err = router
        .dispatch_tool_call_with_code_mode_result(
            Arc::clone(&session),
            step_context,
            CancellationToken::new(),
            tracker,
            call,
            ToolCallSource::Direct,
        )
        .await
        .err()
        .expect("expected fatal error");

    match err {
        FunctionCallError::Fatal(message) => {
            assert_eq!(
                message,
                "tool exec_command invoked with incompatible payload"
            );
        }
        other => panic!("expected FunctionCallError::Fatal, got {other:?}"),
    }
}

async fn sample_rollout(
    session: &Session,
    _turn_context: &TurnContext,
) -> (Vec<RolloutItem>, Vec<ResponseItem>) {
    let mut rollout_items = Vec::new();
    let mut live_history = ContextManager::default();

    // Use the same turn_context source as record_initial_history so model_info matches reconstruction.
    let reconstruction_turn = session.new_default_turn().await;
    let initial_context = build_initial_context(session, &reconstruction_turn).await;
    for item in &initial_context {
        rollout_items.push(RolloutItem::ResponseItem(item.clone().into()));
    }
    live_history.record_items(
        initial_context.iter(),
        reconstruction_turn.model_info().truncation_policy.into(),
    );

    let user1 = user_message("first user");
    live_history.record_items(
        std::iter::once(&user1),
        reconstruction_turn.model_info().truncation_policy.into(),
    );
    rollout_items.push(RolloutItem::ResponseItem(user1.clone().into()));

    let assistant1 = assistant_message("assistant reply one");
    live_history.record_items(
        std::iter::once(&assistant1),
        reconstruction_turn.model_info().truncation_policy.into(),
    );
    rollout_items.push(RolloutItem::ResponseItem(assistant1.clone().into()));

    let summary1 = "summary one";
    let snapshot1 = raw_history_items(&live_history);
    let user_messages1 = collect_user_messages(&snapshot1);
    let rebuilt1 = compact::build_compacted_history(Vec::new(), &user_messages1, summary1);
    live_history.replace_annotated(rebuilt1);
    let (window_number, window_ids) = session.advance_auto_compact_window().await;
    rollout_items.push(RolloutItem::Compacted(Box::new(CompactedItem {
        message: summary1.to_string(),
        replacement_history: None,
        retained_context: None,
        guardian_history: None,
        mcp_resource_origins: None,
        window_number: Some(window_number),
        first_window_id: Some(window_ids.first_window_id.to_string()),
        previous_window_id: window_ids.previous_window_id.map(|id| id.to_string()),
        window_id: Some(window_ids.window_id.to_string()),
        compaction_response_id: None,
        latest_token_usage_record: None,
        resume_metadata: None,
    })));

    let user2 = user_message("second user");
    live_history.record_items(
        std::iter::once(&user2),
        reconstruction_turn.model_info().truncation_policy.into(),
    );
    rollout_items.push(RolloutItem::ResponseItem(user2.clone().into()));

    let assistant2 = assistant_message("assistant reply two");
    live_history.record_items(
        std::iter::once(&assistant2),
        reconstruction_turn.model_info().truncation_policy.into(),
    );
    rollout_items.push(RolloutItem::ResponseItem(assistant2.clone().into()));

    let summary2 = "summary two";
    let snapshot2 = raw_history_items(&live_history);
    let user_messages2 = collect_user_messages(&snapshot2);
    let rebuilt2 = compact::build_compacted_history(Vec::new(), &user_messages2, summary2);
    live_history.replace_annotated(rebuilt2);
    let (window_number, window_ids) = session.advance_auto_compact_window().await;
    rollout_items.push(RolloutItem::Compacted(Box::new(CompactedItem {
        message: summary2.to_string(),
        replacement_history: None,
        retained_context: None,
        guardian_history: None,
        mcp_resource_origins: None,
        window_number: Some(window_number),
        first_window_id: Some(window_ids.first_window_id.to_string()),
        previous_window_id: window_ids.previous_window_id.map(|id| id.to_string()),
        window_id: Some(window_ids.window_id.to_string()),
        compaction_response_id: None,
        latest_token_usage_record: None,
        resume_metadata: None,
    })));

    let user3 = user_message("third user");
    live_history.record_items(
        std::iter::once(&user3),
        reconstruction_turn.model_info().truncation_policy.into(),
    );
    rollout_items.push(RolloutItem::ResponseItem(user3.into()));

    let assistant3 = assistant_message("assistant reply three");
    live_history.record_items(
        std::iter::once(&assistant3),
        reconstruction_turn.model_info().truncation_policy.into(),
    );
    rollout_items.push(RolloutItem::ResponseItem(assistant3.into()));

    (rollout_items, raw_history_items(&live_history))
}

#[tokio::test]
async fn unified_exec_rejects_escalated_permissions_when_policy_not_on_request() {
    use crate::sandboxing::SandboxPermissions;
    use crate::turn_diff_tracker::TurnDiffTracker;
    use codex_protocol::protocol::AskForApproval;

    let (session, mut turn_context_raw) = make_session_and_context().await;
    Arc::make_mut(&mut turn_context_raw.config)
        .permissions
        .approval_policy
        .set(AskForApproval::Never)
        .expect("test setup should allow updating approval policy");
    let session = Arc::new(session);
    let turn_context = Arc::new(turn_context_raw);
    let step_context = StepContext::for_test(Arc::clone(&turn_context));
    let tracker = Arc::new(tokio::sync::Mutex::new(TurnDiffTracker::new()));

    let handler = ExecCommandHandler::default();
    let resp = handler
        .handle(ToolInvocation {
            session: Arc::clone(&session),
            turn: Arc::clone(&turn_context),
            step_context,
            cancellation_token: CancellationToken::new(),
            tracker: Arc::clone(&tracker),
            call_id: "exec-call".to_string(),
            tool_name: codex_tools::ToolName::plain("exec_command"),
            source: crate::tools::context::ToolCallSource::Direct,
            native_call: None,
            environments: Default::default(),
            payload: ToolPayload::Function {
                arguments: serde_json::json!({
                    "cmd": "echo hi",
                    "sandbox_permissions": SandboxPermissions::RequireEscalated,
                    "justification": "need unsandboxed execution",
                })
                .to_string(),
            },
        })
        .await;

    let Err(FunctionCallError::RespondToModel(output)) = resp else {
        panic!("expected error result");
    };

    let expected = format!(
        "approval policy is {policy:?}; reject command — you cannot ask for escalated permissions if the approval policy is {policy:?}",
        policy = turn_context.approval_policy()
    );

    pretty_assertions::assert_eq!(output, expected);
}

#[tokio::test]
async fn session_start_hooks_only_load_from_trusted_project_layers() -> std::io::Result<()> {
    let temp = tempfile::tempdir()?;
    let codex_home = temp.path().join("home");
    let project_root = temp.path().join("project");
    let nested = project_root.join("nested");
    let root_dot_codex = project_root.join(".codex");
    let nested_dot_codex = nested.join(".codex");

    std::fs::create_dir_all(&codex_home)?;
    std::fs::create_dir_all(&nested_dot_codex)?;
    std::fs::write(project_root.join(".git"), "gitdir: here")?;
    write_project_hooks(&root_dot_codex)?;
    write_project_hooks(&nested_dot_codex)?;
    write_project_trust_config(&codex_home, &[(&nested, TrustLevel::Trusted)]).await?;

    let config = ConfigBuilder::default()
        .codex_home(codex_home)
        .fallback_cwd(Some(nested))
        .build()
        .await?;

    let hook_list = codex_hooks::list_hooks(codex_hooks::HooksConfig {
        feature_enabled: true,
        config_layer_stack: Some(config.config_layer_stack.clone()),
        ..codex_hooks::HooksConfig::default()
    });
    let expected_source_path = codex_utils_absolute_path::AbsolutePathBuf::from_absolute_path(
        nested_dot_codex.join("hooks.json"),
    )?;
    assert_eq!(
        hook_list
            .hooks
            .iter()
            .map(|hook| &hook.source_path)
            .collect::<Vec<_>>(),
        vec![&expected_source_path],
    );
    assert_eq!(
        hook_list.hooks[0].trust_status,
        codex_protocol::protocol::HookTrustStatus::Untrusted
    );
    assert!(preview_session_start_hooks(&config).await?.is_empty());

    Ok(())
}

#[tokio::test]
async fn session_start_hooks_require_project_trust_without_config_toml() -> std::io::Result<()> {
    let temp = tempfile::tempdir()?;
    let project_root = temp.path().join("project");
    let nested = project_root.join("nested");
    let dot_codex = project_root.join(".codex");
    std::fs::create_dir_all(&nested)?;
    std::fs::write(project_root.join(".git"), "gitdir: here")?;
    write_project_hooks(&dot_codex)?;

    let cases = [
        ("unknown", Vec::<(&Path, TrustLevel)>::new(), 0_usize),
        (
            "untrusted",
            vec![(&project_root as &Path, TrustLevel::Untrusted)],
            0_usize,
        ),
        (
            "trusted",
            vec![(&project_root as &Path, TrustLevel::Trusted)],
            1_usize,
        ),
    ];

    for (name, trust_entries, expected_hooks) in cases {
        let codex_home = temp.path().join(format!("home_{name}"));
        std::fs::create_dir_all(&codex_home)?;
        write_project_trust_config(&codex_home, &trust_entries).await?;

        let config = ConfigBuilder::default()
            .codex_home(codex_home)
            .fallback_cwd(Some(nested.clone()))
            .build()
            .await?;

        let hook_list = codex_hooks::list_hooks(codex_hooks::HooksConfig {
            feature_enabled: true,
            config_layer_stack: Some(config.config_layer_stack.clone()),
            ..codex_hooks::HooksConfig::default()
        });
        assert_eq!(
            hook_list.hooks.len(),
            expected_hooks,
            "unexpected discovered hook count for {name}",
        );
        assert!(preview_session_start_hooks(&config).await?.is_empty());
        if expected_hooks == 1 {
            assert_eq!(
                hook_list.hooks[0].trust_status,
                codex_protocol::protocol::HookTrustStatus::Untrusted
            );
        }
    }

    Ok(())
}

#[tokio::test]
async fn rejected_mcp_refresh_then_corrected_user_config_blocks_ordinary_replacement() {
    let (session, _turn_context) = make_session_and_context().await;
    let base = session.get_config().await;
    let enterprise: McpServerConfig = serde_json::from_value(json!({
        "url": "https://enterprise.example/mcp", "auth": "ema_auth", "enabled": true
    }))
    .unwrap();
    let ordinary: McpServerConfig = serde_json::from_value(json!({
        "url": "https://ordinary.example/mcp", "enabled": true
    }))
    .unwrap();
    let mut initial = base.as_ref().clone();
    initial.mcp_servers = crate::config::Constrained::allow_any(HashMap::from([(
        "enterprise".to_string(),
        enterprise,
    )]));
    initial.config_layer_stack = ConfigLayerStack::new(
        vec![codex_config::ConfigLayerEntry::new(
            codex_config::ConfigLayerSource::User {
                file: initial.codex_home.join(CONFIG_TOML_FILE),
                profile: None,
            },
            toml::from_str("features = 'invalid retained features'").unwrap(),
        )],
        base.config_layer_stack.requirements().clone(),
        base.config_layer_stack.requirements_toml().clone(),
    )
    .unwrap();
    session
        .state
        .lock()
        .await
        .session_configuration
        .original_config_do_not_use = Arc::new(initial);
    let current = session.get_config().await;
    let mut corrected = base.as_ref().clone();
    corrected.mcp_servers = crate::config::Constrained::allow_any(HashMap::from([(
        "enterprise".to_string(),
        ordinary,
    )]));
    assert_eq!(
        session.refresh_mcp_config(current, corrected.clone()).await,
        crate::ConfigRefreshOutcome::Rejected
    );
    let rejected = session.get_config().await;
    assert!(rejected.mcp_enterprise_managed_auth.is_none());
    assert_eq!(
        session.refresh_runtime_config(rejected, corrected).await,
        crate::ConfigRefreshOutcome::Published
    );
    let final_config = session.get_config().await;
    assert!(
        !final_config
            .mcp_servers
            .get()
            .get("enterprise")
            .is_some_and(|server| server.enabled),
        "rejection must not permit same-name ordinary-auth replacement in this session"
    );
}
