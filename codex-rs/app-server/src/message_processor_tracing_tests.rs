use super::ConnectionSessionState;
use super::MessageProcessor;
use super::MessageProcessorArgs;
use crate::analytics_utils::analytics_events_client_from_config;
use crate::config_manager::ConfigManager;
use crate::outgoing_message::ConnectionId;
use crate::outgoing_message::OutgoingMessageSender;
use crate::transport::AppServerTransport;
use anyhow::Result;
use app_test_support::create_mock_responses_server_repeating_assistant;
use app_test_support::write_mock_responses_config_toml;
use codex_analytics::AppServerRpcTransport;
use codex_app_server_protocol::ClientInfo;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::InitializeCapabilities;
use codex_app_server_protocol::InitializeParams;
use codex_app_server_protocol::InitializeResponse;
use codex_app_server_protocol::JSONRPCErrorError;
use codex_app_server_protocol::JSONRPCRequest;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::ServerNotification;
use codex_app_server_protocol::ThreadStartParams;
use codex_app_server_protocol::ThreadStartResponse;
use codex_app_server_protocol::TurnEnvironmentParams;
use codex_app_server_protocol::TurnStartParams;
use codex_app_server_protocol::TurnStartResponse;
use codex_app_server_protocol::TurnStatus;
use codex_app_server_protocol::UserInput;
use codex_arg0::Arg0DispatchPaths;
use codex_config::CloudConfigBundleLoader;
use codex_config::LoaderOverrides;
use codex_core::config::Config;
use codex_core::config::ConfigBuilder;
use codex_exec_server::EnvironmentManager;
use codex_exec_server::LOCAL_ENVIRONMENT_ID;
use codex_feedback::CodexFeedback;
use codex_login::AuthManager;
use codex_login::CodexAuth;
use codex_login::ExternalAuth;
use codex_login::ExternalAuthFuture;
use codex_login::ExternalAuthRefreshContext;
use codex_protocol::ThreadId;
use codex_protocol::automatic_turn::AutomaticTurnProvenance;
use codex_protocol::protocol::CodexErrorInfo;
use codex_protocol::protocol::ErrorEvent;
use codex_protocol::protocol::Event;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::W3cTraceContext;
use codex_state::StateRuntime;
use codex_utils_absolute_path::test_support::PathExt;
use opentelemetry::global;
use opentelemetry::trace::SpanId;
use opentelemetry::trace::SpanKind;
use opentelemetry::trace::TraceId;
use opentelemetry::trace::TracerProvider as _;
use opentelemetry_sdk::propagation::TraceContextPropagator;
use opentelemetry_sdk::trace::InMemorySpanExporter;
use opentelemetry_sdk::trace::SdkTracerProvider;
use opentelemetry_sdk::trace::SpanData;
use pretty_assertions::assert_eq;
use serial_test::serial;
use std::collections::BTreeMap;
use std::collections::VecDeque;
use std::future::Future;
use std::path::Path;
use std::sync::Arc;
use std::sync::OnceLock;
use std::task::Poll;
use tempfile::TempDir;
use tokio::sync::mpsc;
use tracing_subscriber::layer::SubscriberExt;
use wiremock::MockServer;

const TEST_CONNECTION_ID: ConnectionId = ConnectionId(7);

struct TestTracing {
    exporter: InMemorySpanExporter,
    provider: SdkTracerProvider,
}

struct RemoteTrace {
    trace_id: TraceId,
    parent_span_id: SpanId,
    context: W3cTraceContext,
}

impl RemoteTrace {
    fn new(trace_id: &str, parent_span_id: &str) -> Self {
        let trace_id = TraceId::from_hex(trace_id).expect("trace id");
        let parent_span_id = SpanId::from_hex(parent_span_id).expect("parent span id");
        let context = W3cTraceContext {
            traceparent: Some(format!("00-{trace_id}-{parent_span_id}-01")),
            tracestate: Some("vendor=value".to_string()),
        };

        Self {
            trace_id,
            parent_span_id,
            context,
        }
    }
}

fn init_test_tracing() -> &'static TestTracing {
    static TEST_TRACING: OnceLock<TestTracing> = OnceLock::new();
    TEST_TRACING.get_or_init(|| {
        let exporter = InMemorySpanExporter::default();
        let provider = SdkTracerProvider::builder()
            .with_simple_exporter(exporter.clone())
            .build();
        let tracer = provider.tracer("codex-app-server-message-processor-tests");
        global::set_text_map_propagator(TraceContextPropagator::new());
        let subscriber =
            tracing_subscriber::registry().with(tracing_opentelemetry::layer().with_tracer(tracer));
        tracing::subscriber::set_global_default(subscriber)
            .expect("global tracing subscriber should only be installed once");
        TestTracing { exporter, provider }
    })
}

fn request_from_client_request(request: ClientRequest) -> JSONRPCRequest {
    serde_json::from_value(serde_json::to_value(request).expect("serialize client request"))
        .expect("client request should convert to JSON-RPC")
}

struct TracingHarness {
    server: MockServer,
    _codex_home: TempDir,
    auth_manager: Arc<AuthManager>,
    processor: Arc<MessageProcessor>,
    outgoing_rx: mpsc::Receiver<crate::outgoing_message::OutgoingEnvelope>,
    pending_outgoing: VecDeque<crate::outgoing_message::OutgoingEnvelope>,
    session: Arc<ConnectionSessionState>,
    tracing: &'static TestTracing,
    state_db: Option<codex_rollout::StateDbHandle>,
}

impl TracingHarness {
    async fn new() -> Result<Self> {
        Self::new_inner(/*with_state_db*/ false).await
    }

    async fn new_with_state_db() -> Result<Self> {
        Self::new_inner(/*with_state_db*/ true).await
    }

