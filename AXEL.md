---
workspace: axel-next
skills:
  - ./skills
  - ~/.config/axel/skills
---

# Axel v2

Terminal-first workbench for coding agents, built on Rex. Rust workspace, edition 2024.

- `docs/DESIGN.md` is the source of truth for concepts and the launch flow.
- `docs/CONTRACTS.md` fixes the public API of every crate; change shape only there first.
- `docs/RESEARCH.md` records what we learned from Rex, t3code, Buzz and Axel v1.
- Build offline: `cargo build --offline`; test: `cargo test --offline`.
- Never drive agents by typing into their terminals. Status comes from hooks, ACP or OSC 7501.
