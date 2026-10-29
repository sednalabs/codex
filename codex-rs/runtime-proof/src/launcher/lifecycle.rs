//! Issuer-bound lifecycle events for the fixed root launcher.

use crate::wire;
use anyhow::Result;
use anyhow::bail;
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chrono::DateTime;
use chrono::SecondsFormat;
use chrono::Utc;
use codex_http_client::HttpClient;
use ed25519_dalek::Signature;
use ed25519_dalek::Signer as _;
use ed25519_dalek::SigningKey;
use ed25519_dalek::VerifyingKey;
use serde::Deserialize;
use serde::Deserializer;
use serde::Serialize;
use std::fs;
use std::fs::File;
use std::fs::OpenOptions;
use std::io::Read as _;
use std::io::Write as _;
use std::os::fd::AsRawFd as _;
use std::os::unix::fs::MetadataExt as _;
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;
use uuid::Uuid;
use zeroize::Zeroizing;

const WITNESS_SECONDS: i64 = 45;
const ROUTE: &str = "/v0/work-items/lifecycle";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(8);
const MAX_RESPONSE_BYTES: usize = 64 * 1024;
const STATE_DIRECTORY: &str = "/var/lib/codex-runtime-proof-lifecycle";
const MAX_CONTEXT_BYTES: usize = 64 * 1024;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum Phase {
    Start,
    End,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum Source {
    Launcher,
    Systemd,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum Cause {
    LaunchRegistered,
    Exited,
    Signalled,
    Timeout,
    Unobserved,
}

/// Closed event sent to the lifecycle endpoint; null fields remain on the wire.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Event {
    pub(super) version: u8,
    pub(super) principal_id: Uuid,
    pub(super) project_id: u64,
    pub(super) work_item_ref: String,
    pub(super) issuer_kid: String,
    pub(super) certificate_sha256: String,
    pub(super) runtime_incarnation: Uuid,
    pub(super) event_id: Uuid,
    pub(super) phase: Phase,
    pub(super) source: Source,
    pub(super) manager_invocation: Uuid,
    pub(super) observer_artifact_sha256: String,
    pub(super) observed_at: String,
    pub(super) cause: Cause,
    #[serde(deserialize_with = "required_nullable")]
    pub(super) exit_code: Option<i32>,
    #[serde(deserialize_with = "required_nullable")]
    pub(super) signal: Option<i32>,
    pub(super) absence_proven: bool,
}

/// A persisted event and its immutable potentially-sent proof interval.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct PendingEvent {
    pub(super) event: Event,
    pub(super) operation_sha256: String,
    pub(super) proof_issued_at: i64,
    pub(super) proof_expires_at: i64,
    pub(super) may_have_been_sent: bool,
}

/// Root-owned public context; it never stores a seed, bearer, or witness.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Context {
    version: u8,
    certificate: String,
    policy_sha256: String,
    selected_uid: u32,
    pending_start: PendingEvent,
    start_acknowledged: bool,
    end_event_id: Uuid,
    observed_end: Option<EndObservation>,
    pending_end: Option<PendingEvent>,
    end_acknowledged: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct EndObservation {
    observed_at: String,
    cause: Cause,
    exit_code: Option<i32>,
    signal: Option<i32>,
}

fn state_directory() -> Result<File> {
    let path = Path::new(STATE_DIRECTORY);
    if path.canonicalize()? != path {
        bail!("lifecycle state directory is not canonical");
    }
    let directory = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)?;
    let metadata = directory.metadata()?;
    if !metadata.is_dir() || metadata.uid() != 0 || metadata.mode() & 0o7077 != 0 {
        bail!("lifecycle state directory is not root-only");
    }
    Ok(directory)
}

/// Serialize selected-UID admission and stop across cooperating launchers.
pub(super) fn lock_cohort() -> Result<File> {
    let _directory = state_directory()?;
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(Path::new(STATE_DIRECTORY).join("cohort.lock"))?;
    let metadata = lock.metadata()?;
    if !metadata.is_file()
        || metadata.uid() != 0
        || metadata.mode() & 0o077 != 0
        || unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0
    {
        bail!("lifecycle cohort lock is unavailable");
    }
    Ok(lock)
}

fn context_path(invocation: Uuid) -> PathBuf {
    Path::new(STATE_DIRECTORY).join(format!("{invocation}.json"))
}

