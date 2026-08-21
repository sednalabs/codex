const GOAL_ERROR_CONTINUATION_ENV: &str = "CODEX_EXPERIMENTAL_GOAL_ERROR_CONTINUATION";

pub fn goal_error_continuation_enabled() -> bool {
    std::env::var(GOAL_ERROR_CONTINUATION_ENV)
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
