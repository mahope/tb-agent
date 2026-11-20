# tb-agent protocol, version 1

The daemon `tbd` is a hub. Two kinds of peers connect to it over a WebSocket on localhost:

- the **extension** (the Thunderbird add-on) executes operations against Thunderbird,
- **clients** (`tb`, the MCP server, anything else) send operations and read the answers.

The daemon authenticates peers, checks requests, relays them to the extension and relays the
answers back. It never talks to a mail server itself; Thunderbird owns accounts and logins.

The machine-readable operation list is `daemon/protocol/ops.json`. `tb ops` prints it, and the
MCP server builds its tools from it.

## Connecting

The daemon listens on a loopback address only (default `127.0.0.1`, random free port) and serves
the WebSocket at `/ws`. At startup it generates independent client and extension tokens
(256 bits, 64 lowercase hex characters each). It writes the connection files atomically with
owner-only permissions where the platform supports them. Both files have the same shape:

```json
{ "protocol": 1, "host": "127.0.0.1", "port": 49321, "token": "9f3a...", "pid": 4711 }
```

Location: `$TB_CONNECTION_FILE`, else `<runtime dir>/tb-agent/connection.json`
(`$XDG_RUNTIME_DIR` on Linux, the local data directory on macOS and Windows). The daemon removes
both files when it shuts down cleanly. The extension uses the sibling file with its final
extension replaced by `.extension.json` (normally `connection.extension.json`). Each token
authorizes only its matching peer role. Give agent clients access only to the client
credential, and isolate agents that can run shell commands from the extension credential,
policy, audit files and upstream bridge.

The WebSocket upgrade must carry the token in one of two forms, otherwise the daemon answers
`401` and no WebSocket is established:

| Form | Used by |
| --- | --- |
| `Authorization: Bearer <token>` | `tb`, MCP server, tests |
| `Sec-WebSocket-Protocol: tb.token.<token>` | the extension, because the browser WebSocket API cannot set headers. The daemon echoes the subprotocol back. |

The token is never accepted in the URL, so it does not end up in logs. Upgrades with an `Origin`
header other than `moz-extension://...` are refused with `403`, so a web page cannot use the
daemon even if it learned the token. Non-browser clients send no `Origin`.

Frames are JSON objects sent as WebSocket **text** messages, one object per message. Binary
messages are a protocol error. A message may be at most 16 MiB; larger results are streamed.

## Handshake and capabilities

The first frame from every peer is `hello`. It must arrive within 5 seconds.

```json
{ "type": "hello", "protocol": 1, "role": "client", "name": "tb 0.1.0" }
```

An extension also announces what it can do:

```json
{
  "type": "hello", "protocol": 1, "role": "extension", "name": "tb-agent extension 0.11.0",
  "capabilities": {
    "thunderbird": "154.0",
    "extensionVersion": "0.11.0",
    "modules": ["core", "experiment.attachments"]
  }
}
```

`modules` lists the capability modules the extension implements. `core` is required and means
"everything that works with plain WebExtension APIs". Other modules may depend on Thunderbird
Experiment APIs and are optional, for example `experiment.attachments` and
`experiment.folder-sync`. Every operation in `ops.json` names the module it needs. The daemon
refuses an operation whose module the extension did not announce, with `unsupported_op`, before
anything reaches the extension.

The daemon answers with `welcome`:

```json
{ "type": "welcome", "protocol": 1, "daemonVersion": "0.1.0",
  "extensionConnected": true, "extension": { "modules": ["core"], "thunderbird": "154.0" } }
```

If `protocol` differs from the daemon's, or an extension omits the `core` module, the daemon
closes the connection with code 1008. Only one extension is connected at a time. A newer
extension connection replaces the older one (close code 1012), and requests that were waiting on
the old one fail with `extension_disconnected`.

## Requests and responses

```json
{ "type": "request", "id": "7", "op": "searchMessages", "params": { "query": "invoice" } }
```

- `id` is chosen by the sender and must be unique among its open requests.
- `op` is an operation name from `ops.json`. Names follow the upstream thunderbird-mcp tool
  names, and `params` follow the upstream tool input schemas, so the extension can reuse them.
- `params` is a JSON object (or absent when there are none). The daemon checks required fields
  and the top-level type of known fields. It drops unrecognized fields before forwarding.
  Fields prefixed with `_` are for the daemon and cannot be supplied by clients.
- `timeoutMs` (optional) overrides the daemon's default of 120 000 ms.

The daemon relays the request to the extension under its own id, so ids from different clients
never collide. The extension answers with a `response`, and the daemon hands it back with the
client's id.

```json
{ "type": "response", "id": "7", "ok": true, "result": [ { "id": "m1@example.com" } ] }
{ "type": "response", "id": "7", "ok": false,
  "error": { "code": "not_found", "message": "message not found" } }
```

