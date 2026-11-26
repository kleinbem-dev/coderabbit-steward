default:
    @just --list

# Run all Rust tests
test:
    cargo test --all-targets

# Run clippy with warnings as errors
lint:
    cargo clippy --all-targets -- -D warnings

# Build the static SvelteKit dashboard
web-build:
    cd web && npm run build

# Check the web frontend
web-check:
    cd web && npm test && npm run check