    async fn new_inner(with_state_db: bool) -> Result<Self> {
        let server = create_mock_responses_server_repeating_assistant("Done").await;
        let codex_home = TempDir::new()?;
        let config = Arc::new(build_test_config(codex_home.path(), &server.uri()).await?);
        let state_db = if with_state_db {
            Some(
                StateRuntime::init(
                    codex_state::SqliteConfig::new_for_testing(codex_home.path().abs()),
                    "mock_provider".to_string(),
                )
                .await?,
            )
        } else {
            None
        };
        let (processor, outgoing_rx, auth_manager) =
            build_test_processor(config, state_db.clone()).await;
        let tracing = init_test_tracing();
        tracing.exporter.reset();
        tracing::callsite::rebuild_interest_cache();
        let mut harness = Self {
            server,
            _codex_home: codex_home,
            auth_manager,
            processor,
            outgoing_rx,
            pending_outgoing: VecDeque::new(),
            session: Arc::new(ConnectionSessionState::new()),
            tracing,
            state_db,
        };

        let _: InitializeResponse = harness
            .request(
                ClientRequest::Initialize {
                    request_id: RequestId::Integer(1),
                    params: InitializeParams {
                        client_info: ClientInfo {
                            name: "codex-app-server-tests".to_string(),
                            title: None,
                            version: "0.1.0".to_string(),
                        },
                        capabilities: Some(InitializeCapabilities {
                            experimental_api: true,
                            ..Default::default()
                        }),
                    },
                },
                /*trace*/ None,
            )
            .await;
        assert!(harness.session.initialized());

        Ok(harness)
    }

    fn reset_tracing(&self) {
        self.tracing.exporter.reset();
    }

    async fn shutdown(self) {
        self.processor.shutdown_threads().await;
        self.processor.drain_background_tasks().await;
    }

    async fn request<T>(&mut self, request: ClientRequest, trace: Option<W3cTraceContext>) -> T
    where
        T: serde::de::DeserializeOwned,
    {
        let request_id = match request.id() {
            RequestId::Integer(request_id) => *request_id,
            request_id => panic!("expected integer request id in test harness, got {request_id:?}"),
        };
        let mut request = request_from_client_request(request);
        request.trace = trace;

        self.processor
            .process_request(
                TEST_CONNECTION_ID,
                request,
                &AppServerTransport::Stdio,
                Arc::clone(&self.session),
            )
            .await;
        read_response(
            &mut self.pending_outgoing,
            &mut self.outgoing_rx,
            request_id,
        )
        .await
    }

    async fn start_thread(
        &mut self,
        request_id: i64,
        trace: Option<W3cTraceContext>,
    ) -> ThreadStartResponse {
        let response = self
            .request(
                ClientRequest::ThreadStart {
                    request_id: RequestId::Integer(request_id),
                    params: ThreadStartParams {
                        ephemeral: Some(true),
                        ..ThreadStartParams::default()
                    },
                },
                trace,
            )
            .await;
        read_thread_started_notification(&mut self.pending_outgoing, &mut self.outgoing_rx).await;
        response
    }

    async fn request_error(
        &mut self,
        request: ClientRequest,
        trace: Option<W3cTraceContext>,
    ) -> JSONRPCErrorError {
        let request_id = match request.id() {
            RequestId::Integer(request_id) => *request_id,
            request_id => panic!("expected integer request id in test harness, got {request_id:?}"),
        };
        let mut request = request_from_client_request(request);
        request.trace = trace;
        self.processor
            .process_request(
                TEST_CONNECTION_ID,
                request,
                &AppServerTransport::Stdio,
                Arc::clone(&self.session),
            )
            .await;
        read_error(
            &mut self.pending_outgoing,
            &mut self.outgoing_rx,
            request_id,
        )
        .await
    }
}

async fn build_test_config(codex_home: &Path, server_uri: &str) -> Result<Config> {
    write_mock_responses_config_toml(
        codex_home,
        server_uri,
        &BTreeMap::new(),
        /*auto_compact_limit*/ 8_192,
        Some(false),
        "mock_provider",
        "compact",
    )?;

    Ok(ConfigBuilder::default()
        .codex_home(codex_home.to_path_buf())
        .build()
        .await?)
}

async fn build_test_processor(
    config: Arc<Config>,
    state_db: Option<codex_rollout::StateDbHandle>,
) -> (
    Arc<MessageProcessor>,
    mpsc::Receiver<crate::outgoing_message::OutgoingEnvelope>,
    Arc<AuthManager>,
) {
    let (outgoing_tx, outgoing_rx) = mpsc::channel(16);
    let auth_manager =
        AuthManager::shared_from_config(config.as_ref(), /*enable_codex_api_key_env*/ false).await;
    let config_manager = ConfigManager::new(
        config.codex_home.to_path_buf(),
        Vec::new(),
        LoaderOverrides::default(),
        /*strict_config*/ false,
        CloudConfigBundleLoader::default(),
        Arg0DispatchPaths::default(),
        Arc::new(codex_config::NoopThreadConfigLoader),
    );
    let analytics_events_client =
        analytics_events_client_from_config(Arc::clone(&auth_manager), config.as_ref());
    let outgoing = Arc::new(OutgoingMessageSender::new(
        outgoing_tx,
        analytics_events_client.clone(),
    ));
    let processor = Arc::new(MessageProcessor::new(MessageProcessorArgs {
        outgoing,
        analytics_events_client,
        arg0_paths: Arg0DispatchPaths::default(),
        config,
        config_manager,
        environment_manager: Arc::new(EnvironmentManager::default_for_tests()),
        feedback: CodexFeedback::new(),
        log_db: None,
        state_db,
        config_warnings: Vec::new(),
        session_source: SessionSource::VSCode,
        auth_manager: Arc::clone(&auth_manager),
        installation_id: "11111111-1111-4111-8111-111111111111".to_string(),
        code_mode_session_provider: None,
        rpc_transport: AppServerRpcTransport::Stdio,
        remote_control_handle: None,
        plugin_startup_tasks: crate::PluginStartupTasks::Start,
    }));
    (processor, outgoing_rx, auth_manager)
}