pub(super) fn load_context(invocation: Uuid) -> Result<Option<Context>> {
    let _directory = state_directory()?;
    let file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
        .open(context_path(invocation))
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let metadata = file.metadata()?;
    if !metadata.is_file()
        || metadata.uid() != 0
        || metadata.mode() & 0o077 != 0
        || metadata.len() > MAX_CONTEXT_BYTES as u64
    {
        bail!("invalid lifecycle context file");
    }
    let mut bytes = Vec::new();
    file.take((MAX_CONTEXT_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_CONTEXT_BYTES {
        bail!("lifecycle context exceeds its limit");
    }
    Ok(Some(serde_json::from_slice(&bytes)?))
}

fn persist_context(context: &Context, create: bool) -> Result<()> {
    let directory = state_directory()?;
    let bytes = serde_json::to_vec(context)?;
    if bytes.len() > MAX_CONTEXT_BYTES {
        bail!("lifecycle context exceeds its limit");
    }
    let invocation = context.pending_start.event.manager_invocation;
    let target = context_path(invocation);
    let temporary = Path::new(STATE_DIRECTORY).join(format!(".{}.tmp", Uuid::new_v4()));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&temporary)?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    if create {
        // A colliding manager invocation is never overwritten.
        fs::hard_link(&temporary, &target)?;
        fs::remove_file(&temporary)?;
    } else {
        // The root-only directory and cohort lock protect this replacement.
        if load_context(invocation)?.is_none() {
            bail!("lifecycle context disappeared");
        }
        fs::rename(&temporary, &target)?;
    }
    directory.sync_all()?;
    Ok(())
}

pub(super) fn manager_invocation() -> Result<Uuid> {
    let raw = std::env::var("INVOCATION_ID")?;
    parse_manager_invocation(&raw)
}

fn parse_manager_invocation(raw: &str) -> Result<Uuid> {
    if raw.len() != 32 || !raw.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("system manager invocation is invalid");
    }
    let invocation = Uuid::parse_str(raw)?;
    if invocation.is_nil() {
        bail!("system manager invocation is invalid");
    }
    Ok(invocation)
}

/// Fail closed on incomplete process visibility or a process using the UID.
pub(super) fn require_uid_absent(uid: u32) -> Result<()> {
    if uid == 0 {
        bail!("lifecycle selected UID must not be root");
    }
    // Matching /proc/self and /proc/1 only describes this PID namespace. The
    // cohort lock excludes cooperating launchers, not unrelated UID admission.
    // No trusted host-namespace and admission binding is available yet, so a
    // scan cannot authorize an absence assertion or a lifecycle-enabled start.
    require_trusted_absence_binding()?;
    let mut stat: libc::statfs = unsafe { std::mem::zeroed() };
    let proc_path = std::ffi::CString::new("/proc")?;
    if unsafe { libc::statfs(proc_path.as_ptr(), &mut stat) } != 0
        || stat.f_type != libc::PROC_SUPER_MAGIC as libc::c_long
    {
        bail!("lifecycle process view is unavailable");
    }
    let mounts = fs::read_to_string("/proc/self/mountinfo")?;
    let mut proc_mounts = 0;
    for line in mounts.lines() {
        let Some((left, right)) = line.split_once(" - ") else {
            bail!("lifecycle mount view is incomplete");
        };
        let fields = left.split_whitespace().collect::<Vec<_>>();
        let filesystem = right.split_whitespace().collect::<Vec<_>>();
        if fields.len() < 6 || filesystem.len() < 3 {
            bail!("lifecycle mount view is incomplete");
        }
        if fields[4] != "/proc" {
            continue;
        }
        proc_mounts += 1;
        if filesystem[0] != "proc"
            || fields[5]
                .split(',')
                .chain(filesystem[2].split(','))
                .any(|option| {
                    option == "subset=pid"
                        || (option.starts_with("hidepid=")
                            && option != "hidepid=0"
                            && option != "hidepid=off")
                })
        {
            bail!("lifecycle process view is restricted");
        }
    }
    if proc_mounts != 1 {
        bail!("lifecycle process mount is ambiguous");
    }
    let own_namespace = fs::metadata("/proc/self/ns/pid")?;
    let init_namespace = fs::metadata("/proc/1/ns/pid")?;
    if own_namespace.dev() != init_namespace.dev() || own_namespace.ino() != init_namespace.ino() {
        bail!("lifecycle process namespace is incomplete");
    }
    for entry in fs::read_dir("/proc")? {
        let entry = entry?;
        if !entry
            .file_name()
            .as_encoded_bytes()
            .iter()
            .all(u8::is_ascii_digit)
        {
            continue;
        }
        let status = match fs::read_to_string(entry.path().join("status")) {
            Ok(status) => status,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        if status_has_uid(&status, uid)? {
            bail!("lifecycle selected UID absence is not established");
        }
    }
    Ok(())
}

fn require_trusted_absence_binding() -> Result<()> {
    bail!("lifecycle trusted manager and epoch containment binding unavailable")
}

fn status_has_uid(status: &str, uid: u32) -> Result<bool> {
    let uid_line = status
        .lines()
        .find(|line| line.starts_with("Uid:"))
        .ok_or_else(|| anyhow::anyhow!("process identity is unavailable"))?;
    let ids = uid_line
        .split_whitespace()
        .skip(1)
        .map(str::parse::<u32>)
        .collect::<std::result::Result<Vec<_>, _>>()?;
    if ids.len() != 4 {
        bail!("process identity is incomplete");
    }
    Ok(ids.contains(&uid))
}

#[derive(Serialize)]
struct WitnessClaims<'a> {
    version: u8,
    recipient: &'a str,
    method: &'static str,
    route: &'static str,
    purpose: &'static str,
    operation_sha256: &'a str,
    iat: i64,
    exp: i64,
    event_id: Uuid,
    certificate_sha256: &'a str,
    runtime_incarnation: Uuid,
}

