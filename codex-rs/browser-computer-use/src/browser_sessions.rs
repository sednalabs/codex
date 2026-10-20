//! Process-owned Playwright contexts for the native Browser tools.
//!
//! A Browser tool request may be aborted by either frontend.  The retained
//! profile actor therefore owns the Node process and cleanup independently of
//! the request future that initiated it.

use super::PLAYWRIGHT_BRIDGE_SCRIPT;
use super::PlaywrightProviderConfig;
use super::playwright_provider_envs;
use codex_app_server_protocol::DynamicToolCallParams;
use codex_app_server_protocol::DynamicToolCallResponse;
use serde::Deserialize;
use serde_json::Value;
use serde_json::json;
use sha2::Digest as _;
use sha2::Sha256;
use std::collections::HashMap;
use std::io::Write as _;
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::{Mutex as AsyncMutex, Notify, mpsc, oneshot, watch};
use tokio::time::timeout;

const CLEANUP_TIMEOUT: Duration = Duration::from_secs(10);
static NEXT_REQUEST_ID: AtomicU64 = AtomicU64::new(1);
static PROCESS_MANAGER: OnceLock<BrowserSessionManager> = OnceLock::new();

#[derive(Default)]
pub(super) struct BrowserSessionManager {
    slots: Mutex<HashMap<ProfileKey, Arc<AsyncMutex<ProfileSlot>>>>,
}

#[derive(Debug, Clone, Hash, PartialEq, Eq)]
struct ProfileKey {
    directory: PathBuf,
    thread_id: String,
}

struct ProfileSlot {
    config: Option<PlaywrightProviderConfig>,
    actor: Option<ActorHandle>,
    unavailable: bool,
    request_gate: Arc<AsyncMutex<()>>,
    close_epoch: u64,
}

impl Default for ProfileSlot {
    fn default() -> Self {
        Self {
            config: None,
            actor: None,
            unavailable: false,
            request_gate: Arc::new(AsyncMutex::new(())),
            close_epoch: 0,
        }
    }
}

#[derive(Clone)]
struct ActorHandle {
    commands: mpsc::UnboundedSender<ActorCommand>,
    status_tx: watch::Sender<ActorState>,
    status_rx: watch::Receiver<ActorState>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ActorState {
    Starting,
    Busy,
    Active,
    Closing,
    Released,
    Unavailable,
}

enum ActorCommand {
    Request {
        request_id: u64,
        params: DynamicToolCallParams,
        cancel: Arc<RequestCancellation>,
        reply: oneshot::Sender<Result<DynamicToolCallResponse, String>>,
    },
    Cancel {
        request_id: u64,
    },
    Close {
        reply: oneshot::Sender<Result<(), String>>,
    },
}

#[derive(Default)]
struct RequestCancellation {
    cancelled: AtomicBool,
    notify: Notify,
}

impl RequestCancellation {
    fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
        self.notify.notify_one();
    }

    fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }
}

struct RequestLease {
    request_id: u64,
    cancellation: Arc<RequestCancellation>,
    commands: mpsc::UnboundedSender<ActorCommand>,
    completed: bool,
}

impl RequestLease {
    fn complete(&mut self) {
        self.completed = true;
    }
}

impl Drop for RequestLease {
    fn drop(&mut self) {
        if self.completed {
            return;
        }
        self.cancellation.cancel();
        let _ = self.commands.send(ActorCommand::Cancel {
            request_id: self.request_id,
        });
    }
}

#[derive(Deserialize)]
struct ProviderFrame {
    kind: String,
    #[serde(rename = "profileKey")]
    profile_key: Option<String>,
    generation: Option<String>,
    #[serde(rename = "requestId")]
    request_id: Option<u64>,
    response: Option<Value>,
    error: Option<String>,
}

pub(super) async fn handle(
    params: &DynamicToolCallParams,
    config: &PlaywrightProviderConfig,
) -> Result<DynamicToolCallResponse, String> {
    process_manager().handle(params, config).await
}

pub(super) async fn has_session_for_thread(thread_id: &str) -> bool {
    process_manager().has_session_for_thread(thread_id).await
}

