//! Exercises retained review history through compaction, eviction, and rollback.

use anyhow::Result;
use codex_core::config::Constrained;
use codex_features::Feature;
use codex_protocol::config_types::ApprovalsReviewer;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::Op;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_function_call;
use core_test_support::responses::mount_sse_sequence;
use core_test_support::responses::sse;
use core_test_support::responses::start_mock_server;
use core_test_support::skip_if_no_network;
use core_test_support::skip_if_wine_exec;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use pretty_assertions::assert_eq;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn guardian_history_survives_compaction_and_eviction_but_not_rollback() -> Result<()> {
    skip_if_no_network!(Ok(()));
    skip_if_wine_exec!(
        Ok(()),
        "Guardian approval actions require host-native paths"
    );
    let server = start_mock_server().await;
    let test = test_codex()
        .with_config(|config| {
            config.features.enable(Feature::TokenBudget).unwrap();
            config.update_plan_enabled = true;
            config.permissions.approval_policy = Constrained::allow_any(AskForApproval::OnRequest);
            config.approvals_reviewer = ApprovalsReviewer::AutoReview;
        })
        .build_with_auto_env(&server)
        .await?;
    // Enough real tool traffic to evict earlier tools when retention starts.
    let plan = r#"{"plan":[{"step":"verify repository visibility","status":"completed"}]}"#;
    let mut inspection: Vec<_> = (0..130)
        .map(|index| ev_function_call(&format!("inspect-{index}"), "update_plan", plan))
        .collect();
    inspection.push(ev_completed("inspection"));
    mount_sse_sequence(
        &server,
        vec![
            sse(inspection),
            sse(vec![
                ev_function_call("inspect-latest", "update_plan", plan),
                ev_completed("latest-inspection"),
            ]),
            sse(vec![
                ev_assistant_message("inspected", "Inspection complete."),
                ev_completed("inspection-done"),
            ]),
        ],
    )
    .await;
    test.submit_text_turn("Only publish to a private repository.")
        .await?;
    test.codex.submit(Op::Compact).await?;
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;

    let command = r#"{"cmd":"echo publish","sandbox_permissions":"require_escalated","justification":"Publish the inspected change."}"#;
    for (prompt, retained) in [
        ("Now publish.", true),
        ("Inspect a different repository.", false),
    ] {
        let review = mount_sse_sequence(
            &server,
            vec![
                sse(vec![
                    ev_function_call("publish", "exec_command", command),
                    ev_completed("publish"),
                ]),
                sse(vec![
                    ev_assistant_message("review", r#"{"outcome":"deny"}"#),
                    ev_completed("review"),
                ]),
                sse(vec![ev_completed("publish-done")]),
            ],
        )
        .await;
        test.submit_text_turn(prompt).await?;
        let requests = review.requests();
        let guardian = requests
            .iter()
            .find(|request| {
                request.body_json()["client_metadata"]["x-openai-subagent"] == "guardian"
            })
            .expect("Guardian request");
        let transcript = serde_json::to_string(&guardian.input())?;
        assert!(transcript.contains(prompt));
        if retained {
            let positions = [
                "Only publish to a private repository.",
                "tool update_plan call",
                "tool update_plan result",
                "Now publish.",
            ]
            .map(|text| {
                transcript
                    .find(text)
                    .unwrap_or_else(|| panic!("missing {text}: {transcript}"))
            });
            let mut ordered = positions;
            ordered.sort();
            assert_eq!(positions, ordered);
            assert!(
                requests[0]
                    .input()
                    .iter()
                    .all(|item| item["call_id"] != "inspect-0")
            );
            test.codex.ensure_rollout_materialized().await;
            test.codex
                .submit(Op::ThreadRollback { num_turns: 2 })
                .await?;
            wait_for_event(&test.codex, |event| {
                matches!(event, EventMsg::ThreadRolledBack(_))
            })
            .await;
        } else {
            assert!(!transcript.contains("Only publish to a private repository."));
            assert!(!transcript.contains("tool update_plan call"));
            assert!(!transcript.contains("tool update_plan result"));
        }
    }
    Ok(())
}
