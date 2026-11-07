use super::*;
use crate::agent::goal_notifications::GoalNotificationBinding;
use crate::agent::goal_notifications::GoalNotificationProjection;

fn exact_goal(status: ThreadGoalStatus) -> (GoalNotificationStore, GoalNotificationTurnToken) {
    let store = GoalNotificationStore::default();
    let binding = GoalNotificationBinding {
        parent_thread_id: ThreadId::new(),
        child_thread_id: ThreadId::new(),
        goal_id: "goal".to_string(),
        control_generation: 7,
    };
    let mut projection = GoalNotificationProjection::new(binding.clone(), status);
    projection.opt_in(&binding);
    store.install_authoritative(projection);
    store.set_source_turn("turn");
    let token = GoalNotificationTurnToken {
        incarnation: store.incarnation(),
        binding,
        source_turn_id: "turn".to_string(),
        generation: 7,
    };
    (store, token)
}

#[test]
fn ordinary_completion_has_no_goal_continuation() {
    assert_eq!(terminal_continuation(&AgentStatus::Completed(None), "turn", None, None).unwrap(),
        (Outcome::Completed, Continuation::None));
}

#[test]
fn active_goal_completion_keeps_exact_binding_pending() {
    let (store, token) = exact_goal(ThreadGoalStatus::Active);
    let (outcome, continuation) = terminal_continuation(&AgentStatus::Completed(None), "turn", Some(&token), Some(&store)).unwrap();
    assert_eq!(outcome, Outcome::Completed);
    let Continuation::PendingGoalBinding(binding) = continuation else { panic!("expected exact pending binding"); };
    assert_eq!(binding.incarnation, Some(store.incarnation()));
    assert_eq!(binding.control_generation, 7);
}

#[test]
fn stale_source_turn_cannot_claim_current_admissions() {
    let (store, token) = exact_goal(ThreadGoalStatus::Active);
    assert!(terminal_continuation(&AgentStatus::Completed(None), "replacement", Some(&token), Some(&store)).is_err());
}

#[test]
fn abort_and_error_do_not_leave_active_goal_work_pending() {
    let (store, token) = exact_goal(ThreadGoalStatus::Active);
    for status in [AgentStatus::Interrupted, AgentStatus::Errored("error".to_string())] {
        let (_, continuation) = terminal_continuation(&status, "turn", Some(&token), Some(&store)).unwrap();
        assert!(matches!(continuation, Continuation::Terminal(_)));
    }
}

#[test]
fn private_sessions_are_not_durable_recipients() {
    assert!(public_recipient(&SessionSource::Cli));
    assert!(!public_recipient(&SessionSource::SubAgent(SubAgentSource::Review)));
}