pub(super) async fn close_thread(thread_id: &str) {
    process_manager().close_thread(thread_id).await;
}

pub(super) async fn shutdown_all() {
    process_manager().shutdown_all().await;
}

fn process_manager() -> &'static BrowserSessionManager {
    PROCESS_MANAGER.get_or_init(BrowserSessionManager::default)
}

impl BrowserSessionManager {
    pub(super) async fn handle(
        &self,
        params: &DynamicToolCallParams,
        config: &PlaywrightProviderConfig,
    ) -> Result<DynamicToolCallResponse, String> {
        let key = profile_key(config, &params.thread_id)?;
        let slot = self.slot(&key);
        let (request_gate, invocation_epoch) = {
            let slot_state = slot.lock().await;
            (Arc::clone(&slot_state.request_gate), slot_state.close_epoch)
        };
        let _request_guard = request_gate.lock().await;

        let actor = loop {
            let mut slot_state = slot.lock().await;
            if slot_state.close_epoch != invocation_epoch {
                return Err("Browser thread closed before its request could start.".to_string());
            }
            if slot_state.unavailable {
                return Err(
                    "Browser profile is unavailable after unconfirmed owner cleanup.".to_string(),
                );
            }

            if let Some(state) = slot_state.actor.as_ref().map(actor_state) {
                match state {
                    ActorState::Closing | ActorState::Starting | ActorState::Busy => {
                        let mut actor = slot_state
                            .actor
                            .as_ref()
                            .expect("actor state came from an actor")
                            .clone();
                        drop(slot_state);
                        let terminal = timeout(
                            CLEANUP_TIMEOUT + Duration::from_secs(1),
                            wait_for_terminal(&mut actor),
                        )
                        .await
                        .unwrap_or(ActorState::Unavailable);
                        match terminal {
                            ActorState::Released => {
                                let mut slot_state = slot.lock().await;
                                slot_state.actor = None;
                                slot_state.config = None;
                            }
                            ActorState::Unavailable => {
                                let mut slot_state = slot.lock().await;
                                slot_state.unavailable = true;
                                return Err(
                                    "Browser profile is unavailable after unconfirmed owner cleanup."
                                        .to_string(),
                                );
                            }
                            _ => {}
                        }
                        continue;
                    }
                    ActorState::Unavailable => {
                        slot_state.unavailable = true;
                        return Err(
                            "Browser profile is unavailable after unconfirmed owner cleanup."
                                .to_string(),
                        );
                    }
                    ActorState::Active => {}
                    ActorState::Released => {
                        slot_state.actor = None;
                        slot_state.config = None;
                    }
                }
            }

            if slot_state.actor.is_some() && slot_state.config.as_ref() != Some(config) {
                let mut actor = slot_state
                    .actor
                    .as_ref()
                    .expect("actor checked above")
                    .clone();
                drop(slot_state);
                if close_actor(&mut actor).await.is_err() {
                    let mut slot_state = slot.lock().await;
                    slot_state.unavailable = true;
                    return Err("Browser profile cleanup was not confirmed.".to_string());
                }
                let mut slot_state = slot.lock().await;
                slot_state.actor = None;
                slot_state.config = None;
                continue;
            }

            if slot_state.actor.is_none() {
                let observed_close_epoch = slot_state.close_epoch;
                drop(slot_state);
                let mut new_actor = spawn_actor(key.clone(), config).await?;
                let mut slot_state = slot.lock().await;
                if slot_state.close_epoch != observed_close_epoch {
                    drop(slot_state);
                    let _ = close_actor(&mut new_actor).await;
                    return Err("Browser thread closed before its provider could start.".to_string());
                }
                if slot_state.unavailable {
                    drop(slot_state);
                    let _ = close_actor(&mut new_actor).await;
                    return Err(
                        "Browser profile is unavailable after unconfirmed owner cleanup."
                            .to_string(),
                    );
                }
                slot_state.actor = Some(new_actor);
                slot_state.config = Some(config.clone());
            }
            let actor = slot_state
                .actor
                .as_ref()
                .expect("actor created above")
                .clone();
            drop(slot_state);
            break actor;
        };

        let result = actor.request(params.clone()).await;
        let mut slot_state = slot.lock().await;
        if slot_state
            .actor
            .as_ref()
            .is_some_and(|actor| actor_state(actor) == ActorState::Unavailable)
        {
            slot_state.unavailable = true;
        }
        result
    }