#[derive(Clone)]
struct StaticExternalAuth(CodexAuth);

impl ExternalAuth for StaticExternalAuth {
    fn resolve(&self) -> ExternalAuthFuture<'_, CodexAuth> {
        Box::pin(async { Ok(self.0.clone()) })
    }

    fn refresh(&self, _context: ExternalAuthRefreshContext) -> ExternalAuthFuture<'_, CodexAuth> {
        Box::pin(async { Ok(self.0.clone()) })
    }
}

fn into_server_notification(
    envelope: crate::outgoing_message::OutgoingEnvelope,
) -> Option<ServerNotification> {
    let message = match envelope {
        crate::outgoing_message::OutgoingEnvelope::ToConnection {
            connection_id,
            message,
            ..
        } if connection_id == TEST_CONNECTION_ID => message,
        crate::outgoing_message::OutgoingEnvelope::Broadcast { message } => message,
        _ => return None,
    };
    let crate::outgoing_message::OutgoingMessage::AppServerNotification(notification) = message
    else {
        return None;
    };
    Some(notification.notification)
}

async fn read_turn_denial_notifications(
    pending_outgoing: &mut VecDeque<crate::outgoing_message::OutgoingEnvelope>,
    outgoing_rx: &mut mpsc::Receiver<crate::outgoing_message::OutgoingEnvelope>,
    turn_id: &str,
) -> (usize, usize, TurnStatus, bool) {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(/*secs*/ 5);
    let mut errors = 0;
    let mut terminals = 0;
    let mut terminal_status = None;
    let mut unauthorized = false;
    loop {
        let now = tokio::time::Instant::now();
        let wait = if terminal_status.is_some() {
            std::time::Duration::from_millis(/*millis*/ 100)
        } else {
            deadline.saturating_duration_since(now)
        };
        if wait.is_zero() {
            panic!("timed out waiting for terminal notification for turn {turn_id}");
        }
        let envelope = if let Some(envelope) = take_pending_outgoing(pending_outgoing, |envelope| {
            is_turn_notification_for(envelope, turn_id)
        }) {
            envelope
        } else {
            match tokio::time::timeout(wait, outgoing_rx.recv()).await {
                Ok(Some(envelope)) => envelope,
                Ok(None) => panic!("outgoing channel closed"),
                Err(_) if terminal_status.is_some() => break,
                Err(_) => panic!("timed out waiting for terminal notification for turn {turn_id}"),
            }
        };
        if !is_turn_notification_for(&envelope, turn_id) {
            pending_outgoing.push_back(envelope);
            continue;
        }
        let Some(notification) = into_server_notification(envelope) else {
            continue;
        };
        match notification {
            ServerNotification::Error(notification) if notification.turn_id == turn_id => {
                errors += 1;
                unauthorized |= notification
                    .error
                    .codex_error_info
                    .as_ref()
                    .is_some_and(|info| {
                        matches!(
                            info,
                            codex_app_server_protocol::CodexErrorInfo::Unauthorized
                        )
                    });
            }
            ServerNotification::TurnCompleted(notification) if notification.turn.id == turn_id => {
                terminals += 1;
                terminal_status = Some(notification.turn.status);
            }
            _ => {}
        }
    }
    (
        errors,
        terminals,
        terminal_status.expect("terminal notification"),
        unauthorized,
    )
}

fn run_current_thread_test_with_stack<F>(name: &str, future: F) -> Result<()>
where
    F: Future<Output = Result<()>> + Send + 'static,
{
    const TEST_STACK_SIZE_BYTES: usize = 4 * 1024 * 1024;

    let handle = std::thread::Builder::new()
        .name(name.to_string())
        .stack_size(TEST_STACK_SIZE_BYTES)
        .spawn(move || -> Result<()> {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            runtime.block_on(Box::pin(future))
        })?;

    match handle.join() {
        Ok(result) => result,
        Err(_) => Err(anyhow::anyhow!("{name} thread panicked")),
    }
}

fn span_attr<'a>(span: &'a SpanData, key: &str) -> Option<&'a str> {
    span.attributes
        .iter()
        .find(|kv| kv.key.as_str() == key)
        .and_then(|kv| match &kv.value {
            opentelemetry::Value::String(value) => Some(value.as_str()),
            _ => None,
        })
}

fn find_rpc_span_with_trace<'a>(
    spans: &'a [SpanData],
    kind: SpanKind,
    method: &str,
    trace_id: TraceId,
) -> &'a SpanData {
    spans
        .iter()
        .find(|span| {
            span.span_kind == kind
                && span_attr(span, "rpc.system") == Some("jsonrpc")
                && span_attr(span, "rpc.method") == Some(method)
                && span.span_context.trace_id() == trace_id
        })
        .unwrap_or_else(|| {
            panic!(
                "missing {kind:?} span for rpc.method={method} trace={trace_id}; exported spans:\n{}",
                format_spans(spans)
            )
        })
}

fn find_span_with_trace<'a, F>(
    spans: &'a [SpanData],
    trace_id: TraceId,
    description: &str,
    predicate: F,
) -> &'a SpanData
where
    F: Fn(&SpanData) -> bool,
{
    spans
        .iter()
        .find(|span| span.span_context.trace_id() == trace_id && predicate(span))
        .unwrap_or_else(|| {
            panic!(
                "missing span matching {description} for trace={trace_id}; exported spans:\n{}",
                format_spans(spans)
            )
        })
}

