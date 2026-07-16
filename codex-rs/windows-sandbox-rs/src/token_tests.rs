use super::build_default_dacl_sids;
use super::build_restricted_sid_entries;
use std::ffi::c_void;

#[test]
fn restricted_sids_exclude_everyone() {
    let cap_a = 0x10usize as *mut c_void;
    let cap_b = 0x20usize as *mut c_void;
    let extra = 0x30usize as *mut c_void;
    let logon = 0x40usize as *mut c_void;
    let everyone = 0x50usize as *mut c_void;
    let caps = [cap_a, cap_b];
    let extras = [extra];

    let entries = build_restricted_sid_entries(&caps, &extras, logon);
    let restricted = entries.iter().map(|entry| entry.Sid).collect::<Vec<_>>();

    assert_eq!(restricted, vec![caps[0], caps[1], extras[0], logon]);
    assert!(!restricted.contains(&everyone));
}

#[test]
fn default_dacl_keeps_everyone_for_ipc_compatibility() {
    let cap_a = 0x10usize as *mut c_void;
    let cap_b = 0x20usize as *mut c_void;
    let logon = 0x30usize as *mut c_void;
    let everyone = 0x40usize as *mut c_void;
    let caps = [cap_a, cap_b];

    let dacl_sids = build_default_dacl_sids(&caps, logon, everyone);

    assert_eq!(dacl_sids, vec![logon, everyone, caps[0], caps[1]]);
}