#[derive(Serialize)]
struct Envelope<'a> {
    certificate: &'a str,
    witness: &'a str,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Receipt {
    event_id: String,
    issuer_kid: String,
    certificate_sha256: String,
    runtime_incarnation: String,
    work_item_id: u64,
    project_id: u64,
    principal_id: String,
    phase: Phase,
    manager_invocation: String,
    observer_artifact_sha256: String,
    operation_sha256: String,
    source: Source,
    cause: Cause,
    #[serde(deserialize_with = "required_nullable")]
    exit_code: Option<i32>,
    #[serde(deserialize_with = "required_nullable")]
    signal: Option<i32>,
    absence_proven: bool,
    observed_at: String,
    proof_issued_at: String,
    proof_expires_at: String,
    runtime_epoch_recorded: bool,
    ended: bool,
    terminalized_execution_instances: Vec<String>,
    #[serde(deserialize_with = "required_nullable")]
    released_current_claim: Option<ReleasedClaim>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReleasedClaim {
    execution_instance_id: String,
    generation: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PostResponse {
    status: String,
    request_id: String,
    historical: bool,
    lifecycle_receipt: Receipt,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadResponse {
    status: String,
    request_id: String,
    #[serde(deserialize_with = "required_nullable")]
    lifecycle_receipt: Option<Receipt>,
}

fn valid_request_id(value: &str) -> bool {
    !value.is_empty() && value.len() <= 256 && !value.bytes().any(|byte| byte.is_ascii_control())
}

fn canonical_uuid(value: &str) -> bool {
    Uuid::parse_str(value).is_ok_and(|id| !id.is_nil() && id.to_string() == value)
}

async fn bounded_body(mut response: codex_http_client::HttpResponse) -> Result<Zeroizing<Vec<u8>>> {
    if response.status().as_u16() != 200
        || response
            .content_length()
            .is_some_and(|length| length > MAX_RESPONSE_BYTES as u64)
    {
        bail!("lifecycle response unavailable");
    }
    let mut body = Zeroizing::new(Vec::new());
    while let Some(chunk) = response.chunk().await? {
        if body
            .len()
            .checked_add(chunk.len())
            .is_none_or(|length| length > MAX_RESPONSE_BYTES)
        {
            bail!("lifecycle response exceeds its limit");
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

async fn read_receipt(
    client: &HttpClient,
    recipient: &str,
    bearer: &str,
    pending: &PendingEvent,
) -> Result<bool> {
    let authorization = Zeroizing::new(format!("Bearer {bearer}"));
    let event = &pending.event;
    let url = format!(
        "{recipient}/v0/work-items/{}/lifecycle-events/{}?project_id={}",
        event.work_item_ref, event.event_id, event.project_id
    );
    let response = client
        .get(url)
        .header("authorization", authorization.as_str())
        .header("x-ops-project-id", event.project_id.to_string())
        .timeout(REQUEST_TIMEOUT)
        .send()
        .await?;
    let body = bounded_body(response).await?;
    let result: ReadResponse = serde_json::from_slice(&body)?;
    if result.status != "ok" || !valid_request_id(&result.request_id) {
        bail!("invalid lifecycle receipt response");
    }
    match result.lifecycle_receipt {
        Some(receipt) => {
            pending.validate_receipt(&receipt)?;
            Ok(true)
        }
        None => Ok(false),
    }
}

/// One send followed by at most one exact readback; an absent read stays unresolved.
pub(super) fn deliver_once(
    recipient: &str,
    bearer: &str,
    certificate: &str,
    seed: &[u8; 32],
    pending: &PendingEvent,
    send: bool,
) -> Result<bool> {
    let client = super::api_claim::api_http_client()?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async {
        if send {
            let now = super::api_claim::unix_seconds()?;
            if now < pending.proof_issued_at || now >= pending.proof_expires_at {
                bail!("lifecycle witness expired before send");
            }
            let proof = pending.header(certificate, seed, recipient)?;
            let body = Zeroizing::new(serde_json::to_vec(&pending.event)?);
            if body.len() > 16 * 1024 || proof.len() > 48 * 1024 {
                bail!("lifecycle request exceeds its limit");
            }
            let authorization = Zeroizing::new(format!("Bearer {bearer}"));
            let response = client
                .post(format!("{recipient}{ROUTE}"))
                .header("authorization", authorization.as_str())
                .header("x-ops-project-id", pending.event.project_id.to_string())
                .header("x-ops-runtime-lifecycle", proof.as_str())
                .header("content-type", "application/json")
                .body(body.as_slice().to_vec())
                .timeout(REQUEST_TIMEOUT)
                .send()
                .await;
            if let Ok(response) = response
                && let Ok(body) = bounded_body(response).await
                && let Ok(result) = serde_json::from_slice::<PostResponse>(&body)
                && result.status == "ok"
                && valid_request_id(&result.request_id)
                && pending.validate_receipt(&result.lifecycle_receipt).is_ok()
            {
                let _historical = result.historical;
                return Ok(true);
            }
        }
        read_receipt(&client, recipient, bearer, pending).await
    })
}

fn required_nullable<'de, D, T>(deserializer: D) -> std::result::Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer)
}

pub(super) fn timestamp(seconds: i64) -> Result<String> {
    let timestamp = DateTime::<Utc>::from_timestamp(seconds, 0)
        .ok_or_else(|| anyhow::anyhow!("invalid lifecycle timestamp"))?;
    Ok(timestamp.to_rfc3339_opts(SecondsFormat::Secs, true))
}

fn timestamp_seconds(value: &str) -> Result<i64> {
    Ok(DateTime::parse_from_rfc3339(value)?.timestamp())
}

fn same_timestamp(first: &str, second: &str) -> Result<bool> {
    Ok(DateTime::parse_from_rfc3339(first)? == DateTime::parse_from_rfc3339(second)?)
}

fn validate_event(event: &Event) -> Result<()> {
    let valid_digest = |value: &str| {
        value.len() == 64
            && value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    };
    let valid_cause = match (event.phase, event.source, event.cause) {
        (Phase::Start, Source::Launcher, Cause::LaunchRegistered) => {
            event.exit_code.is_none() && event.signal.is_none() && !event.absence_proven
        }
        (Phase::End, Source::Launcher, Cause::Exited) => {
            event
                .exit_code
                .is_some_and(|code| (0..=255).contains(&code))
                && event.signal.is_none()
                && event.absence_proven
        }
        (Phase::End, Source::Launcher, Cause::Signalled) => {
            event
                .signal
                .is_some_and(|signal| (1..=64).contains(&signal))
                && event.exit_code.is_none()
                && event.absence_proven
        }
        (Phase::End, Source::Launcher, Cause::Timeout)
        | (Phase::End, Source::Launcher | Source::Systemd, Cause::Unobserved) => {
            event.exit_code.is_none() && event.signal.is_none() && event.absence_proven
        }
        _ => false,
    };
    if event.version != 1
        || event.principal_id.is_nil()
        || event.project_id == 0
        || event.project_id > ((1_u64 << 53) - 1)
        || event.runtime_incarnation.is_nil()
        || event.event_id.is_nil()
        || event.manager_invocation.is_nil()
        || !valid_digest(&event.certificate_sha256)
        || !valid_digest(&event.observer_artifact_sha256)
        || event.issuer_kid.is_empty()
        || event.issuer_kid.len() > 128
        || !event
            .issuer_kid
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
        || event.work_item_ref.strip_prefix('w').is_none_or(|digits| {
            digits.is_empty()
                || digits.starts_with('0')
                || !digits.bytes().all(|byte| byte.is_ascii_digit())
                || digits
                    .parse::<u64>()
                    .ok()
                    .is_none_or(|value| value > ((1_u64 << 53) - 1))
        })
        || !valid_cause
    {
        bail!("invalid lifecycle event");
    }
    timestamp_seconds(&event.observed_at)?;
    Ok(())
}

impl PendingEvent {
    pub(super) fn new(event: Event, now: i64) -> Result<Self> {
        validate_event(&event)?;
        if timestamp_seconds(&event.observed_at)? > now {
            bail!("lifecycle observation is in the future");
        }
        let operation = wire::canonical_lifecycle_operation(&serde_json::to_value(&event)?)?;
        Ok(Self {
            event,
            operation_sha256: wire::digest(&operation),
            proof_issued_at: now,
            proof_expires_at: now
                .checked_add(WITNESS_SECONDS)
                .ok_or_else(|| anyhow::anyhow!("lifecycle witness expiry overflow"))?,
            may_have_been_sent: false,
        })
    }

    pub(super) fn header(
        &self,
        certificate: &str,
        seed: &[u8; 32],
        recipient: &str,
    ) -> Result<Zeroizing<String>> {
        validate_event(&self.event)?;
        if wire::certificate_digest(certificate.as_bytes()) != self.event.certificate_sha256
            || self.proof_expires_at.checked_sub(self.proof_issued_at) != Some(WITNESS_SECONDS)
            || wire::digest(&wire::canonical_lifecycle_operation(
                &serde_json::to_value(&self.event)?,
            )?) != self.operation_sha256
        {
            bail!("lifecycle witness bindings changed");
        }
        let header = wire::RequestHeader {
            alg: "EdDSA",
            typ: "runtime-lifecycle+jwt",
            kid: &self.event.issuer_kid,
        };
        let claims = WitnessClaims {
            version: 1,
            recipient,
            method: "POST",
            route: ROUTE,
            purpose: "work-lifecycle",
            operation_sha256: &self.operation_sha256,
            iat: self.proof_issued_at,
            exp: self.proof_expires_at,
            event_id: self.event.event_id,
            certificate_sha256: &self.event.certificate_sha256,
            runtime_incarnation: self.event.runtime_incarnation,
        };
        let encoded_header = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&header)?);
        let encoded_claims = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims)?);
        let input = Zeroizing::new(format!("{encoded_header}.{encoded_claims}"));
        let signature = SigningKey::from_bytes(seed).sign(input.as_bytes());
        let witness = Zeroizing::new(format!(
            "{}.{}",
            input.as_str(),
            URL_SAFE_NO_PAD.encode(signature.to_bytes())
        ));
        Ok(Zeroizing::new(URL_SAFE_NO_PAD.encode(serde_json::to_vec(
            &Envelope {
                certificate,
                witness: witness.as_str(),
            },
        )?)))
    }

    pub(super) fn validate_receipt(&self, receipt: &Receipt) -> Result<()> {
        let event = &self.event;
        let work_id = event
            .work_item_ref
            .strip_prefix('w')
            .and_then(|id| id.parse::<u64>().ok());
        if receipt.event_id != event.event_id.to_string()
            || receipt.issuer_kid != event.issuer_kid
            || receipt.certificate_sha256 != event.certificate_sha256
            || receipt.runtime_incarnation != event.runtime_incarnation.to_string()
            || Some(receipt.work_item_id) != work_id
            || receipt.project_id != event.project_id
            || receipt.principal_id != event.principal_id.to_string()
            || receipt.phase != event.phase
            || receipt.manager_invocation != event.manager_invocation.to_string()
            || receipt.observer_artifact_sha256 != event.observer_artifact_sha256
            || receipt.operation_sha256 != self.operation_sha256
            || receipt.source != event.source
            || receipt.cause != event.cause
            || receipt.exit_code != event.exit_code
            || receipt.signal != event.signal
            || receipt.absence_proven != event.absence_proven
            || !same_timestamp(&receipt.observed_at, &event.observed_at)?
            || !same_timestamp(&receipt.proof_issued_at, &timestamp(self.proof_issued_at)?)?
            || !same_timestamp(
                &receipt.proof_expires_at,
                &timestamp(self.proof_expires_at)?,
            )?
            || !receipt.runtime_epoch_recorded
            || receipt.ended != (event.phase == Phase::End)
            || (event.phase == Phase::Start
                && (!receipt.terminalized_execution_instances.is_empty()
                    || receipt.released_current_claim.is_some()))
            || !receipt
                .terminalized_execution_instances
                .iter()
                .all(|id| canonical_uuid(id))
            || !receipt
                .terminalized_execution_instances
                .windows(2)
                .all(|pair| pair[0] < pair[1])
            || receipt
                .released_current_claim
                .as_ref()
                .is_some_and(|claim| {
                    !canonical_uuid(&claim.execution_instance_id)
                        || claim.generation == 0
                        || !receipt
                            .terminalized_execution_instances
                            .contains(&claim.execution_instance_id)
                })
        {
            bail!("lifecycle receipt does not bind the event");
        }
        Ok(())
    }
}

