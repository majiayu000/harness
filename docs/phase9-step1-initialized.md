# Phase 9 Step 1: `initialized` Handshake Support

Issue: #41  
Step: 1 of 3 protocol parity items

## What changed

- Added `Initialized` to `harness_protocol::Method` so clients can send:
  - `{"jsonrpc":"2.0","method":"initialized"}`
  - `{"jsonrpc":"2.0","method":"initialized","params":{}}` (accepted for compatibility)
- Added server handler `handlers::thread::initialized(...)` that returns a success result.
- Routed `Method::Initialized` in `router::handle_request`.

## Why

OpenAI Codex App Server handshake is two-step:

1. Client sends `initialize`
2. Client sends `initialized`

Before this change, harness only supported step 1.

## Validation

- Protocol codec test verifies `initialized` request roundtrip.
- Router tests verify:
  - `initialized` returns success
  - `initialize` then `initialized` both succeed in sequence