    fn slot(&self, key: &ProfileKey) -> Arc<AsyncMutex<ProfileSlot>> {
        let mut slots = self
            .slots
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Arc::clone(
            slots
                .entry(key.clone())
                .or_insert_with(|| Arc::new(AsyncMutex::new(ProfileSlot::default()))),
        )
    }

    async fn matching_slots(&self, thread_id: Option<&str>) -> Vec<Arc<AsyncMutex<ProfileSlot>>> {
        let slots = self
            .slots
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let keys = slots
            .keys()
            .filter(|key| thread_id.is_none_or(|thread_id| key.thread_id == thread_id))
            .cloned()
            .collect::<Vec<_>>();
        keys.into_iter()
            .filter_map(|key| slots.get(&key).cloned())
            .collect()
    }

    async fn has_session_for_thread(&self, thread_id: &str) -> bool {
        for slot in self.matching_slots(Some(thread_id)).await {
            let slot = match timeout(Duration::from_millis(50), slot.lock()).await {
                Ok(slot) => slot,
                // On brief slot contention, conservatively treat the thread
                // as live until its close notification is confirmed.
                Err(_) => return true,
            };
            if let Some(actor) = &slot.actor
                && actor_state(actor) != ActorState::Released
            {
                return true;
            }
        }
        false
    }

    pub(super) async fn close_thread(&self, thread_id: &str) {
        self.close_matching(Some(thread_id), /*wait_for_ack*/ false)
            .await;
    }

    pub(super) async fn shutdown_all(&self) {
        self.close_matching(None, /*wait_for_ack*/ true).await;
    }

    async fn close_matching(&self, thread_id: Option<&str>, wait_for_ack: bool) {
        for slot in self.matching_slots(thread_id).await {
            let (actor, request_gate) = {
                let mut slot_state = slot.lock().await;
                slot_state.close_epoch = slot_state.close_epoch.wrapping_add(1);
                (
                    slot_state.actor.clone(),
                    Arc::clone(&slot_state.request_gate),
                )
            };

            let Some(mut actor) = actor else {
                if wait_for_ack {
                    let _ = timeout(
                        CLEANUP_TIMEOUT + Duration::from_secs(1),
                        request_gate.lock(),
                    )
                    .await;
                }
                continue;
            };

            if wait_for_ack {
                let result = close_actor(&mut actor).await;
                let mut slot_state = slot.lock().await;
                if result.is_err() {
                    slot_state.unavailable = true;
                } else if slot_state
                    .actor
                    .as_ref()
                    .is_some_and(|current| actor_state(current) == ActorState::Released)
                {
                    slot_state.actor = None;
                    slot_state.config = None;
                }
            } else if queue_close(&mut actor).is_err() {
                slot.lock().await.unavailable = true;
            }
        }
    }
}

impl ActorHandle {
    async fn request(
        &self,
        params: DynamicToolCallParams,
    ) -> Result<DynamicToolCallResponse, String> {
        let request_id = NEXT_REQUEST_ID.fetch_add(1, Ordering::Relaxed);
        let cancellation = Arc::new(RequestCancellation::default());
        let (reply, receiver) = oneshot::channel();
        self.commands
            .send(ActorCommand::Request {
                request_id,
                params,
                cancel: Arc::clone(&cancellation),
                reply,
            })
            .map_err(|_| "Browser provider actor is unavailable.".to_string())?;

        let mut lease = RequestLease {
            request_id,
            cancellation,
            commands: self.commands.clone(),
            completed: false,
        };
        match receiver.await {
            Ok(result) => {
                lease.complete();
                result
            }
            Err(_) => {
                // The actor may have completed cleanup and dropped its reply
                // while handling ThreadClosed. Do not send a stale cancel
                // after that terminal transition. If this future itself is
                // dropped while awaiting, RequestLease still cancels it.
                lease.complete();
                Err("Browser provider request was cancelled.".to_string())
            }
        }
    }
}

