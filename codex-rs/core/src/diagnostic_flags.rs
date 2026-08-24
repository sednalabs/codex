const GOAL_OWNER_CONTINUITY_ENV: &str = "CODEX_EXPERIMENTAL_GOAL_OWNER_CONTINUITY";
const GOAL_CONTINUATION_HEALTH_CHECK_ENV: &str =
    "CODEX_EXPERIMENTAL_GOAL_CONTINUATION_HEALTH_CHECK";

pub fn goal_owner_continuity_enabled() -> bool {
    env_enabled(GOAL_OWNER_CONTINUITY_ENV)
}

pub fn goal_continuation_health_check_enabled() -> bool {
    env_enabled(GOAL_CONTINUATION_HEALTH_CHECK_ENV)
}

fn env_enabled(name: &str) -> bool {
    std::env::var(name)
        .ok()
        .is_some_and(|value| is_truthy(value.as_str()))
}

fn is_truthy(value: &str) -> bool {
    let value = value.trim();
    value == "1"
        || value.eq_ignore_ascii_case("true")
        || value.eq_ignore_ascii_case("yes")
        || value.eq_ignore_ascii_case("on")
}

#[cfg(test)]
mod tests {
    use super::is_truthy;

    #[test]
    fn parses_truthy_values() {
        for value in ["1", "true", "TRUE", "yes", "On", " on "] {
            assert!(is_truthy(value), "expected {value:?} to be truthy");
        }
    }

    #[test]
    fn rejects_other_values() {
        for value in ["", "0", "false", "off", "no", "anything"] {
            assert!(!is_truthy(value), "expected {value:?} to be falsey");
        }
    }
}
