//! `axel claude-acp` as a standalone binary, for tests.

fn main() -> anyhow::Result<()> {
    axel_acp::claude::serve_stdio()
}
