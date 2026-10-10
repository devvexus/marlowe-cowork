//! The scripted stdio MCP server the tests start (feature `fake-server`).
//!
//! Flags: `--extra-tools N`, `--list-changed`, `--stderr-note`, `--exit-after-init`, `--protocol V`, `--desc-file PATH`.

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let value = |flag: &str| args.iter().position(|a| a == flag).and_then(|i| args.get(i + 1)).cloned();
    let opts = marlowe_mcp::fake::FakeOpts {
        extra_tools: value("--extra-tools").and_then(|v| v.parse().ok()).unwrap_or(0),
        protocol: value("--protocol"),
        list_changed: args.iter().any(|a| a == "--list-changed"),
        echo_description: value("--desc-file").and_then(|p| std::fs::read_to_string(p).ok()),
    };
    marlowe_mcp::fake::run_stdio(opts, &args);
}
