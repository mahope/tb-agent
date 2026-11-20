# Optional Thunderbird integrations

The core background page uses MailExtension APIs and a loopback WebSocket.
The core XPI (`node scripts/build-xpi.cjs --core`) has no Experiment declaration.
The full XPI (`node scripts/build-xpi.cjs`) can use the integrations below. Its
tool modules are off until enabled in tb-agent settings. The daemon exposes the
loaded module names through `tb status` and rejects calls to unavailable modules.

| Capability | What still uses an Experiment |
| --- | --- |
| Automatic connection discovery | Reads the daemon's connection file using privileged local file I/O. The core XPI can read a file explicitly selected in the settings page. |
| `experiment.gloda` | `searchMessages(searchBody: true)` calls Thunderbird's internal Gloda index when enabled. Ordinary header search uses `messages.query`. |
| `experiment.attachments` | `searchAttachments` uses upstream's bounded raw MIME inspection and local/offline checks. |
| `experiment.folder-sync` | `refreshFolder` uses native per-folder refresh and completion notifications. |
| `experiment.folders` | Folder creation, rename, move, deletion, Trash/Junk emptying and saved searches use upstream's folder handlers. MailExtensions offer basic folder CRUD, but the upstream tools also guard virtual folders, Outbox, destination aliases and destructive empty operations using native folder flags and services. |
| `experiment.filters` | Filter list, creation, editing, deletion, ordering and manual application use native filter lists and XPCOM search terms/actions. Upstream restrictions on custom actions and automatic forwarding/replies continue to apply. |
| `experiment.calendar` | The upstream calendar, event and task tools use native calendar providers, recurrence handling and review dialogs. Enable this only to grant access to the profile's calendars. |
| `experiment.contacts` | The upstream contact tools use native address book and vCard handling for their full field and provider support. Access is profile-wide when enabled. Writes require a separate checkbox. |
| `experiment.account-setup` | `openAccountSetup` opens Thunderbird's interactive Account Hub. The user completes the setup in Thunderbird and can then add the new account to the allowlist. |
| `experiment.advanced-mail` | Local attachment import/export paths, inline image content, custom draft threading headers, explicit signature suppression, Send Later, and `dryRunCompose` use upstream's native compose and MIME helpers. This module also handles direct sends with the upstream preflight checks and unattended draft saves under a partial account allowlist, where the identity's configured Drafts destination must be checked before saving. |

Core message updates, moves, copies, moves to Trash, body/header reads and compose
review windows use only MailExtension APIs. A plain draft can also be saved using
MailExtensions when the allowlist includes every mail account. MailExtensions
save to the identity's configured Drafts folder and offer no preflight lookup
of that destination.

Folder identifiers from the core XPI are opaque MailExtension IDs, for example
`account1://Inbox`. Pass them to other calls as returned. The full XPI translates
the identifiers when an upstream handler expects a native Thunderbird folder URI.
MailExtension headers do not expose Thunderbird's native thread-tree ID.
