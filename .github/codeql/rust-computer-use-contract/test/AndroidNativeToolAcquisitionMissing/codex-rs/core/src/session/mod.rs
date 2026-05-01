struct SessionConfiguration {
    dynamic_tools: Vec<String>,
}

fn bad_startup(dynamic_tools: Vec<String>) -> SessionConfiguration {
    SessionConfiguration { dynamic_tools }
}

fn augment_with_acquired_native_android_tools(dynamic_tools: Vec<String>) -> Vec<String> {
    dynamic_tools
}

fn good_startup(dynamic_tools: Vec<String>) -> SessionConfiguration {
    let dynamic_tools = augment_with_acquired_native_android_tools(dynamic_tools);
    SessionConfiguration { dynamic_tools }
}
