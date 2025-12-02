# S3 Migration Manager

Terminal UI for browsing S3 buckets, defining object masks, and managing storage tier transitions or restores without leaving the shell.

## Features

- **Bucket & object browser**: list all accessible buckets and their objects, including size and current storage class.
- **Mask-driven selection**: build prefix/suffix/contains/regex masks, test matches live, and reuse them when defining migration policies.
- **Storage class transitions**: interactively choose a target tier; confirmations include an option to request restores before the copy operation.
- **Restore workflow**: request temporary Glacier restores (default 7 days) for the current selection.
- **Policy library**: persist mask + target-class rules to `~/.config/s3-migration-manager/policies.json` for later reuse or auditing.
- **Deep storage visibility**: refresh metadata for any object to fetch its latest restore status before acting.

## Requirements

- Rust 1.78+ (toolchain with `cargo`).
- AWS credentials/profile accessible via the standard SDK lookup chain (env vars, `~/.aws/credentials`, SSO, etc.).

## Getting Started

```bash
cargo run
```

The first launch will download crates, create a config directory if needed, and enter the TUI.

## Key Bindings

| Key | Action |
| --- | --- |
| `Tab` / `Shift+Tab` | Cycle focus across panes |
| `Enter` (bucket pane) | Load objects for the highlighted bucket |
| `m` | Open mask editor (Tab moves through fields, arrows/space adjust mode/case) |
| `Esc` | Clear the active mask (while browsing) or exit dialogs |
| `s` | Start a storage-class transition for the current selection |
| `p` | Save the current mask as a policy (prompts for target class) |
| `r` | Request a 7‑day restore for the current selection |
| `i` | Inspect the highlighted object (runs a `HeadObject` to refresh metadata) |
| `l` | Open the status log overlay to read full error/details |
| `?` | Show/hide the in-app cheat sheet |
| `f` | Refresh the bucket list |
| `q` / `Ctrl+C` | Quit |

Selections depend on context:

- With an active mask, transitions/restores target every matching object.
- Without a mask, actions apply to the highlighted object.

## Storage Policies

Saved policies live at `~/.config/s3-migration-manager/policies.json`. Each entry records:

- Bucket name
- Mask definition
- Desired destination storage class
- Whether a restore should run before transition
- Timestamp and optional notes

You can version-control this file or edit it manually if needed.

## Testing & Validation

- `cargo check` (run during development) ensures the project builds and dependencies resolve.
- Most behavior depends on live AWS APIs; prefer running against a test account or buckets with dummy data before touching production buckets.

## Next Steps

Ideas for follow-up iterations:

1. Mask-aware previews (count + byte size estimations) before executing transitions.
2. Background task queue so long copy/restore operations don’t block the UI.
3. Tag-based and size/date filters alongside the current key-based masks.
4. Optional cost estimation per plan using cached pricing tables.
5. CloudTrail-friendly dry-run mode that just logs intended actions.