fn actor_state(actor: &ActorHandle) -> ActorState {
    *actor.status_rx.borrow()
}

async fn wait_for_terminal(actor: &mut ActorHandle) -> ActorState {
    loop {
        let state = actor_state(actor);
        if matches!(state, ActorState::Released | ActorState::Unavailable) {
            return state;
        }
        if actor.status_rx.changed().await.is_err() {
            return ActorState::Unavailable;
        }
    }
}

async fn close_actor(actor: &mut ActorHandle) -> Result<(), String> {
    match actor_state(actor) {
        ActorState::Released => return Ok(()),
        ActorState::Unavailable => {
            return Err("Browser profile cleanup is unconfirmed.".to_string());
        }
        ActorState::Closing => {
            return match timeout(
                CLEANUP_TIMEOUT + Duration::from_secs(1),
                wait_for_terminal(actor),
            )
            .await
            {
                Ok(ActorState::Released) => Ok(()),
                _ => Err("Browser profile cleanup is unconfirmed.".to_string()),
            };
        }
        ActorState::Starting | ActorState::Busy | ActorState::Active => {}
    }

    let Some(receiver) = queue_close(actor)? else {
        return match timeout(
            CLEANUP_TIMEOUT + Duration::from_secs(1),
            wait_for_terminal(actor),
        )
        .await
        {
            Ok(ActorState::Released) => Ok(()),
            _ => Err("Browser profile cleanup acknowledgement was not received.".to_string()),
        };
    };
    match timeout(CLEANUP_TIMEOUT + Duration::from_secs(1), receiver).await {
        Ok(Ok(Ok(()))) => Ok(()),
        Ok(Ok(Err(error))) => Err(error),
        Ok(Err(_)) | Err(_) => {
            Err("Browser profile cleanup acknowledgement was not received.".to_string())
        }
    }
}

fn queue_close(
    actor: &mut ActorHandle,
) -> Result<Option<oneshot::Receiver<Result<(), String>>>, String> {
    match actor_state(actor) {
        ActorState::Released => return Ok(None),
        ActorState::Unavailable => {
            return Err("Browser profile cleanup is unconfirmed.".to_string());
        }
        ActorState::Closing => return Ok(None),
        ActorState::Starting | ActorState::Busy | ActorState::Active => {}
    }
    actor.status_tx.send_replace(ActorState::Closing);
    let (reply, receiver) = oneshot::channel();
    if actor.commands.send(ActorCommand::Close { reply }).is_err() {
        actor.status_tx.send_replace(ActorState::Unavailable);
        return Err("Browser profile cleanup is unconfirmed.".to_string());
    }
    Ok(Some(receiver))
}

async fn spawn_actor(
    key: ProfileKey,
    config: &PlaywrightProviderConfig,
) -> Result<ActorHandle, String> {
    let mut script_file = tempfile::Builder::new()
        .prefix("codex-browser-provider-")
        .suffix(".mjs")
        .tempfile()
        .map_err(|err| format!("failed to create browser provider script: {err}"))?;
    script_file
        .as_file_mut()
        .write_all(PLAYWRIGHT_BRIDGE_SCRIPT.as_bytes())
        .map_err(|err| format!("failed to write browser provider script: {err}"))?;
    let script_file = script_file.into_temp_path();
    let script_path = script_file.as_os_str().to_string_lossy().into_owned();
    let envs = playwright_provider_envs(config);
    let mut child = Command::new(&config.node)
        .arg(script_path)
        .envs(envs.iter().map(|(key, value)| (key, value)))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .await
        .map_err(|err| format!("failed to start browser provider `{}`: {err}", config.node))?;
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| "failed to open browser provider stdin".to_string())?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "failed to open browser provider stdout".to_string())?;
    if let Some(stderr) = child.stderr.take() {
        tokio::spawn(async move {
            let mut stderr = stderr;
            let mut sink = tokio::io::sink();
            let _ = tokio::io::copy(&mut stderr, &mut sink).await;
        });
    }

    let (commands, receiver) = mpsc::unbounded_channel();
    let (status_tx, status_rx) = watch::channel(ActorState::Starting);
    tokio::spawn(run_actor(
        child,
        stdin,
        stdout,
        receiver,
        status_tx.clone(),
        key,
        script_file,
    ));
    Ok(ActorHandle {
        commands,
        status_tx,
        status_rx,
    })
}

