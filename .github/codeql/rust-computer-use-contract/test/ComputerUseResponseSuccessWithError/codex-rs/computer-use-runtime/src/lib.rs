struct ComputerUseCallResponse {
    content: Vec<String>,
    success: bool,
    error: Option<String>,
}

fn successful_response() -> ComputerUseCallResponse {
    ComputerUseCallResponse {
        content: vec!["ok".to_string()],
        success: true,
        error: None,
    }
}

fn failed_response() -> ComputerUseCallResponse {
    ComputerUseCallResponse {
        content: Vec::new(),
        success: false,
        error: Some("android.inspect_ui failed".to_string()),
    }
}

fn contradictory_response() -> ComputerUseCallResponse {
    ComputerUseCallResponse {
        content: vec!["partial".to_string()],
        success: true,
        error: Some("android.inspect_ui failed".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pre_guard_test_response_is_ignored() -> ComputerUseCallResponse {
        ComputerUseCallResponse {
            content: vec!["test".to_string()],
            success: true,
            error: Some("android.inspect_ui failed".to_string()),
        }
    }
}
