# HTTP API reference

Every Cloudroom feature is available over this API. It is plain HTTP on `127.0.0.1:9840` by default; put an HTTPS proxy in front for remote clients ([setup](setup.md)). The TypeScript client in `integrations/bb/client.ts` wraps it.

## Conventions

- **Auth:** send `Authorization: Bearer CLOUDROOM_TOKEN` on every request, including health checks and streams. Anything else returns `401`.
- **Commands are asynchronous.** A `POST` returns `202` with `{session_id, receipt, saving}`. `202` means saved, not finished.
- **Request IDs:** every command carries a `request_id` of 1–64 letters, digits, `_` or `-`. Retrying an ID returns the same receipt and never repeats work. Reusing an ID for different content returns `request_conflict`.
- **Session IDs** are `cr_` plus the start request's ID. `POST /v1/sessions` with `request_id: "demo"` creates `cr_demo`.
- **Receipts** move from `accepted` to `delivered`, then end as `completed`, `interrupted`, `failed`, `unknown` or `unknown_after_restart`. Uncertain work is never resent.
- **Limits:** JSON bodies up to 64 KiB, prompt text up to 32,768 bytes. Check `GET /v1/capabilities` before using optional features; a missing flag means unsupported.

## Health and discovery

- `GET /v1/health`: service status and disk safety snapshot.
- `GET /v1/ready`: `200` only when storage and the database are usable, otherwise `503`.
- `GET /v1/capabilities`: feature flags, plus each configured harness with its models, reasoning levels and options.
- `GET /v1/dashboard` and `GET /v1/metrics`: safe machine and session summaries for dashboards ([dashboard API](dashboard.md)).

## Sessions

- `GET /v1/sessions`: the latest 1,000 sessions, newest activity first, as `{total, sessions}`. Each summary has `session_id`, `harness`, `model`, `provider`, `state`, `workspace`, `parent_session`, `current_request`, `queued`, `last_sequence` and `last_activity_ms`.
- `POST /v1/sessions`: start a session. Body: `request_id`, plus optional `harness` (`codex`, `claude-code`, `pi`, `cursor`), `model`, `reasoning`, `provider` (Pi only), `workspace`, `workspace_name` and `command_guard_enabled`.
- `GET /v1/sessions/{id}`: full state, including receipts and the queue.
- `GET /v1/sessions/{id}/workspace`: the session's folder, branch and commit.
- `GET /v1/sessions/{id}/recovery`: whether a stopped session can resume ([lifecycle](session-lifecycle.md)).

Commands, all `POST /v1/sessions/{id}/...` with a `request_id`:

- `prompts`: send a message. Body: `text`, plus optional `content`, `attachments`, `reasoning` and `service_tier`. Messages sent while busy queue in order.
- `edit`: replace a queued message. Adds `target_request_id` and `expected_revision`.
- `cancel`: remove a queued message (`target_request_id`).
- `steer`: add guidance to the running turn (`target_request_id`, `text`), where the harness supports it.
- `interrupt`: stop the current turn (`target_request_id`). Queued messages continue.
- `stop` and `resume`: pause and restart the queue.
- `sleep`: release an idle harness process. The next message wakes it.
- `compact`: ask the harness to compact its context.
- `rewind`: go back to an earlier message (`before` or `last_turn_id`), with an optional `replacement` prompt.
- `attachments?request_id=ID&name=NAME&kind=image|file`: upload a raw file body (images up to 10 MiB, files up to 25 MiB).
- `close`: end the session and keep its history.

`POST /v1/sessions/{id}/secrets/{request}` answers an agent's secret request with the requested values. It takes no `request_id`.

## History and live events

- `GET /v1/sessions/{id}/events?after=N`: up to 256 records after sequence `N`. Page with the last returned `sequence`.
- `GET /v1/sessions/{id}/stream?after=N`: server-sent events named `record`. Each event's `id` is its sequence, so reconnecting with `Last-Event-ID` resumes exactly.

