struct ComputerUseCallResponse;

async fn observation_response() -> ComputerUseCallResponse {
    ComputerUseCallResponse
}

async fn screenshot_fallback_response() -> ComputerUseCallResponse {
    observation_response().await
}