async fn run_actor(
    mut child: Child,
    mut stdin: ChildStdin,
    stdout: tokio::process::ChildStdout,
    mut commands: mpsc::UnboundedReceiver<ActorCommand>,
    status: watch::Sender<ActorState>,
    key: ProfileKey,
    _script_file: tempfile::TempPath,
) {
    let mut stdout = BufReader::new(stdout).lines();
    let mut started = false;
    let mut generation: Option<String> = None;

    while let Some(command) = commands.recv().await {
        match command {
            ActorCommand::Request {
                request_id,
                params,
                cancel,
                reply,
            } => {
                if cancel.is_cancelled() && !started {
                    let _ = child.kill().await;
                    status.send_replace(ActorState::Released);
                    let _ = reply.send(Err(
                        "Browser request was cancelled before startup.".to_string()
                    ));
                    return;
                }
                let frame = if started {
                    json!({"kind":"request","requestId":request_id,"request":params})
                } else {
                    json!({"kind":"start","requestId":request_id,"request":params})
                };
                if write_frame(&mut stdin, &frame).await.is_err() {
                    fail_actor(&mut child, &status).await;
                    let _ = reply.send(Err("failed to write Browser provider request".to_string()));
                    return;
                }
                started = true;
                status.send_replace(ActorState::Busy);

                let result = wait_for_request_result(
                    request_id,
                    &key,
                    generation.as_deref(),
                    &mut child,
                    &mut stdin,
                    &mut stdout,
                    &mut commands,
                    &status,
                    &cancel,
                )
                .await;
                match result {
                    RequestResult::Response {
                        response,
                        generation: current_generation,
                    } => {
                        generation = Some(current_generation);
                        if cancel.is_cancelled() {
                            status.send_replace(ActorState::Closing);
                            if !cancel_and_release(
                                request_id,
                                &key,
                                generation.as_deref(),
                                &mut child,
                                &mut stdin,
                                &mut stdout,
                            )
                            .await
                            {
                                status.send_replace(ActorState::Unavailable);
                                return;
                            }
                            status.send_replace(ActorState::Released);
                            return;
                        }
                        status.send_replace(ActorState::Active);
                        if reply.send(Ok(response)).is_err() {
                            status.send_replace(ActorState::Closing);
                            cancel.cancel();
                            if !cancel_and_release(
                                request_id,
                                &key,
                                generation.as_deref(),
                                &mut child,
                                &mut stdin,
                                &mut stdout,
                            )
                            .await
                            {
                                status.send_replace(ActorState::Unavailable);
                                return;
                            }
                            status.send_replace(ActorState::Released);
                            return;
                        }
                    }
                    RequestResult::Cancelled {
                        generation: current_generation,
                    } => {
                        generation = current_generation.or(generation);
                        status.send_replace(ActorState::Released);
                        let _ = reply.send(Err("Browser request was cancelled.".to_string()));
                        return;
                    }
                    RequestResult::Unavailable(error) => {
                        status.send_replace(ActorState::Unavailable);
                        let _ = child.kill().await;
                        let _ = reply.send(Err(error));
                        return;
                    }
                }
            }
            ActorCommand::Cancel { request_id } => {
                if !started {
                    let _ = child.kill().await;
                    status.send_replace(ActorState::Released);
                    return;
                }
                status.send_replace(ActorState::Closing);
                if !cancel_and_release(
                    request_id,
                    &key,
                    generation.as_deref(),
                    &mut child,
                    &mut stdin,
                    &mut stdout,
                )
                .await
                {
                    status.send_replace(ActorState::Unavailable);
                    return;
                }
                status.send_replace(ActorState::Released);
                return;
            }
            ActorCommand::Close { reply } => {
                if !started {
                    let _ = child.kill().await;
                    status.send_replace(ActorState::Released);
                    let _ = reply.send(Ok(()));
                    return;
                }
                status.send_replace(ActorState::Closing);
                if release_after_close(
                    &key,
                    generation.as_deref(),
                    &mut child,
                    &mut stdin,
                    &mut stdout,
                )
                .await
                {
                    status.send_replace(ActorState::Released);
                    let _ = reply.send(Ok(()));
                } else {
                    status.send_replace(ActorState::Unavailable);
                    let _ = child.kill().await;
                    let _ =
                        reply.send(Err("Browser profile cleanup was not confirmed.".to_string()));
                }
                return;
            }
        }
    }

    // A supervisor disappearing is not proof that its profile was released.
    let _ = child.kill().await;
    status.send_replace(ActorState::Unavailable);
}

