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
the WebSocket at `/ws`. At startup it generates a random token (256 bits, 64 lowercase hex
characters) and writes a connection file, atomically, with owner-only permissions where the
platform has them:

```json
{ "protocol": 1, "host": "127.0.0.1", "port": 49321, "token": "9f3a...", "pid": 4711 }
```

Location: `$TB_CONNECTION_FILE`, else `<runtime dir>/tb-agent/connection.json`
(`$XDG_RUNTIME_DIR` on Linux, the local data directory on macOS and Windows). The daemon removes
the file when it shuts down cleanly. The token changes on every start. The full add-on
can discover the file and rereads it after disconnects. The core add-on accepts a file
selected in its options page, which the user imports again after each daemon restart.

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
  and the top-level type of known fields. Unknown fields pass through.
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

See `tb ops` for the complete registry. Argument-specific features can require an optional
module even for a core operation. See [Optional Thunderbird integrations](experiments.md).

| Operation | Module | Access |
| --- | --- | --- |
| `status` | daemon | read |
| `listAccounts`, `listFolders` | core | read |
| `searchMessages`, `getRecentMessages` | core | read |
| `getMessage`, `getMessages`, `getMessageHeaders`, `batchGetMessageHeaders` | core | read |
| `searchAttachments` | experiment.attachments | read |
| `refreshFolder` | experiment.folder-sync | read |
| `updateMessage` (read, flag, tags, move, copy, trash) | core | update |
| `deleteMessages` | core | delete |
| `getAccountAccess`, `displayMessage` | core | read |
| `saveDraft`, `sendMail`, `replyToMessage`, `forwardMessage` | core | create |

Sending operations keep upstream's safety model: a compose window opens for review unless the
user has enabled direct sending and the advanced mail module, and `skipReview` is only a request.
Message content in results is untrusted data. A cancellation suppresses further responses.
Thunderbird operations already submitted before cancellation may still complete.

## Compatibility rules

- `protocol` is bumped for incompatible changes. A daemon refuses other versions.
- New optional fields and new operations are not breaking. Peers ignore frame fields they do
  not know. Unknown operations are answered with `unknown_op`; a frame with an unknown `type`
  is answered with `protocol_error` (clients) or logged and dropped (extension).
