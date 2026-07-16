use super::build_default_dacl_sids;
use super::build_restricted_sid_entries;
use std::ffi::c_void;

#[test]
fn restricted_sids_exclude_everyone() {
    let cap_a_addr = 0x10usize;
    let cap_b_addr = 0x20usize;
    let extra_addr = 0x30usize;
    let logon_addr = 0x40usize;
    let everyone_addr = 0x50usize;
    let cap_a = std::ptr::without_provenance_mut::<c_void>(cap_a_addr);
    let cap_b = std::ptr::without_provenance_mut::<c_void>(cap_b_addr);
    let extra = std::ptr::without_provenance_mut::<c_void>(extra_addr);
    let logon = std::ptr::without_provenance_mut::<c_void>(logon_addr);
    let everyone = std::ptr::without_provenance_mut::<c_void>(everyone_addr);
    let caps = [cap_a, cap_b];
    let extras = [extra];

    let entries = build_restricted_sid_entries(&caps, &extras, logon);
    let restricted = entries.iter().map(|entry| entry.Sid).collect::<Vec<_>>();

    assert_eq!(restricted, vec![caps[0], caps[1], extras[0], logon]);
    assert!(!restricted.contains(&everyone));
}

#[test]
fn default_dacl_keeps_everyone_for_ipc_compatibility() {
    let cap_a_addr = 0x10usize;
    let cap_b_addr = 0x20usize;
    let logon_addr = 0x30usize;
    let everyone_addr = 0x40usize;
    let cap_a = std::ptr::without_provenance_mut::<c_void>(cap_a_addr);
    let cap_b = std::ptr::without_provenance_mut::<c_void>(cap_b_addr);
    let logon = std::ptr::without_provenance_mut::<c_void>(logon_addr);
    let everyone = std::ptr::without_provenance_mut::<c_void>(everyone_addr);
    let caps = [cap_a, cap_b];

    let dacl_sids = build_default_dacl_sids(&caps, logon, everyone);

    assert_eq!(dacl_sids, vec![logon, everyone, caps[0], caps[1]]);
}
