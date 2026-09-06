# Codex backend gateway

The experimental Codex backend uses `/backend-api/codex/responses` and
`/backend-api/codex/models`. Both switches default to false. Changes to
`codex_backend_enabled` and `codex_models_enabled` take effect on the next
request after the gateway configuration is reloaded; no restart is required.
Disabling a switch does not cancel requests or WebSocket sessions already in progress.

## Authentication

A gateway-issued key remains the preferred credential. A virtual login places
that key in `auth.json` as the access token. This is a client compatibility
mechanism, not a real ChatGPT account or subscription. Account-specific services
may still attempt OAuth refresh, and full context-management behavior depends
on the Codex version. OcHub refuses to overwrite an existing real ChatGPT login
with a virtual login. With no real login present, virtual login installation and
key rotation work even when the generic preserve-official-login setting is on.

From 0.5.16, applying a station with an existing real login automatically preserves it, enables the backend/catalog, and binds its route key. Saving an inactive station does not change the binding. Legacy unscoped URLs use a gateway-wide binding. Newly generated URLs use `/connection/{gateway-key}/backend-api/codex`, resolving the route independently of the account token.

For a manual connection with an existing real login, select the ChatGPT login + OcHub relay mode without
virtual login and explicitly configure both fields in `GatewayConfig`:

```json
{
  "codex_backend_enabled": true,
  "codex_models_enabled": true,
  "codex_backend_accept_any_bearer": true,
  "codex_backend_oauth_key_id": "<existing gateway key ID>"
}
```

The ID must reference an enabled key bound to a station route. Its route,
model policy and usage identity apply to these requests. This mode accepts an
unverified non-empty OAuth-shaped bearer; it does not validate the ChatGPT
account. Missing bindings, disabled keys, retired `rd-` keys and arbitrary
`x-api-key` values are rejected. `/v1` authentication is unaffected. The gateway
continues to listen on loopback only. Existing accept-any configurations must
add a binding before they can accept unknown bearers.

## Catalog and transport

Unknown models use a neutral coding prompt, text-only input, a conservative
32,768-token context window, and no advertised reasoning, search, verbosity or
parallel-tool capabilities. Set per-model context/reasoning overrides to match
the actual upstream. The bundled GPT-5.5 template is used only for its exact
slug; optional official templates are matched by slug.

Official catalog refresh runs in the background, with one refresh in flight
for the active token/account identity. During cold start the local catalog is
served immediately. Failed refreshes retain the last successful template and
retry after 60 seconds; successful templates refresh after 10 minutes. Local
catalog content determines both ETag headers, even if the inference upstream
supplies a different catalog ETag.

HTTP inference accepts zstd requests and enforces the 200 MiB limit on both
compressed and decompressed bodies. Unsupported content encodings return 415,
invalid zstd returns 400, and oversized decoded bodies return 413.

Configuration editing accepts both legacy boolean context-management settings
and modern `experimental_mode` tables. Unmanaged context-management and token
budget fields survive editing and disabling the feature.

## Verification

```sh
cargo test -p ochub-core --lib
cargo test -p ochub-core codex_cli_gateway_smoke --lib -- --ignored --nocapture
```

The opt-in smoke test requires `codex` on PATH. It uses an isolated temporary
Codex home, generated provider configuration and virtual auth, a real local
OcHub gateway, and a mock SSE upstream. Proxy settings prevent external HTTP
access. It verifies catalog decoding and completion through the normal zstd
request path with context-management and token-budget settings enabled. Tested
with Codex CLI 0.153.4. It does not establish compatibility with future clients
or verify long-conversation history management against a real model.

## Remote compaction

Both `/v1/responses/compact` and `/backend-api/codex/responses/compact` use the normal routing, model policy, authentication, credential replacement and usage pipeline. Compaction accepts bounded zstd bodies, uses only Responses upstreams and forwards to their `/responses/compact` endpoint. It preserves opaque compaction output. Ordinary Responses support does not guarantee upstream compaction support.


## Capability and context correction

The former 0.5.16 claim of complete experimental context management was incorrect. Login binding and `/responses/compact` do not implement History / Notes ingestion, search, or notes. These backend services remain unavailable. Automatic station configuration must not enable the experiment; explicit requests are rejected with an actionable configuration error. Official automatic mode defers account/plan/policy checks to Codex.

Supplier `model_capabilities` maps upstream model names to optional `use_responses_lite`, `remote_compaction`, and `context_window` declarations (plus existing Codex model overrides). These are user declarations, not probe results. Unset Lite is conservatively false. Scoped catalog generation resolves aliases before applying capabilities and does not inherit official token-budget activation. Lite HTTP requests only select native Responses upstreams; local route headers are stripped before upstream forwarding.

The scoped URL carries a gateway credential. Revoke its key to revoke access; a nonempty account bearer alone does not grant scoped access or prove subscription eligibility. Legacy unscoped fallback remains supported for existing manual setups.
