use crate::acl::ensure_write_acl_on_handle;
use crate::deny_read_acl::lexical_path_key;
use crate::path_normalization::canonicalize_path;
use crate::token::LocalSid;
use crate::winutil::to_wide;
use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use std::collections::HashMap;
use std::collections::HashSet;
use std::ffi::OsString;
use std::ffi::c_void;
use std::fs;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;
use windows_sys::Win32::Foundation::CloseHandle;
use windows_sys::Win32::Foundation::ERROR_FILE_NOT_FOUND;
use windows_sys::Win32::Foundation::ERROR_PATH_NOT_FOUND;
use windows_sys::Win32::Foundation::GetLastError;
use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
use windows_sys::Win32::Foundation::WAIT_ABANDONED;
use windows_sys::Win32::Foundation::WAIT_OBJECT_0;
use windows_sys::Win32::Storage::FileSystem::BY_HANDLE_FILE_INFORMATION;
use windows_sys::Win32::Storage::FileSystem::CreateFileW;
use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_DIRECTORY;
use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;
use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_BACKUP_SEMANTICS;
use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;
use windows_sys::Win32::Storage::FileSystem::FILE_READ_ATTRIBUTES;
use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ;
use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_WRITE;
use windows_sys::Win32::Storage::FileSystem::GetFileInformationByHandle;
use windows_sys::Win32::Storage::FileSystem::GetFinalPathNameByHandleW;
use windows_sys::Win32::Storage::FileSystem::OPEN_EXISTING;
use windows_sys::Win32::Storage::FileSystem::READ_CONTROL;
use windows_sys::Win32::Storage::FileSystem::WRITE_DAC;
use windows_sys::Win32::System::Threading::CreateMutexW;
use windows_sys::Win32::System::Threading::ReleaseMutex;
use windows_sys::Win32::System::Threading::WaitForSingleObject;

