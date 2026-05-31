struct ComputerUseCallParams;
struct ComputerUseCallResponse;

pub enum AndroidComputerUseOutcome {
    Handled(ComputerUseCallResponse),
    Unavailable,
}

pub async fn handle_android_computer_use(
    _params: &ComputerUseCallParams,
) -> AndroidComputerUseOutcome {
    AndroidComputerUseOutcome::Unavailable
}

fn text_only_response() -> ComputerUseCallResponse {
    ComputerUseCallResponse
}
