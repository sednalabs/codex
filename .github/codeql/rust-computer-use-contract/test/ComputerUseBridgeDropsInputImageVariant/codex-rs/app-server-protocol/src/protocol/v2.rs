enum ComputerUseCallOutputContentItem {
    InputText { text: String },
    InputImage { image_url: String },
}

enum CoreComputerUseOutputContentItem {
    InputText { text: String },
}

fn bad(item: ComputerUseCallOutputContentItem) -> CoreComputerUseOutputContentItem {
    match item {
        ComputerUseCallOutputContentItem::InputText { text } => {
            CoreComputerUseOutputContentItem::InputText { text }
        }
        _ => CoreComputerUseOutputContentItem::InputText {
            text: String::new(),
        },
    }
}