const WRITE_ACL_MUTEX_NAME: &str = "Local\\CodexSandboxWriteAclMutationV1";
const WRITE_ACL_MUTEX_TIMEOUT_MS: u32 = 30_000;
const MAX_REPAIR_DEPTH: usize = 512;
const MAX_REPAIR_OBJECTS: usize = 1_000_000;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct WriteAclRoot {
    pub path: PathBuf,
    pub capability_sid: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteAclRepairMode {
    FullMigration,
    Refresh,
}

struct RootPolicy {
    path: PathBuf,
    key: String,
    capability_sid: String,
}

struct DenyPolicy {
    path: PathBuf,
    key: String,
}

struct WriteAclPolicy {
    roots: Vec<RootPolicy>,
    denies: Vec<DenyPolicy>,
    shared_allow_sid: Option<String>,
    sids: HashMap<String, LocalSid>,
}

#[derive(Clone, Copy)]
enum RepairPhase {
    Deny,
    Allow,
}

impl WriteAclPolicy {
    fn new(
        roots: &[WriteAclRoot],
        deny_paths: &[PathBuf],
        shared_allow_sid: Option<&str>,
    ) -> Result<Self> {
        let mut normalized_roots = Vec::new();
        let mut seen_roots = HashSet::new();
        let mut sids = HashMap::new();
        for root in roots {
            if !root.path.exists() {
                continue;
            }
            let path = canonicalize_path(&root.path);
            let key = lexical_path_key(&path);
            let dedupe_key = (key.clone(), root.capability_sid.clone());
            if !seen_roots.insert(dedupe_key) {
                continue;
            }
            if !sids.contains_key(&root.capability_sid) {
                sids.insert(
                    root.capability_sid.clone(),
                    LocalSid::from_string(&root.capability_sid)?,
                );
            }
            normalized_roots.push(RootPolicy {
                path,
                key,
                capability_sid: root.capability_sid.clone(),
            });
        }

        let shared_allow_sid = shared_allow_sid.map(str::to_owned);
        if let Some(sid) = &shared_allow_sid
            && !sids.contains_key(sid)
        {
            sids.insert(sid.clone(), LocalSid::from_string(sid)?);
        }

        let mut seen_denies = HashSet::new();
        let denies = deny_paths
            .iter()
            .map(|path| canonicalize_path(path))
            .filter_map(|path| {
                let key = lexical_path_key(&path);
                seen_denies
                    .insert(key.clone())
                    .then_some(DenyPolicy { path, key })
            })
            .collect();

        Ok(Self {
            roots: normalized_roots,
            denies,
            shared_allow_sid,
            sids,
        })
    }

    fn top_level_root_paths(&self) -> Vec<PathBuf> {
        let mut paths = Vec::new();
        let mut seen = HashSet::new();
        for root in &self.roots {
            if seen.insert(root.key.clone()) {
                paths.push((root.path.clone(), root.key.clone()));
            }
        }
        paths
            .iter()
            .filter(|(_, key)| {
                !paths.iter().any(|(_, candidate)| {
                    candidate != key && is_same_or_descendant_key(key, candidate)
                })
            })
            .map(|(path, _)| path.clone())
            .collect()
    }

    fn root_boundary_paths(&self) -> Vec<PathBuf> {
        let mut boundaries = Vec::new();
        let mut seen = HashSet::new();
        for root in &self.roots {
            if seen.insert(root.key.clone()) {
                boundaries.push(root.path.clone());
            }
        }
        boundaries
    }

    fn deny_starts_beneath(&self, top_root: &Path) -> Vec<PathBuf> {
        let root_key = lexical_path_key(top_root);
        let mut starts = Vec::new();
        let mut seen = HashSet::new();
        for deny in &self.denies {
            let start = if is_same_or_descendant_key(&root_key, &deny.key) {
                top_root.to_path_buf()
            } else if is_same_or_descendant_key(&deny.key, &root_key) {
                deny.path.clone()
            } else {
                continue;
            };
            let key = lexical_path_key(&start);
            if seen.insert(key.clone()) {
                starts.push((start, key));
            }
        }
        starts
            .iter()
            .filter(|(_, key)| {
                !starts.iter().any(|(_, candidate)| {
                    candidate != key && is_same_or_descendant_key(key, candidate)
                })
            })
            .map(|(path, _)| path.clone())
            .collect()
    }

    fn is_denied(&self, path: &Path) -> bool {
        let key = lexical_path_key(path);
        self.denies
            .iter()
            .any(|deny| is_same_or_descendant_key(&key, &deny.key))
    }

    fn deny_sids_for_path(&self, path: &Path) -> Vec<*mut c_void> {
        let key = lexical_path_key(path);
        let active_deny_keys = self
            .denies
            .iter()
            .filter(|deny| is_same_or_descendant_key(&key, &deny.key))
            .map(|deny| deny.key.as_str())
            .collect::<Vec<_>>();
        if active_deny_keys.is_empty() {
            return Vec::new();
        }
        let mut seen = HashSet::new();
        self.roots
            .iter()
            .filter(|root| {
                active_deny_keys.iter().any(|deny_key| {
                    is_same_or_descendant_key(&root.key, deny_key)
                        || is_same_or_descendant_key(deny_key, &root.key)
                })
            })
            .filter(|root| seen.insert(root.capability_sid.as_str()))
            .filter_map(|root| self.sids.get(&root.capability_sid))
            .map(LocalSid::as_ptr)
            .collect()
    }

    fn allow_sids_for_path(&self, path: &Path) -> Vec<*mut c_void> {
        if self.is_denied(path) {
            return Vec::new();
        }
        let key = lexical_path_key(path);
        let mut allow_sids = Vec::new();
        if let Some(shared) = &self.shared_allow_sid
            && let Some(sid) = self.sids.get(shared)
        {
            allow_sids.push(sid.as_ptr());
        }
        let mut seen = HashSet::new();
        allow_sids.extend(
            self.roots
                .iter()
                .filter(|root| is_same_or_descendant_key(&key, &root.key))
                .filter(|root| seen.insert(root.capability_sid.as_str()))
                .filter_map(|root| self.sids.get(&root.capability_sid))
                .map(LocalSid::as_ptr),
        );
        allow_sids
    }
}

struct PinnedHandle(HANDLE);

impl PinnedHandle {
    fn raw(&self) -> HANDLE {
        self.0
    }
}

impl Drop for PinnedHandle {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0);
        }
    }
}