fn verified_certificate(
    context: &Context,
    policy: &super::LauncherPolicy,
    target: &super::McpLaunchConfig,
    observer_sha256: &str,
    policy_sha256: &str,
    invocation: Uuid,
) -> Result<wire::CertificateClaims> {
    if context.version != 1
        || context.policy_sha256 != policy_sha256
        || context.selected_uid != target.runtime_uid
        || context.pending_start.event.manager_invocation != invocation
        || context.pending_start.event.observer_artifact_sha256 != observer_sha256
        || context.pending_start.event.phase != Phase::Start
        || context.end_event_id.is_nil()
        || context.end_event_id == context.pending_start.event.event_id
        || (context.end_acknowledged && context.pending_end.is_none())
    {
        bail!("lifecycle context binding is invalid");
    }
    let (header, claims) = wire::parse_certificate(&context.certificate)?;
    let mut parts = context.certificate.split('.');
    let (Some(encoded_header), Some(encoded_claims), Some(encoded_signature), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        bail!("invalid original lifecycle certificate");
    };
    let public_key: [u8; 32] = URL_SAFE_NO_PAD
        .decode(&policy.issuer_public_key)?
        .try_into()
        .map_err(|_| anyhow::anyhow!("invalid lifecycle issuer key"))?;
    let signature = Signature::from_slice(&URL_SAFE_NO_PAD.decode(encoded_signature)?)?;
    VerifyingKey::from_bytes(&public_key)?.verify_strict(
        format!("{encoded_header}.{encoded_claims}").as_bytes(),
        &signature,
    )?;
    let event = &context.pending_start.event;
    let expected_operations = vec![
        wire::ProtectedOperation {
            tool: "work_item_claim".to_string(),
            purpose: "work-claim".to_string(),
        },
        wire::ProtectedOperation {
            tool: "work_item_release".to_string(),
            purpose: "work-release".to_string(),
        },
    ];
    if header.alg != "EdDSA"
        || header.typ != "runtime-certificate+jwt"
        || header.kid != policy.issuer_kid
        || claims.version != 2
        || claims.provider != "codex-native-runtime"
        || claims.principal != policy.principal
        || claims.project_id != policy.project_id
        || claims.work_item_ref != policy.work_item_ref
        || claims.recipient != target.mcp_recipient
        || claims.artifact_sha256 != target.codex_dev_sha256
        || claims.mcp_server.as_deref() != Some(super::MCP_SERVER)
        || claims.method != "tools/call"
        || claims.tool.is_some()
        || claims.purpose.is_some()
        || claims.operations.as_ref() != Some(&expected_operations)
        || claims.iat < 0
        || claims.exp <= claims.iat
        || claims
            .exp
            .checked_sub(claims.iat)
            .is_none_or(|duration| duration > wire::MAX_CERTIFICATE_SECONDS)
        || claims.execution_lease_seconds == 0
        || i64::from(claims.execution_lease_seconds) > wire::MAX_CERTIFICATE_SECONDS
        || event.principal_id != policy.principal
        || event.project_id != policy.project_id
        || event.work_item_ref != policy.work_item_ref
        || event.issuer_kid != policy.issuer_kid
        || event.certificate_sha256 != wire::certificate_digest(context.certificate.as_bytes())
        || event.runtime_incarnation != claims.runtime_incarnation
    {
        bail!("original lifecycle certificate does not bind the protected policy");
    }
    validate_pending(&context.pending_start, event)?;
    let start_observed = timestamp_seconds(&event.observed_at)?;
    if start_observed < claims.iat || start_observed >= claims.exp {
        bail!("lifecycle start is outside the certificate interval");
    }
    if let Some(observed) = &context.observed_end {
        let mut candidate = event.clone();
        candidate.event_id = context.end_event_id;
        candidate.phase = Phase::End;
        candidate.source = Source::Launcher;
        candidate.cause = observed.cause;
        candidate.observed_at = observed.observed_at.clone();
        candidate.exit_code = observed.exit_code;
        candidate.signal = observed.signal;
        candidate.absence_proven = true;
        validate_event(&candidate)?;
        if timestamp_seconds(&observed.observed_at)? < claims.iat {
            bail!("lifecycle observation is not bound to its end");
        }
        if let Some(pending) = &context.pending_end {
            candidate.observed_at = pending.event.observed_at.clone();
            if pending.event != candidate
                || timestamp_seconds(&pending.event.observed_at)?
                    < timestamp_seconds(&observed.observed_at)?
            {
                bail!("lifecycle observation is not bound to its end");
            }
        }
    }
    if let Some(end) = &context.pending_end {
        let mut expected = event.clone();
        expected.event_id = end.event.event_id;
        expected.phase = Phase::End;
        expected.source = end.event.source;
        expected.cause = end.event.cause;
        expected.observed_at = end.event.observed_at.clone();
        expected.exit_code = end.event.exit_code;
        expected.signal = end.event.signal;
        expected.absence_proven = end.event.absence_proven;
        if end.event != expected || end.event.event_id != context.end_event_id {
            bail!("lifecycle end is not bound to its start");
        }
        validate_pending(end, &end.event)?;
        if timestamp_seconds(&end.event.observed_at)? < claims.iat {
            bail!("lifecycle end predates its certificate");
        }
    }
    Ok(claims)
}

