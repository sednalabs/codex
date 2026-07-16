use super::WriteAclPolicy;
use super::WriteAclRepairMode;
use super::WriteAclRoot;
use super::descendant_components;
use super::is_same_or_descendant_key;
use super::repair_write_acl_policy;
use crate::acl::dacl_has_write_deny_for_sid;
use crate::acl::fetch_dacl_handle;
use crate::acl::path_mask_allows;
use crate::cap::workspace_write_cap_sid_for_root;
use crate::token::LocalSid;
use pretty_assertions::assert_eq;
use std::collections::HashSet;
use std::path::Path;
use windows_sys::Win32::Foundation::HLOCAL;
use windows_sys::Win32::Foundation::LocalFree;
use windows_sys::Win32::Storage::FileSystem::FILE_GENERIC_WRITE;

#[test]
fn descendant_matching_is_case_insensitive_and_component_bounded() {
    assert!(is_same_or_descendant_key(
        "c:/workspace/readonly/secret.txt",
        "c:/workspace/readonly"
    ));
    assert!(!is_same_or_descendant_key(
        "c:/workspace/readonly-other/allowed.txt",
        "c:/workspace/readonly"
    ));
    assert_eq!(
        descendant_components(
            Path::new(r"C:\Workspace"),
            Path::new(r"c:\workspace\src\main.rs")
        ),
        Some(vec!["src".into(), "main.rs".into()])
    );
}

#[test]
fn deny_policy_includes_nested_root_capabilities() {
    let temp = tempfile::tempdir().expect("tempdir");
    let outer = temp.path().join("outer");
    let denied = outer.join("denied");
    let nested = denied.join("nested");
    std::fs::create_dir_all(&nested).expect("create nested root");
    let outer_sid = "S-1-5-21-1-2-3-1001".to_string();
    let nested_sid = "S-1-5-21-1-2-3-1002".to_string();
    let roots = vec![
        WriteAclRoot {
            path: outer,
            capability_sid: outer_sid.clone(),
        },
        WriteAclRoot {
            path: nested.clone(),
            capability_sid: nested_sid.clone(),
        },
    ];
    let policy = WriteAclPolicy::new(&roots, &[denied], None).expect("build policy");

    let actual = policy
        .deny_sids_for_path(&nested.join("file.txt"))
        .into_iter()
        .map(|sid| sid as usize)
        .collect::<HashSet<_>>();
    let expected = [outer_sid, nested_sid]
        .into_iter()
        .map(|name| policy.sids.get(&name).expect("policy sid").as_ptr() as usize)
        .collect();

    assert_eq!(actual, expected);
}

#[test]
fn repair_coalesces_overlapping_root_sids() {
    let temp = tempfile::tempdir().expect("tempdir");
    let codex_home = temp.path().join("codex-home");
    let outer = temp.path().join("outer");
    let nested = outer.join("nested");
    let existing = nested.join("file.txt");
    std::fs::create_dir_all(&codex_home).expect("create codex home");
    std::fs::create_dir_all(&nested).expect("create nested root");
    std::fs::write(&existing, "existing").expect("write existing file");
    let outer_sid = workspace_write_cap_sid_for_root(&codex_home, &outer, &outer)
        .expect("outer capability sid");
    let nested_sid = workspace_write_cap_sid_for_root(&codex_home, &outer, &nested)
        .expect("nested capability sid");
    let roots = vec![
        WriteAclRoot {
            path: outer,
            capability_sid: outer_sid.clone(),
        },
        WriteAclRoot {
            path: nested,
            capability_sid: nested_sid.clone(),
        },
    ];

    repair_write_acl_policy(&roots, &[], None, WriteAclRepairMode::FullMigration)
        .expect("repair overlapping roots");

    let outer_sid = LocalSid::from_string(&outer_sid).expect("convert outer sid");
    let nested_sid = LocalSid::from_string(&nested_sid).expect("convert nested sid");
    assert_eq!(
        (
            path_mask_allows(&existing, &[outer_sid.as_ptr()], FILE_GENERIC_WRITE, false)
                .expect("outer allow"),
            path_mask_allows(&existing, &[nested_sid.as_ptr()], FILE_GENERIC_WRITE, false)
                .expect("nested allow"),
        ),
        (true, true)
    );
}

