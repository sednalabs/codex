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

    // The query tracks direct local-variable identity: guarding response_a must
    // not satisfy a successful exit that returns response_b.
    Ok(response_b)
}

fn install_build_from_run() -> ProviderResult {
    let mut response = build_response();
    let _ = fallible()?;
    require_native_image_for_visual_response(&mut response, "must include image");
    Ok(response)
}

mod non_variable_success_exit {
    use super::*;

    fn observe() -> ProviderResult {
        let mut response = build_response();
        require_native_image_for_visual_response(&mut response, "must include image");

        // Non-variable success exits are reported because the query cannot prove
        // this fresh response is the same guarded response.
        Ok(build_response())
    }
}
