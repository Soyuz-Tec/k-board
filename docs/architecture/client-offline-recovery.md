# Client offline and recovery contract

This contract distinguishes user-visible local state, browser persistence and
server durability. It is normative for the standalone web client.

## State meanings

| UI state | Exact meaning | Safe user action |
|---|---|---|
| Local-only | The edit exists in the wasm document but browser persistence failed or has not completed | Keep the tab open and export a recovery copy |
| Awaiting durable ack | IndexedDB contains the original batch; the server has not confirmed its durable sequence | Continue editing or close/reload; the batch is retained |
| Sending | One persisted batch was written to the socket; its outcome is unknown | Wait; close/reconnect retries the same batch id |
| Durable | A matching v2 acknowledgement names the batch and durable sequence | The client may remove that batch |
| Recovery required | The server permanently refused the retained batch | Export the recovery JSON; no automatic deletion occurs |

Status text is exposed through the existing live region, so screen-reader
announcements use the same durability facts as the visible indicator.

## Guarantees

1. An operation is not sent until it has been captured in IndexedDB, unless
   storage failed and the UI explicitly says it is local-only.
2. WebSocket open and `send` are never treated as commit.
3. Only an acknowledgement with the exact in-flight batch id removes work.
4. Close, timeout, reload and authorization expiry retain outcome-unknown work
   with its original batch and replica identity.
5. Protocol initialization is identity-checked and merged with restored local
   operations before the outbox starts sending.
6. Retryable overload uses exponential backoff with jitter, capped at 30
   seconds. Permanent refusal does not busy retry.
7. Distinct tabs have distinct replicas, even for the same scope. Reloading one
   tab retains that tab's replica.

## Explicit limits and non-guarantees

- 4,096 queued operations, 8 MiB encoded outbox bytes and 512 operations per
  batch. Capture beyond a limit fails atomically.
- Browser storage can be evicted by the user agent. The application does not
  claim protection against profile deletion, device loss or malicious local
  software.
- If IndexedDB is unavailable, private-mode policy rejects it, or quota is
  exhausted, only the live tab contains the edit until recovery JSON is
  exported. Reload is not safe in that state.
- Recovery JSON may contain board content. Treat it as sensitive tenant data;
  it is downloaded locally and never uploaded automatically.
- Offline work is convergent, not guaranteed to remain visually winning:
  concurrent higher-clock property writes may supersede it after merge.

## Recovery procedure

1. Do not clear site data or close a tab showing `local-only`.
2. Use **Recovery** to download the versioned JSON envelope.
3. Record the scope, replica and refusal reason shown in the envelope.
4. Correct authorization, capacity or semantic cause before retrying. Retrying
   a permanent semantic refusal unchanged cannot succeed.
5. Preserve the file until the intended shapes are confirmed durable. A future
   recovery importer can consume `kboard-recovery-v1`; current recovery is a
   support-assisted inspection/export path.

## Verification

Run `node scripts/client-outbox-check.mjs`. The live browser proof additionally
disconnects before commit, reloads, verifies retained pending work, reconnects
and waits for a matching durable status.