fn validate_pending(pending: &PendingEvent, event: &Event) -> Result<()> {
    validate_event(event)?;
    if pending
        .proof_expires_at
        .checked_sub(pending.proof_issued_at)
        != Some(WITNESS_SECONDS)
        || timestamp_seconds(&event.observed_at)? > pending.proof_issued_at
        || pending.operation_sha256
            != wire::digest(&wire::canonical_lifecycle_operation(
                &serde_json::to_value(event)?,
            )?)
    {
        bail!("lifecycle pending event is invalid");
    }
    Ok(())
}

/// A potentially sent tuple is immutable even after a null receipt read.
fn arm_end(pending: &mut PendingEvent, now: i64) -> Result<bool> {
    if pending.may_have_been_sent {
        return Ok(false);
    }
    let observed = timestamp_seconds(&pending.event.observed_at)?;
    if observed > now || now.saturating_sub(observed) > 60 {
        bail!("lifecycle observation is outside the witness interval");
    }
    pending.proof_issued_at = now;
    pending.proof_expires_at = now
        .checked_add(WITNESS_SECONDS)
        .ok_or_else(|| anyhow::anyhow!("lifecycle witness expiry overflow"))?;
    pending.may_have_been_sent = true;
    Ok(true)
}

fn read_bearer(
    credentials: &Path,
    policy: &super::LauncherPolicy,
    now: i64,
) -> Result<super::api_claim::ApiBearer> {
    let bytes = super::read_credential(
        credentials,
        super::api_claim::API_BEARER_CREDENTIAL,
        super::api_claim::API_BEARER_CREDENTIAL_MAX_BYTES,
    )?;
    let mut secret = super::api_claim::decode_bearer_credential(&bytes)?;
    super::api_claim::validate_bearer(std::mem::take(&mut secret.0), policy.principal, now)
}

