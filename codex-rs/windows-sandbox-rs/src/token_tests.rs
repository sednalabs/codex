use super::*;
use pretty_assertions::assert_eq;
use windows_sys::Win32::Security::EqualSid;
use windows_sys::Win32::Security::TokenRestrictedSids;

/// Observe the deletion rights separately from ambient filesystem ACLs before
/// choosing a legacy-token repair. This changes only disposable test fixtures.
#[test]
fn legacy_deletion_token_access_diagnostic() -> Result<()> {
    use std::os::windows::io::AsRawHandle;
    use std::os::windows::io::FromRawHandle;
    use std::os::windows::io::OwnedHandle;
    use std::path::Path;
    use windows_sys::Win32::Security::ACCESS_ALLOWED_ACE;
    use windows_sys::Win32::Security::AccessCheck;
    use windows_sys::Win32::Security::Authorization::ACCESS_MODE;
    use windows_sys::Win32::Security::Authorization::DENY_ACCESS;
    use windows_sys::Win32::Security::Authorization::SetNamedSecurityInfoW;
    use windows_sys::Win32::Security::DACL_SECURITY_INFORMATION;
    use windows_sys::Win32::Security::DuplicateToken;
    use windows_sys::Win32::Security::GENERIC_MAPPING;
    use windows_sys::Win32::Security::GetAce;
    use windows_sys::Win32::Security::ImpersonateLoggedOnUser;
    use windows_sys::Win32::Security::InitializeSecurityDescriptor;
    use windows_sys::Win32::Security::MapGenericMask;
    use windows_sys::Win32::Security::PROTECTED_DACL_SECURITY_INFORMATION;
    use windows_sys::Win32::Security::RevertToSelf;
    use windows_sys::Win32::Security::SECURITY_DESCRIPTOR;
    use windows_sys::Win32::Security::SecurityImpersonation;
    use windows_sys::Win32::Security::SetSecurityDescriptorDacl;
    use windows_sys::Win32::Security::SetSecurityDescriptorGroup;
    use windows_sys::Win32::Security::SetSecurityDescriptorOwner;
    use windows_sys::Win32::Storage::FileSystem::DELETE;
    use windows_sys::Win32::Storage::FileSystem::FILE_ALL_ACCESS;
    use windows_sys::Win32::Storage::FileSystem::FILE_DELETE_CHILD;
    use windows_sys::Win32::Storage::FileSystem::FILE_GENERIC_EXECUTE;
    use windows_sys::Win32::Storage::FileSystem::FILE_GENERIC_READ;
    use windows_sys::Win32::Storage::FileSystem::FILE_GENERIC_WRITE;
    use windows_sys::Win32::Storage::FileSystem::FILE_READ_DATA;
    use windows_sys::Win32::Storage::FileSystem::FILE_WRITE_DATA;
    use windows_sys::Win32::Storage::FileSystem::READ_CONTROL;
    use windows_sys::Win32::System::Threading::PROCESS_ALL_ACCESS;
    use windows_sys::Win32::System::Threading::PROCESS_QUERY_INFORMATION;
    use windows_sys::Win32::System::Threading::PROCESS_TERMINATE;
    use windows_sys::Win32::System::Threading::PROCESS_VM_OPERATION;
    use windows_sys::Win32::System::Threading::PROCESS_VM_READ;
    use windows_sys::Win32::System::Threading::PROCESS_VM_WRITE;

    struct AclAllocation(*mut ACL);
    impl Drop for AclAllocation {
        fn drop(&mut self) {
            unsafe { LocalFree(self.0.cast()) };
        }
    }

    unsafe fn acl(rules: &[(*mut c_void, u32, ACCESS_MODE)]) -> Result<AclAllocation> {
        let entries = rules
            .iter()
            .map(|(sid, mask, mode)| EXPLICIT_ACCESS_W {
                grfAccessPermissions: *mask,
                grfAccessMode: *mode,
                grfInheritance: 0,
                Trustee: TRUSTEE_W {
                    pMultipleTrustee: std::ptr::null_mut(),
                    MultipleTrusteeOperation: 0,
                    TrusteeForm: TRUSTEE_IS_SID,
                    TrusteeType: TRUSTEE_IS_UNKNOWN,
                    ptstrName: *sid as *mut u16,
                },
            })
            .collect::<Vec<_>>();
        let mut result = std::ptr::null_mut();
        let code = SetEntriesInAclW(
            entries.len() as u32,
            entries.as_ptr(),
            std::ptr::null(),
            &mut result,
        );
        ensure!(
            code == ERROR_SUCCESS,
            "diagnostic ACL creation failed: {code}"
        );
        Ok(AclAllocation(result))
    }

    unsafe fn fixture_acl(path: &Path, dacl: &AclAllocation) -> Result<()> {
        let code = SetNamedSecurityInfoW(
            to_wide(path).as_ptr() as *mut u16,
            1,
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            dacl.0,
            std::ptr::null_mut(),
        );
        ensure!(
            code == ERROR_SUCCESS,
            "diagnostic fixture ACL failed: {code}"
        );
        Ok(())
    }

    unsafe fn token(base: HANDLE, sids: &[*mut c_void], flags: u32) -> Result<OwnedHandle> {
        let entries = sids
            .iter()
            .map(|sid| SID_AND_ATTRIBUTES {
                Sid: *sid,
                Attributes: 0,
            })
            .collect::<Vec<_>>();
        let mut result = std::ptr::null_mut();
        ensure!(
            CreateRestrictedToken(
                base,
                flags,
                0,
                std::ptr::null(),
                0,
                std::ptr::null(),
                entries.len() as u32,
                entries.as_ptr(),
                &mut result,
            ) != 0,
            "diagnostic token creation failed: {}",
            GetLastError()
        );
        Ok(OwnedHandle::from_raw_handle(result as _))
    }

    unsafe fn access(
        token: HANDLE,
        user: *mut c_void,
        dacl: &AclAllocation,
        desired: u32,
        mapping: &GENERIC_MAPPING,
    ) -> Result<bool> {
        let mut impersonation = std::ptr::null_mut();
        ensure!(DuplicateToken(token, SecurityImpersonation, &mut impersonation) != 0);
        let impersonation = OwnedHandle::from_raw_handle(impersonation as _);
        let mut descriptor: SECURITY_DESCRIPTOR = std::mem::zeroed();
        let sd = std::ptr::addr_of_mut!(descriptor).cast();
        ensure!(InitializeSecurityDescriptor(sd, 1) != 0);
        ensure!(SetSecurityDescriptorOwner(sd, user, 0) != 0);
        ensure!(SetSecurityDescriptorGroup(sd, user, 0) != 0);
        ensure!(SetSecurityDescriptorDacl(sd, 1, dacl.0, 0) != 0);
        let mut privileges = [0u64; 128];
        let mut privilege_bytes = std::mem::size_of_val(&privileges) as u32;
        let mut granted = 0;
        let mut allowed = 0;
        ensure!(
            AccessCheck(
                sd,
                impersonation.as_raw_handle() as _,
                desired,
                mapping,
                privileges.as_mut_ptr().cast(),
                &mut privilege_bytes,
                &mut granted,
                &mut allowed,
            ) != 0,
            "diagnostic AccessCheck failed: {}",
            GetLastError()
        );
        Ok(allowed != 0)
    }

    struct Impersonation;
    impl Drop for Impersonation {
        fn drop(&mut self) {
            // Do not continue a test worker with a restricted thread token.
            if unsafe { RevertToSelf() } == 0 {
                std::process::abort();
            }
        }
    }

    unsafe {
        let base = OwnedHandle::from_raw_handle(get_current_token_for_restriction()? as _);
        let base_raw = base.as_raw_handle() as HANDLE;
        let mut user_bytes = get_user_sid_bytes(base_raw)?;
        let user = user_bytes.as_mut_ptr().cast();
        let mut logon_bytes = get_logon_sid_bytes(base_raw)?;
        let logon = logon_bytes.as_mut_ptr().cast();
        let mut everyone_bytes = world_sid()?;
        let everyone = everyone_bytes.as_mut_ptr().cast();
        let capability = LocalSid::from_string("S-1-5-21-10-20-30-40")?;
        let cap = capability.as_ptr();
        let flags = DISABLE_MAX_PRIVILEGE | LUA_TOKEN | WRITE_RESTRICTED;
        let current = token(base_raw, &[cap, logon, everyone], flags)?;
        let no_everyone = token(base_raw, &[cap, logon], flags)?;
        let caps_only = token(base_raw, &[cap], flags)?;
        let full_restriction = token(base_raw, &[cap], DISABLE_MAX_PRIVILEGE | LUA_TOKEN)?;
        let readonly_home = tempfile::tempdir()?;
        let readonly_caps = crate::cap::load_or_create_cap_sids(readonly_home.path())?;
        let readonly_sid = LocalSid::from_string(&readonly_caps.readonly)?;
        let (actual_readonly, _) =
            create_readonly_token_with_cap_from(base_raw, readonly_sid.as_ptr())?;
        let actual_readonly = OwnedHandle::from_raw_handle(actual_readonly as _);
        let tokens = [
            ("current", &current),
            ("no-everyone", &no_everyone),
            ("caps-only", &caps_only),
            ("caps-only-full", &full_restriction),
            ("actual-read-only", &actual_readonly),
        ];
        let file_mapping = GENERIC_MAPPING {
            GenericRead: FILE_GENERIC_READ,
            GenericWrite: FILE_GENERIC_WRITE,
            GenericExecute: FILE_GENERIC_EXECUTE,
            GenericAll: FILE_ALL_ACCESS,
        };
        for (acl_name, extra) in [
            ("user", None),
            ("everyone", Some(everyone)),
            ("logon", Some(logon)),
            ("capability", Some(cap)),
        ] {
            let mut rules = vec![(user, FILE_ALL_ACCESS, GRANT_ACCESS)];
            if let Some(sid) = extra {
                rules.push((sid, FILE_ALL_ACCESS, GRANT_ACCESS));
            }
            let dacl = acl(&rules)?;
            for (token_name, token) in tokens {
                let rights = [FILE_READ_DATA, FILE_WRITE_DATA, DELETE, FILE_DELETE_CHILD]
                    .map(|desired| {
                        access(
                            token.as_raw_handle() as _,
                            user,
                            &dacl,
                            desired,
                            &file_mapping,
                        )
                    })
                    .into_iter()
                    .collect::<Result<Vec<_>>>()?;
                println!(
                    "delete-access acl={acl_name} token={token_name} read/write/delete/delete-child={rights:?}"
                );
            }
        }

        let process_mapping = GENERIC_MAPPING {
            GenericRead: READ_CONTROL | PROCESS_VM_READ | PROCESS_QUERY_INFORMATION,
            GenericWrite: READ_CONTROL | PROCESS_VM_WRITE | PROCESS_VM_OPERATION,
            GenericExecute: READ_CONTROL,
            GenericAll: PROCESS_ALL_ACCESS,
        };
        set_default_dacl(caps_only.as_raw_handle() as _, logon)?;
        let mut needed = 0;
        GetTokenInformation(
            caps_only.as_raw_handle() as _,
            TokenDefaultDacl,
            std::ptr::null_mut(),
            0,
            &mut needed,
        );
        ensure!(needed > 0, "diagnostic default DACL size query failed");
        let mut buffer = vec![0u8; needed as usize];
        ensure!(
            GetTokenInformation(
                caps_only.as_raw_handle() as _,
                TokenDefaultDacl,
                buffer.as_mut_ptr().cast(),
                needed,
                &mut needed,
            ) != 0,
            "diagnostic default DACL query failed"
        );
        let default_dacl = std::ptr::read_unaligned(buffer.as_ptr().cast::<TokenDefaultDaclInfo>());
        // Map generic ACE rights as the kernel does when creating a process.
        for index in 0..u32::from((*default_dacl.default_dacl).AceCount) {
            let mut ace = std::ptr::null_mut();
            ensure!(GetAce(default_dacl.default_dacl, index, &mut ace) != 0);
            MapGenericMask(
                &mut (*ace.cast::<ACCESS_ALLOWED_ACE>()).Mask,
                &process_mapping,
            );
        }
        // AccessCheck does not retain the allocation; this borrowed ACL is still
        // owned by the GetTokenInformation buffer, not LocalFree.
        let borrowed_dacl = std::mem::ManuallyDrop::new(AclAllocation(default_dacl.default_dacl));
        for (token_name, token) in tokens {
            let rights = [PROCESS_VM_READ, PROCESS_VM_WRITE, PROCESS_TERMINATE]
                .map(|desired| {
                    access(
                        token.as_raw_handle() as _,
                        user,
                        &borrowed_dacl,
                        desired,
                        &process_mapping,
                    )
                })
                .into_iter()
                .collect::<Result<Vec<_>>>()?;
            println!("delete-default-object token={token_name} read/write/terminate={rights:?}");
        }

        let fixtures = tempfile::tempdir()?;
        let no_delete_child = FILE_ALL_ACCESS & !FILE_DELETE_CHILD;
        for (token_name, token) in tokens {
            for (case, parent_mask, target_mask, cap_allow, cap_deny) in [
                (
                    "user-target-delete",
                    no_delete_child,
                    FILE_ALL_ACCESS,
                    false,
                    false,
                ),
                (
                    "user-parent-delete-child",
                    FILE_ALL_ACCESS,
                    FILE_GENERIC_READ,
                    false,
                    false,
                ),
                (
                    "capability-target-delete",
                    no_delete_child,
                    FILE_ALL_ACCESS,
                    true,
                    false,
                ),
                (
                    "protected-target-delete",
                    no_delete_child,
                    FILE_ALL_ACCESS,
                    true,
                    true,
                ),
                (
                    "protected-parent-delete-child",
                    FILE_ALL_ACCESS,
                    FILE_ALL_ACCESS,
                    true,
                    true,
                ),
            ] {
                let directory = fixtures.path().join(format!("{token_name}-{case}"));
                std::fs::create_dir(&directory)?;
                let file = directory.join("file");
                let git = directory.join(".git");
                std::fs::write(&file, "read-control")?;
                std::fs::create_dir(&git)?;
                let mut parent_rules = vec![(user, parent_mask, GRANT_ACCESS)];
                let mut target_rules = vec![(user, target_mask, GRANT_ACCESS)];
                if cap_allow {
                    parent_rules.push((cap, no_delete_child, GRANT_ACCESS));
                    target_rules.push((cap, no_delete_child, GRANT_ACCESS));
                }
                if cap_deny {
                    target_rules.insert(0, (cap, DELETE | FILE_GENERIC_WRITE, DENY_ACCESS));
                }
                fixture_acl(&directory, &acl(&parent_rules)?)?;
                let target_dacl = acl(&target_rules)?;
                fixture_acl(&file, &target_dacl)?;
                fixture_acl(&git, &target_dacl)?;
                let observations = {
                    ensure!(
                        ImpersonateLoggedOnUser(token.as_raw_handle() as _) != 0,
                        "diagnostic impersonation failed: {}",
                        GetLastError()
                    );
                    let _impersonation = Impersonation;
                    (
                        std::fs::read_to_string(&file).is_ok(),
                        std::fs::remove_file(&file).is_ok(),
                        std::fs::remove_dir(&git).is_ok(),
                    )
                };
                println!(
                    "delete-operation case={case} token={token_name} read/file-delete/git-rmdir={observations:?}"
                );
            }
        }
    }
    Ok(())
}

