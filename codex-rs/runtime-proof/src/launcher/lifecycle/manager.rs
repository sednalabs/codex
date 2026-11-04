//! Fixed system-manager and cgroup proof for the lifecycle stop observer.

use super::super::sha256_file;
use anyhow::Result;
use anyhow::bail;
use serde::Deserialize;
use serde::Serialize;
use std::collections::BTreeMap;
use std::ffi::CString;
use std::fs;
use std::fs::File;
use std::fs::OpenOptions;
use std::io::Read as _;
use std::os::fd::AsRawFd as _;
use std::os::fd::FromRawFd as _;
use std::os::unix::fs::MetadataExt as _;
use std::os::unix::fs::OpenOptionsExt as _;
use std::process::Command;
use std::process::Stdio;
use uuid::Uuid;

const SYSTEMCTL_PATH: &str = "/usr/bin/systemctl";
const UNIT_NAME: &str = "codex-runtime-proof-claim.service";
const UNIT_CGROUP: &str = "/system.slice/codex-runtime-proof-claim.service";
const CGROUP_ROOT: &str = "/sys/fs/cgroup";
const MAX_SYSTEMCTL_OUTPUT: usize = 4096;
const MAX_CGROUP_MEMBERSHIP_BYTES: usize = 4096;
const MAX_PIDS_CURRENT_BYTES: usize = 32;
const CGROUP2_SUPER_MAGIC: i64 = 0x6367_7270;
const UNIT_PROPERTIES: &str = "Id,InvocationID,ControlGroup,MainPID,ControlPID,TasksAccounting,Delegate,KillMode,SendSIGKILL,NeedDaemonReload,SubState";

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(in crate::launcher) struct ManagerBinding {
    pub(super) boot_id: Uuid,
    pub(super) manager_executable: String,
    pub(super) manager_executable_sha256: String,
    pub(super) manager_pid: u32,
    pub(super) manager_executable_device: u64,
    pub(super) manager_executable_inode: u64,
    pub(super) unit: String,
    pub(super) invocation_id: Uuid,
    pub(super) control_group: String,
    pub(super) control_group_device: u64,
    pub(super) control_group_inode: u64,
}

#[derive(Debug)]
struct UnitSnapshot {
    unit: String,
    invocation_id: Uuid,
    control_group: String,
    main_pid: u32,
    control_pid: u32,
    tasks_accounting: bool,
    delegate: bool,
    kill_mode: String,
    send_sigkill: bool,
    need_daemon_reload: bool,
    sub_state: String,
}

/// Capture the exact systemd invocation before the launcher can release a certificate.
pub(super) fn capture_start_binding(expected_manager_sha256: &str) -> Result<ManagerBinding> {
    let (manager_pid, manager_executable, manager_device, manager_inode, manager_executable_sha256) =
        trusted_system_manager(expected_manager_sha256)?;
    let boot_id = current_boot_id()?;
    let invocation_id = invocation_id_from_environment()?;
    let snapshot = query_unit()?;
    validate_common(&snapshot, invocation_id)?;
    let launcher_pid = std::process::id();
    if snapshot.main_pid != launcher_pid || snapshot.control_pid != 0 {
        bail!("lifecycle start is not the fixed systemd main process");
    }
    require_current_cgroup(UNIT_CGROUP)?;
    let (device, inode) = cgroup_identity(UNIT_CGROUP)?;
    if pids_current(UNIT_CGROUP)? != 1 {
        bail!("lifecycle start cgroup is not exclusive");
    }
    Ok(ManagerBinding {
        boot_id,
        manager_executable,
        manager_executable_sha256,
        manager_pid,
        manager_executable_device: manager_device,
        manager_executable_inode: manager_inode,
        unit: UNIT_NAME.to_string(),
        invocation_id,
        control_group: UNIT_CGROUP.to_string(),
        control_group_device: device,
        control_group_inode: inode,
    })
}