#[test]
fn policy_tightening_denies_existing_nested_root() {
    let temp = tempfile::tempdir().expect("tempdir");
    let codex_home = temp.path().join("codex-home");
    let outer = temp.path().join("outer");
    let denied = outer.join("denied");
    let nested = denied.join("nested");
    let existing = nested.join("sealed.txt");
    let allowed = outer.join("allowed.txt");
    std::fs::create_dir_all(&codex_home).expect("create codex home");
    std::fs::create_dir_all(&nested).expect("create nested root");
    std::fs::write(&existing, "sealed").expect("write denied fixture");
    std::fs::write(&allowed, "allowed").expect("write allowed fixture");
    let outer_sid = workspace_write_cap_sid_for_root(&codex_home, &outer, &outer)
        .expect("outer capability sid");
    let nested_sid = workspace_write_cap_sid_for_root(&codex_home, &outer, &nested)
        .expect("nested capability sid");
    let roots = vec![
        WriteAclRoot {
            path: outer,
            capability_sid: outer_sid.clone(),
        },
        WriteAclRoot {
            path: nested,
            capability_sid: nested_sid.clone(),
        },
    ];
    repair_write_acl_policy(&roots, &[], None, WriteAclRepairMode::FullMigration)
        .expect("seed write ACLs");
    repair_write_acl_policy(
        &roots,
        std::slice::from_ref(&denied),
        None,
        WriteAclRepairMode::FullMigration,
    )
    .expect("tighten write policy");

    let outer_sid = LocalSid::from_string(&outer_sid).expect("convert outer sid");
    let nested_sid = LocalSid::from_string(&nested_sid).expect("convert nested sid");
    let denied_sids = [outer_sid.as_ptr(), nested_sid.as_ptr()];
    let (dacl, descriptor) = unsafe { fetch_dacl_handle(&existing) }.expect("fetch denied DACL");
    let denied_state = unsafe {
        (
            dacl_has_write_deny_for_sid(dacl, denied_sids[0]),
            dacl_has_write_deny_for_sid(dacl, denied_sids[1]),
        )
    };
    if !descriptor.is_null() {
        unsafe {
            LocalFree(descriptor as HLOCAL);
        }
    }
    let allowed_state = path_mask_allows(
        &allowed,
        &[outer_sid.as_ptr()],
        FILE_GENERIC_WRITE,
        false,
    )
    .expect("allowed sibling ACL");

    assert_eq!((denied_state, allowed_state), ((true, true), true));
}

#[test]
fn repair_does_not_follow_directory_reparse_points() {
    let temp = tempfile::tempdir().expect("tempdir");
    let codex_home = temp.path().join("codex-home");
    let root = temp.path().join("root");
    let safe = root.join("safe.txt");
    let outside = temp.path().join("outside");
    let link = root.join("outside-link");
    std::fs::create_dir_all(&codex_home).expect("create codex home");
    std::fs::create_dir_all(&root).expect("create root");
    std::fs::create_dir_all(&outside).expect("create outside");
    std::fs::write(&safe, "safe").expect("write safe file");
    std::os::windows::fs::symlink_dir(&outside, &link).expect("create directory symlink");
    let capability_sid = workspace_write_cap_sid_for_root(&codex_home, &root, &root)
        .expect("capability sid");
    let roots = vec![WriteAclRoot {
        path: root,
        capability_sid: capability_sid.clone(),
    }];

    repair_write_acl_policy(&roots, &[], None, WriteAclRepairMode::FullMigration)
        .expect("repair root");

    let capability_sid = LocalSid::from_string(&capability_sid).expect("convert capability sid");
    assert_eq!(
        (
            path_mask_allows(
                &outside,
                &[capability_sid.as_ptr()],
                FILE_GENERIC_WRITE,
                false,
            )
            .expect("outside ACL"),
            path_mask_allows(
                &safe,
                &[capability_sid.as_ptr()],
                FILE_GENERIC_WRITE,
                false,
            )
            .expect("safe ACL"),
        ),
        (false, true)
    );
}

#[test]
fn deny_subtrees_reject_reparse_points() {
    let temp = tempfile::tempdir().expect("tempdir");
    let codex_home = temp.path().join("codex-home");
    let root = temp.path().join("root");
    let denied = root.join("denied");
    let outside = temp.path().join("outside");
    std::fs::create_dir_all(&codex_home).expect("create codex home");
    std::fs::create_dir_all(&root).expect("create root");
    std::fs::create_dir_all(&outside).expect("create outside");
    std::os::windows::fs::symlink_dir(&outside, &denied).expect("create directory symlink");
    let capability_sid = workspace_write_cap_sid_for_root(&codex_home, &root, &root)
        .expect("capability sid");
    let roots = vec![WriteAclRoot {
        path: root,
        capability_sid,
    }];

    let error = repair_write_acl_policy(
        &roots,
        &[denied],
        None,
        WriteAclRepairMode::FullMigration,
    )
    .expect_err("deny reparse point must fail closed");

    assert!(error.to_string().contains("reparse point"));
}

#[test]
fn allow_migration_rejects_files_hardlinked_outside_the_root() {
    let temp = tempfile::tempdir().expect("tempdir");
    let codex_home = temp.path().join("codex-home");
    let root = temp.path().join("root");
    let outside = temp.path().join("outside.txt");
    let inside = root.join("inside.txt");
    std::fs::create_dir_all(&codex_home).expect("create codex home");
    std::fs::create_dir_all(&root).expect("create root");
    std::fs::write(&outside, "shared").expect("write outside file");
    std::fs::hard_link(&outside, &inside).expect("create hard link");
    let capability_sid = workspace_write_cap_sid_for_root(&codex_home, &root, &root)
        .expect("capability sid");
    let roots = vec![WriteAclRoot {
        path: root,
        capability_sid,
    }];

    let error = repair_write_acl_policy(&roots, &[], None, WriteAclRepairMode::FullMigration)
        .expect_err("hardlinked file must fail closed");

    assert!(error.to_string().contains("multiply linked file"));
}
