#![cfg(target_os = "windows")]

use super::WindowsSandboxSessionRequest;
use super::spawn_windows_sandbox_session_elevated_for_permission_profile;
use super::spawn_windows_sandbox_session_for_level;
use super::spawn_windows_sandbox_session_legacy;
use crate::WindowsSandboxCancellationToken;
use crate::ipc_framed::Message;
use crate::ipc_framed::decode_bytes;
use crate::ipc_framed::read_frame;
use crate::run_windows_sandbox_capture;
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use codex_protocol::config_types::WindowsSandboxLevel;
use codex_protocol::models::PermissionProfile;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_pty::ProcessDriver;
use codex_utils_pty::ProcessSignal;
use codex_windows_sandbox_test_support::WindowsSandboxAccountTestGuard;
use pretty_assertions::assert_eq;
use std::collections::HashMap;
use std::collections::HashSet;
use std::fs;
use std::fs::OpenOptions;
use std::io::Seek;
use std::io::SeekFrom;
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::AsRawHandle;
use std::os::windows::io::FromRawHandle;
use std::os::windows::io::OwnedHandle;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;
use tempfile::TempDir;
use tokio::runtime::Builder;
use tokio::sync::broadcast;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio::time::timeout;
use windows_sys::Win32::Foundation::ERROR_NO_MORE_FILES;
use windows_sys::Win32::Foundation::ERROR_SUCCESS;
use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
use windows_sys::Win32::Foundation::LocalFree;
use windows_sys::Win32::Foundation::WAIT_FAILED;
use windows_sys::Win32::Foundation::WAIT_OBJECT_0;
use windows_sys::Win32::Foundation::WAIT_TIMEOUT;
use windows_sys::Win32::Security::ACCESS_ALLOWED_ACE;
use windows_sys::Win32::Security::ACCESS_DENIED_ACE;
use windows_sys::Win32::Security::ACE_HEADER;
use windows_sys::Win32::Security::ACL_SIZE_INFORMATION;
use windows_sys::Win32::Security::AccessCheck;
use windows_sys::Win32::Security::AclSizeInformation;
use windows_sys::Win32::Security::Authorization::GetSecurityInfo;
use windows_sys::Win32::Security::Authorization::SE_FILE_OBJECT;
use windows_sys::Win32::Security::CreateWellKnownSid;
use windows_sys::Win32::Security::DACL_SECURITY_INFORMATION;
use windows_sys::Win32::Security::DuplicateToken;
use windows_sys::Win32::Security::EqualSid;
use windows_sys::Win32::Security::GENERIC_MAPPING;
use windows_sys::Win32::Security::GROUP_SECURITY_INFORMATION;
use windows_sys::Win32::Security::GetAce;
use windows_sys::Win32::Security::GetAclInformation;
use windows_sys::Win32::Security::GetLengthSid;
use windows_sys::Win32::Security::GetTokenInformation;
use windows_sys::Win32::Security::INHERIT_ONLY_ACE;
use windows_sys::Win32::Security::INHERITED_ACE;
use windows_sys::Win32::Security::MapGenericMask;
use windows_sys::Win32::Security::OWNER_SECURITY_INFORMATION;
use windows_sys::Win32::Security::SID_AND_ATTRIBUTES;
use windows_sys::Win32::Security::SecurityImpersonation;
use windows_sys::Win32::Security::TOKEN_DUPLICATE;
use windows_sys::Win32::Security::TOKEN_QUERY;
use windows_sys::Win32::Security::TokenGroups;
use windows_sys::Win32::Security::TokenRestrictedSids;
use windows_sys::Win32::Security::TokenUser;
use windows_sys::Win32::Security::WinWorldSid;
use windows_sys::Win32::Storage::FileSystem::DELETE;
use windows_sys::Win32::Storage::FileSystem::FILE_ALL_ACCESS;
use windows_sys::Win32::Storage::FileSystem::FILE_DELETE_CHILD;
use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_BACKUP_SEMANTICS;
use windows_sys::Win32::Storage::FileSystem::FILE_GENERIC_EXECUTE;
use windows_sys::Win32::Storage::FileSystem::FILE_GENERIC_READ;
use windows_sys::Win32::Storage::FileSystem::FILE_GENERIC_WRITE;
use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_DELETE;
use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ;
use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_WRITE;
use windows_sys::Win32::Storage::FileSystem::READ_CONTROL;
use windows_sys::Win32::System::Diagnostics::ToolHelp::CreateToolhelp32Snapshot;
use windows_sys::Win32::System::Diagnostics::ToolHelp::PROCESSENTRY32W;
use windows_sys::Win32::System::Diagnostics::ToolHelp::Process32FirstW;
use windows_sys::Win32::System::Diagnostics::ToolHelp::Process32NextW;
use windows_sys::Win32::System::Diagnostics::ToolHelp::TH32CS_SNAPPROCESS;
use windows_sys::Win32::System::SystemServices::ACCESS_ALLOWED_ACE_TYPE;
use windows_sys::Win32::System::SystemServices::ACCESS_DENIED_ACE_TYPE;
use windows_sys::Win32::System::SystemServices::SE_GROUP_ENABLED;
use windows_sys::Win32::System::SystemServices::SE_GROUP_LOGON_ID;
use windows_sys::Win32::System::SystemServices::SE_GROUP_USE_FOR_DENY_ONLY;
use windows_sys::Win32::System::Threading::GetCurrentProcess;
use windows_sys::Win32::System::Threading::GetCurrentProcessId;
use windows_sys::Win32::System::Threading::OpenProcess;
use windows_sys::Win32::System::Threading::OpenProcessToken;
use windows_sys::Win32::System::Threading::PROCESS_QUERY_LIMITED_INFORMATION;
use windows_sys::Win32::System::Threading::PROCESS_SYNCHRONIZE;
use windows_sys::Win32::System::Threading::WaitForSingleObject;

static TEST_HOME_COUNTER: AtomicU64 = AtomicU64::new(0);
static LEGACY_PROCESS_TEST_LOCK: Mutex<()> = Mutex::new(());

const ASSERT_NO_CONSOLE: &str = r#"Add-Type -ErrorAction Stop 'using System; using System.Runtime.InteropServices; public class ConsoleProbe { [DllImport("kernel32.dll")] public static extern IntPtr GetConsoleWindow(); }'; if ([ConsoleProbe]::GetConsoleWindow() -ne [IntPtr]::Zero) { throw 'piped sandbox process unexpectedly has a console' };"#;

fn legacy_process_test_guard() -> MutexGuard<'static, ()> {
    LEGACY_PROCESS_TEST_LOCK
        .lock()
        .expect("legacy Windows sandbox process test lock poisoned")
}

fn current_thread_runtime() -> tokio::runtime::Runtime {
    Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build tokio runtime")
}

fn pwsh_path() -> Option<PathBuf> {
    let program_files = std::env::var_os("ProgramFiles")?;
    let path = PathBuf::from(program_files).join("PowerShell\\7\\pwsh.exe");
    path.is_file().then_some(path)
}

fn sandbox_cwd() -> PathBuf {
    if let Ok(workspace_root) = std::env::var("INSTA_WORKSPACE_ROOT") {
        return PathBuf::from(workspace_root);
    }

    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("repo root")
        .to_path_buf()
}

fn sandbox_home(name: &str) -> TempDir {
    let id = TEST_HOME_COUNTER.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!("codex-windows-sandbox-{name}-{id}"));
    let _ = fs::remove_dir_all(&path);
    fs::create_dir_all(&path).expect("create sandbox home");
    tempfile::TempDir::new_in(&path).expect("create sandbox home tempdir")
}

fn sandbox_log(codex_home: &Path) -> String {
    let log_path = crate::current_log_file_path(&codex_home.join(".sandbox"));
    fs::read_to_string(&log_path)
        .unwrap_or_else(|err| format!("failed to read {}: {err}", log_path.display()))
}

fn workspace_roots_for(root: &Path) -> Vec<AbsolutePathBuf> {
    vec![AbsolutePathBuf::from_absolute_path(root).expect("absolute workspace root")]
}