/// Recheck that certificate release is still inside the captured manager invocation.
pub(super) fn require_start_binding(binding: &ManagerBinding, child_pid: u32) -> Result<()> {
    let snapshot = verify_binding(binding)?;
    require_current_cgroup(&binding.control_group)?;
    require_process_cgroup(child_pid, &binding.control_group)?;
    require_stopped_child(child_pid)?;
    let launcher_tasks = process_task_count(std::process::id())?;
    let child_tasks = process_task_count(child_pid)?;
    validate_start_observation(
        &snapshot,
        binding,
        std::process::id(),
        child_pid,
        launcher_tasks,
        child_tasks,
        pids_current(&binding.control_group)?,
    )
}

/// Recheck the bound manager invocation while the measured child is executing.
pub(super) fn require_running_binding(binding: &ManagerBinding, child_pid: u32) -> Result<()> {
    let snapshot = verify_binding(binding)?;
    require_current_cgroup(&binding.control_group)?;
    require_process_cgroup(child_pid, &binding.control_group)?;
    validate_running_observation(&snapshot, binding, std::process::id())
}

/// Prove that this process is the only task left in this exact stop invocation.
pub(super) fn require_stop_binding(binding: &ManagerBinding) -> Result<()> {
    let snapshot = verify_binding(binding)?;
    require_current_cgroup(&binding.control_group)?;
    validate_stop_observation(
        &snapshot,
        binding,
        std::process::id(),
        pids_current(&binding.control_group)?,
    )
}

impl ManagerBinding {
    pub(in crate::launcher) fn invocation_id(&self) -> Uuid {
        self.invocation_id
    }

    pub(super) fn validate(&self) -> Result<()> {
        super::super::validate_sha256(&self.manager_executable_sha256)?;
        if self.boot_id.is_nil()
            || self.manager_pid != 1
            || !valid_systemd_path(&self.manager_executable)
            || self.manager_executable_device == 0
            || self.manager_executable_inode == 0
            || self.unit != UNIT_NAME
            || self.invocation_id.is_nil()
            || self.control_group != UNIT_CGROUP
            || self.control_group_device == 0
            || self.control_group_inode == 0
        {
            bail!("lifecycle manager binding is invalid");
        }
        Ok(())
    }
}

fn verify_binding(binding: &ManagerBinding) -> Result<UnitSnapshot> {
    binding.validate()?;
    let (manager_pid, manager_executable, manager_device, manager_inode, manager_sha256) =
        trusted_system_manager(&binding.manager_executable_sha256)?;
    validate_manager_identity(
        binding,
        manager_pid,
        &manager_executable,
        manager_device,
        manager_inode,
        &manager_sha256,
    )?;
    if current_boot_id()? != binding.boot_id
        || invocation_id_from_environment()? != binding.invocation_id
    {
        bail!("lifecycle manager invocation changed");
    }
    let snapshot = query_unit()?;
    validate_common(&snapshot, binding.invocation_id)?;
    if snapshot.unit != binding.unit || snapshot.control_group != binding.control_group {
        bail!("lifecycle unit or cgroup identity changed");
    }
    let (device, inode) = cgroup_identity(&binding.control_group)?;
    if device != binding.control_group_device || inode != binding.control_group_inode {
        bail!("lifecycle cgroup object changed");
    }
    Ok(snapshot)
}

fn validate_manager_identity(
    binding: &ManagerBinding,
    manager_pid: u32,
    manager_executable: &str,
    manager_device: u64,
    manager_inode: u64,
    manager_sha256: &str,
) -> Result<()> {
    if manager_pid != binding.manager_pid
        || manager_executable != binding.manager_executable
        || manager_device != binding.manager_executable_device
        || manager_inode != binding.manager_executable_inode
        || manager_sha256 != binding.manager_executable_sha256
    {
        bail!("lifecycle system manager identity changed");
    }
    Ok(())
}

fn validate_common(snapshot: &UnitSnapshot, invocation_id: Uuid) -> Result<()> {
    if snapshot.unit != UNIT_NAME
        || snapshot.invocation_id != invocation_id
        || snapshot.control_group != UNIT_CGROUP
        || !snapshot.tasks_accounting
        || snapshot.delegate
        || snapshot.kill_mode != "control-group"
        || !snapshot.send_sigkill
        || snapshot.need_daemon_reload
    {
        bail!("lifecycle systemd unit is outside its fixed containment contract");
    }
    Ok(())
}

