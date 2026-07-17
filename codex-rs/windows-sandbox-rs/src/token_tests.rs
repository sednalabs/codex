use super::build_default_dacl_sids;
use super::build_restricted_sid_entries;
use std::ffi::c_void;

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