fn powershell_literal(path: &Path) -> String {
    path.to_string_lossy().replace('\'', "''")
}

fn start_powershell_child(
    pwsh: &Path,
    stdio_dir: &Path,
    child_command: &str,
    parent_tail: &str,
) -> String {
    let encoded = BASE64.encode(
        child_command
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect::<Vec<_>>(),
    );
    format!(
        "Start-Process -WindowStyle Hidden -FilePath '{}' -ArgumentList '-NoProfile','-EncodedCommand','{encoded}' -RedirectStandardOutput '{}' -RedirectStandardError '{}'; {parent_tail}",
        powershell_literal(pwsh),
        powershell_literal(&stdio_dir.join("descendant.stdout")),
        powershell_literal(&stdio_dir.join("descendant.stderr")),
    )
}

fn wait_for_path(path: &Path, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if path.exists() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    path.exists()
}

fn open_process_for_wait(pid: u32) -> std::io::Result<OwnedHandle> {
    let handle = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
    if handle.is_null() {
        return Err(std::io::Error::last_os_error());
    }
    Ok(unsafe { OwnedHandle::from_raw_handle(handle as _) })
}

fn wait_for_process_exit(process: &OwnedHandle, timeout: Duration) -> std::io::Result<()> {
    let timeout_ms = u32::try_from(timeout.as_millis())
        .map_err(|_| std::io::Error::other("process wait timeout exceeds u32"))?;
    let result = unsafe { WaitForSingleObject(process.as_raw_handle() as _, timeout_ms) };
    match result {
        WAIT_OBJECT_0 => Ok(()),
        WAIT_TIMEOUT => Err(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "timed out waiting for process to exit",
        )),
        WAIT_FAILED => Err(std::io::Error::last_os_error()),
        result => Err(std::io::Error::other(format!(
            "unexpected process wait result: {result}"
        ))),
    }
}

struct DeleteAccessObservation {
    // Retain the exact child process handle through the fixture's completion.
    _process: Option<OwnedHandle>,
    allowed: Option<[bool; 4]>,
    parent_aces: Option<String>,
}

struct ObservedSid {
    sid: Vec<u8>,
    attributes: u32,
}

struct ChildSidClasses {
    user: Vec<u8>,
    normal_groups: Vec<ObservedSid>,
    restricted_sids: Vec<ObservedSid>,
    logon: Option<Vec<u8>>,
    everyone: Vec<u8>,
}

fn token_information(token: HANDLE, class: i32) -> Option<Vec<u8>> {
    const MAX_TOKEN_INFORMATION_BYTES: u32 = 64 * 1024;
    unsafe {
        let mut needed = 0;
        let _ = GetTokenInformation(token, class, std::ptr::null_mut(), 0, &mut needed);
        if needed == 0 || needed > MAX_TOKEN_INFORMATION_BYTES {
            return None;
        }
        let mut buffer = vec![0u8; needed as usize];
        if GetTokenInformation(
            token,
            class,
            buffer.as_mut_ptr().cast(),
            needed,
            &mut needed,
        ) == 0
        {
            return None;
        }
        if needed as usize > buffer.len() {
            return None;
        }
        buffer.truncate(needed as usize);
        Some(buffer)
    }
}

fn copy_sid(sid: *mut std::ffi::c_void) -> Option<Vec<u8>> {
    if sid.is_null() {
        return None;
    }
    let length = unsafe { GetLengthSid(sid) } as usize;
    if length == 0 || length > 1024 {
        return None;
    }
    Some(unsafe { std::slice::from_raw_parts(sid.cast::<u8>(), length) }.to_vec())
}

fn token_sid_entries(token: HANDLE, class: i32) -> Option<Vec<ObservedSid>> {
    let buffer = token_information(token, class)?;
    let entries_offset = std::mem::offset_of!(windows_sys::Win32::Security::TOKEN_GROUPS, Groups);
    if buffer.len() < entries_offset {
        return None;
    }
    let count = unsafe { std::ptr::read_unaligned(buffer.as_ptr().cast::<u32>()) } as usize;
    if count > 256 {
        return None;
    }
    let entries_size = count.checked_mul(std::mem::size_of::<SID_AND_ATTRIBUTES>())?;
    if entries_offset.checked_add(entries_size)? > buffer.len() {
        return None;
    }
    let mut entries = Vec::with_capacity(count);
    for index in 0..count {
        let offset = entries_offset + index * std::mem::size_of::<SID_AND_ATTRIBUTES>();
        let entry = unsafe {
            std::ptr::read_unaligned(buffer.as_ptr().add(offset).cast::<SID_AND_ATTRIBUTES>())
        };
        entries.push(ObservedSid {
            sid: copy_sid(entry.Sid)?,
            attributes: entry.Attributes,
        });
    }
    Some(entries)
}

fn token_user_sid(token: HANDLE) -> Option<Vec<u8>> {
    let buffer = token_information(token, TokenUser)?;
    if buffer.len() < std::mem::size_of::<SID_AND_ATTRIBUTES>() {
        return None;
    }
    let user = unsafe { std::ptr::read_unaligned(buffer.as_ptr().cast::<SID_AND_ATTRIBUTES>()) };
    copy_sid(user.Sid)
}

fn current_process_base_token_has_no_restricted_sids() -> Option<bool> {
    unsafe {
        let mut token = std::ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return None;
        }
        let token = OwnedHandle::from_raw_handle(token as _);
        Some(token_sid_entries(token.as_raw_handle() as HANDLE, TokenRestrictedSids)?.is_empty())
    }
}

fn child_sid_classes(token: HANDLE) -> Option<ChildSidClasses> {
    let user = token_user_sid(token)?;
    let normal_groups = token_sid_entries(token, TokenGroups)?;
    let restricted_sids = token_sid_entries(token, TokenRestrictedSids)?;
    let logon = normal_groups
        .iter()
        .find(|group| {
            (group.attributes as u32 & SE_GROUP_LOGON_ID as u32) == SE_GROUP_LOGON_ID as u32
        })
        .map(|group| group.sid.clone())?;
    let mut everyone = vec![0u8; 68];
    let mut everyone_length = everyone.len() as u32;
    if unsafe {
        CreateWellKnownSid(
            WinWorldSid,
            std::ptr::null_mut(),
            everyone.as_mut_ptr().cast(),
            &mut everyone_length,
        )
    } == 0
    {
        return None;
    }
    everyone.truncate(everyone_length as usize);
    if !restricted_sids
        .iter()
        .any(|restricted| sids_equal(&restricted.sid, &logon))
        || !restricted_sids
            .iter()
            .any(|restricted| sids_equal(&restricted.sid, &everyone))
    {
        return None;
    }
    Some(ChildSidClasses {
        user,
        normal_groups,
        restricted_sids,
        logon,
        everyone,
    })
}

fn sids_equal(left: &[u8], right: &[u8]) -> bool {
    !left.is_empty()
        && !right.is_empty()
        && unsafe { EqualSid(left.as_ptr() as _, right.as_ptr() as _) != 0 }
}

