# RunnerProtocolV1

The checked-in schemas are generated from the authoritative Rust types. Run
`cargo xtask schema` after contract changes; CI uses `cargo xtask schema --check`.

`RunnerProtocolV1` is the process boundary between the Rust Orchestrator and native AI runners.

- The Orchestrator writes a typed job JSON file.
- The runner emits one UTF-8 JSON event per stdout line.
- Human-readable diagnostics belong on stderr.
- Sequence numbers start at 1 and increase monotonically.
- Success requires both one `completed` event and exit code 0.

The fake-job schema contains test-only behavior and must not be reused as a production AI job schema.