struct PinnedObject {
    handle: PinnedHandle,
    is_directory: bool,
    link_count: u32,
    final_path_key: String,
}

enum PinnedOpen {
    Object(PinnedObject),
    Missing,
    Reparse,
}

impl PinnedObject {
    fn open(path: &Path) -> Result<PinnedOpen> {
        let handle = unsafe {
            CreateFileW(
                to_wide(path).as_ptr(),
                READ_CONTROL | WRITE_DAC | FILE_READ_ATTRIBUTES,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                std::ptr::null_mut(),
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
                0,
            )
        };
        if handle == 0 || handle == INVALID_HANDLE_VALUE {
            let error = unsafe { GetLastError() };
            if error == ERROR_FILE_NOT_FOUND || error == ERROR_PATH_NOT_FOUND {
                return Ok(PinnedOpen::Missing);
            }
            return Err(anyhow!("open pinned ACL path {} failed: {error}", path.display()));
        }
        let handle = PinnedHandle(handle);
        let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
        if unsafe { GetFileInformationByHandle(handle.raw(), &mut info) } == 0 {
            return Err(anyhow!(
                "inspect pinned ACL path {} failed: {}",
                path.display(),
                unsafe { GetLastError() }
            ));
        }
        if info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Ok(PinnedOpen::Reparse);
        }
        let final_path_key = final_path_key(handle.raw())
            .with_context(|| format!("resolve pinned ACL path {}", path.display()))?;
        Ok(PinnedOpen::Object(Self {
            handle,
            is_directory: info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0,
            link_count: info.nNumberOfLinks,
            final_path_key,
        }))
    }
}

struct WriteAclMutex {
    handle: PinnedHandle,
}

struct WriteAclMutexGuard<'a> {
    mutex: &'a WriteAclMutex,
}

#[derive(Default)]
struct RepairBudget {
    visited: usize,
}

impl RepairBudget {
    fn visit(&mut self, path: &Path, depth: usize) -> Result<()> {
        if depth > MAX_REPAIR_DEPTH {
            return Err(anyhow!(
                "write ACL repair depth exceeded at {} (limit {MAX_REPAIR_DEPTH})",
                path.display()
            ));
        }
        self.visited = self
            .visited
            .checked_add(1)
            .ok_or_else(|| anyhow!("write ACL repair object count overflow"))?;
        if self.visited > MAX_REPAIR_OBJECTS {
            return Err(anyhow!(
                "write ACL repair object limit exceeded at {} (limit {MAX_REPAIR_OBJECTS})",
                path.display()
            ));
        }
        Ok(())
    }
}

impl WriteAclMutex {
    fn open() -> Result<Self> {
        let name = to_wide(WRITE_ACL_MUTEX_NAME);
        let handle = unsafe { CreateMutexW(std::ptr::null_mut(), 0, name.as_ptr()) };
        if handle == 0 {
            return Err(anyhow!("create write ACL mutation mutex failed: {}", unsafe {
                GetLastError()
            }));
        }
        Ok(Self {
            handle: PinnedHandle(handle),
        })
    }

    fn lock(&self) -> Result<WriteAclMutexGuard<'_>> {
        let status = unsafe {
            WaitForSingleObject(self.handle.raw(), WRITE_ACL_MUTEX_TIMEOUT_MS)
        };
        if status != WAIT_OBJECT_0 && status != WAIT_ABANDONED {
            return Err(anyhow!("write ACL mutation mutex wait failed: {status}"));
        }
        Ok(WriteAclMutexGuard { mutex: self })
    }
}

impl Drop for WriteAclMutexGuard<'_> {
    fn drop(&mut self) {
        unsafe {
            let _ = ReleaseMutex(self.mutex.handle.raw());
        }
    }
}

