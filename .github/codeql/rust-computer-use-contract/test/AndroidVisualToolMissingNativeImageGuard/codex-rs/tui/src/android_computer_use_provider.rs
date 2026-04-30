struct ComputerUseCallResponse {
    success: bool,
}

type ProviderResult = Result<ComputerUseCallResponse, String>;

fn build_response() -> ComputerUseCallResponse {
    ComputerUseCallResponse { success: true }
}

fn fallible() -> ProviderResult {
    Ok(build_response())
}

fn require_native_image_for_visual_response(
    _response: &mut ComputerUseCallResponse,
    _message: &str,
) {
}

fn observe(early_success: bool) -> ProviderResult {
    let mut response = build_response();
    if early_success {
        return Ok(response);
    }

    if !response.success {
        return Err("observation failed".to_string());
    }

    let _ = fallible()?;
    require_native_image_for_visual_response(&mut response, "must include image");
    Ok(response)
}

fn step() -> ProviderResult {
    let mut response_a = build_response();
    let response_b = build_response();

    let _ = fallible()?;
    require_native_image_for_visual_response(&mut response_a, "must include image");

    // Known future data-flow target: the dominance guard sees a protected path,
    // but does not yet prove the returned response object is response_a.
    Ok(response_b)
}

fn install_build_from_run() -> ProviderResult {
    let mut response = build_response();
    let _ = fallible()?;
    require_native_image_for_visual_response(&mut response, "must include image");
    Ok(response)
}