/// Persist and acknowledge start before the child receives bootstrap authority.
pub(super) fn start(
    policy: &super::LauncherPolicy,
    target: &super::McpLaunchConfig,
    credentials: &Path,
    certificate: &str,
    claims: &wire::CertificateClaims,
    seed: &[u8; 32],
    policy_sha256: &str,
    observer_sha256: &str,
) -> Result<Context> {
    let now = super::api_claim::unix_seconds()?;
    let bearer = read_bearer(credentials, policy, now)?;
    let invocation = manager_invocation()?;
    let event = Event {
        version: 1,
        principal_id: policy.principal,
        project_id: policy.project_id,
        work_item_ref: policy.work_item_ref.clone(),
        issuer_kid: policy.issuer_kid.clone(),
        certificate_sha256: wire::certificate_digest(certificate.as_bytes()),
        runtime_incarnation: claims.runtime_incarnation,
        event_id: Uuid::new_v4(),
        phase: Phase::Start,
        source: Source::Launcher,
        manager_invocation: invocation,
        observer_artifact_sha256: observer_sha256.to_string(),
        observed_at: timestamp(now)?,
        cause: Cause::LaunchRegistered,
        exit_code: None,
        signal: None,
        absence_proven: false,
    };
    let mut pending_start = PendingEvent::new(event, now)?;
    // Persist the potentially-sent tuple before the HTTP request can begin.
    pending_start.may_have_been_sent = true;
    let mut context = Context {
        version: 1,
        certificate: certificate.to_string(),
        policy_sha256: policy_sha256.to_string(),
        selected_uid: target.runtime_uid,
        pending_start,
        start_acknowledged: false,
        end_event_id: Uuid::new_v4(),
        observed_end: None,
        pending_end: None,
        end_acknowledged: false,
    };
    verified_certificate(
        &context,
        policy,
        target,
        observer_sha256,
        policy_sha256,
        invocation,
    )?;
    persist_context(&context, /*create*/ true)?;
    if !deliver_once(
        super::api_claim::API_RECIPIENT,
        &bearer.value.0,
        certificate,
        seed,
        &context.pending_start,
        /*send*/ true,
    )? {
        bail!("lifecycle start remains unresolved");
    }
    context.start_acknowledged = true;
    persist_context(&context, /*create*/ false)?;
    Ok(context)
}

