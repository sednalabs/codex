enum ComputerUseCallOutputContentItem {
    InputText { text: String },
    InputImage { image_url: String },
}

struct CoreComputerUseOutputContentItem;

fn from(item: ComputerUseCallOutputContentItem) -> CoreComputerUseOutputContentItem {
    match item {
        ComputerUseCallOutputContentItem::InputText { text: _ } => {
            CoreComputerUseOutputContentItem
        }
        _ => CoreComputerUseOutputContentItem,
    }
}
