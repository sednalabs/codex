enum ComputerUseOutputContentItem {
    InputText { text: String },
    InputImage { image_url: String },
    Other,
}

enum FunctionCallOutputContentItem {
    InputText { text: String },
    InputImage { image_url: String },
    Other,
}

fn drops_native_image_smell(
    item: ComputerUseOutputContentItem,
) -> FunctionCallOutputContentItem {
    match item {
        ComputerUseOutputContentItem::InputText { text } => {
            FunctionCallOutputContentItem::InputText { text }
        }
        _ => FunctionCallOutputContentItem::Other,
    }
}

fn preserves_native_image(
    item: ComputerUseOutputContentItem,
) -> FunctionCallOutputContentItem {
    match item {
        ComputerUseOutputContentItem::InputText { text } => {
            FunctionCallOutputContentItem::InputText { text }
        }
        ComputerUseOutputContentItem::InputImage { image_url } => {
            FunctionCallOutputContentItem::InputImage { image_url }
        }
        _ => FunctionCallOutputContentItem::Other,
    }
}

fn preserves_imported_native_image(
    item: ComputerUseOutputContentItem,
) -> FunctionCallOutputContentItem {
    use ComputerUseOutputContentItem::*;
    match item {
        InputText { text } => FunctionCallOutputContentItem::InputText { text },
        InputImage { image_url } => FunctionCallOutputContentItem::InputImage { image_url },
        _ => FunctionCallOutputContentItem::Other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inline_test_smell_is_ignored(
        item: ComputerUseOutputContentItem,
    ) -> FunctionCallOutputContentItem {
        match item {
            ComputerUseOutputContentItem::InputText { text } => {
                FunctionCallOutputContentItem::InputText { text }
            }
            _ => FunctionCallOutputContentItem::Other,
        }
    }
}