enum RequestResult {
    Response {
        response: DynamicToolCallResponse,
        generation: String,
    },
    Cancelled {
        generation: Option<String>,
    },
    Unavailable(String),
}

async fn wait_for_request_result(
    request_id: u64,
    key: &ProfileKey,
    known_generation: Option<&str>,
    child: &mut Child,
    stdin: &mut ChildStdin,
    stdout: &mut tokio::io::Lines<BufReader<ChildStdout>>,
    commands: &mut mpsc::UnboundedReceiver<ActorCommand>,
    status: &watch::Sender<ActorState>,
    cancel: &Arc<RequestCancellation>,
) -> RequestResult {
    let mut generation = known_generation.map(ToOwned::to_owned);
    let mut pending_response = None;
    loop {
        enum Event {
            Cancel,
            Command(Option<ActorCommand>),
            Output(Result<Option<String>, std::io::Error>),
        }
        let event = tokio::select! {
            _ = cancel.notify.notified() => Event::Cancel,
            command = commands.recv() => Event::Command(command),
            line = stdout.next_line() => Event::Output(line),
        };
        match event {
            Event::Cancel => {
                status.send_replace(ActorState::Closing);
                return if cancel_and_release(request_id, key, generation.as_deref(), child, stdin, stdout).await {
                    RequestResult::Cancelled { generation }
                } else {
                    RequestResult::Unavailable("Browser cancellation cleanup was not confirmed; the profile remains unavailable.".to_string())
                };
            }
            Event::Command(command) => {
                match command {
                    Some(ActorCommand::Cancel { request_id: cancelled_id }) if cancelled_id == request_id => {
                        status.send_replace(ActorState::Closing);
                        return if cancel_and_release(request_id, key, generation.as_deref(), child, stdin, stdout).await {
                            RequestResult::Cancelled { generation }
                        } else {
                            RequestResult::Unavailable("Browser cancellation cleanup was not confirmed; the profile remains unavailable.".to_string())
                        };
                    }
                    Some(ActorCommand::Close { reply }) => {
                        status.send_replace(ActorState::Closing);
                        let released = cancel_and_release(request_id, key, generation.as_deref(), child, stdin, stdout).await;
                        if released {
                            let _ = reply.send(Ok(()));
                            return RequestResult::Cancelled { generation };
                        }
                        let _ = reply.send(Err("Browser profile cleanup was not confirmed.".to_string()));
                        return RequestResult::Unavailable("Browser profile cleanup was not confirmed; the profile remains unavailable.".to_string());
                    }
                    Some(ActorCommand::Cancel { .. }) | Some(ActorCommand::Request { .. }) => {
                        return RequestResult::Unavailable("unexpected concurrent Browser provider command".to_string());
                    }
                    None => {
                        return RequestResult::Unavailable("Browser provider supervisor disconnected.".to_string());
                    }
                }
            }
            Event::Output(line) => {
                let frame = match line {
                    Ok(Some(line)) => match serde_json::from_str::<ProviderFrame>(&line) {
                        Ok(frame) => frame,
                        Err(error) => return RequestResult::Unavailable(format!("invalid Browser provider frame: {error}")),
                    },
                    Ok(None) => return RequestResult::Unavailable("Browser provider exited before replying.".to_string()),
                    Err(error) => return RequestResult::Unavailable(format!("failed to read Browser provider response: {error}")),
                };
                match frame.kind.as_str() {
                    "ready" => {
                        let Some(frame_key) = frame.profile_key.as_deref() else {
                            return RequestResult::Unavailable("Browser provider omitted its profile identity.".to_string());
                        };
                        if frame_key != profile_key_string(key) {
                            return RequestResult::Unavailable("Browser provider profile identity did not match the requested profile.".to_string());
                        }
                        let Some(token) = frame.generation else {
                            return RequestResult::Unavailable("Browser provider omitted its owner generation.".to_string());
                        };
                        generation = Some(token);
                        status.send_replace(ActorState::Busy);
                    }
                    "response" => {
                        if frame.request_id != Some(request_id) {
                            return RequestResult::Unavailable("Browser provider response request identity mismatch.".to_string());
                        }
                        if pending_response.is_some() {
                            return RequestResult::Unavailable("Browser provider sent more than one response for a request.".to_string());
                        }
                        let Some(value) = frame.response else {
                            return RequestResult::Unavailable("Browser provider omitted its response payload.".to_string());
                        };
                        let response = match serde_json::from_value(value) {
                            Ok(response) => response,
                            Err(error) => return RequestResult::Unavailable(format!("invalid Browser provider response: {error}")),
                        };
                        pending_response = Some(response);
                    }
                    "idle" => {
                        if frame.request_id != Some(request_id) {
                            return RequestResult::Unavailable("Browser provider idle acknowledgement request identity mismatch.".to_string());
                        }
                        let Some(response) = pending_response.take() else {
                            return RequestResult::Unavailable("Browser provider entered idle without a response.".to_string());
                        };
                        let Some(token) = generation else {
                            return RequestResult::Unavailable("Browser provider replied before owner identity was established.".to_string());
                        };
                        return RequestResult::Response { response, generation: token };
                    }
                    "fatal" => {
                        return RequestResult::Unavailable(frame.error.unwrap_or_else(|| "Browser provider failed during startup.".to_string()));
                    }
                    "released" => {
                        if !valid_release(&frame, key, generation.as_deref(), Some(request_id)) {
                            return RequestResult::Unavailable("Browser cleanup acknowledgement identity mismatch.".to_string());
                        }
                        return RequestResult::Cancelled { generation: frame.generation };
                    }
                    _ => return RequestResult::Unavailable("Browser provider sent an unknown frame.".to_string()),
                }
            }
        }
    }
}

