# Contributing to MCP Universal

Thank you for your interest in contributing! This document covers the process for contributing to the project.

## Getting Started

1. Fork the repository
2. Clone your fork: `git clone https://github.com/<you>/rust-mcp-vscode-ext.git`
3. Create a branch: `git checkout -b feat/my-feature`
4. Make your changes
5. Push and open a pull request

## Development Setup

### Rust Server

```bash
# Install Rust (stable)
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh

# Build
cargo build

# Run tests
cargo test --all-targets

# Lint
cargo clippy --all-targets -- -D warnings

# Format
cargo fmt --all
```

### VSCode Extension

```bash
cd vscode-extension

# Install dependencies (uses Bun)
bun install

# Build
bun run build

# Lint
bun run lint

# Type check
bunx tsc --noEmit

# Test
bun test
```

## Project Structure

```
src/                    # Rust server
  providers/            # LLM provider integrations
  routes/               # HTTP route handlers
  mcp/                  # MCP protocol implementation
  tools/                # Agent tools
  memory/               # RAG memory store
  messaging/            # Chat platform adapters
vscode-extension/       # VSCode extension (TypeScript)
  src/chat/             # Chat panel webview
  src/commands/         # Editor commands
  src/safetype/         # Secret detection
tests/                  # Integration tests
```

## Adding a New Provider

1. Create `src/providers/<name>.rs` implementing the `Provider` trait
2. Add `pub mod <name>;` to `src/providers/mod.rs`
3. Add config struct to `src/config.rs` with env var support
4. Wire it in `src/state.rs` in `register_keyed_providers`
5. Add tests in `tests/`
6. Update `config.toml.example` and `README.md`

## Code Style

- Follow `rustfmt` and `clippy::pedantic` conventions
- No `unwrap()` in production code — use proper error handling
- Keep functions focused; extract when > ~80 lines
- Prefer `tracing::info!` / `tracing::warn!` over `println!`
- TypeScript: follow the ESLint config in the extension

## Commit Messages

Use concise imperative messages:

```
Add HuggingFace provider with streaming support
Fix rate limiter window cleanup
Update README with Docker instructions
```

## Pull Request Process

1. Ensure CI passes (`cargo test`, `cargo clippy`, `cargo fmt --check`)
2. Update documentation if adding features
3. Add tests for new functionality
4. Keep PRs focused — one feature or fix per PR
5. Fill in the PR template

## Reporting Issues

- Use GitHub Issues
- Include reproduction steps, expected behavior, and environment details
- For security vulnerabilities, email the maintainers directly

## License

By contributing, you agree that your contributions will be licensed under the MIT License.
