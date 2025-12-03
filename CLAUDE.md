# S3 Migration Manager - Development Guide

This is a terminal-based S3 migration manager built with Rust. It provides an interactive TUI for browsing S3 buckets, defining object masks, and managing storage tier transitions.

## Project Overview

**Language**: Rust (Edition 2024)
**Runtime**: Tokio async runtime
**UI Framework**: ratatui (Terminal UI)
**AWS SDK**: aws-sdk-s3 v1.38.0
**Minimum Rust**: 1.78+

## Quick Start

```bash
# Build the project
cargo build

# Run in development mode
cargo run

# Build optimized release binary
cargo build --release

# Run the release binary
./target/release/s3-migration-manager
```

## Development Workflow

### Building and Checking

```bash
# Fast compilation check (no binary output)
cargo check

# Build with debug symbols (faster compilation)
cargo build

# Build optimized for release
cargo build --release

# Watch mode: rebuild on file changes (requires cargo-watch)
cargo watch -x check
cargo watch -x run
```

### Code Quality

```bash
# Format code with rustfmt
cargo fmt

# Check formatting without modifying files
cargo fmt -- --check

# Run clippy linter
cargo clippy

# Run clippy with all warnings as errors (strict mode)
cargo clippy -- -D warnings

# Fix auto-fixable clippy warnings
cargo clippy --fix
```

### Testing

```bash
# Run all tests
cargo test

# Run tests with output shown
cargo test -- --nocapture

# Run specific test
cargo test test_name

# Run tests in a specific module
cargo test aws::

# Run with parallelism disabled (for debugging)
cargo test -- --test-threads=1
```

## AWS Credentials Setup

The application uses the AWS SDK's default credential chain. Ensure one of the following is configured:

1. **Environment variables**:
   ```bash
   export AWS_ACCESS_KEY_ID="your-access-key"
   export AWS_SECRET_ACCESS_KEY="your-secret-key"
   export AWS_REGION="us-east-1"  # Optional
   ```

2. **AWS credentials file** (`~/.aws/credentials`):
   ```ini
   [default]
   aws_access_key_id = your-access-key
   aws_secret_access_key = your-secret-key
   ```

3. **AWS profile**:
   ```bash
   export AWS_PROFILE="your-profile-name"
   ```

4. **SSO authentication**:
   ```bash
   aws sso login --profile your-profile
   export AWS_PROFILE="your-profile"
   ```

**Testing tip**: Use a test AWS account or buckets with non-production data during development.

## Project Structure

```
s3-migration-manager/
├── src/
│   ├── main.rs         # Application entry point
│   ├── app.rs          # Core application state and logic
│   ├── aws.rs          # AWS S3 service wrapper
│   ├── mask.rs         # Object filtering masks (prefix/suffix/regex)
│   ├── models.rs       # Data structures (BucketInfo, ObjectInfo, etc.)
│   ├── policy.rs       # Migration policy persistence
│   └── tui/
│       └── mod.rs      # Terminal UI rendering and event handling
├── Cargo.toml          # Dependencies and project metadata
└── README.md           # User-facing documentation
```

## Module Responsibilities

### `main.rs`
- Application entry point
- Initializes tokio runtime
- Sets up PolicyStore and S3Service
- Launches the TUI

### `app.rs`
- Core application state (App struct)
- UI mode management (Browsing, EditingMask, Confirming, etc.)
- Pane focus tracking (Buckets, Objects, MaskEditor, Policies)
- Mask draft management
- Status message queue

### `aws.rs`
- S3Service wrapper around AWS SDK
- Operations: list_buckets, list_objects, head_object
- Storage class transitions and Glacier restores
- Error handling for AWS API calls

### `mask.rs`
- ObjectMask implementation
- MaskKind variants: Prefix, Suffix, Contains, Regex
- Case-sensitive/insensitive matching
- Live filtering of object lists

### `models.rs`
- BucketInfo: S3 bucket metadata
- ObjectInfo: Object key, size, storage class, restore status
- StorageClassTier: STANDARD, STANDARD_IA, GLACIER, etc.

### `policy.rs`
- PolicyStore: Loads/saves to `~/.config/s3-migration-manager/policies.json`
- MigrationPolicy: Reusable mask + target class + restore settings
- JSON serialization via serde

### `tui/mod.rs`
- Terminal initialization and restoration
- Event loop (keyboard input, rendering)
- UI layout with ratatui widgets
- Pane rendering (buckets, objects, mask editor, policies, status)
- Help overlay and log viewer

