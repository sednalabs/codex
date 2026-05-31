enum ToolHandlerKind {
    ComputerUse,
    DynamicTool,
}

fn canonical_android_dynamic_tool(_tool: &str) -> Option<String> {
    Some("android_observe".to_string())
}

struct Plan;

impl Plan {
    fn register_handler(&mut self, _name: String, _kind: ToolHandlerKind) {}
}

fn bad(tool: &str, plan: &mut Plan) {
    if let Some(converted_tool) = canonical_android_dynamic_tool(tool) {
        plan.register_handler(converted_tool, ToolHandlerKind::DynamicTool);
    }
}

fn good(tool: &str, plan: &mut Plan) {
    if let Some(converted_tool) = canonical_android_dynamic_tool(tool) {
        plan.register_handler(converted_tool, ToolHandlerKind::ComputerUse);
    }
}
