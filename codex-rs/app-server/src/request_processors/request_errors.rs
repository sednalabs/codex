use super::*;

pub(super) fn environment_selection_error(err: CodexErr) -> JSONRPCErrorError {
    match err {
        CodexErr::InvalidRequest(message) => invalid_request(message),
        err => internal_error(format!("failed to validate environment selections: {err}")),
    }
}

pub(super) fn environment_selection_error_message(err: CodexErr) -> String {
    match err {
        CodexErr::InvalidRequest(message) => message,
        err => err.to_string(),
    }
}