unsafe fn token_has_restricting_sid(token: HANDLE, expected_sid: *mut c_void) -> Result<bool> {
    let mut needed = 0;
    GetTokenInformation(
        token,
        TokenRestrictedSids,
        std::ptr::null_mut(),
        0,
        &mut needed,
    );
    if needed == 0 {
        return Err(anyhow!(
            "GetTokenInformation(TokenRestrictedSids) size query failed: {}",
            GetLastError()
        ));
    }

    let mut buffer = vec![0_u8; needed as usize];
    if GetTokenInformation(
        token,
        TokenRestrictedSids,
        buffer.as_mut_ptr().cast(),
        needed,
        &mut needed,
    ) == 0
    {
        return Err(anyhow!(
            "GetTokenInformation(TokenRestrictedSids) failed: {}",
            GetLastError()
        ));
    }

    let group_count = std::ptr::read_unaligned(buffer.as_ptr().cast::<u32>()) as usize;
    let after_count = buffer.as_ptr().add(std::mem::size_of::<u32>()) as usize;
    let align = std::mem::align_of::<SID_AND_ATTRIBUTES>();
    let entries_addr = (after_count + (align - 1)) & !(align - 1);
    let restricting_sids =
        std::slice::from_raw_parts(entries_addr as *const SID_AND_ATTRIBUTES, group_count);
    Ok(restricting_sids
        .iter()
        .any(|entry| EqualSid(entry.Sid, expected_sid) != 0))
}