Each record is `{sequence, session_id, kind, data, native?, timestamp_ms?}`. `native` holds the harness's original output line.

- **Lifecycle:** `receipt`, `state`, `harness`, `workspace`, `native_identity`, `launch_reasoning`, `checkpoint`, `usage`, `usage_limited`, `child`, `child_result`, `rewind`, `rewind_ready`, `rewind_failed`, `teleport`, `secret_request`, `interaction_cancelled`, `native_history_unavailable`.
- **Disk safety:** `storage_warning`, `storage_warning_delivery`, `storage_pause`, `storage_recovered`.
- **Harness output:** `text_delta`, `thinking_delta`, `item_started`, `item_completed`, `tool_delta`, `tool_snapshot`, `native_event` and `native_record`.

Output records are not yet harness-neutral. Kinds are shared, but `data` differs by harness; Codex records also carry the native app-server `method`. Check `session.harness` before parsing `data`.

Session `state` values: `pending`, `starting`, `resuming`, `starting_turn`, `running`, `interrupting`, `closing`, `idle`, `sleeping`, `suspended`, `rewinding`, `closed`, `failed`, `process_lost`, `waiting_for_files`, and `saved_history_only` for history read from the database.

## Accounts

Harness logins run on the VM. Responses report status and sign-in links, never tokens ([harnesses](harnesses.md)).

- Codex: `GET /v1/accounts/codex`; `POST .../login`, `.../import`, `.../switched`, `.../cancel`.
- Claude Code: `GET /v1/accounts/claude`; `POST .../{login|cancel|complete|token}`.
- Cursor: `GET /v1/accounts/cursor`; `POST .../{login|cancel|key}`.
- Pi: `GET /v1/accounts/pi`; `POST .../import`, `.../key`.

## Workspaces, sync, previews and transfers

- `GET /v1/workspaces/{id}`: a cloud folder mapping ([cloud folders](setup.md#cloud-folders)). An unknown mapping returns `404` and an unusable one returns `409`, both with `invalid_workspace`.
- `GET /v1/settings`, `POST /v1/sync`, `GET /v1/sync/{id}`, `GET|PUT /v1/sync/{id}/file`: skills and settings sync ([sync](setup.md#skills-and-login-sync)).
- `/v1/previews...`: open cloud web servers on your laptop's localhost ([previews](previews.md)).
- `POST /v1/teleports`, `POST /v1/teleports/check`, `GET /v1/teleports/{id}`, `POST .../activate`, `.../cancel`, `.../files/{index}`: move a local conversation and its files to the cloud.
- `GET /v1/mac/jobs`, `POST /v1/mac/results/{id}`, `POST /v1/vm/run`: two-way Mac and VM access for a paired Mac.

## Errors

Errors return `{"error": "...", "code": "..."}`. Show `error` to people; branch on `code`.

- `401`: missing or wrong token.
- `404`: unknown session or other resource.
- `409`: rejected. Some codes are temporary: `storage_blocked`, `service_stopping`, `model_catalog_unavailable`, and the `*_auth_unavailable` codes.
- `503` with `storage_unavailable`: saving failed. Retry with the same `request_id`.

Codes: `invalid_model`, `invalid_provider`, `invalid_reasoning_effort`, `invalid_service_tier`, `invalid_workspace`, `invalid_attachment`, `attachment_too_large`, `attachment_permission_denied`, `request_conflict`, `harness_not_configured`, `unsupported_command`, `storage_blocked`, `service_stopping`, `model_catalog_unavailable`, `codex_auth_required`, `codex_auth_unavailable`, `codex_auth_busy`, `codex_usage_limit`, `claude_auth_required`, `claude_auth_unavailable`, `cursor_auth_required`, `cursor_auth_unavailable`, `cursor_auth_busy`, `teleport_rejected`, `teleport_cancelled`, `teleport_running`, and `request_rejected` for everything else.