fn validate_start_observation(
    snapshot: &UnitSnapshot,
    binding: &ManagerBinding,
    launcher_pid: u32,
    child_pid: u32,
    launcher_tasks: u64,
    child_tasks: u64,
    hierarchical_tasks: u64,
) -> Result<()> {
    if launcher_pid == 0
        || child_pid == 0
        || child_pid == launcher_pid
        || snapshot.invocation_id != binding.invocation_id
        || snapshot.main_pid != launcher_pid
        || snapshot.control_pid != 0
        || launcher_tasks == 0
        || child_tasks == 0
        || launcher_tasks.checked_add(child_tasks) != Some(hierarchical_tasks)
    {
        bail!("lifecycle start manager binding changed");
    }
    Ok(())
}

fn validate_stop_observation(
    snapshot: &UnitSnapshot,
    binding: &ManagerBinding,
    observer_pid: u32,
    hierarchical_tasks: u64,
) -> Result<()> {
    if observer_pid == 0
        || snapshot.invocation_id != binding.invocation_id
        || snapshot.main_pid != 0
        || snapshot.control_pid != observer_pid
        || snapshot.sub_state != "stop-post"
        || hierarchical_tasks != 1
    {
        bail!("lifecycle stop is not the fixed systemd post-stop observer");
    }
    Ok(())
}

fn validate_running_observation(
    snapshot: &UnitSnapshot,
    binding: &ManagerBinding,
    launcher_pid: u32,
) -> Result<()> {
    if launcher_pid == 0
        || snapshot.invocation_id != binding.invocation_id
        || snapshot.main_pid != launcher_pid
        || snapshot.control_pid != 0
        || snapshot.sub_state != "running"
    {
        bail!("lifecycle manager invocation is not running its bound task");
    }
    Ok(())
}

fn query_unit() -> Result<UnitSnapshot> {
    let output = Command::new(SYSTEMCTL_PATH)
        .env_clear()
        .env("LC_ALL", "C")
        .env("SYSTEMD_COLORS", "0")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .arg("--system")
        .arg("--no-ask-password")
        .arg("--no-pager")
        .arg("show")
        .arg(format!("--property={UNIT_PROPERTIES}"))
        .arg(UNIT_NAME)
        .output()
        .map_err(|_| anyhow::anyhow!("query fixed systemd unit properties"))?;
    if !output.status.success() || output.stdout.len() > MAX_SYSTEMCTL_OUTPUT {
        bail!("fixed systemd unit properties are unavailable");
    }
    let text = std::str::from_utf8(&output.stdout)
        .map_err(|_| anyhow::anyhow!("fixed systemd unit properties are invalid"))?;
    parse_unit_snapshot(text)
}

fn parse_unit_snapshot(text: &str) -> Result<UnitSnapshot> {
    const EXPECTED: [&str; 11] = [
        "Id",
        "InvocationID",
        "ControlGroup",
        "MainPID",
        "ControlPID",
        "TasksAccounting",
        "Delegate",
        "KillMode",
        "SendSIGKILL",
        "NeedDaemonReload",
        "SubState",
    ];
    let mut fields = BTreeMap::new();
    for line in text.lines() {
        let (key, value) = line
            .split_once('=')
            .ok_or_else(|| anyhow::anyhow!("fixed systemd unit property is malformed"))?;
        if !EXPECTED.contains(&key)
            || value.len() > 512
            || value.chars().any(char::is_control)
            || fields.insert(key, value).is_some()
        {
            bail!("fixed systemd unit properties are not a closed response");
        }
    }
    if fields.len() != EXPECTED.len() {
        bail!("fixed systemd unit properties are incomplete");
    }
    Ok(UnitSnapshot {
        unit: required_field(&fields, "Id")?.to_string(),
        invocation_id: parse_systemd_invocation(required_field(&fields, "InvocationID")?)?,
        control_group: required_field(&fields, "ControlGroup")?.to_string(),
        main_pid: parse_pid(required_field(&fields, "MainPID")?)?,
        control_pid: parse_pid(required_field(&fields, "ControlPID")?)?,
        tasks_accounting: parse_bool(required_field(&fields, "TasksAccounting")?)?,
        delegate: parse_bool(required_field(&fields, "Delegate")?)?,
        kill_mode: required_field(&fields, "KillMode")?.to_string(),
        send_sigkill: parse_bool(required_field(&fields, "SendSIGKILL")?)?,
        need_daemon_reload: parse_bool(required_field(&fields, "NeedDaemonReload")?)?,
        sub_state: required_field(&fields, "SubState")?.to_string(),
    })
}