async fn cancel_and_release(
    request_id: u64,
    key: &ProfileKey,
    generation: Option<&str>,
    child: &mut Child,
    stdin: &mut ChildStdin,
    stdout: &mut tokio::io::Lines<BufReader<ChildStdout>>,
) -> bool {
    if write_frame(stdin, &json!({"kind":"cancel","requestId":request_id}))
        .await
        .is_err()
    {
        let _ = child.kill().await;
        return false;
    }
    wait_for_release(key, generation, Some(request_id), child, stdout).await
}

async fn release_after_close(
    key: &ProfileKey,
    generation: Option<&str>,
    child: &mut Child,
    stdin: &mut ChildStdin,
    stdout: &mut tokio::io::Lines<BufReader<ChildStdout>>,
) -> bool {
    if write_frame(stdin, &json!({"kind":"close"})).await.is_err() {
        let _ = child.kill().await;
        return false;
    }
    wait_for_release(key, generation, None, child, stdout).await
}

async fn wait_for_release(
    key: &ProfileKey,
    generation: Option<&str>,
    request_id: Option<u64>,
    child: &mut Child,
    stdout: &mut tokio::io::Lines<BufReader<ChildStdout>>,
) -> bool {
    let deadline = tokio::time::Instant::now() + CLEANUP_TIMEOUT;
    loop {
        match timeout(
            deadline.saturating_duration_since(tokio::time::Instant::now()),
            stdout.next_line(),
        )
        .await
        {
            Ok(Ok(Some(line))) => {
                let Ok(frame) = serde_json::from_str::<ProviderFrame>(&line) else {
                    let _ = child.kill().await;
                    return false;
                };
                if frame.kind == "released" {
                    return valid_release(&frame, key, generation, request_id);
                }
                // A ready/response frame can race cancellation; it is discarded,
                // never returned to the interrupted caller.
            }
            Ok(Ok(None)) | Ok(Err(_)) | Err(_) => {
                let _ = child.kill().await;
                return false;
            }
        }
    }
}