/// Repair the write-capability ACL policy without following or reopening a checked reparse path.
///
/// The full migration streams each tree while keeping the active ancestor chain pinned. Refreshes
/// inspect only policy boundaries unless a boundary changed, in which case they repeat the bounded
/// streaming migration. ACL read/modify/write operations are serialized across helper processes.
pub fn repair_write_acl_policy(
    roots: &[WriteAclRoot],
    deny_paths: &[PathBuf],
    shared_allow_sid: Option<&str>,
    mode: WriteAclRepairMode,
) -> Result<()> {
    let policy = WriteAclPolicy::new(roots, deny_paths, shared_allow_sid)?;
    if policy.roots.is_empty() {
        return Ok(());
    }
    let mutex = WriteAclMutex::open()?;
    let top_roots = policy.top_level_root_paths();

    for top_root in &top_roots {
        materialize_deny_boundaries(top_root, &policy)?;
    }

    let mut budget = RepairBudget::default();
    for top_root in &top_roots {
        repair_deny_subtrees(
            top_root,
            &policy,
            &mutex,
            mode,
            &mut budget,
        )?;
    }

    let requires_full_migration = match mode {
        WriteAclRepairMode::FullMigration => true,
        WriteAclRepairMode::Refresh => {
            let mut changed = false;
            for top_root in &top_roots {
                changed |= repair_boundaries_beneath(top_root, &policy, &mutex)?;
            }
            changed
        }
    };
    if !requires_full_migration {
        return Ok(());
    }

    for top_root in top_roots {
        let root = match PinnedObject::open(&top_root)? {
            PinnedOpen::Object(root) => root,
            PinnedOpen::Missing => continue,
            PinnedOpen::Reparse => {
                return Err(anyhow!("write ACL root is a reparse point: {}", top_root.display()));
            }
        };
        let root_final_key = root.final_path_key.clone();
        repair_tree(
            &top_root,
            root,
            &root_final_key,
            &policy,
            &mutex,
            &mut budget,
            0,
            RepairPhase::Allow,
        )?;
    }
    Ok(())
}

fn repair_boundaries_beneath(
    top_root: &Path,
    policy: &WriteAclPolicy,
    mutex: &WriteAclMutex,
) -> Result<bool> {
    let root = match PinnedObject::open(top_root)? {
        PinnedOpen::Object(root) => root,
        PinnedOpen::Missing => return Ok(false),
        PinnedOpen::Reparse => {
            return Err(anyhow!("write ACL root is a reparse point: {}", top_root.display()));
        }
    };
    let root_final_key = root.final_path_key.clone();
    let mut changed = apply_policy_to_object(
        top_root,
        &root,
        policy,
        mutex,
        RepairPhase::Allow,
    )?;
    for boundary in policy.root_boundary_paths() {
        let boundary_key = lexical_path_key(&boundary);
        let root_key = lexical_path_key(top_root);
        if boundary_key == root_key || !is_same_or_descendant_key(&boundary_key, &root_key) {
            continue;
        }
        changed |= repair_descendant_boundary(
            top_root,
            &root_final_key,
            &boundary,
            false,
            policy,
            mutex,
            RepairPhase::Allow,
        )?;
    }
    Ok(changed)
}

fn materialize_deny_boundaries(top_root: &Path, policy: &WriteAclPolicy) -> Result<()> {
    let root = match PinnedObject::open(top_root)? {
        PinnedOpen::Object(root) => root,
        PinnedOpen::Missing => return Ok(()),
        PinnedOpen::Reparse => {
            return Err(anyhow!("write ACL root is a reparse point: {}", top_root.display()));
        }
    };
    let root_final_key = root.final_path_key.clone();
    let root_key = lexical_path_key(top_root);
    for deny in &policy.denies {
        if deny.key == root_key || !is_same_or_descendant_key(&deny.key, &root_key) {
            continue;
        }
        let _ = open_descendant_chain(
            top_root,
            &root_final_key,
            &deny.path,
            true,
        )?;
    }
    Ok(())
}