fn required_field<'a>(fields: &'a BTreeMap<&str, &str>, key: &str) -> Result<&'a str> {
    fields
        .get(key)
        .copied()
        .ok_or_else(|| anyhow::anyhow!("fixed systemd unit property is missing"))
}

fn parse_bool(value: &str) -> Result<bool> {
    match value {
        "yes" => Ok(true),
        "no" => Ok(false),
        _ => bail!("fixed systemd boolean property is invalid"),
    }
}

fn parse_pid(value: &str) -> Result<u32> {
    let pid = value.parse::<u32>()?;
    if pid.to_string() != value {
        bail!("fixed systemd process property is not canonical");
    }
    Ok(pid)
}

fn parse_systemd_invocation(value: &str) -> Result<Uuid> {
    if value.len() != 32 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("systemd invocation ID is invalid");
    }
    let invocation = Uuid::parse_str(value)?;
    if invocation.is_nil() {
        bail!("systemd invocation ID is invalid");
    }
    Ok(invocation)
}

fn invocation_id_from_environment() -> Result<Uuid> {
    parse_systemd_invocation(&std::env::var("INVOCATION_ID")?)
}

fn trusted_system_manager(expected_sha256: &str) -> Result<(u32, String, u64, u64, String)> {
    if unsafe { libc::geteuid() } != 0 || unsafe { libc::getppid() } != 1 {
        bail!("lifecycle observer is not a root child of the system manager");
    }
    let executable = fs::canonicalize("/proc/1/exe")?;
    if !valid_systemd_path(&executable.to_string_lossy()) {
        bail!("lifecycle system manager executable is invalid");
    }
    let file = File::open("/proc/1/exe")?;
    let metadata = file.metadata()?;
    let path_metadata = fs::metadata(&executable)?;
    if !metadata.is_file()
        || metadata.uid() != 0
        || metadata.mode() & 0o111 == 0
        || metadata.mode() & 0o022 != 0
        || metadata.dev() != path_metadata.dev()
        || metadata.ino() != path_metadata.ino()
    {
        bail!("lifecycle system manager executable is not root controlled");
    }
    let digest = sha256_file(&file)?;
    validate_manager_digest(expected_sha256, &digest)?;
    Ok((
        1,
        executable.to_string_lossy().into_owned(),
        metadata.dev(),
        metadata.ino(),
        digest,
    ))
}

fn validate_manager_digest(expected_sha256: &str, observed_sha256: &str) -> Result<()> {
    super::super::validate_sha256(expected_sha256)?;
    if observed_sha256 != expected_sha256 {
        bail!("lifecycle system manager differs from the root policy pin");
    }
    Ok(())
}

fn valid_systemd_path(value: &str) -> bool {
    let path = std::path::Path::new(value);
    path.is_absolute()
        && path.file_name().is_some_and(|name| name == "systemd")
        && !value.chars().any(char::is_control)
}

fn current_boot_id() -> Result<Uuid> {
    let value = fs::read_to_string("/proc/sys/kernel/random/boot_id")?;
    let value = value.trim();
    let boot_id = Uuid::parse_str(value)?;
    if boot_id.is_nil() || boot_id.to_string() != value {
        bail!("kernel boot ID is invalid");
    }
    Ok(boot_id)
}

fn require_current_cgroup(expected: &str) -> Result<()> {
    require_process_cgroup(std::process::id(), expected)
}

