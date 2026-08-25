use super::*;
use crate::config::PermissionProfileState;
use crate::session::session::SessionConfiguration;
use crate::session::session::SessionSettingsUpdate;
use crate::session::tests::make_session_configuration_for_tests;
use codex_config::ConfigLayerStack;
use codex_config::RequirementSource;
use codex_config::Sourced;
use codex_features::Feature;
use codex_protocol::models::PermissionProfile;
use codex_protocol::permissions::FileSystemAccessMode;
use codex_protocol::permissions::FileSystemPath;
use codex_protocol::permissions::FileSystemSandboxEntry;
use codex_protocol::permissions::FileSystemSandboxKind;
use codex_protocol::permissions::FileSystemSandboxPolicy;
use codex_protocol::permissions::FileSystemSpecialPath;
use codex_protocol::permissions::NetworkSandboxPolicy;
use codex_protocol::protocol::TurnEnvironmentSelections;
use core_test_support::test_codex::local;
use pretty_assertions::assert_eq;
use std::collections::BTreeSet;
use std::sync::Arc;

fn set_requirements(configuration: &mut SessionConfiguration, requirements: ConfigRequirements) {
    let config = Arc::make_mut(&mut configuration.original_config_do_not_use);
    config.config_layer_stack = ConfigLayerStack::new(
        config
            .config_layer_stack
            .all_layers_low_to_high()
            .cloned()
            .collect(),
        requirements,
        config.config_layer_stack.requirements_toml().clone(),
    )
    .expect("replace test requirements");
}

#[tokio::test]
async fn proposed_permission_profile_is_checked_before_step_settings() {
    let mut configuration = make_session_configuration_for_tests().await;
    let permission = Constrained::allow_only(PermissionProfile::read_only());
    let permission_error = permission
        .can_set(&PermissionProfile::Disabled)
        .unwrap_err();
    configuration.permission_profile_state =
        PermissionProfileState::from_constrained_legacy(permission).unwrap();
    let approval = Constrained::allow_only(AskForApproval::OnRequest);
    let approval_error = approval.can_set(&AskForApproval::Never).unwrap_err();
    Arc::make_mut(&mut configuration.step_settings).approval_policy = approval;
    let mut requirements = configuration
        .original_config_do_not_use
        .config_layer_stack
        .requirements()
        .clone();
    requirements.approvals_reviewer.value = Constrained::allow_only(ApprovalsReviewer::User);
    let reviewer_error = requirements
        .approvals_reviewer
        .can_set(&ApprovalsReviewer::AutoReview)
        .unwrap_err();
    requirements.auto_review_required_models = Some(Sourced::new(
        BTreeSet::from(["protected-model".to_string()]),
        RequirementSource::Unknown,
    ));
    set_requirements(&mut configuration, requirements);
    let protected_mode = configuration.step_settings.collaboration_mode.with_updates(
        Some("protected-model".to_string()),
        /*effort*/ None,
        /*developer_instructions*/ None,
    );

    let invalid_profile = SessionSettingsUpdate {
        permission_profile: Some(PermissionProfile::Disabled),
        ..Default::default()
    };
    assert_eq!(
        configuration.apply(&invalid_profile, &[]).err().as_ref(),
        Some(&permission_error)
    );
    for (step_settings, expected) in [
        (
            StepSettingsUpdate {
                approval_policy: Some(AskForApproval::Never),
                ..Default::default()
            },
            &approval_error,
        ),
        (
            StepSettingsUpdate {
                approvals_reviewer: Some(ApprovalsReviewer::AutoReview),
                ..Default::default()
            },
            &reviewer_error,
        ),
        (
            StepSettingsUpdate {
                collaboration_mode: Some(protected_mode),
                ..Default::default()
            },
            &reviewer_error,
        ),
    ] {
        assert_eq!(
            configuration
                .step_settings
                .apply(
                    &step_settings,
                    &configuration.step_settings_constraints(&[]),
                )
                .err()
                .as_ref(),
            Some(expected),
        );
        assert_eq!(
            configuration
                .apply(
                    &SessionSettingsUpdate {
                        step_settings,
                        ..invalid_profile.clone()
                    },
                    &[]
                )
                .err()
                .as_ref(),
            Some(&permission_error),
        );
    }
}