/// Deliver a launcher-observed end, or a systemd unobserved end after teardown.
pub(super) fn finish(
    context: &mut Context,
    policy: &super::LauncherPolicy,
    target: &super::McpLaunchConfig,
    credentials: &Path,
    policy_sha256: &str,
    observer_sha256: &str,
    source: Source,
    cause: Cause,
    exit_code: Option<i32>,
    signal: Option<i32>,
) -> Result<()> {
    let invocation = manager_invocation()?;
    verified_certificate(
        context,
        policy,
        target,
        observer_sha256,
        policy_sha256,
        invocation,
    )?;
    if context.end_acknowledged {
        return Ok(());
    }
    if source == Source::Launcher && context.observed_end.is_none() && context.pending_end.is_none()
    {
        let observed = super::api_claim::unix_seconds()?;
        let mut candidate = context.pending_start.event.clone();
        candidate.event_id = context.end_event_id;
        candidate.phase = Phase::End;
        candidate.source = source;
        candidate.cause = cause;
        candidate.observed_at = timestamp(observed)?;
        candidate.exit_code = exit_code;
        candidate.signal = signal;
        // Validate the intended closed cause; absence is not asserted or sent yet.
        candidate.absence_proven = true;
        validate_event(&candidate)?;
        context.observed_end = Some(EndObservation {
            observed_at: candidate.observed_at,
            cause,
            exit_code,
            signal,
        });
        persist_context(context, /*create*/ false)?;
    }
    // A launcher-observed outcome is only retained here. The synchronous
    // stop observer is solely responsible for proving the epoch boundary and
    // publishing an end; source selection alone is not observer authentication.
    if !is_stop_observer(source) {
        return Ok(());
    }
    require_uid_absent(context.selected_uid)?;
    let now = super::api_claim::unix_seconds()?;
    if context.pending_end.is_none() {
        let mut event = context.pending_start.event.clone();
        event.event_id = context.end_event_id;
        event.phase = Phase::End;
        if let Some(observed) = &context.observed_end {
            event.source = Source::Launcher;
            event.cause = observed.cause;
            event.observed_at = timestamp(now)?;
            event.exit_code = observed.exit_code;
            event.signal = observed.signal;
        } else {
            event.source = source;
            event.cause = cause;
            event.observed_at = timestamp(now)?;
            event.exit_code = exit_code;
            event.signal = signal;
        }
        event.absence_proven = true;
        context.pending_end = Some(PendingEvent::new(event, now)?);
        persist_context(context, /*create*/ false)?;
    }
    // Preserve a known child outcome even when credentials are unavailable.
    // Refresh proof times only before the event can possibly have been sent.
    let bearer = read_bearer(credentials, policy, now)?;
    let seed = super::read_issuer_seed(credentials, policy)?;
    let pending = context
        .pending_end
        .as_mut()
        .ok_or_else(|| anyhow::anyhow!("missing lifecycle end"))?;
    let send = arm_end(pending, super::api_claim::unix_seconds()?)?;
    if send {
        persist_context(context, /*create*/ false)?;
    }
    let pending = context
        .pending_end
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("missing lifecycle end"))?;
    if send {
        // Recheck after credential loading and durable preparation, immediately
        // before the request that can assert the absence observation.
        require_uid_absent(context.selected_uid)?;
    }
    if !deliver_once(
        super::api_claim::API_RECIPIENT,
        &bearer.value.0,
        &context.certificate,
        &seed,
        pending,
        send,
    )? {
        bail!("lifecycle end remains unresolved");
    }
    context.end_acknowledged = true;
    persist_context(context, /*create*/ false)?;
    Ok(())
}

fn is_stop_observer(source: Source) -> bool {
    source == Source::Systemd
}

#[cfg(test)]
#[path = "lifecycle_tests.rs"]
mod tests;
