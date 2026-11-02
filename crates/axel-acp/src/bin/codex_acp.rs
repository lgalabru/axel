//! `axel codex-acp` as a standalone binary, for tests.

fn main() -> anyhow::Result<()> {
    axel_acp::codex::serve_stdio()
}