fn parent_acl_ace_summary(
    dacl: *mut windows_sys::Win32::Security::ACL,
    owner: *mut std::ffi::c_void,
    classes: &ChildSidClasses,
    base_token_unrestricted: Option<bool>,
    access_allowed: Option<bool>,
) -> String {
    let Some(true) = base_token_unrestricted else {
        return "inconclusive".to_string();
    };
    let Some(access_allowed) = access_allowed else {
        return "inconclusive".to_string();
    };
    unsafe {
        if dacl.is_null() {
            return format!("baseline={access_allowed};null_dacl=true;inconclusive");
        }
        let mut information: ACL_SIZE_INFORMATION = std::mem::zeroed();
        if GetAclInformation(
            dacl,
            (&mut information as *mut ACL_SIZE_INFORMATION).cast(),
            std::mem::size_of::<ACL_SIZE_INFORMATION>() as u32,
            AclSizeInformation,
        ) == 0
            || information.AceCount > 256
        {
            return "inconclusive".to_string();
        }
        let mut entries = Vec::new();
        let mut relevant_count = 0usize;
        let mapping = GENERIC_MAPPING {
            GenericRead: FILE_GENERIC_READ,
            GenericWrite: FILE_GENERIC_WRITE,
            GenericExecute: FILE_GENERIC_EXECUTE,
            GenericAll: FILE_ALL_ACCESS,
        };
        for index in 0..information.AceCount {
            let mut raw_ace = std::ptr::null_mut();
            if GetAce(dacl, index as u32, &mut raw_ace) == 0 || raw_ace.is_null() {
                return "inconclusive".to_string();
            }
            let header = std::ptr::read_unaligned(raw_ace.cast::<ACE_HEADER>());
            if header.AceSize < 8 {
                return "inconclusive".to_string();
            }
            if header.AceFlags & INHERIT_ONLY_ACE as u8 != 0 {
                continue;
            }
            let raw_mask = std::ptr::read_unaligned(raw_ace.cast::<u8>().add(4).cast::<u32>());
            let mut mapped_mask = raw_mask;
            MapGenericMask(&mut mapped_mask, &mapping);
            if mapped_mask & FILE_DELETE_CHILD == 0 {
                continue;
            }
            if header.AceType != ACCESS_ALLOWED_ACE_TYPE as u8
                && header.AceType != ACCESS_DENIED_ACE_TYPE as u8
            {
                return "inconclusive".to_string();
            }
            let sid_offset = std::mem::offset_of!(ACCESS_ALLOWED_ACE, SidStart);
            if (header.AceSize as usize) < sid_offset + 8 {
                return "inconclusive".to_string();
            }
            let sid_start = if header.AceType == ACCESS_ALLOWED_ACE_TYPE as u8 {
                std::ptr::addr_of!((*raw_ace.cast::<ACCESS_ALLOWED_ACE>()).SidStart)
            } else {
                std::ptr::addr_of!((*raw_ace.cast::<ACCESS_DENIED_ACE>()).SidStart)
            };
            let sid_pointer = sid_start.cast_mut().cast();
            let sid_length = GetLengthSid(sid_pointer) as usize;
            if sid_length < 8 || sid_length > header.AceSize as usize - sid_offset {
                return "inconclusive".to_string();
            }
            let Some(ace_sid) = copy_sid(sid_pointer) else {
                return "inconclusive".to_string();
            };
            let user_match = sids_equal(&ace_sid, &classes.user);
            let enabled_group_match = classes.normal_groups.iter().any(|group| {
                group.attributes & SE_GROUP_ENABLED as u32 != 0
                    && group.attributes & SE_GROUP_USE_FOR_DENY_ONLY as u32 == 0
                    && sids_equal(&ace_sid, &group.sid)
            });
            let deny_only_group_match = classes.normal_groups.iter().any(|group| {
                group.attributes & SE_GROUP_USE_FOR_DENY_ONLY as u32 != 0
                    && sids_equal(&ace_sid, &group.sid)
            });
            let logon_match = classes
                .logon
                .as_ref()
                .is_some_and(|sid| sids_equal(&ace_sid, sid));
            let everyone_match = sids_equal(&ace_sid, &classes.everyone);
            let capability_match = classes.restricted_sids.iter().any(|restricted| {
                !logon_match && !everyone_match && sids_equal(&ace_sid, &restricted.sid)
            });
            let restricted_match = classes
                .restricted_sids
                .iter()
                .any(|restricted| sids_equal(&ace_sid, &restricted.sid));
            let owner_match = !owner.is_null() && EqualSid(owner, ace_sid.as_ptr() as _) != 0;
            let allow = header.AceType == ACCESS_ALLOWED_ACE_TYPE as u8;
            let normal_match =
                user_match || enabled_group_match || (!allow && deny_only_group_match);
            if !normal_match && !restricted_match && !owner_match {
                continue;
            }
            relevant_count += 1;
            if relevant_count > 32 {
                return "inconclusive".to_string();
            }
            entries.push(format!(
                "{}:{}:{:#010x}:{:#010x}:{:#04x}:inherited={}:user={}:enabled_group={}:deny_only_group={}:restricted={}:capability={}:logon={}:everyone={}:owner={}",
                index,
                if allow { "allow" } else { "deny" },
                raw_mask,
                mapped_mask,
                header.AceFlags,
                header.AceFlags & INHERITED_ACE as u8 != 0,
                user_match,
                enabled_group_match,
                deny_only_group_match,
                restricted_match,
                capability_match,
                logon_match,
                everyone_match,
                owner_match,
            ));
        }
        format!("baseline={access_allowed};aces=[{}]", entries.join(","))
    }
}

fn direct_child_cmd_pids() -> Option<HashSet<u32>> {
    // Toolhelp exposes only PID, parent PID, and image name. The caller uses a
    // before/after delta around this fixture's sole spawn; it does not attest argv.
    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snapshot.is_null() || snapshot == INVALID_HANDLE_VALUE {
            return None;
        }
        let snapshot = OwnedHandle::from_raw_handle(snapshot as _);
        let parent_pid = GetCurrentProcessId();
        let mut entry: PROCESSENTRY32W = std::mem::zeroed();
        entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        if Process32FirstW(snapshot.as_raw_handle() as HANDLE, &mut entry) == 0 {
            return None;
        }

        let mut children = HashSet::new();
        loop {
            if entry.th32ParentProcessID == parent_pid {
                let name_end = entry
                    .szExeFile
                    .iter()
                    .position(|character| *character == 0)
                    .unwrap_or(entry.szExeFile.len());
                let name = String::from_utf16_lossy(&entry.szExeFile[..name_end]);
                if name.eq_ignore_ascii_case("cmd.exe") {
                    children.insert(entry.th32ProcessID);
                }
            }
            if Process32NextW(snapshot.as_raw_handle() as HANDLE, &mut entry) == 0 {
                return (windows_sys::Win32::Foundation::GetLastError() == ERROR_NO_MORE_FILES)
                    .then_some(children);
            }
        }
    }
}

fn access_check_for_path(
    token: HANDLE,
    path: &Path,
    desired_access: u32,
    sid_diagnostic: Option<(&ChildSidClasses, Option<bool>)>,
) -> Option<(bool, Option<String>)> {
    unsafe {
        let handle = OpenOptions::new()
            .access_mode(READ_CONTROL)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
            .open(path)
            .ok()?;
        let mut owner = std::ptr::null_mut();
        let mut group = std::ptr::null_mut();
        let mut dacl = std::ptr::null_mut();
        let mut descriptor = std::ptr::null_mut();
        let status = GetSecurityInfo(
            handle.as_raw_handle() as HANDLE,
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | GROUP_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            &mut owner,
            &mut group,
            &mut dacl,
            std::ptr::null_mut(),
            &mut descriptor,
        );
        if status != ERROR_SUCCESS {
            if !descriptor.is_null() {
                LocalFree(descriptor as _);
            }
            return None;
        }
        let mapping = GENERIC_MAPPING {
            GenericRead: FILE_GENERIC_READ,
            GenericWrite: FILE_GENERIC_WRITE,
            GenericExecute: FILE_GENERIC_EXECUTE,
            GenericAll: FILE_ALL_ACCESS,
        };
        let mut privilege_set = [0u64; 128];
        let mut privilege_set_size = std::mem::size_of_val(&privilege_set) as u32;
        let mut granted_access = 0;
        let mut access_status = 0;
        let checked = AccessCheck(
            descriptor,
            token,
            desired_access,
            &mapping,
            privilege_set.as_mut_ptr().cast(),
            &mut privilege_set_size,
            &mut granted_access,
            &mut access_status,
        );
        let allowed = (checked != 0).then_some(access_status != 0);
        let parent_aces = allowed.and_then(|allowed| {
            sid_diagnostic.map(|(classes, base_token_unrestricted)| {
                parent_acl_ace_summary(dacl, owner, classes, base_token_unrestricted, Some(allowed))
            })
        });
        LocalFree(descriptor as _);
        allowed.map(|allowed| (allowed, parent_aces))
    }
}

