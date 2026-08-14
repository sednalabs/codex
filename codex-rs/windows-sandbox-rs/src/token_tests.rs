use super::*;
use std::ffi::c_void;
use windows_sys::Win32::Security::EqualSid;
use windows_sys::Win32::Security::TokenRestrictedSids;

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

fn fake_ptr(value: usize) -> *mut c_void {
    std::ptr::without_provenance_mut(value)
}

#[test]
fn restricted_sids_exclude_everyone() {
    let caps = [fake_ptr(/*value*/ 0x10), fake_ptr(/*value*/ 0x20)];
    let extras = [fake_ptr(/*value*/ 0x30)];
    let logon = fake_ptr(/*value*/ 0x40);
    let everyone = fake_ptr(/*value*/ 0x50);

    let entries = build_restricted_sid_entries(&caps, &extras, logon);
    let restricted = entries.iter().map(|entry| entry.Sid).collect::<Vec<_>>();

    assert_eq!(restricted, vec![caps[0], caps[1], extras[0], logon]);
    assert!(!restricted.contains(&everyone));
}

#[test]
fn default_dacl_keeps_everyone_for_ipc_compatibility() {
    let caps = [fake_ptr(/*value*/ 0x10), fake_ptr(/*value*/ 0x20)];
    let logon = fake_ptr(/*value*/ 0x30);
    let everyone = fake_ptr(/*value*/ 0x40);

    let dacl_sids = build_default_dacl_sids(&caps, &[], logon, everyone);

    assert_eq!(dacl_sids, vec![logon, everyone, caps[0], caps[1]]);
}

#[test]
fn elevated_user_is_in_default_dacl_but_everyone_is_not_restricting() {
    let caps = [fake_ptr(/*value*/ 0x10), fake_ptr(/*value*/ 0x20)];
    let user = fake_ptr(/*value*/ 0x30);
    let logon = fake_ptr(/*value*/ 0x40);
    let everyone = fake_ptr(/*value*/ 0x50);

    let restricted = build_restricted_sid_entries(&caps, &[user], logon)
        .iter()
        .map(|entry| entry.Sid)
        .collect::<Vec<_>>();
    let dacl_sids = build_default_dacl_sids(&caps, &[user, caps[0], everyone], logon, everyone);

    assert_eq!(restricted, vec![caps[0], caps[1], user, logon]);
    assert_eq!(dacl_sids, vec![logon, everyone, caps[0], caps[1], user]);
}