fn repair_deny_subtrees(
    top_root: &Path,
    policy: &WriteAclPolicy,
    mutex: &WriteAclMutex,
    mode: WriteAclRepairMode,
    budget: &mut RepairBudget,
) -> Result<()> {
    let root_key = lexical_path_key(top_root);
    for start in policy.deny_starts_beneath(top_root) {
        let start_key = lexical_path_key(&start);
        let mut ancestor_chain = Vec::new();
        let object = if start_key == root_key {
            match PinnedObject::open(top_root)? {
                PinnedOpen::Object(root) => root,
                PinnedOpen::Missing => continue,
                PinnedOpen::Reparse => {
                    return Err(anyhow!(
                        "write ACL root is a reparse point: {}",
                        top_root.display()
                    ));
                }
            }
        } else {
            let root = match PinnedObject::open(top_root)? {
                PinnedOpen::Object(root) => root,
                PinnedOpen::Missing => continue,
                PinnedOpen::Reparse => {
                    return Err(anyhow!(
                        "write ACL root is a reparse point: {}",
                        top_root.display()
                    ));
                }
            };
            let root_final_key = root.final_path_key.clone();
            let Some(mut chain) = open_descendant_chain(
                top_root,
                &root_final_key,
                &start,
                true,
            )? else {
                continue;
            };
            let Some(object) = chain.pop() else {
                continue;
            };
            ancestor_chain.push(root);
            ancestor_chain.extend(chain);
            object
        };
        let root_final_key = object.final_path_key.clone();
        let changed = apply_policy_to_object(
            &start,
            &object,
            policy,
            mutex,
            RepairPhase::Deny,
        )?;
        if mode == WriteAclRepairMode::FullMigration || changed {
            repair_tree(
                &start,
                object,
                &root_final_key,
                policy,
                mutex,
                budget,
                0,
                RepairPhase::Deny,
            )?;
        }
        drop(ancestor_chain);
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn repair_descendant_boundary(
    top_root: &Path,
    root_final_key: &str,
    boundary: &Path,
    create_missing: bool,
    policy: &WriteAclPolicy,
    mutex: &WriteAclMutex,
    phase: RepairPhase,
) -> Result<bool> {
    let Some(chain) = open_descendant_chain(
        top_root,
        root_final_key,
        boundary,
        create_missing,
    )? else {
        return Ok(false);
    };
    let Some(object) = chain.last() else {
        return Ok(false);
    };
    apply_policy_to_object(boundary, object, policy, mutex, phase)
}

fn open_descendant_chain(
    top_root: &Path,
    root_final_key: &str,
    target: &Path,
    create_missing: bool,
) -> Result<Option<Vec<PinnedObject>>> {
    let Some(components) = descendant_components(top_root, target) else {
        return Ok(None);
    };
    let mut current = top_root.to_path_buf();
    let mut chain = Vec::new();
    for (index, component) in components.iter().enumerate() {
        current.push(component);
        let mut object = PinnedObject::open(&current)?;
        if matches!(&object, PinnedOpen::Missing) && create_missing {
            match fs::create_dir(&current) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("materialize deny path {}", current.display()));
                }
            }
            object = PinnedObject::open(&current)?;
        }
        let object = match object {
            PinnedOpen::Object(object) => object,
            PinnedOpen::Missing => return Ok(None),
            PinnedOpen::Reparse => {
                return Err(anyhow!(
                    "ACL policy boundary is a reparse point: {}",
                    current.display()
                ));
            }
        };
        if !is_same_or_descendant_key(&object.final_path_key, root_final_key) {
            return Err(anyhow!(
                "pinned ACL path escaped root: {}",
                current.display()
            ));
        }
        if index + 1 < components.len() && !object.is_directory {
            return Err(anyhow!(
                "ACL boundary parent is not a directory: {}",
                current.display()
            ));
        }
        chain.push(object);
    }
    Ok(Some(chain))
}

