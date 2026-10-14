struct Response {
    success: bool,
    content_items: Vec<String>,
}

type ProviderResult = Result<Response, String>;

fn build_response() -> Response {
    Response {
        success: true,
        content_items: Vec::new(),
    }
}

fn append_text(items: &mut Vec<String>, message: &str) {
    items.insert(0, message.to_string());
}

fn require_native_image_for_visual_response(response: &mut Response, message: &str) {
    if response.content_items.is_empty() {
        append_text(&mut response.content_items, message);
        response.success = false;
    }
}

fn observe() -> ProviderResult {
    let mut response = build_response();
    require_native_image_for_visual_response(&mut response, "missing image");
    Ok(response)
}

mod helper_loses_detail {
    use super::*;

    fn append_text(_items: &mut Vec<String>, _message: &str) {}

    fn require_native_image_for_visual_response(response: &mut Response, message: &str) {
        if response.content_items.is_empty() {
            append_text(&mut response.content_items, message);
            response.success = false;
        }
    }

    fn step() -> ProviderResult {
        let mut response = build_response();
        require_native_image_for_visual_response(&mut response, "missing image");
        Ok(response)
    }
}

mod helper_discards_message {
    use super::*;

    fn append_text(items: &mut Vec<String>, _message: &str) {
        items.insert(0, String::new());
    }

    fn require_native_image_for_visual_response(response: &mut Response, message: &str) {
        if response.content_items.is_empty() {
            append_text(&mut response.content_items, message);
            response.success = false;
        }
    }

    fn install_build_from_run() -> ProviderResult {
        let mut response = build_response();
        require_native_image_for_visual_response(&mut response, "missing image");
        Ok(response)
    }
}

mod helper_skips_detail {
    use super::*;

    fn append_text(items: &mut Vec<String>, message: &str) {
        if message.is_empty() {
            items.insert(0, message.to_string());
        }
    }

    fn require_native_image_for_visual_response(response: &mut Response, message: &str) {
        if response.content_items.is_empty() {
            append_text(&mut response.content_items, message);
            response.success = false;
        }
    }

    fn observe() -> ProviderResult {
        let mut response = build_response();
        require_native_image_for_visual_response(&mut response, "missing image");
        Ok(response)
    }
}