fn observe_delete_access(
    before: Option<HashSet<u32>>,
    outside_file: &Path,
    outside_parent: &Path,
    protected_git_dir: &Path,
    workspace_parent: &Path,
    base_token_unrestricted: Option<bool>,
) -> DeleteAccessObservation {
    let Some(before) = before else {
        return DeleteAccessObservation {
            _process: None,
            allowed: None,
            parent_aces: None,
        };
    };
    let Some(after) = direct_child_cmd_pids() else {
        return DeleteAccessObservation {
            _process: None,
            allowed: None,
            parent_aces: None,
        };
    };
    let children: Vec<_> = after.difference(&before).copied().collect();
    if children.len() != 1 {
        return DeleteAccessObservation {
            _process: None,
            allowed: None,
            parent_aces: None,
        };
    }

    unsafe {
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, children[0]);
        if process.is_null() {
            return DeleteAccessObservation {
                _process: None,
                allowed: None,
                parent_aces: None,
            };
        }
        let process = OwnedHandle::from_raw_handle(process as _);
        let mut token = std::ptr::null_mut();
        if OpenProcessToken(
            process.as_raw_handle() as HANDLE,
            TOKEN_QUERY | TOKEN_DUPLICATE,
            &mut token,
        ) == 0
        {
            return DeleteAccessObservation {
                _process: Some(process),
                allowed: None,
                parent_aces: None,
            };
        }
        let token = OwnedHandle::from_raw_handle(token as _);
        let mut impersonation_token = std::ptr::null_mut();
        if DuplicateToken(
            token.as_raw_handle() as HANDLE,
            SecurityImpersonation,
            &mut impersonation_token,
        ) == 0
        {
            return DeleteAccessObservation {
                _process: Some(process),
                allowed: None,
                parent_aces: None,
            };
        }
        let impersonation_token = OwnedHandle::from_raw_handle(impersonation_token as _);
        let classes = child_sid_classes(impersonation_token.as_raw_handle() as HANDLE);
        let outside_file_check = access_check_for_path(
            impersonation_token.as_raw_handle() as HANDLE,
            outside_file,
            DELETE,
            None,
        );
        let outside_parent_check = access_check_for_path(
            impersonation_token.as_raw_handle() as HANDLE,
            outside_parent,
            FILE_DELETE_CHILD,
            classes
                .as_ref()
                .map(|classes| (classes, base_token_unrestricted)),
        );
        let git_check = access_check_for_path(
            impersonation_token.as_raw_handle() as HANDLE,
            protected_git_dir,
            DELETE,
            None,
        );
        let workspace_parent_check = access_check_for_path(
            impersonation_token.as_raw_handle() as HANDLE,
            workspace_parent,
            FILE_DELETE_CHILD,
            None,
        );
        let checks = [
            outside_file_check.map(|check| check.0),
            outside_parent_check.as_ref().map(|check| check.0),
            git_check.map(|check| check.0),
            workspace_parent_check.map(|check| check.0),
        ];
        if checks.iter().any(Option::is_none) {
            return DeleteAccessObservation {
                _process: Some(process),
                allowed: None,
                parent_aces: None,
            };
        }
        let parent_aces = outside_parent_check.and_then(|check| check.1);
        DeleteAccessObservation {
            _process: Some(process),
            allowed: Some([
                checks[0].unwrap_or(false),
                checks[1].unwrap_or(false),
                checks[2].unwrap_or(false),
                checks[3].unwrap_or(false),
            ]),
            parent_aces,
        }
    }
}