fn repair_tree(
    path: &Path,
    object: PinnedObject,
    root_final_key: &str,
    policy: &WriteAclPolicy,
    mutex: &WriteAclMutex,
    budget: &mut RepairBudget,
    depth: usize,
    phase: RepairPhase,
) -> Result<()> {
    budget.visit(path, depth)?;
    if !is_same_or_descendant_key(&object.final_path_key, root_final_key) {
        return Err(anyhow!("pinned ACL path escaped root: {}", path.display()));
    }
    if matches!(phase, RepairPhase::Allow) && policy.is_denied(path) {
        return Ok(());
    }
    if matches!(phase, RepairPhase::Allow) && !object.is_directory && object.link_count > 1 {
        return Err(anyhow!(
            "write ACL repair refuses a multiply linked file: {}",
            path.display()
        ));
    }
    apply_policy_to_object(path, &object, policy, mutex, phase)?;
    if !object.is_directory {
        return Ok(());
    }

    let entries = match fs::read_dir(path) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("enumerate write descendants under {}", path.display()));
        }
    };
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("enumerate write descendant under {}", path.display())
                });
            }
        };
        let child_path = entry.path();
        let child = match PinnedObject::open(&child_path)? {
            PinnedOpen::Object(child) => child,
            PinnedOpen::Missing => continue,
            PinnedOpen::Reparse if matches!(phase, RepairPhase::Allow) => continue,
            PinnedOpen::Reparse => {
                return Err(anyhow!(
                    "deny-write subtree contains a reparse point: {}",
                    child_path.display()
                ));
            }
        };
        repair_tree(
            &child_path,
            child,
            root_final_key,
            policy,
            mutex,
            budget,
            depth + 1,
            phase,
        )?;
    }
    Ok(())
}

fn apply_policy_to_object(
    path: &Path,
    object: &PinnedObject,
    policy: &WriteAclPolicy,
    mutex: &WriteAclMutex,
    phase: RepairPhase,
) -> Result<bool> {
    let (allow_sids, deny_sids) = match phase {
        RepairPhase::Deny => (Vec::new(), policy.deny_sids_for_path(path)),
        RepairPhase::Allow => (policy.allow_sids_for_path(path), Vec::new()),
    };
    if allow_sids.is_empty() && deny_sids.is_empty() {
        return Ok(false);
    }
    let _guard = mutex.lock()?;
    unsafe {
        ensure_write_acl_on_handle(
            object.handle.raw(),
            &allow_sids,
            &deny_sids,
            object.is_directory,
        )
    }
    .with_context(|| format!("reconcile write ACL on {}", path.display()))
}

fn final_path_key(handle: HANDLE) -> Result<String> {
    let mut buffer = vec![0_u16; 512];
    loop {
        let length = unsafe {
            GetFinalPathNameByHandleW(handle, buffer.as_mut_ptr(), buffer.len() as u32, 0)
        };
        if length == 0 {
            return Err(anyhow!("GetFinalPathNameByHandleW failed: {}", unsafe {
                GetLastError()
            }));
        }
        if (length as usize) < buffer.len() {
            buffer.truncate(length as usize);
            break;
        }
        buffer.resize(length as usize + 1, 0);
    }
    let path = String::from_utf16_lossy(&buffer).replace('/', "\\");
    let path = path
        .strip_prefix(r"\\?\UNC\")
        .map(|suffix| format!(r"\\{suffix}"))
        .or_else(|| path.strip_prefix(r"\\?\").map(str::to_owned))
        .unwrap_or(path);
    Ok(path.trim_end_matches('\\').to_ascii_lowercase())
}

fn is_same_or_descendant_key(path_key: &str, root_key: &str) -> bool {
    path_key == root_key
        || path_key
            .strip_prefix(root_key)
            .is_some_and(|suffix| suffix.starts_with('/') || suffix.starts_with('\\'))
}

fn descendant_components(root: &Path, target: &Path) -> Option<Vec<OsString>> {
    let root_components = normal_components(root)?;
    let target_components = normal_components(target)?;
    if root_components.len() > target_components.len()
        || !root_components
            .iter()
            .zip(&target_components)
            .all(|(left, right)| left.eq_ignore_ascii_case(right))
    {
        return None;
    }
    let descendants = target_components[root_components.len()..].to_vec();
    (descendants.len() <= MAX_REPAIR_DEPTH).then_some(descendants)
}

fn normal_components(path: &Path) -> Option<Vec<OsString>> {
    let mut components = Vec::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => components.push(prefix.as_os_str().to_os_string()),
            Component::RootDir | Component::CurDir => {}
            Component::ParentDir => return None,
            Component::Normal(value) => components.push(value.to_os_string()),
        }
    }
    Some(components)
}

#[cfg(test)]
#[path = "write_acl_repair_tests.rs"]
mod tests;