#[tokio::test]
async fn model_review_requirement_uses_the_proposed_permission_profile() {
    let mut configuration = make_session_configuration_for_tests().await;
    configuration.permission_profile_state = PermissionProfileState::from_constrained_legacy(
        Constrained::allow_any(PermissionProfile::Disabled),
    )
    .unwrap();
    Arc::make_mut(&mut configuration.step_settings).approvals_reviewer = ApprovalsReviewer::User;
    Arc::make_mut(&mut configuration.original_config_do_not_use)
        .features
        .enable(Feature::GuardianApproval)
        .unwrap();
    let mut requirements = configuration
        .original_config_do_not_use
        .config_layer_stack
        .requirements()
        .clone();
    requirements.auto_review_required_models = Some(Sourced::new(
        BTreeSet::from(["protected-model".to_string()]),
        RequirementSource::Unknown,
    ));
    set_requirements(&mut configuration, requirements);
    let collaboration_mode = configuration.step_settings.collaboration_mode.with_updates(
        Some("protected-model".to_string()),
        /*effort*/ None,
        /*developer_instructions*/ None,
    );
    let updates = SessionSettingsUpdate {
        step_settings: StepSettingsUpdate {
            collaboration_mode: Some(collaboration_mode.clone()),
            ..Default::default()
        },
        ..Default::default()
    };
    assert_eq!(
        configuration.apply(&updates, &[]).err(),
        Some(ConstraintError::AutoReviewRequired {
            model: "protected-model".to_string()
        }),
    );
    let updated = configuration
        .apply(
            &SessionSettingsUpdate {
                permission_profile: Some(PermissionProfile::read_only()),
                ..updates
            },
            &[],
        )
        .expect("the proposed restricted profile permits the protected model");
    assert_eq!(
        updated.step_settings.as_ref(),
        &StepSettings {
            collaboration_mode,
            approvals_reviewer: ApprovalsReviewer::AutoReview,
            ..configuration.step_settings.as_ref().clone()
        }
    );
    assert_eq!(updated.permission_profile(), PermissionProfile::read_only());
    assert_eq!(
        updated
            .apply(
                &SessionSettingsUpdate {
                    permission_profile: Some(PermissionProfile::Disabled),
                    ..Default::default()
                },
                &[],
            )
            .err(),
        Some(ConstraintError::AutoReviewRequired {
            model: "protected-model".to_string(),
        }),
    );
}

#[tokio::test]
async fn environment_only_update_revalidates_existing_step_settings() {
    let mut configuration = make_session_configuration_for_tests().await;
    let profile = PermissionProfile::from_runtime_permissions(
        &FileSystemSandboxPolicy {
            kind: FileSystemSandboxKind::Restricted,
            glob_scan_max_depth: None,
            entries: vec![
                FileSystemSandboxEntry::new(
                    FileSystemPath::Special {
                        value: FileSystemSpecialPath::Root,
                    },
                    FileSystemAccessMode::Write,
                ),
                FileSystemSandboxEntry::new(
                    FileSystemPath::Special {
                        value: FileSystemSpecialPath::project_roots(/*subpath*/ None),
                    },
                    FileSystemAccessMode::Read,
                ),
            ],
        },
        NetworkSandboxPolicy::Restricted,
    );
    configuration.permission_profile_state =
        PermissionProfileState::from_constrained_legacy(Constrained::allow_any(profile)).unwrap();
    let settings = Arc::make_mut(&mut configuration.step_settings);
    settings.collaboration_mode = settings.collaboration_mode.with_updates(
        Some("protected-model".to_string()),
        /*effort*/ None,
        /*developer_instructions*/ None,
    );
    settings.approvals_reviewer = ApprovalsReviewer::AutoReview;
    Arc::make_mut(&mut configuration.original_config_do_not_use)
        .features
        .enable(Feature::GuardianApproval)
        .unwrap();
    let mut requirements = configuration
        .original_config_do_not_use
        .config_layer_stack
        .requirements()
        .clone();
    requirements.auto_review_required_models = Some(Sourced::new(
        BTreeSet::from(["protected-model".to_string()]),
        RequirementSource::Unknown,
    ));
    set_requirements(&mut configuration, requirements);

    let environments = vec![local(configuration.cwd().clone())];
    assert_eq!(configuration.validate(&environments), Ok(()));
    // Removing the selected workspace also removes its read-only carveout,
    // leaving this profile with full-disk write access.
    assert_eq!(
        configuration
            .apply(
                &SessionSettingsUpdate {
                    environments: Some(TurnEnvironmentSelections::new(
                        configuration.cwd().clone(),
                        Vec::new(),
                    )),
                    ..Default::default()
                },
                &environments,
            )
            .err(),
        Some(ConstraintError::AutoReviewRequired {
            model: "protected-model".to_string(),
        }),
    );
}
