use super::*;
use windows_sys::Win32::Security::EqualSid;
use windows_sys::Win32::Security::TokenRestrictedSids;
use windows_sys::Win32::Security::WinRestrictedCodeSid;

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

fn fake_ptr(value: usize) -> *mut c_void {
    value as *mut c_void
}

#[test]
fn restricted_sids_keep_everyone_for_loader_compatibility() {
    let caps = [fake_ptr(0x10), fake_ptr(0x20)];
    let extras = [fake_ptr(0x30)];
    let logon = fake_ptr(0x40);
    let everyone = fake_ptr(0x50);

    let entries = build_restricted_sid_entries(&caps, &extras, logon, everyone);
    let restricted = entries.iter().map(|entry| entry.Sid).collect::<Vec<_>>();

    assert_eq!(
        restricted,
        vec![caps[0], caps[1], extras[0], logon, everyone]
    );
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
fn write_restricted_token_uses_capabilities_and_everyone_restrictions() -> Result<()> {
    let capability_sid = LocalSid::from_string("S-1-5-21-10-20-30-40")?;
    let base_token = unsafe { get_current_token_for_restriction()? };
    let restricted_token = unsafe {
        create_workspace_write_token_with_caps_from(base_token, &[capability_sid.as_ptr()])?
    };
    let everyone = unsafe { world_sid()? };
    let capability_is_restricting =
        unsafe { token_has_restricting_sid(restricted_token, capability_sid.as_ptr()) };
    let has_restricted_code = unsafe {
        let restricted_code = well_known_sid(WinRestrictedCodeSid)?;
        token_has_restricting_sid(restricted_token, restricted_code.as_ptr() as *mut c_void)
    };
    let has_everyone =
        unsafe { token_has_restricting_sid(restricted_token, everyone.as_ptr() as *mut c_void) };
    unsafe {
        CloseHandle(restricted_token);
        CloseHandle(base_token);
    }

    assert!(capability_is_restricting?);
    assert!(!has_restricted_code?);
    assert!(has_everyone?);
    Ok(())
}