#[test]
fn elevated_token_includes_network_proxy_restricting_sid() -> Result<()> {
    let capability_sid = LocalSid::from_string("S-1-5-21-10-20-30-40")?;
    let network_proxy_sid = LocalSid::from_string("S-1-5-21-50-60-70-80")?;
    let base_token = unsafe { get_current_token_for_restriction()? };
    let restricted_token = unsafe {
        create_readonly_token_with_caps_and_user_from(
            base_token,
            &[capability_sid.as_ptr()],
            &[network_proxy_sid.as_ptr()],
        )?
    };

    let has_network_proxy_sid =
        unsafe { token_has_restricting_sid(restricted_token, network_proxy_sid.as_ptr()) };
    unsafe {
        CloseHandle(restricted_token);
        CloseHandle(base_token);
    }

    assert!(has_network_proxy_sid?);
    Ok(())
}

#[test]
fn default_objects_deny_other_logons_even_with_the_same_owner() -> Result<()> {
    use std::os::windows::io::AsRawHandle;
    use std::os::windows::io::FromRawHandle;
    use std::os::windows::io::OwnedHandle;
    use windows_sys::Win32::Security::ACCESS_ALLOWED_ACE;
    use windows_sys::Win32::Security::AccessCheck;
    use windows_sys::Win32::Security::DuplicateToken;
    use windows_sys::Win32::Security::GENERIC_MAPPING;
    use windows_sys::Win32::Security::GetAce;
    use windows_sys::Win32::Security::InitializeSecurityDescriptor;
    use windows_sys::Win32::Security::MapGenericMask;
    use windows_sys::Win32::Security::SECURITY_DESCRIPTOR;
    use windows_sys::Win32::Security::SecurityImpersonation;
    use windows_sys::Win32::Security::SetSecurityDescriptorDacl;
    use windows_sys::Win32::Security::SetSecurityDescriptorGroup;
    use windows_sys::Win32::Security::SetSecurityDescriptorOwner;
    use windows_sys::Win32::Storage::FileSystem::READ_CONTROL;
    use windows_sys::Win32::Storage::FileSystem::WRITE_DAC;
    use windows_sys::Win32::System::Threading::PROCESS_ALL_ACCESS;
    use windows_sys::Win32::System::Threading::PROCESS_CREATE_THREAD;
    use windows_sys::Win32::System::Threading::PROCESS_QUERY_INFORMATION;
    use windows_sys::Win32::System::Threading::PROCESS_TERMINATE;
    use windows_sys::Win32::System::Threading::PROCESS_VM_OPERATION;
    use windows_sys::Win32::System::Threading::PROCESS_VM_READ;
    use windows_sys::Win32::System::Threading::PROCESS_VM_WRITE;

    unsafe {
        let base = OwnedHandle::from_raw_handle(get_current_token_for_restriction()? as _);
        let base_raw = base.as_raw_handle() as HANDLE;
        let capability = LocalSid::from_string("S-1-5-21-10-20-30-40")?;
        let victim =
            OwnedHandle::from_raw_handle(create_workspace_write_token_with_caps_and_user_from(
                base_raw,
                &[capability.as_ptr()],
                &[],
            )? as _);
        let mut needed = 0;
        GetTokenInformation(
            victim.as_raw_handle() as _,
            TokenDefaultDacl,
            std::ptr::null_mut(),
            0,
            &mut needed,
        );
        let mut buffer = vec![0u8; needed as usize];
        ensure!(
            GetTokenInformation(
                victim.as_raw_handle() as _,
                TokenDefaultDacl,
                buffer.as_mut_ptr().cast(),
                needed,
                &mut needed
            ) != 0,
            "GetTokenInformation(TokenDefaultDacl) failed: {}",
            GetLastError()
        );
        let default_dacl = std::ptr::read_unaligned(buffer.as_ptr().cast::<TokenDefaultDaclInfo>());
        let mapping = GENERIC_MAPPING {
            GenericRead: READ_CONTROL | PROCESS_VM_READ | PROCESS_QUERY_INFORMATION,
            GenericWrite: READ_CONTROL | PROCESS_VM_WRITE | PROCESS_VM_OPERATION,
            GenericExecute: READ_CONTROL,
            GenericAll: PROCESS_ALL_ACCESS,
        };
        // Object creation maps the token default DACL's generic ACE masks to
        // object-specific rights. AccessCheck expects that mapping already done.
        for index in 0..u32::from((*default_dacl.default_dacl).AceCount) {
            let mut ace = std::ptr::null_mut();
            ensure!(GetAce(default_dacl.default_dacl, index, &mut ace) != 0);
            MapGenericMask(&mut (*ace.cast::<ACCESS_ALLOWED_ACE>()).Mask, &mapping);
        }
        let mut owner = get_user_sid_bytes(base_raw)?;
        let mut descriptor: SECURITY_DESCRIPTOR = std::mem::zeroed();
        let sd = std::ptr::addr_of_mut!(descriptor).cast();
        ensure!(InitializeSecurityDescriptor(sd, 1) != 0);
        ensure!(SetSecurityDescriptorOwner(sd, owner.as_mut_ptr().cast(), 0) != 0);
        ensure!(SetSecurityDescriptorGroup(sd, owner.as_mut_ptr().cast(), 0) != 0);
        ensure!(SetSecurityDescriptorDacl(sd, 1, default_dacl.default_dacl, 0) != 0);

        // Model another logon of the same account without the victim's logon
        // SID. Build from the base token, retaining the shared user, Everyone,
        // and filesystem capability in the write-restricting SID set.
        let mut logon = get_logon_sid_bytes(base_raw)?;
        let mut everyone = world_sid()?;
        let disabled = SID_AND_ATTRIBUTES {
            Sid: logon.as_mut_ptr().cast(),
            Attributes: 0,
        };
        let restricting = [
            capability.as_ptr(),
            owner.as_mut_ptr().cast(),
            everyone.as_mut_ptr().cast(),
        ]
        .map(|sid| SID_AND_ATTRIBUTES {
            Sid: sid,
            Attributes: 0,
        });
        let mut other = std::ptr::null_mut();
        ensure!(
            CreateRestrictedToken(
                base_raw,
                DISABLE_MAX_PRIVILEGE | LUA_TOKEN | WRITE_RESTRICTED,
                1,
                &disabled,
                0,
                std::ptr::null(),
                restricting.len() as u32,
                restricting.as_ptr(),
                &mut other
            ) != 0,
            "CreateRestrictedToken(other logon) failed: {}",
            GetLastError()
        );
        let other = OwnedHandle::from_raw_handle(other as _);
        for (token, same_logon) in [(&victim, true), (&other, false)] {
            let mut impersonation = std::ptr::null_mut();
            ensure!(
                DuplicateToken(
                    token.as_raw_handle() as _,
                    SecurityImpersonation,
                    &mut impersonation
                ) != 0,
                "DuplicateToken failed: {}",
                GetLastError()
            );
            let impersonation = OwnedHandle::from_raw_handle(impersonation as _);
            for access in [
                PROCESS_VM_READ,
                PROCESS_VM_WRITE,
                PROCESS_VM_OPERATION,
                PROCESS_CREATE_THREAD,
                PROCESS_TERMINATE,
                PROCESS_QUERY_INFORMATION,
                WRITE_DAC,
                0x43a,
            ] {
                let mut privileges = [0u64; 128];
                let mut privilege_bytes = std::mem::size_of_val(&privileges) as u32;
                let mut granted = 0;
                let mut allowed = 0;
                ensure!(
                    AccessCheck(
                        sd,
                        impersonation.as_raw_handle() as _,
                        access,
                        &mapping,
                        privileges.as_mut_ptr().cast(),
                        &mut privilege_bytes,
                        &mut granted,
                        &mut allowed
                    ) != 0,
                    "AccessCheck failed: {}",
                    GetLastError()
                );
                assert_eq!(allowed != 0, same_logon, "access {access:#x}");
            }
        }
    }
    Ok(())
}
