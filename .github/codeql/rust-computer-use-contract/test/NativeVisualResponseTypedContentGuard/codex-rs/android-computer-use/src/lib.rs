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

mod helper_erases_detail {
    use super::*;

    fn append_text(items: &mut Vec<String>, message: &str) {
        items.insert(0, message.to_string());
        items.clear();
    }

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

mod helper_replaces_detail {
    use super::*;

    fn append_text(items: &mut Vec<String>, message: &str) {
        items.insert(0, message.to_string());
        *items = Vec::new();
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

mod helper_calls_unknown_mutator {
    use super::*;

    fn erase(items: &mut Vec<String>) {
        items.clear();
    }

    fn append_text(items: &mut Vec<String>, message: &str) {
        items.insert(0, message.to_string());
        erase(items);
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

mod typed_content_helper {
    enum Content {
        InputText { text: String },
        InputImage,
    }

    struct Response {
        content_items: Vec<Content>,
        success: bool,
    }

    fn append_text(items: &mut Vec<Content>, extra: &str) {
        if let Some(Content::InputText { text }) = items.first_mut() {
            text.push_str(extra);
        } else {
            items.insert(
                0,
                Content::InputText {
                    text: extra.trim().to_string(),
                },
            );
        }
    }

    fn require_native_image_for_visual_response(response: &mut Response, message: &str) {
        if !response
            .content_items
            .iter()
            .any(|item| matches!(item, Content::InputImage))
        {
            append_text(&mut response.content_items, message);
            response.success = false;
        }
    }

    fn observe() -> Result<Response, String> {
        let mut response = Response {
            content_items: Vec::new(),
            success: true,
        };
        require_native_image_for_visual_response(&mut response, "missing image");
        Ok(response)
    }
}