fn format_spans(spans: &[SpanData]) -> String {
    spans
        .iter()
        .map(|span| {
            let rpc_method = span_attr(span, "rpc.method").unwrap_or("-");
            format!(
                "name={} span_id={} kind={:?} parent={} trace={} rpc.method={}",
                span.name,
                span.span_context.span_id(),
                span.span_kind,
                span.parent_span_id,
                span.span_context.trace_id(),
                rpc_method
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn span_depth_from_ancestor(
    spans: &[SpanData],
    child: &SpanData,
    ancestor: &SpanData,
) -> Option<usize> {
    let ancestor_span_id = ancestor.span_context.span_id();
    let mut parent_span_id = child.parent_span_id;
    let mut depth = 1;
    while parent_span_id != SpanId::INVALID {
        if parent_span_id == ancestor_span_id {
            return Some(depth);
        }
        let Some(parent_span) = spans
            .iter()
            .find(|span| span.span_context.span_id() == parent_span_id)
        else {
            break;
        };
        parent_span_id = parent_span.parent_span_id;
        depth += 1;
    }

    None
}

fn assert_span_descends_from(spans: &[SpanData], child: &SpanData, ancestor: &SpanData) {
    if span_depth_from_ancestor(spans, child, ancestor).is_some() {
        return;
    }

    panic!(
        "span {} does not descend from {}; exported spans:\n{}",
        child.name,
        ancestor.name,
        format_spans(spans)
    );
}

fn assert_has_internal_descendant_at_min_depth(
    spans: &[SpanData],
    ancestor: &SpanData,
    min_depth: usize,
) {
    if spans.iter().any(|span| {
        span.span_kind == SpanKind::Internal
            && span.span_context.trace_id() == ancestor.span_context.trace_id()
            && span_depth_from_ancestor(spans, span, ancestor)
                .is_some_and(|depth| depth >= min_depth)
    }) {
        return;
    }

    panic!(
        "missing internal descendant at depth >= {min_depth} below {}; exported spans:\n{}",
        ancestor.name,
        format_spans(spans)
    );
}

fn take_pending_outgoing<F>(
    pending_outgoing: &mut VecDeque<crate::outgoing_message::OutgoingEnvelope>,
    predicate: F,
) -> Option<crate::outgoing_message::OutgoingEnvelope>
where
    F: FnMut(&crate::outgoing_message::OutgoingEnvelope) -> bool,
{
    let index = pending_outgoing.iter().position(predicate)?;
    pending_outgoing.remove(index)
}

fn as_server_notification(
    envelope: &crate::outgoing_message::OutgoingEnvelope,
) -> Option<&ServerNotification> {
    let message = match envelope {
        crate::outgoing_message::OutgoingEnvelope::ToConnection {
            connection_id,
            message,
            ..
        } if *connection_id == TEST_CONNECTION_ID => message,
        crate::outgoing_message::OutgoingEnvelope::Broadcast { message } => message,
        _ => return None,
    };
    let crate::outgoing_message::OutgoingMessage::AppServerNotification(notification) = message
    else {
        return None;
    };
    Some(&notification.notification)
}

fn is_response_for_request(
    envelope: &crate::outgoing_message::OutgoingEnvelope,
    request_id: i64,
) -> bool {
    let crate::outgoing_message::OutgoingEnvelope::ToConnection {
        connection_id,
        message,
        ..
    } = envelope
    else {
        return false;
    };
    if *connection_id != TEST_CONNECTION_ID {
        return false;
    }
    matches!(
        message,
        crate::outgoing_message::OutgoingMessage::Response(response)
            if response.id == RequestId::Integer(request_id)
    )
}

fn is_error_for_request(
    envelope: &crate::outgoing_message::OutgoingEnvelope,
    request_id: i64,
) -> bool {
    let crate::outgoing_message::OutgoingEnvelope::ToConnection {
        connection_id,
        message,
        ..
    } = envelope
    else {
        return false;
    };
    if *connection_id != TEST_CONNECTION_ID {
        return false;
    }
    matches!(
        message,
        crate::outgoing_message::OutgoingMessage::Error(error)
            if error.id == RequestId::Integer(request_id)
    )
}

fn is_thread_started_notification(envelope: &crate::outgoing_message::OutgoingEnvelope) -> bool {
    matches!(
        as_server_notification(envelope),
        Some(ServerNotification::ThreadStarted(_))
    )
}

fn is_turn_notification_for(
    envelope: &crate::outgoing_message::OutgoingEnvelope,
    turn_id: &str,
) -> bool {
    as_server_notification(envelope).is_some_and(|notification| match notification {
        ServerNotification::Error(notification) => notification.turn_id == turn_id,
        ServerNotification::TurnCompleted(notification) => notification.turn.id == turn_id,
        _ => false,
    })
}

async fn read_response<T: serde::de::DeserializeOwned>(
    pending_outgoing: &mut VecDeque<crate::outgoing_message::OutgoingEnvelope>,
    outgoing_rx: &mut mpsc::Receiver<crate::outgoing_message::OutgoingEnvelope>,
    request_id: i64,
) -> T {
    loop {
        let envelope = if let Some(envelope) = take_pending_outgoing(pending_outgoing, |envelope| {
            is_response_for_request(envelope, request_id)
        }) {
            envelope
        } else {
            tokio::time::timeout(std::time::Duration::from_secs(5), outgoing_rx.recv())
                .await
                .expect("timed out waiting for response")
                .expect("outgoing channel closed")
        };
        if !is_response_for_request(&envelope, request_id) {
            pending_outgoing.push_back(envelope);
            continue;
        }
        let crate::outgoing_message::OutgoingEnvelope::ToConnection {
            connection_id,
            message,
            ..
        } = envelope
        else {
            continue;
        };
        if connection_id != TEST_CONNECTION_ID {
            continue;
        }
        let crate::outgoing_message::OutgoingMessage::Response(response) = message else {
            continue;
        };
        if response.id != RequestId::Integer(request_id) {
            continue;
        }
        return serde_json::from_value(response.result)
            .expect("response payload should deserialize");
    }
}

async fn read_error(
    pending_outgoing: &mut VecDeque<crate::outgoing_message::OutgoingEnvelope>,
    outgoing_rx: &mut mpsc::Receiver<crate::outgoing_message::OutgoingEnvelope>,
    request_id: i64,
) -> JSONRPCErrorError {
    loop {
        let envelope = if let Some(envelope) = take_pending_outgoing(pending_outgoing, |envelope| {
            is_error_for_request(envelope, request_id)
        }) {
            envelope
        } else {
            tokio::time::timeout(std::time::Duration::from_secs(5), outgoing_rx.recv())
                .await
                .expect("timed out waiting for error")
                .expect("outgoing channel closed")
        };
        if !is_error_for_request(&envelope, request_id) {
            pending_outgoing.push_back(envelope);
            continue;
        }
        let crate::outgoing_message::OutgoingEnvelope::ToConnection {
            connection_id,
            message,
            ..
        } = envelope
        else {
            continue;
        };
        if connection_id != TEST_CONNECTION_ID {
            continue;
        }
        let crate::outgoing_message::OutgoingMessage::Error(error) = message else {
            continue;
        };
        if error.id == RequestId::Integer(request_id) {
            return error.error;
        }
    }
}

async fn read_thread_started_notification(
    pending_outgoing: &mut VecDeque<crate::outgoing_message::OutgoingEnvelope>,
    outgoing_rx: &mut mpsc::Receiver<crate::outgoing_message::OutgoingEnvelope>,
) {
    loop {
        let envelope = if let Some(envelope) =
            take_pending_outgoing(pending_outgoing, is_thread_started_notification)
        {
            envelope
        } else {
            tokio::time::timeout(std::time::Duration::from_secs(5), outgoing_rx.recv())
                .await
                .expect("timed out waiting for thread/started notification")
                .expect("outgoing channel closed")
        };
        if !is_thread_started_notification(&envelope) {
            pending_outgoing.push_back(envelope);
            continue;
        }
        match envelope {
            crate::outgoing_message::OutgoingEnvelope::ToConnection {
                connection_id,
                message,
                ..
            } => {
                if connection_id != TEST_CONNECTION_ID {
                    continue;
                }
                let crate::outgoing_message::OutgoingMessage::AppServerNotification(notification) =
                    message
                else {
                    continue;
                };
                if matches!(
                    notification.notification,
                    codex_app_server_protocol::ServerNotification::ThreadStarted(_)
                ) {
                    return;
                }
            }
            crate::outgoing_message::OutgoingEnvelope::Broadcast { message } => {
                let crate::outgoing_message::OutgoingMessage::AppServerNotification(notification) =
                    message
                else {
                    continue;
                };
                if matches!(
                    notification.notification,
                    codex_app_server_protocol::ServerNotification::ThreadStarted(_)
                ) {
                    return;
                }
            }
        }
    }
}

async fn wait_for_exported_spans<F>(tracing: &TestTracing, predicate: F) -> Vec<SpanData>
where
    F: Fn(&[SpanData]) -> bool,
{
    let mut last_spans = Vec::new();
    for _ in 0..200 {
        tokio::task::yield_now().await;
        tracing
            .provider
            .force_flush()
            .expect("force flush should succeed");
        let spans = tracing.exporter.get_finished_spans().expect("span export");
        last_spans = spans.clone();
        if predicate(&spans) {
            return spans;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    panic!(
        "timed out waiting for expected exported spans:\n{}",
        format_spans(&last_spans)
    );
}

async fn wait_for_new_exported_spans<F>(
    tracing: &TestTracing,
    baseline_len: usize,
    predicate: F,
) -> Vec<SpanData>
where
    F: Fn(&[SpanData]) -> bool,
{
    let spans = wait_for_exported_spans(tracing, |spans| {
        spans.len() > baseline_len && predicate(&spans[baseline_len..])
    })
    .await;
    spans.into_iter().skip(baseline_len).collect()
}

async fn wait_for_automatic_turn_row(
    pool: &sqlx::SqlitePool,
    thread_id: &str,
    client_user_message_id: &str,
) -> Result<(String, String, String, String, i64, i64, String)> {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(/*secs*/ 5);
    loop {
        let row = sqlx::query_as::<_, (String, String, String, String, i64, i64, String)>(
            "SELECT thread_id, client_user_message_id, trigger_turn_id, turn_id, attempt, max_attempts, outcome FROM usage_automatic_turns WHERE thread_id = ? AND client_user_message_id = ? ORDER BY id",
        )
        .bind(thread_id)
        .bind(client_user_message_id)
        .fetch_optional(pool)
        .await?;
        if let Some(row) = row {
            if row.6 != "started" {
                return Ok(row);
            }
        }
        let now = tokio::time::Instant::now();
        if now >= deadline {
            return Err(anyhow::anyhow!(
                "timed out waiting for automatic-turn state projection"
            ));
        }
        tokio::time::sleep((deadline - now).min(std::time::Duration::from_millis(/*millis*/ 50)))
            .await;
    }
}

async fn prepare_automatic_turn(
    harness: &TracingHarness,
    thread_id: ThreadId,
    trigger_turn_id: &str,
) -> Result<(String, codex_core::ThreadConfigSnapshot)> {
    let principal =
        crate::outgoing_message::automatic_turn_connection_principal(TEST_CONNECTION_ID);
    let state_db = harness
        .state_db
        .as_ref()
        .expect("state db enabled for this harness");
    state_db
        .record_automatic_turn_event_with_principal(
            thread_id,
            &Event {
                id: trigger_turn_id.to_string(),
                msg: EventMsg::Error(ErrorEvent {
                    message: "blocked".to_string(),
                    codex_error_info: Some(CodexErrorInfo::CyberPolicy),
                }),
            },
            Some(&principal),
        )
        .await;
    let capability = state_db
        .automatic_turn_capability(thread_id, trigger_turn_id)
        .await
        .expect("policy event should issue a server capability");
    let client_user_message_id = AutomaticTurnProvenance::policy_retry(
        thread_id,
        trigger_turn_id,
        /*attempt*/ 1,
        /*max_attempts*/ 3,
        capability.clone(),
    )
    .expect("valid automatic-turn provenance")
    .to_client_user_message_id()
    .expect("valid automatic-turn provenance");
    let context_snapshot = harness
        .processor
        .config_snapshot_for_test(thread_id)
        .await
        .expect("thread should remain loaded");
    let context_fingerprint = codex_core::automatic_turn_context_fingerprint(&context_snapshot);
    state_db
        .bind_automatic_turn_capability_contract(
            thread_id,
            trigger_turn_id,
            &capability,
            "start",
            /*expected_turn_id*/ None,
            &context_fingerprint,
        )
        .await?;
    Ok((client_user_message_id, context_snapshot))
}

fn automatic_turn_start_params(
    thread_id: String,
    client_user_message_id: String,
) -> TurnStartParams {
    TurnStartParams {
        thread_id,
        client_user_message_id: Some(client_user_message_id),
        input: vec![UserInput::Text {
            text: "continue".to_string(),
            text_elements: Vec::new(),
        }],
        ..TurnStartParams::default()
    }
}

#[test]
#[serial(app_server_tracing)]
fn thread_start_jsonrpc_span_exports_server_span_and_parents_children() -> Result<()> {
    run_current_thread_test_with_stack(
        "thread_start_jsonrpc_span_exports_server_span_and_parents_children",
        async {
            let mut harness = TracingHarness::new().await?;

            let RemoteTrace {
                trace_id: remote_trace_id,
                parent_span_id: remote_parent_span_id,
                context: remote_trace,
                ..
            } = RemoteTrace::new("00000000000000000000000000000011", "0000000000000022");

            let _: ThreadStartResponse = harness
                .start_thread(/*request_id*/ 20_002, /*trace*/ None)
                .await;
            let untraced_spans = wait_for_exported_spans(harness.tracing, |spans| {
                spans.iter().any(|span| {
                    span.span_kind == SpanKind::Server
                        && span_attr(span, "rpc.method") == Some("thread/start")
                })
            })
            .await;
            let untraced_server_span = find_rpc_span_with_trace(
                &untraced_spans,
                SpanKind::Server,
                "thread/start",
                untraced_spans
                    .iter()
                    .rev()
                    .find(|span| {
                        span.span_kind == SpanKind::Server
                            && span_attr(span, "rpc.system") == Some("jsonrpc")
                            && span_attr(span, "rpc.method") == Some("thread/start")
                    })
                    .unwrap_or_else(|| {
                        panic!(
                            "missing latest thread/start server span; exported spans:\n{}",
                            format_spans(&untraced_spans)
                        )
                    })
                    .span_context
                    .trace_id(),
            );
            assert_has_internal_descendant_at_min_depth(
                &untraced_spans,
                untraced_server_span,
                /*min_depth*/ 1,
            );

            let baseline_len = untraced_spans.len();
            let _: ThreadStartResponse = harness
                .start_thread(/*request_id*/ 20_003, Some(remote_trace))
                .await;
            let spans = wait_for_new_exported_spans(harness.tracing, baseline_len, |spans| {
                spans.iter().any(|span| {
                    span.span_kind == SpanKind::Server
                        && span_attr(span, "rpc.method") == Some("thread/start")
                        && span.span_context.trace_id() == remote_trace_id
                }) && spans.iter().any(|span| {
                    span.name.as_ref() == "app_server.thread_start.notify_started"
                        && span.span_context.trace_id() == remote_trace_id
                })
            })
            .await;

            let server_request_span =
                find_rpc_span_with_trace(&spans, SpanKind::Server, "thread/start", remote_trace_id);
            assert_eq!(server_request_span.name.as_ref(), "thread/start");
            assert_eq!(server_request_span.parent_span_id, remote_parent_span_id);
            assert!(server_request_span.parent_span_is_remote);
            assert_eq!(server_request_span.span_context.trace_id(), remote_trace_id);
            assert_ne!(server_request_span.span_context.span_id(), SpanId::INVALID);
            assert_has_internal_descendant_at_min_depth(
                &spans,
                server_request_span,
                /*min_depth*/ 1,
            );
            assert_has_internal_descendant_at_min_depth(
                &spans,
                server_request_span,
                /*min_depth*/ 2,
            );
            harness.shutdown().await;

            Ok(())
        },
    )
}

#[tokio::test(flavor = "current_thread")]
#[serial(app_server_tracing)]
async fn turn_start_jsonrpc_span_parents_core_turn_spans() -> Result<()> {
    let mut harness = TracingHarness::new().await?;
    let thread_start_response = harness.start_thread(/*request_id*/ 2, /*trace*/ None).await;
    let thread_id = thread_start_response.thread.id.clone();

    harness.reset_tracing();

    let RemoteTrace {
        trace_id: remote_trace_id,
        parent_span_id: remote_parent_span_id,
        context: remote_trace,
    } = RemoteTrace::new("00000000000000000000000000000077", "0000000000000088");
    let turn_start_response: TurnStartResponse = harness
        .request(
            ClientRequest::TurnStart {
                request_id: RequestId::Integer(3),
                params: TurnStartParams {
                    environments: None,
                    thread_id,
                    client_user_message_id: None,
                    input: vec![UserInput::Text {
                        text: "hello".to_string(),
                        text_elements: Vec::new(),
                    }],
                    responsesapi_client_metadata: None,
                    additional_context: None,
                    cwd: None,
                    runtime_workspace_roots: None,
                    approval_policy: None,
                    sandbox_policy: None,
                    permissions: None,
                    approvals_reviewer: None,
                    model: None,
                    service_tier: None,
                    effort: None,
                    summary: None,
                    personality: None,
                    output_schema: None,
                    collaboration_mode: None,
                    multi_agent_mode: None,
                },
            },
            Some(remote_trace),
        )
        .await;
    let spans = wait_for_exported_spans(harness.tracing, |spans| {
        spans.iter().any(|span| {
            span.span_kind == SpanKind::Server
                && span_attr(span, "rpc.method") == Some("turn/start")
                && span.span_context.trace_id() == remote_trace_id
        }) && spans.iter().any(|span| {
            span_attr(span, "codex.op") == Some("user_input")
                && span.span_context.trace_id() == remote_trace_id
        })
    })
    .await;

    let server_request_span =
        find_rpc_span_with_trace(&spans, SpanKind::Server, "turn/start", remote_trace_id);
    let core_turn_span =
        find_span_with_trace(&spans, remote_trace_id, "codex.op=user_input", |span| {
            span_attr(span, "codex.op") == Some("user_input")
        });

    assert_eq!(server_request_span.parent_span_id, remote_parent_span_id);
    assert!(server_request_span.parent_span_is_remote);
    assert_eq!(server_request_span.span_context.trace_id(), remote_trace_id);
    assert_eq!(
        span_attr(server_request_span, "turn.id"),
        Some(turn_start_response.turn.id.as_str())
    );
    assert_span_descends_from(&spans, core_turn_span, server_request_span);
    harness.shutdown().await;

    Ok(())
}

#[tokio::test(flavor = "current_thread")]
#[serial(app_server_tracing)]
async fn turn_start_projects_validated_automatic_user_message() -> Result<()> {
    let mut harness = TracingHarness::new_with_state_db().await?;
    let thread_start_response = harness
        .start_thread(/*request_id*/ 30_001, /*trace*/ None)
        .await;
    let thread_id = thread_start_response.thread.id.clone();
    let thread_id_value = ThreadId::from_string(&thread_id)?;
    let trigger_turn_id = "trigger-turn";
    // A policy event creates the server-owned eligibility ticket. The request below then travels
    // through the real app-server turn/start and core session event path before the state
    // projection accepts the client envelope.
    let (client_user_message_id, _) =
        prepare_automatic_turn(&harness, thread_id_value, trigger_turn_id).await?;

    let turn_start_response: TurnStartResponse = harness
        .request(
            ClientRequest::TurnStart {
                request_id: RequestId::Integer(30_002),
                params: automatic_turn_start_params(
                    thread_id.clone(),
                    client_user_message_id.clone(),
                ),
            },
            /*trace*/ None,
        )
        .await;
    let usage_pool = harness
        .state_db
        .as_ref()
        .expect("state db enabled for this harness")
        .usage_pool();
    let row = wait_for_automatic_turn_row(usage_pool.as_ref(), &thread_id, &client_user_message_id)
        .await?;
    assert_eq!(
        row,
        (
            thread_id,
            client_user_message_id,
            trigger_turn_id.to_string(),
            turn_start_response.turn.id,
            1,
            3,
            "recovered".to_string(),
        )
    );

    harness.shutdown().await;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
#[serial(app_server_tracing)]
async fn automatic_turn_auth_transition_at_provider_boundary_is_terminal() -> Result<()> {
    let mut harness = TracingHarness::new_with_state_db().await?;
    harness
        .auth_manager
        .set_external_auth(Arc::new(StaticExternalAuth(CodexAuth::from_api_key(
            "key-a",
        ))))
        .await?;
    let thread_start_response = harness
        .start_thread(/*request_id*/ 31_001, /*trace*/ None)
        .await;
    let thread_id = thread_start_response.thread.id.clone();
    let thread_id_value = ThreadId::from_string(&thread_id)?;
    let trigger_turn_id = "provider-boundary-trigger";
    let (client_user_message_id, _) =
        prepare_automatic_turn(&harness, thread_id_value, trigger_turn_id).await?;
    let admitted_revision = harness.auth_manager.auth_revision();
    let boundary_barrier = harness
        .auth_manager
        .provider_request_guard(admitted_revision)
        .await
        .expect("admitted credential revision should be current");

    let auth_manager = Arc::clone(&harness.auth_manager);
    let transition = auth_manager.set_external_auth(Arc::new(StaticExternalAuth(
        CodexAuth::from_api_key("key-b"),
    )));
    tokio::pin!(transition);
    assert!(matches!(futures::poll!(transition.as_mut()), Poll::Pending));

    let turn_start_response: TurnStartResponse = harness
        .request(
            ClientRequest::TurnStart {
                request_id: RequestId::Integer(31_002),
                params: automatic_turn_start_params(
                    thread_id.clone(),
                    client_user_message_id.clone(),
                ),
            },
            /*trace*/ None,
        )
        .await;
    tokio::task::yield_now().await;
    drop(boundary_barrier);
    transition
        .await
        .expect("credential transition should complete before provider send");

    let notifications = read_turn_denial_notifications(
        &mut harness.pending_outgoing,
        &mut harness.outgoing_rx,
        &turn_start_response.turn.id,
    )
    .await;
    assert_eq!(notifications, (1, 1, TurnStatus::Interrupted, true));
    assert!(
        harness
            .server
            .received_requests()
            .await
            .expect("request log")
            .is_empty(),
        "a denied automatic turn must send no provider request"
    );
    let row = wait_for_automatic_turn_row(
        harness
            .state_db
            .as_ref()
            .expect("state db enabled")
            .usage_pool()
            .as_ref(),
        &thread_id,
        &client_user_message_id,
    )
    .await?;
    assert_eq!(row.6, "failed");

    harness.shutdown().await;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
#[serial(app_server_tracing)]
async fn automatic_turn_omitted_environments_preserve_sticky_selection() -> Result<()> {
    let mut harness = TracingHarness::new_with_state_db().await?;
    let environment_cwd = TempDir::new()?;
    let extra_root = TempDir::new()?;
    let environment_cwd = environment_cwd.path().abs();
    let extra_root = extra_root.path().abs();
    let thread_start_response: ThreadStartResponse = harness
        .request(
            ClientRequest::ThreadStart {
                request_id: RequestId::Integer(32_001),
                params: ThreadStartParams {
                    cwd: Some(environment_cwd.to_string_lossy().into_owned()),
                    runtime_workspace_roots: Some(vec![
                        environment_cwd.clone(),
                        extra_root.clone(),
                    ]),
                    environments: Some(vec![TurnEnvironmentParams {
                        environment_id: LOCAL_ENVIRONMENT_ID.to_string(),
                        cwd: environment_cwd.clone().into(),
                        runtime_workspace_roots: Some(vec![
                            environment_cwd.clone().into(),
                            extra_root.clone().into(),
                        ]),
                    }]),
                    ephemeral: Some(true),
                    ..ThreadStartParams::default()
                },
            },
            /*trace*/ None,
        )
        .await;
    read_thread_started_notification(&mut harness.pending_outgoing, &mut harness.outgoing_rx).await;
    let thread_id = thread_start_response.thread.id.clone();
    let thread_id_value = ThreadId::from_string(&thread_id)?;
    let before = harness
        .processor
        .config_snapshot_for_test(thread_id_value)
        .await
        .expect("thread should remain loaded");
    let (client_user_message_id, admitted_snapshot) =
        prepare_automatic_turn(&harness, thread_id_value, "sticky-environment-trigger").await?;
    assert_eq!(admitted_snapshot.environments, before.environments);

    let turn_start_response: TurnStartResponse = harness
        .request(
            ClientRequest::TurnStart {
                request_id: RequestId::Integer(32_002),
                params: automatic_turn_start_params(
                    thread_id.clone(),
                    client_user_message_id.clone(),
                ),
            },
            /*trace*/ None,
        )
        .await;
    let row = wait_for_automatic_turn_row(
        harness
            .state_db
            .as_ref()
            .expect("state db enabled")
            .usage_pool()
            .as_ref(),
        &thread_id,
        &client_user_message_id,
    )
    .await?;
    assert_eq!(row.6, "recovered");
    assert_eq!(row.3, turn_start_response.turn.id);
    let after = harness
        .processor
        .config_snapshot_for_test(thread_id_value)
        .await
        .expect("thread should remain loaded");
    assert_eq!(after.environments, before.environments);

    harness.shutdown().await;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
#[serial(app_server_tracing)]
async fn automatic_turn_rejects_fingerprint_changing_settings() -> Result<()> {
    let mut harness = TracingHarness::new_with_state_db().await?;
    let thread_start_response = harness
        .start_thread(/*request_id*/ 32_101, /*trace*/ None)
        .await;
    let thread_id = thread_start_response.thread.id.clone();
    let thread_id_value = ThreadId::from_string(&thread_id)?;
    let (client_user_message_id, _) =
        prepare_automatic_turn(&harness, thread_id_value, "settings-change-trigger").await?;
    let mut params = automatic_turn_start_params(thread_id, client_user_message_id);
    params.model = Some("different-model".to_string());

    let error = harness
        .request_error(
            ClientRequest::TurnStart {
                request_id: RequestId::Integer(32_102),
                params,
            },
            /*trace*/ None,
        )
        .await;
    assert_eq!(
        error.message,
        "automatic turn capability no longer matches the server-canonical context"
    );
    assert!(
        harness
            .server
            .received_requests()
            .await
            .expect("request log")
            .is_empty()
    );

    harness.shutdown().await;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
#[serial(app_server_tracing)]
async fn ordinary_app_server_turns_continue_after_auth_transition() -> Result<()> {
    let mut harness = TracingHarness::new().await?;
    harness
        .auth_manager
        .set_external_auth(Arc::new(StaticExternalAuth(CodexAuth::from_api_key(
            "key-a",
        ))))
        .await?;
    let thread_start_response = harness
        .start_thread(/*request_id*/ 33_001, /*trace*/ None)
        .await;
    let thread_id = thread_start_response.thread.id.clone();

    for (request_id, key) in [(33_002, "key-a"), (33_003, "key-b")] {
        harness
            .auth_manager
            .set_external_auth(Arc::new(StaticExternalAuth(CodexAuth::from_api_key(key))))
            .await?;
        let response: TurnStartResponse = harness
            .request(
                ClientRequest::TurnStart {
                    request_id: RequestId::Integer(request_id),
                    params: TurnStartParams {
                        thread_id: thread_id.clone(),
                        input: vec![UserInput::Text {
                            text: "continue".to_string(),
                            text_elements: Vec::new(),
                        }],
                        ..TurnStartParams::default()
                    },
                },
                /*trace*/ None,
            )
            .await;
        let notifications = read_turn_denial_notifications(
            &mut harness.pending_outgoing,
            &mut harness.outgoing_rx,
            &response.turn.id,
        )
        .await;
        assert_eq!(notifications, (0, 1, TurnStatus::Completed, false));
    }
    assert_eq!(
        harness
            .server
            .received_requests()
            .await
            .expect("request log")
            .len(),
        2
    );

    harness.shutdown().await;
    Ok(())
}