## Common Development Tasks

### Adding a New Storage Class

1. Add variant to `StorageClassTier` in `src/models.rs:1`
2. Update `as_str()` and `all()` methods
3. Update parsing logic in AWS response handling (`src/aws.rs`)

### Adding a New Mask Type

1. Add variant to `MaskKind` in `src/mask.rs:1`
2. Implement matching logic in `ObjectMask::matches()`
3. Add UI cycle logic in `app.rs` (`cycle_mask_kind` methods)
4. Update mask editor rendering in `tui/mod.rs`

### Modifying Key Bindings

1. Edit event handlers in `tui/mod.rs`
2. Look for `KeyCode::` patterns in match statements
3. Update help text in the `render_help()` function
4. Update README.md keybinding table

## Configuration Files

### User Configuration
- **Location**: `~/.config/s3-migration-manager/policies.json`
- **Format**: JSON array of MigrationPolicy objects
- **Created**: Automatically on first policy save

### Cargo.toml
- Edition 2024 (requires Rust 1.78+)
- All dependencies use stable versions
- Features enabled: `behavior-version-latest` for AWS SDK

## Error Handling Patterns

This project uses:
- `anyhow::Result<T>` for main error handling
- `thiserror` for custom error types (if needed)
- `push_status()` for user-facing error messages in the TUI

When adding new AWS operations:
```rust
match s3.some_operation().await {
    Ok(result) => {
        app.push_status("Operation succeeded");
        // handle result
    }
    Err(err) => {
        app.push_status(&format!("Error: {err}"));
    }
}
```

## Async Patterns

All AWS operations are async and use tokio:
```rust
#[tokio::main]
async fn main() -> Result<()> { ... }
```

Event loop in `tui/mod.rs` uses:
- `tokio::select!` for concurrent event handling
- Channel-based communication for async operations
- `tokio::signal::ctrl_c()` for graceful shutdown

## Performance Considerations

### Bucket/Object Listing
- Lists are loaded on-demand (not cached globally)
- Filtered objects are computed incrementally as masks change
- Use pagination for very large buckets (may need future enhancement)

### UI Rendering
- Terminal renders on every event (key press)
- Status log capped at 20 entries (see `STATUS_LIMIT` in `app.rs:7`)
- Object lists display all items (consider virtualization for 10k+ objects)

## Debugging Tips

### Enable Tokio Console
```bash
# Add to Cargo.toml dependencies:
# tokio = { version = "1.37", features = ["full", "tracing"] }
# console-subscriber = "0.2"

# Run with tracing:
RUSTFLAGS="--cfg tokio_unstable" cargo run
```

### Log AWS SDK Calls
```bash
# Set log level before running:
export RUST_LOG=aws_sdk_s3=debug
cargo run
```

### TUI Debugging
- Use `app.push_status()` for runtime debugging info
- Press `l` in the app to view the status log overlay
- For panics, terminal state may be corrupted; run `reset` command

### Common Issues

**Terminal not restored after panic**:
```bash
reset
# or
stty sane
```

**AWS credential errors**:
```bash
# Verify credentials work:
aws s3 ls --profile your-profile

# Check which profile is active:
echo $AWS_PROFILE
```

**Build errors with aws-lc-sys**:
- Ensure you have a C compiler installed (clang on macOS, gcc on Linux)
- On macOS: `xcode-select --install`
- On Ubuntu: `apt-get install build-essential`

## Before Committing

```bash
# Format code
cargo fmt

# Check for issues
cargo clippy -- -D warnings

# Ensure it compiles
cargo check

# Run any tests
cargo test

# Verify it runs
cargo run
```

## Release Checklist

1. Update version in `Cargo.toml`
2. Update `README.md` with any new features
3. Run `cargo fmt` and `cargo clippy`
4. Build release binary: `cargo build --release`
5. Test release binary against live AWS account
6. Tag release: `git tag v0.x.x`
7. Push tags: `git push --tags`

## Future Enhancements (from README)

- Mask-aware previews (count + byte size estimations)
- Background task queue for long operations
- Tag-based and size/date filters
- Cost estimation per migration plan
- CloudTrail-friendly dry-run mode

## Additional Resources

- [Ratatui Documentation](https://docs.rs/ratatui/)
- [AWS SDK for Rust](https://docs.rs/aws-sdk-s3/)
- [Tokio Tutorial](https://tokio.rs/tokio/tutorial)
- [Rust Async Book](https://rust-lang.github.io/async-book/)