fn wait_for_frame_count(frames_path: &Path, expected_frames: usize) -> Vec<Message> {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let mut reader = OpenOptions::new()
            .read(true)
            .open(frames_path)
            .expect("open frame file for read");
        reader
            .seek(SeekFrom::Start(0))
            .expect("seek to start of frame file");

        let mut frames = Vec::new();
        loop {
            match read_frame(&mut reader) {
                Ok(Some(frame)) => frames.push(frame.message),
                Ok(None) => break,
                Err(_) => break,
            }
        }

        if frames.len() >= expected_frames {
            return frames;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {expected_frames} frames, saw {}",
            frames.len()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

async fn collect_stdout_and_exit(
    spawned: codex_utils_pty::SpawnedProcess,
    codex_home: &Path,
    timeout_duration: Duration,
) -> (Vec<u8>, i32) {
    let codex_utils_pty::SpawnedProcess {
        session: _session,
        mut stdout_rx,
        stderr_rx: _stderr_rx,
        exit_rx,
    } = spawned;
    let stdout_task = tokio::spawn(async move {
        let mut stdout = Vec::new();
        while let Some(chunk) = stdout_rx.recv().await {
            stdout.extend(chunk);
        }
        stdout
    });
    let exit_code = timeout(timeout_duration, exit_rx)
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for exit\n{}", sandbox_log(codex_home)))
        .unwrap_or(-1);
    let stdout = timeout(timeout_duration, stdout_task)
        .await
        .unwrap_or_else(|_| {
            panic!(
                "timed out waiting for stdout task\n{}",
                sandbox_log(codex_home)
            )
        })
        .expect("stdout task join");
    (stdout, exit_code)
}

#[test]
fn restricted_token_rejects_managed_network_before_spawn() {
    current_thread_runtime().block_on(async {
        let cwd = sandbox_cwd();
        let codex_home = sandbox_home("restricted-token-managed-network");
        let permission_profile = PermissionProfile::workspace_write();
        let error = spawn_windows_sandbox_session_for_level(WindowsSandboxSessionRequest {
            permission_profile: &permission_profile,
            workspace_roots: &[],
            codex_home: codex_home.path(),
            command: Vec::new(),
            cwd: cwd.as_path(),
            env_map: HashMap::new(),
            windows_sandbox_level: WindowsSandboxLevel::RestrictedToken,
            proxy_enforced: true,
            network_proxy_restricting_sid: None,
            proxy_settings_mode: crate::WindowsSandboxProxySettingsMode::Preserve,
            timeout_ms: None,
            read_roots_override: None,
            read_roots_include_platform_defaults: false,
            write_roots_override: None,
            deny_read_paths_override: &[],
            deny_write_paths_override: &[],
            tty: false,
            stdin_open: false,
        })
        .await
        .expect_err("managed networking must fail before spawning an unelevated sandbox");

        assert_eq!(
            error.to_string(),
            "managed networking requires the elevated Windows sandbox backend"
        );
    });
}

#[test]
fn legacy_non_tty_cmd_emits_output() {
    let _guard = legacy_process_test_guard();
    let runtime = current_thread_runtime();
    runtime.block_on(async move {
        let cwd = sandbox_cwd();
        let codex_home = sandbox_home("legacy-non-tty-cmd");
        println!("cmd codex_home={}", codex_home.path().display());
        let permission_profile = PermissionProfile::workspace_write();
        let spawned = spawn_windows_sandbox_session_legacy(
            &permission_profile,
            workspace_roots_for(cwd.as_path()).as_slice(),
            codex_home.path(),
            vec![
                "C:\\Windows\\System32\\cmd.exe".to_string(),
                "/c".to_string(),
                "echo LEGACY-NONTTY-CMD".to_string(),
            ],
            cwd.as_path(),
            HashMap::new(),
            Some(5_000),
            &[],
            &[],
            /*tty*/ false,
            /*stdin_open*/ false,
        )
        .await
        .expect("spawn legacy non-tty cmd session");
        println!("cmd spawn returned");
        let (stdout, exit_code) =
            collect_stdout_and_exit(spawned, codex_home.path(), Duration::from_secs(10)).await;
        println!("cmd collect returned exit_code={exit_code}");
        let stdout = String::from_utf8_lossy(&stdout);
        assert_eq!(exit_code, 0, "stdout={stdout:?}");
        assert!(stdout.contains("LEGACY-NONTTY-CMD"), "stdout={stdout:?}");
    });
}

#[test]
fn elevated_non_tty_cmd_forwards_env_output_and_exit() {
    let _account_guard =
        WindowsSandboxAccountTestGuard::acquire().expect("lock Windows sandbox test accounts");
    let _guard = legacy_process_test_guard();
    let runtime = current_thread_runtime();
    runtime.block_on(async move {
        let cwd = sandbox_cwd();
        let codex_home = sandbox_home("elevated-non-tty-cmd");
        let permission_profile = PermissionProfile::workspace_write();
        let env_map = HashMap::from([(
            "CODEX_ELEVATED_TEST".to_string(),
            "ELEVATED-ENV-OK".to_string(),
        )]);
        let spawned = spawn_windows_sandbox_session_elevated_for_permission_profile(
            &permission_profile,
            workspace_roots_for(cwd.as_path()).as_slice(),
            codex_home.path(),
            vec![
                "C:\\Windows\\System32\\cmd.exe".to_string(),
                "/d".to_string(),
                "/c".to_string(),
                "echo %CODEX_ELEVATED_TEST% & exit /b 23".to_string(),
            ],
            cwd.as_path(),
            env_map,
            /*proxy_enforced*/ false,
            /*network_proxy_restricting_sid*/ None,
            Some(5_000),
            /*read_roots_override*/ None,
            /*read_roots_include_platform_defaults*/ true,
            /*write_roots_override*/ None,
            &[],
            &[],
            /*tty*/ false,
            /*stdin_open*/ false,
        )
        .await
        .unwrap_or_else(|err| {
            panic!(
                "spawn elevated non-tty cmd session: {err:#}\nsandbox log:\n{}",
                sandbox_log(codex_home.path())
            )
        });
        let (stdout, exit_code) =
            collect_stdout_and_exit(spawned, codex_home.path(), Duration::from_secs(10)).await;
        let stdout = String::from_utf8_lossy(&stdout);
        assert_eq!(exit_code, 23, "stdout={stdout:?}");
        assert!(stdout.contains("ELEVATED-ENV-OK"), "stdout={stdout:?}");
    });
}

#[test]
#[ignore = "requires this test binary in an installed test MSIX, launched with package identity, CODEX_WINDOWS_REGISTERED_CORE=1, and CODEX_HOME provisioned by that package's service in a disposable Windows VM"]
fn registered_non_tty_cmd_forwards_env_output_and_exit() {
    assert!(
        crate::registered_core_requested(),
        "registered Core opt-in is required"
    );
    let codex_home = PathBuf::from(std::env::var_os("CODEX_HOME").expect("fixture CODEX_HOME"));
    assert!(
        crate::app_package::registered_setup_is_ready(&codex_home)
            .expect("validate the installed package and service receipt"),
        "the test process must have package identity and completed service setup",
    );
    let cwd = tempfile::tempdir().expect("isolated command workspace");
    current_thread_runtime().block_on(async {
        let mut env_map: HashMap<String, String> = std::env::vars().collect();
        env_map.insert("CODEX_REGISTERED_TEST".into(), "REGISTERED-ENV-OK".into());
        let spawned = spawn_windows_sandbox_session_elevated_for_permission_profile(
            &PermissionProfile::workspace_write(),
            workspace_roots_for(cwd.path()).as_slice(),
            &codex_home,
            vec![
                "C:\\Windows\\System32\\cmd.exe".into(),
                "/d".into(),
                "/c".into(),
                "echo %CODEX_REGISTERED_TEST%& exit /b 23".into(),
            ],
            cwd.path(),
            env_map,
            /*proxy_enforced*/ false,
            /*network_proxy_restricting_sid*/ None,
            Some(5_000),
            /*read_roots_override*/ None,
            /*read_roots_include_platform_defaults*/ true,
            /*write_roots_override*/ None,
            &[],
            &[],
            /*tty*/ false,
            /*stdin_open*/ false,
        )
        .await
        .expect("launch through the service-recorded alias and authenticated pipes");
        let (stdout, exit_code) =
            collect_stdout_and_exit(spawned, &codex_home, Duration::from_secs(10)).await;
        assert_eq!(
            (
                String::from_utf8(stdout).expect("command output"),
                exit_code
            ),
            ("REGISTERED-ENV-OK\r\n".to_owned(), 23),
        );
    });
}

#[test]
fn legacy_non_tty_cmd_rejects_deny_read_overrides() {
    let _guard = legacy_process_test_guard();
    let runtime = current_thread_runtime();
    runtime.block_on(async move {
        let cwd = sandbox_cwd();
        let codex_home = sandbox_home("legacy-non-tty-deny-read");
        let secret_path =
            AbsolutePathBuf::from_absolute_path(cwd.join("legacy-non-tty-deny-read-secret.env"))
                .expect("absolute deny-read fixture path");
        let permission_profile = PermissionProfile::workspace_write();
        let err = spawn_windows_sandbox_session_legacy(
            &permission_profile,
            workspace_roots_for(cwd.as_path()).as_slice(),
            codex_home.path(),
            vec![
                "C:\\Windows\\System32\\cmd.exe".to_string(),
                "/c".to_string(),
                "echo deny-read".to_string(),
            ],
            cwd.as_path(),
            HashMap::new(),
            Some(5_000),
            std::slice::from_ref(&secret_path),
            &[],
            /*tty*/ false,
            /*stdin_open*/ false,
        )
        .await
        .expect_err("legacy deny-read should require the elevated backend");
        assert!(
            err.to_string()
                .contains("deny-read overrides require the elevated Windows sandbox backend"),
            "unexpected error: {err:#}"
        );
    });
}

#[test]
fn legacy_non_tty_powershell_interrupt_terminates_process() {
    let Some(pwsh) = pwsh_path() else {
        return;
    };
    let _guard = legacy_process_test_guard();
    let runtime = current_thread_runtime();
    runtime.block_on(async move {
        let cwd = sandbox_cwd();
        let codex_home = sandbox_home("legacy-non-tty-pwsh");
        println!("pwsh codex_home={}", codex_home.path().display());
        let permission_profile = PermissionProfile::workspace_write();
        let spawned = spawn_windows_sandbox_session_legacy(
            &permission_profile,
            workspace_roots_for(cwd.as_path()).as_slice(),
            codex_home.path(),
            vec![
                pwsh.display().to_string(),
                "-NoProfile".to_string(),
                "-Command".to_string(),
                format!("{ASSERT_NO_CONSOLE} Write-Output LEGACY-NONTTY-DIRECT; [System.Threading.ManualResetEvent]::new($false).WaitOne()"),
            ],
            cwd.as_path(),
            HashMap::new(),
            /*timeout_ms*/ None,
            &[],
            &[],
            /*tty*/ false,
            /*stdin_open*/ false,
            )
        .await
        .expect("spawn legacy non-tty powershell session");
        println!("pwsh spawn returned");
        let codex_utils_pty::SpawnedProcess {
            session,
            mut stdout_rx,
            stderr_rx: _stderr_rx,
            exit_rx,
        } = spawned;
        timeout(Duration::from_secs(5), async {
            let mut stdout = Vec::new();
            while let Some(chunk) = stdout_rx.recv().await {
                stdout.extend(chunk);
                if String::from_utf8_lossy(&stdout).contains("LEGACY-NONTTY-DIRECT") {
                    return;
                }
            }
            panic!(
                "PowerShell exited before emitting its readiness marker\n{}",
                sandbox_log(codex_home.path())
            );
        })
        .await
        .unwrap_or_else(|_| {
            panic!(
                "timed out waiting for PowerShell readiness marker\n{}",
                sandbox_log(codex_home.path())
            )
        });

        session
            .signal(ProcessSignal::Interrupt)
            .expect("interrupt should terminate the restricted-token process job");
        let exit_code = timeout(Duration::from_secs(5), exit_rx)
            .await
            .unwrap_or_else(|_| panic!("timed out waiting for exit\n{}", sandbox_log(codex_home.path())))
            .expect("interrupted process should report an exit code");
        assert_eq!(exit_code, 1);
    });
}

#[test]
fn finish_driver_spawn_keeps_stdin_open_when_requested() {
    let runtime = current_thread_runtime();
    runtime.block_on(async move {
        let (writer_tx, mut writer_rx) = mpsc::channel::<Vec<u8>>(1);
        let (_stdout_tx, stdout_rx) = broadcast::channel::<Vec<u8>>(1);
        let (exit_tx, exit_rx) = oneshot::channel::<i32>();
        drop(exit_tx);

        let spawned = super::finish_driver_spawn(
            ProcessDriver {
                writer_tx,
                stdout_rx,
                stderr_rx: None,
                exit_rx,
                terminator: None,
                writer_handle: None,
                resizer: None,
                tty: false,
            },
            /*stdin_open*/ true,
        );

        spawned
            .session
            .writer_sender()
            .send(b"open".to_vec())
            .await
            .expect("stdin should stay open");
        assert_eq!(writer_rx.recv().await, Some(b"open".to_vec()));
    });
}

#[test]
fn finish_driver_spawn_closes_stdin_when_not_requested() {
    let runtime = current_thread_runtime();
    runtime.block_on(async move {
        let (writer_tx, _writer_rx) = mpsc::channel::<Vec<u8>>(1);
        let (_stdout_tx, stdout_rx) = broadcast::channel::<Vec<u8>>(1);
        let (exit_tx, exit_rx) = oneshot::channel::<i32>();
        drop(exit_tx);

        let spawned = super::finish_driver_spawn(
            ProcessDriver {
                writer_tx,
                stdout_rx,
                stderr_rx: None,
                exit_rx,
                terminator: None,
                writer_handle: None,
                resizer: None,
                tty: false,
            },
            /*stdin_open*/ false,
        );

        assert!(
            spawned
                .session
                .writer_sender()
                .send(b"closed".to_vec())
                .await
                .is_err(),
            "stdin should be closed when streaming input is disabled"
        );
    });
}

#[test]
fn runner_stdin_writer_sends_close_stdin_after_input_eof() {
    let runtime = current_thread_runtime();
    runtime.block_on(async move {
        let tempdir = TempDir::new().expect("create tempdir");
        let frames_path = tempdir.path().join("runner-stdin-frames.bin");
        let file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .read(true)
            .write(true)
            .open(&frames_path)
            .expect("create frame file");
        let outbound_tx = super::start_runner_pipe_writer(file);
        let (writer_tx, writer_rx) = mpsc::channel::<Vec<u8>>(1);
        let writer_handle = super::start_runner_stdin_writer(
            writer_rx,
            outbound_tx,
            /*normalize_newlines*/ false,
            /*stdin_open*/ true,
        );

        writer_tx
            .send(b"hello".to_vec())
            .await
            .expect("send stdin bytes");
        drop(writer_tx);
        writer_handle.await.expect("join stdin writer");

        let frames = wait_for_frame_count(&frames_path, 2);

        match &frames[0] {
            Message::Stdin { payload } => {
                let bytes = decode_bytes(&payload.data_b64).expect("decode stdin payload");
                assert_eq!(bytes, b"hello".to_vec());
            }
            other => panic!("expected stdin frame, got {other:?}"),
        }

        match &frames[1] {
            Message::CloseStdin { .. } => {}
            other => panic!("expected close-stdin frame, got {other:?}"),
        }
    });
}

#[test]
fn runner_resizer_sends_resize_frame() {
    let runtime = current_thread_runtime();
    runtime.block_on(async move {
        let tempdir = TempDir::new().expect("create tempdir");
        let frames_path = tempdir.path().join("runner-resize-frames.bin");
        let file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .read(true)
            .write(true)
            .open(&frames_path)
            .expect("create frame file");
        let outbound_tx = super::start_runner_pipe_writer(file);
        let mut resizer = super::make_runner_resizer(outbound_tx);

        resizer(codex_utils_pty::TerminalSize {
            rows: 45,
            cols: 132,
        })
        .expect("send resize frame");

        let frames = wait_for_frame_count(&frames_path, 1);
        match &frames[0] {
            Message::Resize { payload } => {
                assert_eq!(payload.rows, 45);
                assert_eq!(payload.cols, 132);
            }
            other => panic!("expected resize frame, got {other:?}"),
        }
    });
}

#[test]
fn legacy_capture_emits_output_and_preserves_descendant_after_normal_exit() {
    let Some(pwsh) = pwsh_path() else {
        return;
    };
    let _guard = legacy_process_test_guard();
    let cwd = sandbox_cwd();
    let codex_home = sandbox_home("legacy-capture-pwsh");
    println!("capture pwsh codex_home={}", codex_home.path().display());
    let ready_marker = codex_home.path().join("descendant-started");
    let release_marker = codex_home.path().join("release-descendant");
    let survival_marker = codex_home.path().join("descendant-survived");
    let descendant_command = format!(
        "$deadline=(Get-Date).AddSeconds(30); Set-Content -LiteralPath '{}' -Value $PID; while (-not (Test-Path -LiteralPath '{}')) {{ if ((Get-Date) -ge $deadline) {{ exit 3 }}; Start-Sleep -Milliseconds 25 }}; Set-Content -LiteralPath '{}' -Value survived",
        powershell_literal(&ready_marker),
        powershell_literal(&release_marker),
        powershell_literal(&survival_marker),
    );
    let parent_tail = format!(
        "while (-not (Test-Path -LiteralPath '{}')) {{ Start-Sleep -Milliseconds 25 }}",
        powershell_literal(&ready_marker),
    );
    let parent_command = format!(
        "{ASSERT_NO_CONSOLE} Write-Output LEGACY-CAPTURE-DIRECT; {}",
        start_powershell_child(&pwsh, codex_home.path(), &descendant_command, &parent_tail,),
    );
    let permission_profile = PermissionProfile::workspace_write();
    let result = run_windows_sandbox_capture(
        &permission_profile,
        workspace_roots_for(cwd.as_path()).as_slice(),
        codex_home.path(),
        vec![
            pwsh.display().to_string(),
            "-NoProfile".to_string(),
            "-Command".to_string(),
            parent_command,
        ],
        cwd.as_path(),
        HashMap::new(),
        Some(10_000),
        /*cancellation*/ None,
    )
    .expect("run legacy capture powershell");
    let descendant_pid = fs::read_to_string(&ready_marker)
        .expect("read descendant pid")
        .trim()
        .parse()
        .expect("parse descendant pid");
    let descendant_process = open_process_for_wait(descendant_pid);
    fs::write(&release_marker, "release").expect("release descendant after root exit");
    let descendant_process = descendant_process.expect("open descendant after normal capture exit");

    println!("capture pwsh exit_code={}", result.exit_code);
    println!("capture pwsh timed_out={}", result.timed_out);
    let stdout = String::from_utf8_lossy(&result.stdout);
    let stderr = String::from_utf8_lossy(&result.stderr);
    println!("capture pwsh stderr={stderr:?}");
    assert_eq!(result.exit_code, 0, "stdout={stdout:?} stderr={stderr:?}");
    assert!(
        stdout.contains("LEGACY-CAPTURE-DIRECT"),
        "stdout={stdout:?}"
    );
    assert!(
        wait_for_path(&survival_marker, Duration::from_secs(10)),
        "sandbox descendant did not survive normal capture exit"
    );
    wait_for_process_exit(&descendant_process, Duration::from_secs(10))
        .expect("sandbox descendant did not exit after release");
}

#[test]
fn legacy_workspace_write_delete_is_limited_to_writable_roots() {
    let _guard = legacy_process_test_guard();
    let runtime = current_thread_runtime();
    runtime.block_on(async move {
        // Keep writable roots out of USERPROFILE exclusions such as AppData.
        let test_root = TempDir::new_in(sandbox_cwd()).expect("create legacy delete test root");
        let codex_home = sandbox_home("legacy-delete-writable-roots");
        let workspace = test_root.path().join("workspace");
        let temp_root = test_root.path().join("temp");
        let tmp_root = test_root.path().join("tmp");
        let outside_root = test_root.path().join("outside");
        for directory in [&workspace, &temp_root, &tmp_root, &outside_root] {
            fs::create_dir_all(directory).expect("create legacy delete test directory");
        }
        let protected_git_dir = workspace.join(".git");
        fs::create_dir(&protected_git_dir).expect("create protected .git directory");

        let workspace_file = workspace.join("workspace-delete.txt");
        let temp_file = temp_root.join("temp-delete.txt");
        let tmp_file = tmp_root.join("tmp-delete.txt");
        let outside_file = outside_root.join("outside-delete.txt");
        fs::write(&workspace_file, "workspace").expect("seed workspace file");
        fs::write(&temp_file, "temp").expect("seed TEMP file");
        fs::write(&tmp_file, "tmp").expect("seed TMP file");
        fs::write(&outside_file, "outside").expect("seed outside file");

        let script = workspace.join("delete-fixtures.cmd");
        fs::write(
            &script,
            concat!(
                "@echo off\r\n",
                "set /p \"__CODEX_DELETE_DIAGNOSTIC_GATE=\"\r\n",
                "del /f /q \"%WORKSPACE_DELETE%\"\r\n",
                "del /f /q \"%TEMP_DELETE%\"\r\n",
                "del /f /q \"%TMP_DELETE%\"\r\n",
                "del /f /q \"%OUTSIDE_DELETE%\"\r\n",
                "rmdir \"%PROTECTED_GIT_DIR%\"\r\n",
                "exit /b 0\r\n",
            ),
        )
        .expect("write delete script");

        let env_map = HashMap::from([
            ("TEMP".to_string(), temp_root.to_string_lossy().into_owned()),
            ("TMP".to_string(), tmp_root.to_string_lossy().into_owned()),
            (
                "WORKSPACE_DELETE".to_string(),
                workspace_file.to_string_lossy().into_owned(),
            ),
            (
                "TEMP_DELETE".to_string(),
                temp_file.to_string_lossy().into_owned(),
            ),
            (
                "TMP_DELETE".to_string(),
                tmp_file.to_string_lossy().into_owned(),
            ),
            (
                "OUTSIDE_DELETE".to_string(),
                outside_file.to_string_lossy().into_owned(),
            ),
            (
                "PROTECTED_GIT_DIR".to_string(),
                protected_git_dir.to_string_lossy().into_owned(),
            ),
        ]);

        let permission_profile = PermissionProfile::workspace_write();
        // The legacy spawn reads this same process token as its base token.
        // Observe its restricted-SID list without changing it; if empty cannot
        // be proven, the per-ACE attribution remains inconclusive.
        let base_token_unrestricted = current_process_base_token_has_no_restricted_sids();
        let before_children = direct_child_cmd_pids();
        let spawned = spawn_windows_sandbox_session_legacy(
            &permission_profile,
            workspace_roots_for(workspace.as_path()).as_slice(),
            codex_home.path(),
            vec![
                "C:\\Windows\\System32\\cmd.exe".to_string(),
                "/d".to_string(),
                "/c".to_string(),
                script.display().to_string(),
            ],
            workspace.as_path(),
            env_map,
            /*timeout_ms*/ Some(5_000),
            &[],
            &[],
            /*tty*/ false,
            /*stdin_open*/ true,
        )
        .await
        .expect("spawn legacy delete session");

        let observation = observe_delete_access(
            before_children,
            &outside_file,
            &outside_root,
            &protected_git_dir,
            &workspace,
            base_token_unrestricted,
        );
        let gate_released = spawned
            .session
            .writer_sender()
            .send(b"release\r\n".to_vec())
            .await
            .is_ok();
        spawned.session.close_stdin();
        if !gate_released {
            eprintln!("legacy delete access diagnostic: inconclusive");
        } else if let Some([outside_file_delete, outside_parent_delete_child, git_delete, workspace_delete_child]) = observation.allowed {
            eprintln!(
                "legacy delete access diagnostic: outside_file_delete={outside_file_delete} outside_parent_delete_child={outside_parent_delete_child} git_delete={git_delete} workspace_delete_child={workspace_delete_child} parent_aces={}",
                observation.parent_aces.as_deref().unwrap_or("inconclusive")
            );
        } else {
            eprintln!("legacy delete access diagnostic: inconclusive");
        }
        let (stdout, exit_code) =
            collect_stdout_and_exit(spawned, codex_home.path(), Duration::from_secs(/*secs*/ 10))
                .await;
        drop(observation);
        let stdout = String::from_utf8_lossy(&stdout);

        assert_eq!(
            (
                exit_code,
                workspace_file.exists(),
                temp_file.exists(),
                tmp_file.exists(),
                fs::read_to_string(&outside_file).ok(),
                protected_git_dir.is_dir(),
            ),
            (0, false, false, false, Some("outside".to_string()), true),
            "stdout={stdout:?}\n{}",
            sandbox_log(codex_home.path())
        );
    });
}

#[test]
fn legacy_capture_cancellation_terminates_descendants_without_timeout() {
    let Some(pwsh) = pwsh_path() else {
        eprintln!("skipping cancellation regression test: PowerShell 7 is not installed");
        return;
    };
    let _guard = legacy_process_test_guard();
    let cwd = sandbox_cwd();
    let codex_home = sandbox_home("legacy-capture-cancel");
    let descendant_marker = codex_home.path().join("descendant-survived");
    let ready_marker = codex_home.path().join("descendant-started");
    let descendant_command = format!(
        "Set-Content -LiteralPath '{}' -Value $PID; Start-Sleep -Seconds 1; Set-Content -LiteralPath '{}' -Value survived",
        powershell_literal(&ready_marker),
        powershell_literal(&descendant_marker),
    );
    let parent_command = start_powershell_child(
        &pwsh,
        codex_home.path(),
        &descendant_command,
        "Start-Sleep -Seconds 30",
    );
    let descendant_process = Arc::new(Mutex::new(None));
    let descendant_process_for_cancellation = Arc::clone(&descendant_process);
    let cancellation = WindowsSandboxCancellationToken::new(move || {
        let Ok(pid) = fs::read_to_string(&ready_marker).and_then(|pid| {
            pid.trim()
                .parse()
                .map_err(|err| std::io::Error::other(format!("invalid descendant pid: {err}")))
        }) else {
            return false;
        };
        let Ok(process) = open_process_for_wait(pid) else {
            return false;
        };
        *descendant_process_for_cancellation
            .lock()
            .expect("descendant process lock poisoned") = Some(process);
        true
    });

    let started_at = Instant::now();
    let permission_profile = PermissionProfile::workspace_write();
    let result = run_windows_sandbox_capture(
        &permission_profile,
        workspace_roots_for(cwd.as_path()).as_slice(),
        codex_home.path(),
        vec![
            pwsh.display().to_string(),
            "-NoProfile".to_string(),
            "-Command".to_string(),
            parent_command,
        ],
        cwd.as_path(),
        HashMap::new(),
        Some(30_000),
        /*cancellation*/ Some(cancellation),
    )
    .expect("run legacy capture powershell with cancellation");

    assert!(
        started_at.elapsed() < Duration::from_secs(10),
        "cancellation should end capture before the timeout"
    );
    assert!(
        !result.timed_out,
        "cancellation should not be reported as a timeout"
    );
    assert_ne!(result.exit_code, 0);
    let descendant_process = descendant_process
        .lock()
        .expect("descendant process lock poisoned")
        .take()
        .expect("cancellation did not capture descendant process");
    wait_for_process_exit(&descendant_process, Duration::from_secs(10))
        .expect("sandbox descendant did not exit after cancellation");
    assert!(
        !descendant_marker.exists(),
        "sandbox descendant survived cancellation"
    );
}

#[derive(Clone, Copy, Debug)]
enum LegacyTtyDescendantLifecycle {
    Terminate,
    Preserve,
}

async fn assert_legacy_tty_descendant_lifecycle(
    pwsh: &Path,
    lifecycle: LegacyTtyDescendantLifecycle,
) {
    let cwd = sandbox_cwd();
    let codex_home = sandbox_home(match lifecycle {
        LegacyTtyDescendantLifecycle::Terminate => "legacy-tty-descendant-terminate",
        LegacyTtyDescendantLifecycle::Preserve => "legacy-tty-descendant-preserve",
    });
    let ready_marker = codex_home.path().join("descendant-started");
    let release_marker = codex_home.path().join("release-descendant");
    let survival_marker = codex_home.path().join("descendant-survived");
    let child_tail = match lifecycle {
        LegacyTtyDescendantLifecycle::Terminate => "Start-Sleep -Seconds 30".to_string(),
        LegacyTtyDescendantLifecycle::Preserve => format!(
            "$deadline=(Get-Date).AddSeconds(30); while (-not (Test-Path -LiteralPath '{}')) {{ if ((Get-Date) -ge $deadline) {{ exit 3 }}; Start-Sleep -Milliseconds 25 }}; Set-Content -LiteralPath '{}' -Value survived",
            powershell_literal(&release_marker),
            powershell_literal(&survival_marker),
        ),
    };
    let child_command = format!(
        "Set-Content -LiteralPath '{}' -Value $PID; {child_tail}",
        powershell_literal(&ready_marker),
    );
    let parent_tail = match lifecycle {
        LegacyTtyDescendantLifecycle::Terminate => "Start-Sleep -Seconds 30".to_string(),
        LegacyTtyDescendantLifecycle::Preserve => format!(
            "while (-not (Test-Path -LiteralPath '{}')) {{ Start-Sleep -Milliseconds 25 }}",
            powershell_literal(&ready_marker),
        ),
    };
    let parent_command =
        start_powershell_child(pwsh, codex_home.path(), &child_command, &parent_tail);
    let permission_profile = PermissionProfile::workspace_write();
    let spawned = spawn_windows_sandbox_session_legacy(
        &permission_profile,
        workspace_roots_for(cwd.as_path()).as_slice(),
        codex_home.path(),
        vec![
            pwsh.display().to_string(),
            "-NoProfile".to_string(),
            "-Command".to_string(),
            parent_command,
        ],
        cwd.as_path(),
        HashMap::new(),
        Some(30_000),
        &[],
        &[],
        /*tty*/ true,
        /*stdin_open*/ false,
    )
    .await
    .expect("spawn legacy sandbox ConPTY lifecycle test");
    assert!(
        wait_for_path(&ready_marker, Duration::from_secs(10)),
        "{lifecycle:?} descendant did not start"
    );
    let descendant_pid = fs::read_to_string(&ready_marker)
        .expect("read descendant pid")
        .trim()
        .parse()
        .expect("parse descendant pid");
    let descendant_process = open_process_for_wait(descendant_pid);

    if matches!(lifecycle, LegacyTtyDescendantLifecycle::Terminate) {
        spawned.session.request_terminate();
    }
    let (_, exit_code) =
        collect_stdout_and_exit(spawned, codex_home.path(), Duration::from_secs(15)).await;
    if matches!(lifecycle, LegacyTtyDescendantLifecycle::Preserve) {
        fs::write(&release_marker, "release").expect("release preserved descendant");
    }
    let descendant_process = descendant_process.expect("open sandbox ConPTY descendant");

    match lifecycle {
        LegacyTtyDescendantLifecycle::Terminate => assert_ne!(exit_code, 0),
        LegacyTtyDescendantLifecycle::Preserve => {
            assert_eq!(exit_code, 0);
            assert!(
                wait_for_path(&survival_marker, Duration::from_secs(10)),
                "sandbox ConPTY descendant did not survive normal exit"
            );
        }
    }
    wait_for_process_exit(&descendant_process, Duration::from_secs(10))
        .expect("sandbox ConPTY descendant did not exit");
}

#[test]
fn legacy_tty_job_terminates_and_preserves_descendants() {
    let Some(pwsh) = pwsh_path() else {
        eprintln!("skipping sandbox ConPTY lifecycle test: PowerShell 7 is not installed");
        return;
    };
    let _guard = legacy_process_test_guard();
    current_thread_runtime().block_on(async move {
        assert_legacy_tty_descendant_lifecycle(&pwsh, LegacyTtyDescendantLifecycle::Terminate)
            .await;
        assert_legacy_tty_descendant_lifecycle(&pwsh, LegacyTtyDescendantLifecycle::Preserve).await;
    });
}

#[test]
fn legacy_tty_powershell_emits_output_and_accepts_input() {
    let Some(pwsh) = pwsh_path() else {
        return;
    };
    let _guard = legacy_process_test_guard();
    let runtime = current_thread_runtime();
    runtime.block_on(async move {
        let cwd = sandbox_cwd();
        let codex_home = sandbox_home("legacy-tty-pwsh");
        println!("tty pwsh codex_home={}", codex_home.path().display());
        let permission_profile = PermissionProfile::workspace_write();
        let spawned = spawn_windows_sandbox_session_legacy(
            &permission_profile,
            workspace_roots_for(cwd.as_path()).as_slice(),
            codex_home.path(),
            vec![
                pwsh.display().to_string(),
                "-NoLogo".to_string(),
                "-NoProfile".to_string(),
                "-NoExit".to_string(),
                "-Command".to_string(),
                "$PID; Write-Output ready".to_string(),
            ],
            cwd.as_path(),
            HashMap::new(),
            Some(10_000),
            &[],
            &[],
            /*tty*/ true,
            /*stdin_open*/ true,
        )
        .await
        .expect("spawn legacy tty powershell session");
        println!("tty pwsh spawn returned");

        let writer = spawned.session.writer_sender();
        writer
            .send(b"Write-Output second\n".to_vec())
            .await
            .expect("send second command");
        writer
            .send(b"exit\n".to_vec())
            .await
            .expect("send exit command");
        spawned.session.close_stdin();

        let (stdout, exit_code) =
            collect_stdout_and_exit(spawned, codex_home.path(), Duration::from_secs(15)).await;
        let stdout = String::from_utf8_lossy(&stdout);
        assert_eq!(exit_code, 0, "stdout={stdout:?}");
        assert!(stdout.contains("ready"), "stdout={stdout:?}");
        assert!(stdout.contains("second"), "stdout={stdout:?}");
    });
}
#[test]
#[ignore = "TODO: legacy ConPTY cmd.exe exits with STATUS_DLL_INIT_FAILED in CI"]
fn legacy_tty_cmd_emits_output_and_accepts_input() {
    let runtime = current_thread_runtime();
    runtime.block_on(async move {
        let cwd = sandbox_cwd();
        let codex_home = sandbox_home("legacy-tty-cmd");
        println!("tty cmd codex_home={}", codex_home.path().display());
        let permission_profile = PermissionProfile::workspace_write();
        let spawned = spawn_windows_sandbox_session_legacy(
            &permission_profile,
            workspace_roots_for(cwd.as_path()).as_slice(),
            codex_home.path(),
            vec![
                "C:\\Windows\\System32\\cmd.exe".to_string(),
                "/K".to_string(),
                "echo ready".to_string(),
            ],
            cwd.as_path(),
            HashMap::new(),
            Some(10_000),
            &[],
            &[],
            /*tty*/ true,
            /*stdin_open*/ true,
        )
        .await
        .expect("spawn legacy tty cmd session");
        println!("tty cmd spawn returned");

        let writer = spawned.session.writer_sender();
        writer
            .send(b"echo second\n".to_vec())
            .await
            .expect("send second command");
        writer
            .send(b"exit\n".to_vec())
            .await
            .expect("send exit command");
        spawned.session.close_stdin();

        let (stdout, exit_code) =
            collect_stdout_and_exit(spawned, codex_home.path(), Duration::from_secs(15)).await;
        let stdout = String::from_utf8_lossy(&stdout);
        assert_eq!(exit_code, 0, "stdout={stdout:?}");
        assert!(stdout.contains("ready"), "stdout={stdout:?}");
        assert!(stdout.contains("second"), "stdout={stdout:?}");
    });
}