fn require_process_cgroup(pid: u32, expected: &str) -> Result<()> {
    if pid == 0 {
        bail!("lifecycle process identity is invalid");
    }
    let contents = fs::read_to_string(format!("/proc/{pid}/cgroup"))?;
    if contents.len() > MAX_CGROUP_MEMBERSHIP_BYTES {
        bail!("lifecycle cgroup membership exceeds its limit");
    }
    let mut unified = None;
    for line in contents.lines() {
        if let Some((hierarchy, path)) = line.split_once("::")
            && hierarchy == "0"
            && unified.replace(path).is_some()
        {
            bail!("lifecycle cgroup membership is ambiguous");
        }
    }
    if unified != Some(expected) {
        bail!("lifecycle observer is outside its bound cgroup");
    }
    Ok(())
}

fn require_stopped_child(pid: u32) -> Result<()> {
    let status = fs::read_to_string(format!("/proc/{pid}/status"))?;
    if status.len() > MAX_CGROUP_MEMBERSHIP_BYTES {
        bail!("lifecycle child status exceeds its limit");
    }
    let state = status
        .lines()
        .find_map(|line| line.strip_prefix("State:"))
        .and_then(|value| value.split_whitespace().next());
    if state != Some("T") {
        bail!("lifecycle child is not held before authority release");
    }
    Ok(())
}

fn process_task_count(pid: u32) -> Result<u64> {
    let entries = fs::read_dir(format!("/proc/{pid}/task"))?;
    let mut count = 0_u64;
    for entry in entries {
        entry?;
        count = count
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("lifecycle process task count overflow"))?;
    }
    if count == 0 {
        bail!("lifecycle process has no tasks");
    }
    Ok(count)
}

fn cgroup_identity(path: &str) -> Result<(u64, u64)> {
    let directory = open_cgroup_directory(path)?;
    let metadata = directory.metadata()?;
    if !metadata.is_dir() || metadata.uid() != 0 {
        bail!("lifecycle cgroup directory is not manager controlled");
    }
    Ok((metadata.dev(), metadata.ino()))
}

fn pids_current(path: &str) -> Result<u64> {
    let directory = open_cgroup_directory(path)?;
    let name = CString::new("pids.current")?;
    let fd = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK,
        )
    };
    if fd < 0 {
        bail!("hierarchical cgroup task count is unavailable");
    }
    let file = unsafe { File::from_raw_fd(fd) };
    let mut value = String::new();
    file.take((MAX_PIDS_CURRENT_BYTES + 1) as u64)
        .read_to_string(&mut value)?;
    if value.len() > MAX_PIDS_CURRENT_BYTES {
        bail!("hierarchical cgroup task count exceeds its limit");
    }
    parse_pids_current(&value)
}

fn parse_pids_current(value: &str) -> Result<u64> {
    let value = value.strip_suffix('\n').unwrap_or(value);
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        bail!("hierarchical cgroup task count is invalid");
    }
    let count = value.parse::<u64>()?;
    if count.to_string() != value {
        bail!("hierarchical cgroup task count is not canonical");
    }
    Ok(count)
}

