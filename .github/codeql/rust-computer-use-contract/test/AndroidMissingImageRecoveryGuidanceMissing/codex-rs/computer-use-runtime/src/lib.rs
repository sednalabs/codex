struct ComputerUseCallResponse;

fn require_native_image_for_visual_response(
    _response: &mut ComputerUseCallResponse,
    _missing_image_message: &str,
) {
}

async fn observe() -> Result<ComputerUseCallResponse, String> {
    let mut response = ComputerUseCallResponse;
    require_native_image_for_visual_response(
        &mut response,
        "Android observation missing native image output.",
    );
    Ok(response)
}