Exactly one `response` ends every request. `error.data` may carry extra structured detail.

### Error codes

The daemon produces these. The extension may use any other string, which the daemon passes
through unchanged (the fake extension uses `not_found`).

| Code | Meaning |
| --- | --- |
| `bad_request` | `params` missing a required field or of the wrong type |
| `unknown_op` | the operation name is not in the registry |
| `unsupported_op` | the extension did not announce the module the operation needs |
| `extension_unavailable` | no extension is connected |
| `extension_disconnected` | the extension went away before answering |
| `timeout` | the extension did not answer in time |
| `cancelled` | the client cancelled the request |
| `payload_too_large` | a streamed result exceeded 256 MiB |
| `protocol_error` | malformed frame |
| `permission_denied` | a required account permission is absent, or a resource cannot be scoped |
| `audit_failed` | write intent or outcome could not be confirmed in the audit journal |
| `undo_unavailable` | no reversible, confirmed, unclaimed write has this audit ID |
| `conflict` | current message state differs from the snapshot, or its locator is ambiguous |
| `internal` | daemon bug |

## Streaming large results

An extension may deliver a result in pieces. It sends any number of `chunk` frames, then a
`response` with `streamed: true` and no `result`:

```json
{ "type": "chunk", "id": "d12", "seq": 0, "data": "{\"id\":\"m1\",\"body\":\"aaaa" }
{ "type": "chunk", "id": "d12", "seq": 1, "data": "aaaa\"}" }
{ "type": "response", "id": "d12", "ok": true, "streamed": true }
```

`data` is a slice of the **JSON text** of the result. Concatenating the slices in `seq` order,
starting at 0, gives the complete JSON document. Slices may split anywhere between characters,
so they are only meaningful after concatenation. This works for any result shape and keeps
each frame well below the 16 MiB message limit. The daemon relays chunks as they arrive, maps
the id and counts bytes; it fails the request with `payload_too_large` above 256 MiB and tells
the extension to stop with `cancel`. Clients reassemble. Out-of-order chunks are a protocol
error. The extension is free to choose the threshold; a reasonable value is 64 KiB.
For results that require account filtering or header projection, the daemon buffers
and validates the stream before sending any client-visible data.

An extension must send either a plain `result` or a streamed one, never both.

## Cancelling

```json
{ "type": "cancel", "id": "7" }
```

A client sends `cancel` to abandon a request. The daemon answers that request with a `cancelled`
error and forwards `cancel` to the extension, which should stop the work and send nothing more
for that id. The daemon does the same when a client disconnects or a request times out, and it
ignores late frames for requests it no longer tracks.

## Keep-alive

Either side may send `{ "type": "ping" }`; the daemon answers `{ "type": "pong" }`. An extension
should ping every 20 to 30 seconds so that Thunderbird keeps its background page alive and the
daemon notices dead connections.

## Local operations

`status` is answered by the daemon itself and works without an extension:

```json
{ "daemonVersion": "0.1.0", "protocol": 1, "extensionConnected": true,
  "extension": { "thunderbird": "154.0", "modules": ["core"] }, "pendingRequests": 0 }
```

## Operations in version 1

| Operation | Module | Access |
| --- | --- | --- |
| `status` | daemon | read |
| `undo` | daemon | update |
| `listAccounts`, `listFolders` | core | read |
| `searchMessages`, `getRecentMessages` | core | read |
| `getMessage`, `getMessages`, `getMessageHeaders`, `batchGetMessageHeaders` | core | read |
| `searchAttachments` | experiment.attachments | read |
| `refreshFolder` | experiment.folder-sync | read |
| `updateMessage` (read, flag, tags, move, copy, trash) | core | update |
| `deleteMessages` | core | delete |
| `saveDraft`, `sendMail`, `replyToMessage`, `forwardMessage` | core | create |

Sending operations keep upstream's safety model: a compose window opens for review unless the
user has allowed direct sending in the extension, and `skipReview` is only a request. Message
content in results is untrusted data.

## Account policy

