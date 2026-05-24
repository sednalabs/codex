use std::borrow::Cow;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ComputerUseDisplayState {
    InProgress,
    Completed,
    Failed,
}

pub(crate) fn computer_use_action_label(adapter: &str, state: ComputerUseDisplayState) -> String {
    let surface = computer_use_surface_name(adapter);
    match state {
        ComputerUseDisplayState::InProgress => format!("Using {surface}"),
        ComputerUseDisplayState::Completed => format!("Used {surface}"),
        ComputerUseDisplayState::Failed => format!("Failed using {surface}"),
    }
}

pub(crate) fn computer_use_surface_name(adapter: &str) -> Cow<'_, str> {
    match adapter {
        "android" | "android_emulator" | "android-emulator" => Cow::Borrowed("Android emulator"),
        "browser" => Cow::Borrowed("browser"),
        "desktop" | "computer" => Cow::Borrowed("computer"),
        other => Cow::Owned(other.replace(['_', '-'], " ")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn action_labels_match_native_surfaces() {
        assert_eq!(
            computer_use_action_label("browser", ComputerUseDisplayState::Completed),
            "Used browser"
        );
        assert_eq!(
            computer_use_action_label("desktop", ComputerUseDisplayState::Completed),
            "Used computer"
        );
        assert_eq!(
            computer_use_action_label("android", ComputerUseDisplayState::Completed),
            "Used Android emulator"
        );
        assert_eq!(
            computer_use_action_label("android_emulator", ComputerUseDisplayState::InProgress),
            "Using Android emulator"
        );
    }
}