fn open_cgroup_directory(path: &str) -> Result<File> {
    if path != UNIT_CGROUP {
        bail!("lifecycle cgroup path is not the fixed unit path");
    }
    let mut stat = std::mem::MaybeUninit::<libc::statfs>::zeroed();
    let root = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(CGROUP_ROOT)?;
    if unsafe { libc::fstatfs(root.as_raw_fd(), stat.as_mut_ptr()) } != 0
        || unsafe { stat.assume_init_ref().f_type } != CGROUP2_SUPER_MAGIC
    {
        bail!("lifecycle requires the unified cgroup hierarchy");
    }
    let mut directory = root;
    for component in path.trim_start_matches('/').split('/') {
        let name = CString::new(component)?;
        let fd = unsafe {
            libc::openat(
                directory.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            bail!("lifecycle cgroup directory is unavailable");
        }
        directory = unsafe { File::from_raw_fd(fd) };
        if !directory.metadata()?.is_dir() {
            bail!("lifecycle cgroup path contains a non-directory");
        }
    }
    Ok(directory)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn properties() -> String {
        format!(
            "Id={UNIT_NAME}\nInvocationID=00000000000040008000000000000001\nControlGroup={UNIT_CGROUP}\nMainPID=42\nControlPID=0\nTasksAccounting=yes\nDelegate=no\nKillMode=control-group\nSendSIGKILL=yes\nNeedDaemonReload=no\nSubState=running\n"
        )
    }

    fn binding() -> ManagerBinding {
        ManagerBinding {
            boot_id: Uuid::from_u128(1),
            manager_executable: "/usr/lib/systemd/systemd".to_string(),
            manager_executable_sha256: "a".repeat(64),
            manager_pid: 1,
            manager_executable_device: 1,
            manager_executable_inode: 2,
            unit: UNIT_NAME.to_string(),
            invocation_id: Uuid::from_u128(0x00000000000040008000000000000001),
            control_group: UNIT_CGROUP.to_string(),
            control_group_device: 1,
            control_group_inode: 2,
        }
    }

    #[test]
    fn manager_executable_must_match_the_protected_policy_pin() {
        let expected = "a".repeat(64);
        assert!(validate_manager_digest(&expected, &expected).is_ok());
        assert!(validate_manager_digest(&expected, &"b".repeat(64)).is_err());
        assert!(validate_manager_digest("", &expected).is_err());
    }

    #[test]
    fn parses_the_closed_systemd_property_set() {
        let snapshot = parse_unit_snapshot(&properties()).expect("systemd properties");
        assert_eq!(snapshot.unit, UNIT_NAME);
        assert_eq!(
            snapshot.invocation_id,
            Uuid::from_u128(0x00000000000040008000000000000001)
        );
        assert_eq!(snapshot.control_group, UNIT_CGROUP);
        assert_eq!(snapshot.main_pid, 42);
        assert_eq!(snapshot.control_pid, 0);
        assert!(snapshot.tasks_accounting);
        assert!(!snapshot.delegate);
        assert_eq!(snapshot.kill_mode, "control-group");
        assert!(snapshot.send_sigkill);
        assert!(!snapshot.need_daemon_reload);
    }

    #[test]
    fn rejects_missing_duplicate_unknown_and_noncanonical_properties() {
        assert!(parse_unit_snapshot("").is_err());
        assert!(parse_unit_snapshot(&(properties() + "MainPID=42\n")).is_err());
        assert!(parse_unit_snapshot(&(properties() + "PrivatePIDs=no\n")).is_err());
        assert!(parse_unit_snapshot(&properties().replace("MainPID=42", "MainPID=042")).is_err());
        assert!(
            parse_unit_snapshot(
                &properties().replace("TasksAccounting=yes", "TasksAccounting=true")
            )
            .is_err()
        );
    }

    #[test]
    fn fixed_unit_rejects_containment_contract_drift() {
        let invocation = Uuid::from_u128(0x00000000000040008000000000000001);
        let snapshot = parse_unit_snapshot(&properties()).expect("unit snapshot");
        assert!(validate_common(&snapshot, invocation).is_ok());
        for (before, after) in [
            ("TasksAccounting=yes", "TasksAccounting=no"),
            ("Delegate=no", "Delegate=yes"),
            ("KillMode=control-group", "KillMode=mixed"),
            ("SendSIGKILL=yes", "SendSIGKILL=no"),
            ("NeedDaemonReload=no", "NeedDaemonReload=yes"),
        ] {
            let drifted = parse_unit_snapshot(&properties().replace(before, after))
                .expect("well-formed drifted unit snapshot");
            assert!(validate_common(&drifted, invocation).is_err());
        }
    }

    #[test]
    fn invocation_and_hierarchical_task_count_are_strict() {
        assert!(parse_systemd_invocation("00000000000040008000000000000001").is_ok());
        assert!(parse_systemd_invocation("00000000-0000-4000-8000-000000000001").is_err());
        assert!(parse_systemd_invocation("00000000000000000000000000000000").is_err());
        assert_eq!(parse_pids_current("1\n").expect("task count"), 1);
        assert_eq!(parse_pids_current("0\n").expect("empty cgroup"), 0);
        assert!(parse_pids_current("1\n2\n").is_err());
        assert!(parse_pids_current("01\n").is_err());
    }

    #[test]
    fn binding_is_fixed_to_one_system_unit_and_epoch() {
        let binding = binding();
        assert!(binding.validate().is_ok());
        assert!(
            validate_manager_identity(
                &binding,
                1,
                "/usr/lib/systemd/systemd",
                1,
                2,
                &binding.manager_executable_sha256,
            )
            .is_ok()
        );
        assert!(
            validate_manager_identity(
                &binding,
                1,
                "/usr/lib/systemd/systemd",
                1,
                3,
                &binding.manager_executable_sha256,
            )
            .is_err()
        );
        assert!(
            validate_manager_identity(
                &binding,
                1,
                "/usr/lib/systemd/systemd",
                2,
                2,
                &binding.manager_executable_sha256,
            )
            .is_err()
        );
        assert!(
            validate_manager_identity(
                &binding,
                2,
                "/usr/lib/systemd/systemd",
                1,
                2,
                &binding.manager_executable_sha256,
            )
            .is_err()
        );
        assert!(
            validate_manager_identity(
                &binding,
                1,
                "/usr/lib/systemd/systemd",
                1,
                2,
                &"b".repeat(64),
            )
            .is_err()
        );
        let mut changed = binding;
        changed.control_group = "/system.slice/other.service".to_string();
        assert!(changed.validate().is_err());
    }

    #[test]
    fn start_requires_only_the_launcher_and_stopped_child_tasks_in_the_cgroup() {
        let binding = binding();
        let snapshot = parse_unit_snapshot(&properties()).expect("unit snapshot");
        assert!(validate_start_observation(&snapshot, &binding, 42, 77, 1, 1, 2).is_ok());
        assert!(validate_start_observation(&snapshot, &binding, 43, 77, 1, 1, 2).is_err());
        assert!(validate_start_observation(&snapshot, &binding, 42, 42, 1, 1, 2).is_err());
        assert!(validate_start_observation(&snapshot, &binding, 42, 77, 1, 1, 3).is_err());
    }

    #[test]
    fn running_manager_binding_requires_the_same_live_main_invocation() {
        let binding = binding();
        let snapshot = parse_unit_snapshot(&properties()).expect("running unit snapshot");
        assert!(validate_running_observation(&snapshot, &binding, 42).is_ok());
        assert!(validate_running_observation(&snapshot, &binding, 43).is_err());

        let replacement = parse_unit_snapshot(&properties().replace(
            "InvocationID=00000000000040008000000000000001",
            "InvocationID=00000000000040008000000000000002",
        ))
        .expect("well-formed replacement invocation");
        assert!(validate_running_observation(&replacement, &binding, 42).is_err());

        let stop_post =
            parse_unit_snapshot(&properties().replace("SubState=running", "SubState=stop-post"))
                .expect("well-formed stop-post snapshot");
        assert!(validate_running_observation(&stop_post, &binding, 42).is_err());
    }

    #[test]
    fn end_requires_main_pid_zero_control_pid_self_and_hierarchical_one() {
        let binding = binding();
        let stop = properties()
            .replace("MainPID=42", "MainPID=0")
            .replace("ControlPID=0", "ControlPID=77")
            .replace("SubState=running", "SubState=stop-post");
        let snapshot = parse_unit_snapshot(&stop).expect("stop observer snapshot");
        assert!(validate_stop_observation(&snapshot, &binding, 77, 1).is_ok());
        assert!(validate_stop_observation(&snapshot, &binding, 78, 1).is_err());
        assert!(validate_stop_observation(&snapshot, &binding, 77, 2).is_err());

        let live_main = parse_unit_snapshot(&stop.replace("MainPID=0", "MainPID=42"))
            .expect("live-main snapshot");
        assert!(validate_stop_observation(&live_main, &binding, 77, 1).is_err());
        let wrong_state =
            parse_unit_snapshot(&stop.replace("SubState=stop-post", "SubState=running"))
                .expect("wrong-state snapshot");
        assert!(validate_stop_observation(&wrong_state, &binding, 77, 1).is_err());
    }
}
