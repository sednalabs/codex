struct Client;

impl Client {
    async fn call_tool(&mut self, _tool: &str, _args: ()) {}
}

async fn inspect_ui(client: &mut Client) {
    let inspect_args = ();
    client.call_tool("android.inspect_ui", inspect_args).await;
}
