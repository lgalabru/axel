# Extra syntaxes

The languages syntect's bundled set predates, for highlighting in the tool
calls pane. `build.rs` compiles them with syntect's own set into one dump that
is embedded in the binary, so they cost nothing to load and add no runtime
dependency.

| File | Source | License |
|---|---|---|
| `Swift.sublime-syntax` | [quiqueg/Swift-Sublime-Package](https://github.com/quiqueg/Swift-Sublime-Package), as converted by [bat](https://github.com/sharkdp/bat) | MIT |
| `TypeScript.sublime-syntax`, `TypeScriptReact.sublime-syntax` | [Microsoft/TypeScript-Sublime-Plugin](https://github.com/Microsoft/TypeScript-Sublime-Plugin), as converted by bat | Apache-2.0 |
| `TOML.sublime-syntax` | [jasonwilliams/sublime_toml_highlighting](https://github.com/jasonwilliams/sublime_toml_highlighting) | MIT |
| `Kotlin.sublime-syntax` | [guille/sublime-kotlin](https://github.com/guille/sublime-kotlin) | Public domain (Unlicense) |
| `Dockerfile.sublime-syntax` | [asbjornenge/Docker.tmbundle](https://github.com/asbjornenge/Docker.tmbundle) | MIT |
| `Zig.sublime-syntax` | [ziglang/sublime-zig-language](https://codeberg.org/ziglang/sublime-zig-language) | MIT |
| `Protobuf.sublime-syntax`, `ProtobufText.sublime-syntax` | [VcamX/protobuf-syntax-highlighting](https://github.com/VcamX/protobuf-syntax-highlighting) | MIT |
| `GraphQL.sublime-syntax` | [dncrews/GraphQL-SublimeText3](https://github.com/dncrews/GraphQL-SublimeText3) | WTFPL |
| `Terraform.sublime-syntax` | [alexlouden/Terraform.tmLanguage](https://github.com/alexlouden/Terraform.tmLanguage) | MIT |

Each license is in `licenses/`, named after its syntax.