fn valid_release(
    frame: &ProviderFrame,
    key: &ProfileKey,
    known_generation: Option<&str>,
    request_id: Option<u64>,
) -> bool {
    let expected_profile_key = profile_key_string(key);
    frame.kind == "released"
        && frame.profile_key.as_deref() == Some(expected_profile_key.as_str())
        && frame
            .generation
            .as_deref()
            .is_some_and(|generation| known_generation.is_none_or(|known| known == generation))
        && frame.request_id == request_id
}

async fn write_frame(stdin: &mut ChildStdin, value: &Value) -> Result<(), String> {
    let mut bytes = serde_json::to_vec(value).map_err(|error| error.to_string())?;
    bytes.push(b'\n');
    stdin
        .write_all(&bytes)
        .await
        .map_err(|error| format!("failed to write Browser provider command: {error}"))
}

async fn fail_actor(child: &mut Child, status: &watch::Sender<ActorState>) {
    let _ = child.kill().await;
    status.send_replace(ActorState::Unavailable);
}

fn profile_key(config: &PlaywrightProviderConfig, thread_id: &str) -> Result<ProfileKey, String> {
    if thread_id.trim().is_empty() {
        return Err("Browser thread isolation requires a non-empty threadId.".to_string());
    }
    let root = if let Some(state_dir) = config
        .state_dir
        .as_deref()
        .filter(|state_dir| !state_dir.trim().is_empty())
    {
        resolve_absolute(Path::new(state_dir))?
    } else {
        active_state_root()?
    };
    let directory = root.join("profiles").join(safe_path_component(thread_id));
    Ok(ProfileKey {
        directory,
        thread_id: thread_id.to_string(),
    })
}

fn active_state_root() -> Result<PathBuf, String> {
    if let Some(codex_home) = std::env::var_os("CODEX_HOME") {
        let candidate = resolve_absolute(Path::new(&codex_home))?;
        return std::fs::canonicalize(&candidate)
            .map(|path| path.join("browser-computer-use-playwright"))
            .map_err(|error| format!("Cannot resolve active CODEX_HOME: {error}"));
    }
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| "Cannot resolve the active home directory.".to_string())?;
    std::fs::canonicalize(home)
        .map(|path| path.join(".codex/browser-computer-use-playwright"))
        .map_err(|error| format!("Cannot resolve the active home directory: {error}"))
}

fn resolve_absolute(path: &Path) -> Result<PathBuf, String> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|error| format!("Cannot resolve the current directory: {error}"))?
            .join(path)
    };
    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            Component::Normal(part) => normalized.push(part),
        }
    }
    Ok(normalized)
}

fn safe_path_component(value: &str) -> String {
    let mut normalized = String::new();
    let mut previous_invalid = false;
    for character in value.chars() {
        if character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '-') {
            normalized.push(character);
            previous_invalid = false;
        } else if !previous_invalid {
            normalized.push('_');
            previous_invalid = true;
        }
    }
    let slug = normalized
        .trim_matches('_')
        .chars()
        .take(64)
        .collect::<String>();
    let slug = if slug.is_empty() { "default" } else { &slug };
    let digest = Sha256::digest(value.as_bytes());
    let hash = digest[..6]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    format!("{slug}-{hash}")
}

fn profile_key_string(key: &ProfileKey) -> String {
    key.directory.to_string_lossy().into_owned()
}