Account policy is loaded at daemon startup. Each `[accounts.<Thunderbird account ID>]`
table requires an `access` value and a `permissions` array. An absent default policy or
unlisted account means `headers` plus `read`. An invalid policy or a missing explicitly
configured file prevents startup. See the [policy example](../README.md#account-policy-and-undo).

| Access | Effective permissions |
| --- | --- |
| `off` | None |
| `headers` | `read` when listed, limited to message headers and account/folder discovery |
| `full` | Every listed permission, including full content when `read` is listed |

| Permission | Operations |
| --- | --- |
| `read` | Account and folder discovery, searches, message reads. Body and attachment searches and folder refresh require full access. |
| `move` | Move and copy |
| `tag` | Add/remove tags, mark read/unread, flag/unflag |
| `trash` | Trash, delete and replace an existing draft |
| `send` | Save draft, compose, send, reply and forward, including review mode |
| `filters`, `rules`, `accounts` | Reserved for future management operations |

Every permission needed for a compound write must be granted on its affected accounts.
Reply and forward also require `read` on the source account. Compose always uses the
identity ID resolved by the daemon from `from` or the advertised default. The daemon
checks source, destination and identity owners before forwarding. Global reads run
only over the eligible accounts. A request targeting an inaccessible account explicitly
fails. Full-content searches spanning accounts skip header-only accounts.

For a header-only account the daemon routes `getMessage` to `getMessageHeaders`.
Message reads and searches expose only `id`, `author`, `subject`, `date`,
`folderPath`, `read`, `flagged` and `tags`. A mixed `getMessages` request retains regular
content for full-access messages. When any requested account is header-only, the daemon
clears batch-wide raw-source and attachment-saving options.

### Extension routing contract

The authenticated extension sends `capabilities.inventory` in its `hello`. The daemon
keeps it private. Without an inventory, the daemon rejects mailbox requests.

```json
{
  "accounts": ["account1"],
  "folders": {
    "imap://work.example/INBOX": "account1",
    "imap://work.example/Drafts": "account1",
    "imap://work.example/Sent": "account1",
    "imap://work.example/Trash": "account1"
  },
  "identities": {
    "id1": {
      "accountId": "account1",
      "email": "agent@work.example",
      "draftsFolder": "imap://work.example/Drafts",
      "sentFolder": "imap://work.example/Sent"
    }
  },
  "defaultIdentity": "id1",
  "trashFolders": { "account1": "imap://work.example/Trash" }
}
```

Folder keys are exact, unambiguous locators from Thunderbird. A null draft or sent folder
means that identity has no such destination. When accounts, identities, ownership or
destination settings change, refresh the inventory by reconnecting. Reject a request if a
resource can no longer be resolved as advertised, or if it would touch an unlisted resource.
Never substitute an identity or fall back to a different account.

Every relayed request carries `params._scope` with `accounts` (the allowed account IDs)
and `headers` (the subset limited to headers). Restrict enumeration, search predicates,
deduplication, counts and pagination to the authorized resources before executing the
operation. For header-only messages, search only sender, subject, date, folder and flags.
For mixed batch reads, fetch only headers for entries in header-only accounts. Do not
open their bodies, parse attachments or save them to disk. Enforce the scope on every
resolved resource, including configured copies and destination folders.

### Audit and undo

For writes referencing existing messages, the daemon first asks the extension for
`snapshotMessages` with `folderPath` and `messageIds`. Its result must be an array of
message states with `messageId`, `folderPath`, `read`, `flagged` and `tags` (a sorted
array of unique tag keys). Each locator must resolve to exactly one message.

The daemon durably appends an `intent` before sending the mutation. It includes
`auditId`, Unix epoch milliseconds (`timeMs`), `accounts`, `operation`,
`messageIds`, permissions and the `previous` message states when applicable.
The forwarded mutation carries those states in `_expected`. The extension must verify
that all expected messages still match before changing any of them. On a successful
reversible `updateMessage`, return the new states in the result's `_after` field, with
the same message IDs and the actual folder and flags after the write. Write responses
and snapshots are single frames.

The daemon appends `finished` with the confirmation status and the post-write states
needed for undo. New-message IDs are logged when the extension returns `messageId`.
Successful client responses carry `auditId`. An intent without a confirmed outcome
may have reached Thunderbird, including after cancellation, timeout or disconnect.
Check the mailbox before retrying. The journal records metadata and state, and
excludes body, recipient and attachment parameters.

`undo` accepts `{ "auditId": "..." }` and dispatches a daemon-only `restoreMessages`
request with `expected` (post-write states) and `restore` (previous states). The daemon
rechecks current account permissions and journals the undo before dispatch. An audit ID
can be claimed for undo only once, including if the outcome is uncertain. The extension
checks all expected states and destination locators before restoring. If a locator is
ambiguous or state changed, return `conflict` without applying the restore. Copy,
permanent deletion and compose operations have no undo.

## Compatibility rules

- `protocol` is bumped for incompatible changes. A daemon refuses other versions.
- Account policy routing requires an advertised inventory and separate extension credential.
  Peers using the earlier test protocol must adopt these additions to handle mailbox requests.
- New optional fields and new operations are not breaking. Peers ignore frame fields they do
  not know. Unknown operations are answered with `unknown_op`; a frame with an unknown `type`
  is answered with `protocol_error` (clients) or logged and dropped (extension).
