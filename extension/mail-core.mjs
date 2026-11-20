import { ToolError } from "./daemon-transport.mjs";

const FOLDER_COLUMNS = ["name", "path", "type", "accountId", "totalMessages", "unreadMessages", "depth", "isFavorite"];
const INVISIBLE = /[\u200B\u2060\uFEFF\u202A-\u202E\u2066-\u2069\u{E0000}-\u{E007F}]/gu;

function fail(code, message) {
  throw new ToolError(code, message);
}

function text(value) {
  return (value ?? "").replace(INVISIBLE, "");
}

function summarize(message, full = true) {
  function recipients(addresses) {
    const list = (addresses ?? []).map(text);
    return full || list.length <= 3 ? list : [...list.slice(0, 3), `(+${list.length - 3} more)`];
  }
  return {
    id: message.headerMessageId || `tb:${message.id}`,
    folderPath: message.folder.id,
    subject: text(message.subject),
    author: text(message.author),
    recipients: recipients(message.recipients),
    ccList: recipients(message.ccList),
    date: message.date,
    read: message.read,
    flagged: message.flagged,
    tags: message.tags,
  };
}

function flatten(root, depth = -1) {
  if (root.isVirtual || root.isUnified || root.isTag) return [];
  return [{ ...root, depth }, ...((root.subFolders ?? []).flatMap(folder => flatten(folder, depth + 1)))];
}

function bound(value, fallback, max) {
  return Math.max(0, Math.min(max, Math.trunc(value ?? fallback)));
}

function date(value) {
  const parsed = new Date(value);
  if (!Number.isFinite(parsed.getTime())) fail("bad_request", "Invalid date.");
  return parsed;
}

function searchTerms(query = "") {
  return Array.from(query.matchAll(/(?:(from|subject|to|cc):)?(?:"([^"]+)"|([^\s]+))/gi),
    ([, field, phrase, token]) => [field?.toLowerCase(), (phrase ?? token).toLowerCase()]);
}

function matches(message, terms, headersOnly) {
  const fields = {
    from: message.author,
    subject: message.subject,
    to: headersOnly ? "" : (message.recipients ?? []).join(" "),
    cc: headersOnly ? "" : (message.ccList ?? []).join(" "),
  };
  return terms.every(([field, token]) => (field ? fields[field] : Object.values(fields).join(" "))
    ?.toLowerCase().includes(token));
}

function isProtected(part) {
  const type = part.contentType?.split(";")[0].toLowerCase();
  return (part.decryptionStatus && part.decryptionStatus !== "none") ||
    ["multipart/encrypted", "application/pkcs7-mime", "application/x-pkcs7-mime", "application/pgp-encrypted"].includes(type) ||
    /-----BEGIN PGP MESSAGE-----/.test(part.body ?? "") || (part.parts ?? []).some(isProtected);
}

function bodyParts(part) {
  if (!part.partName?.startsWith("1.") && part.partName !== "" && part.partName !== "1") return [];
  if (part.headers?.["content-disposition"]?.some(value => /^attachment/i.test(value))) return [];
  if (part.contentType === "message/rfc822" && part.partName) return [];
  if (["text/plain", "text/html"].includes(part.contentType) && !part.name && part.body) return [part];
  return (part.parts ?? []).flatMap(bodyParts);
}

function attachmentParts(part, root = true) {
  if (!root && (part.name || part.contentType === "message/rfc822")) {
    return [{ name: text(part.name || "attached-message.eml"), contentType: part.contentType, size: part.size }];
  }
  return (part.parts ?? []).flatMap(child => attachmentParts(child, false));
}

export async function accountInventory(api, allowed, readDraftFolder) {
  const { accounts } = api;
  const selected = (await accounts.list(true)).filter(account => allowed.includes(account.id));
  const folders = {};
  const identities = {};
  const trashFolders = {};
  let defaultIdentity;
  const preferred = await accounts.getDefault(false);
  for (const account of selected) {
    const tree = account.rootFolder ? flatten(account.rootFolder) : [];
    const special = role => tree.find(folder => folder.specialUse?.includes(role))?.id ?? null;
    for (const folder of tree) folders[folder.id] = account.id;
    for (const { id, email } of account.identities) {
      const draftsFolder = readDraftFolder ? await readDraftFolder(id).catch(() => null) : special("drafts");
      identities[id] = { accountId: account.id, email, draftsFolder, sentFolder: special("sent") };
      defaultIdentity ??= id;
    }
    if (account.id === preferred?.id) {
      const identity = await accounts.getDefaultIdentity(account.id);
      if (identity && Object.hasOwn(identities, identity.id)) defaultIdentity = identity.id;
    }
    const trash = special("trash");
    if (trash) trashFolders[account.id] = trash;
  }
  return { accounts: selected.map(account => account.id), folders, identities, defaultIdentity, trashFolders };
}

export async function dispatchCore(api, op, args, signal, settings) {
  const { accounts, compose, folders, messages, tabs } = api;
  const scope = args._scope;
  const allowed = settings.allowedAccounts;
  const permits = id => allowed.includes(id) && (!scope || scope.accounts.includes(id));
  const headerOnly = id => scope?.headers.includes(id);
  const session = settings.messageSession;
  if (!Array.isArray(allowed) || !allowed.every(id => typeof id === "string")) {
    fail("access_denied", "Choose the accounts to expose in tb-agent settings.");
  }
  const allAccounts = await accounts.list(true);
  const accessible = allAccounts.filter(account => permits(account.id));
  const tree = accessible.flatMap(account => account.rootFolder ? flatten(account.rootFolder) : []);
  const byId = new Map(tree.map(folder => [folder.id, folder]));
  const special = (folder, type) => folder.specialUse?.includes(type);
  const whollyAllowed = allAccounts.every(account => allowed.includes(account.id) &&
    (!scope || args._draftAccounts?.includes(account.id)));
  const withinTrash = candidate => tree.some(parent => special(parent, "trash") && inScope(candidate, parent));

  function trashFolder(accountId) {
    const target = tree.find(item => item.accountId === accountId && special(item, "trash"));
    if (scope && target?.id !== args._trashFolder) fail("conflict", "Trash folder changed. Reconnect to refresh the account inventory.");
    return target;
  }

  function snapshot(message, messageId) {
    return { messageId, instanceId: `${session}:${message.id}`, folderPath: message.folder.id,
      read: message.read, flagged: message.flagged, tags: [...new Set(message.tags ?? [])].sort() };
  }

  function verify(expected, targets) {
    if (!scope) return;
    if (!Array.isArray(expected) || expected.length !== targets.length) fail("conflict", "Missing message snapshots.");
    for (const { message, messageId } of targets) {
      const current = snapshot(message, messageId);
      const before = expected.find(item => item.messageId === messageId);
      if (!before || Object.keys(current).some(key => JSON.stringify(current[key]) !== JSON.stringify(before[key]))) {
        fail("conflict", "A message changed since its snapshot.");
      }
    }
  }

  async function moveTracked(message, destination) {
    if (message.folder.id === destination.id) return messages.get(message.id);
    let notify;
    const completion = new Promise(resolve => { notify = resolve; });
    const listener = (source, target) => {
      if (source.messages.length !== 1 || source.messages[0].id !== message.id ||
          source.messages[0].folder?.id !== message.folder.id) return;
      const [moved] = target.messages;
      if (target.messages.length === 1 && moved.folder?.id === destination.id &&
          moved.headerMessageId === message.headerMessageId) notify(moved);
    };
    const cancelled = () => notify(null);
    messages.onMoved.addListener(listener);
    signal.addEventListener("abort", cancelled, { once: true });
    const timer = setTimeout(cancelled, 15_000);
    try {
      signal.throwIfAborted();
      await messages.move([message.id], destination.id);
      const moved = await completion;
      if (!moved) fail("operation_failed", "Thunderbird did not confirm the moved message instance.");
      return moved;
    } finally {
      clearTimeout(timer);
      signal.removeEventListener("abort", cancelled);
      messages.onMoved.removeListener(listener);
    }
  }

  async function afterWrite(targets, destination) {
    const result = [];
    for (const { message, messageId } of targets) {
      signal.throwIfAborted();
      const current = destination ? await moveTracked(message, destination) : await messages.get(message.id);
      if (current.external || current.folder?.id !== (destination?.id ?? message.folder.id) ||
          current.headerMessageId !== message.headerMessageId) {
        fail("operation_failed", "Thunderbird did not confirm the updated message instance.");
      }
      result.push(snapshot(current, messageId));
    }
    return result;
  }

  function inScope(candidate, parent, includeChildren = true) {
    return candidate.accountId === parent.accountId &&
      (candidate.id === parent.id ||
        (includeChildren && candidate.path.startsWith(parent.path.replace(/\/$/, "") + "/")));
  }

  function folder(id, root = false) {
    const result = byId.get(id);
    if (!result || (!root && result.isRoot)) fail("not_found", "Folder is unavailable.");
    return result;
  }

  async function* pages(list) {
    try {
      while (true) {
        signal.throwIfAborted();
        for (const message of list.messages) {
          // Recheck the returned folder, including for server-side or virtual aliases.
          if (byId.has(message.folder?.id) && !message.external) yield message;
        }
        if (!list.id) break;
        list = await messages.continueList(list.id);
      }
    } finally {
      if (list.id) await messages.abortList(list.id).catch(() => {});
    }
  }

  async function find(id, path) {
    if (typeof id !== "string" || !id) fail("bad_request", "A message ID is required.");
    const source = folder(path);
    const query = id.replace(/^<|>$/g, "");
    let found;
    for await (const message of pages(await messages.query({ folderId: source.id, headerMessageId: query }))) {
      if (message.folder.id !== source.id || message.headerMessageId !== query) continue;
      if (found && scope) fail("conflict", "Message ID is ambiguous in this folder.");
      found = message;
      if (!scope) break;
    }
    if (found) return found;
    if (/^tb:\d+$/.test(query)) {
      const message = await messages.get(Number(query.slice(3))).catch(() => undefined);
      if (message?.folder?.id === source.id && !message.external && !message.headerMessageId) return message;
    }
    fail("not_found", "Message is unavailable in that folder.");
  }

  async function headers(message) {
    const raw = await messages.getHeaders(message.id);
    const references = text(raw.references?.join(" "));
    const reply = raw["in-reply-to"]?.join(" ") ?? references;
    const ancestors = (references.match(/<[^<>\s]+>/g) ?? []).map(id => id.slice(1, -1));
    const inReplyTo = text(reply.match(/<[^<>\s]+>/g)?.at(-1)?.slice(1, -1));
    const output = { ...summarize(message), references, inReplyTo, size: message.size };
    // Use the stored outer Subject instead of an index that may contain a decrypted subject.
    if (settings.allowEncrypted !== true) output.subject = text(raw.subject?.[0]);
    if (ancestors.length) output.threadId = ancestors[0];
    return output;
  }

  async function outerSubject(message) {
    const raw = await messages.getHeaders(message.id).catch(() => null);
    return text(raw?.subject?.[0]);
  }

  async function messageContent(message) {
    const full = await messages.getFull(message.id, { decrypt: settings.allowEncrypted === true });
    if (settings.allowEncrypted !== true && isProtected(full)) {
      fail("access_denied", "Protected message content is withheld.");
    }
    if (full.decryptionStatus === "fail") fail("operation_failed", "Thunderbird could not decrypt the message.");
    return full;
  }

  async function read(message, options) {
    if (headerOnly(message.folder.accountId)) return headers(message);
    const full = await messageContent(message);
    const summary = summarize(message);
    if (settings.allowEncrypted !== true) summary.subject = await outerSubject(message);
    if (options.rawSource) {
      const rawSource = await messages.getRaw(message.id, { decrypt: false, data_format: "BinaryString" });
      return { id: summary.id, subject: summary.subject, rawSource };
    }
    const format = options.bodyFormat ?? "markdown";
    if (!["html", "text", "markdown"].includes(format)) fail("bad_request", "Unknown body format.");
    const parts = bodyParts(full);
    const html = parts.filter(part => part.contentType === "text/html").map(part => part.body).join("\n");
    const plain = parts.filter(part => part.contentType === "text/plain").map(part => part.body).join("\n");
    const body = format === "html" && html ? html : plain || (html && await api.messengerUtilities.convertToPlainText(html));
    return { ...summary, body: text(body), bodyFormat: html && format === "html" ? "html" : format === "markdown" ? "markdown" : "text",
      attachments: attachmentParts(full) };
  }

  async function search(recent) {
    const from = args.folderPath ? folder(args.folderPath, true) : null;
    const ids = tree.filter(item => !item.isRoot && (from ? inScope(item, from, args.includeSubfolders !== false) :
      !recent || args.includeSubfolders !== false))
      .map(item => item.id);
    const query = {};
    if (recent) query.fromDate = new Date(Date.now() - bound(args.daysBack, 7, 36500) * 86_400_000);
    if (args.startDate) query.fromDate = date(args.startDate);
    if (args.endDate) query.toDate = date(args.endDate);
    if (args.unreadOnly) query.unread = true;
    if (args.flaggedOnly) query.flagged = true;
    if (args.tag) query.tags = { mode: "all", tags: { [args.tag]: true } };
    const terms = searchTerms(args.query);
    const found = [];
    const selected = new Set(ids);
    if (ids.length) {
      for await (const message of pages(await messages.query({ ...query, folderId: ids }))) {
        signal.throwIfAborted();
        if (!selected.has(message.folder.id)) continue;
        const subject = settings.allowEncrypted === true ? message.subject : await outerSubject(message);
        const candidate = { ...message, subject };
        if (matches(candidate, terms, headerOnly(message.folder.accountId))) found.push(candidate);
      }
    }
    const direction = args.sortOrder === "asc" ? 1 : -1;
    found.sort((a, b) => direction * (a.date - b.date));
    const seen = new Map();
    const results = [];
    for (const message of found) {
      const key = !recent && args.dedupByMessageId !== false && message.headerMessageId;
      const existing = key && seen.get(key);
      if (existing) {
        if (existing.message.folder.id !== message.folder.id) existing.dupLocations.add(message.folder.id);
      } else {
        const result = { message, dupLocations: new Set() };
        results.push(result);
        if (key) seen.set(key, result);
      }
    }
    const offset = bound(args.offset, 0, Number.MAX_SAFE_INTEGER);
    const size = bound(args.maxResults, 50, 200);
    if (args.countOnly) return { count: results.length };
    const rows = results.slice(offset, offset + size).map(({ message, dupLocations }) => ({
      ...summarize(message, args.fullRecipients === true),
      ...(dupLocations.size ? { dupLocations: [...dupLocations] } : {}),
    }));
    if (args.offset !== undefined) return { messages: rows, totalMatches: results.length, offset, limit: size, hasMore: offset + size < results.length };
    return rows;
  }

  async function composeMessage(source) {
    if (args.sendAt || args.inReplyTo || args.references || args.includeSignature === false) {
      fail("unsupported_op", "This compose option requires the advanced mail module.");
    }
    const isDraft = op === "saveDraft" || (op === "replyToMessage" && args.saveAsDraft === true);
    // saveMessage uses the identity's configured draft destination. MailExtensions
    // cannot inspect or override it ahead of time.
    if (isDraft && !settings.readDraftFolder && !whollyAllowed) fail("access_denied", "The configured draft destination cannot be verified within the account policy.");
    const draftFolder = isDraft && scope ? folder(args._draftFolder) : null;
    if (source) {
      if (headerOnly(source.folder.accountId)) fail("access_denied", "Reply and forward require full message access.");
      verify(args._expected, [{ message: source, messageId: args.messageId }]);
    }
    if ((isDraft || args.skipReview === true) && source && isProtected(await messages.getFull(source.id, { decrypt: false }))) {
      fail("access_denied", "Open a review window for a protected original.");
    }
    let replacement;
    let replacementTrash;
    if (args.replaceMessageId !== undefined || args.replaceFolderPath !== undefined) {
      replacement = await find(args.replaceMessageId, args.replaceFolderPath);
      if (!special(folder(replacement.folder.id), "drafts")) fail("bad_request", "Only drafts can be replaced.");
      replacementTrash = trashFolder(replacement.folder.accountId);
      verify(args._expected, [{ message: replacement, messageId: args.replaceMessageId }]);
      if (!replacementTrash) fail("bad_request", "No separate Trash folder is available for the original draft.");
    }
    const preferred = source ? accessible.find(account => account.id === source.folder.accountId) : await accounts.getDefault(false);
    const identities = accessible.flatMap(account => account.identities);
    const identity = args.from ? identities.find(item => item.id === args.from || item.email.toLowerCase() === args.from.toLowerCase()) :
      (preferred && allowed.includes(preferred.id) && await accounts.getDefaultIdentity(preferred.id)) || identities[0];
    if (!identity || !identities.some(item => item.id === identity.id)) fail("access_denied", "No allowed sending identity.");
    const attachments = [];
    for (const attachment of args.attachments ?? []) {
      if (!attachment || typeof attachment.name !== "string" || typeof attachment.base64 !== "string") {
        fail("unsupported_op", "Pass attachments as {name, contentType, base64} or enable the advanced mail module for file paths.");
      }
      const bytes = Uint8Array.from(atob(attachment.base64), char => char.charCodeAt(0));
      attachments.push({ file: new File([bytes], attachment.name, { type: attachment.contentType }) });
    }
    const account = accessible.find(item => item.identities.some(candidate => candidate.id === identity.id));
    if (scope && (identity.id !== args.from || account.id !== args._identityAccount)) {
      fail("conflict", "Sending identity changed. Reconnect to refresh the account inventory.");
    }
    if (replacement && (draftFolder ? draftFolder.id !== replacement.folder.id : replacement.folder.accountId !== account.id)) {
      fail("bad_request", "The selected identity belongs to a different account than the original draft.");
    }
    if (isDraft && !draftFolder && !tree.some(item => item.accountId === account.id && special(item, "drafts"))) {
      fail("access_denied", "No Drafts folder in the selected identity's account.");
    }
    const details = { identityId: identity.id, attachments };
    // Keep the client's choice of Sent folder within the allowed account.
    const sent = scope ? (args._sentFolder && folder(args._sentFolder)) :
      tree.find(item => item.accountId === account.id && special(item, "sent"));
    details.overrideDefaultFccFolderId = sent?.id ?? "";
    details.additionalFccFolderId = null;
    for (const field of ["to", "cc", "bcc", "subject"]) {
      if (args[field] !== undefined) details[field] = args[field];
    }
    if (args.body !== undefined) details[args.isHtml ? "body" : "plainTextBody"] = args.body;
    signal.throwIfAborted();
    let tab;
    if (op === "replyToMessage") tab = await compose.beginReply(source.id, args.replyAll ? "replyToAll" : "replyToSender", details);
    else if (op === "forwardMessage") tab = await compose.beginForward(source.id, "forwardInline", details);
    else tab = await compose.beginNew(details);
    if (!isDraft && args.skipReview !== true) return { success: true, mode: "review" };
    if (!isDraft) {
      try {
        signal.throwIfAborted();
        await compose.sendMessage(tab.id, { mode: "sendNow" });
        return { success: true, mode: "sent" };
      } catch (error) {
        await tabs.remove(tab.id).catch(() => {});
        throw error;
      }
    }
    try {
      signal.throwIfAborted();
      if (draftFolder && settings.readDraftFolder && await settings.readDraftFolder(identity.id) !== draftFolder.id) {
        fail("conflict", "Draft folder changed. Reconnect to refresh the account inventory.");
      }
      signal.throwIfAborted();
      const saved = (await compose.saveMessage(tab.id, { mode: "draft" })).messages[0];
      if (!saved || !byId.has(saved.folder?.id) || (draftFolder && saved.folder.id !== draftFolder.id)) fail("operation_failed", "Check the identity's Copies & Folders settings for the saved draft.");
      if (replacement && replacement.folder.id !== saved.folder.id) {
        fail("operation_failed", "The new draft was saved in a different folder. The original draft was preserved.");
      }
      if (replacement && replacement.id !== saved.id) {
        signal.throwIfAborted();
        if (scope) await moveTracked(replacement, replacementTrash);
        else await messages.move([replacement.id], replacementTrash.id);
      }
      return { success: true, messageId: summarize(saved).id, folderPath: saved.folder.id };
    } finally {
      await tabs.remove(tab.id).catch(() => {});
    }
  }

  switch (op) {
    case "snapshotMessages": {
      const result = [];
      for (const id of args.messageIds) result.push(snapshot(await find(id, args.folderPath), id));
      return result;
    }
    case "restoreMessages": {
      const targets = [];
      const previous = args.expected.map(({ messageId }) => args.restore.find(item => item.messageId === messageId));
      const destinations = previous.map(item => folder(item.folderPath));
      if (destinations.some(item => special(item, "outbox"))) fail("conflict", "Cannot restore messages to Outbox.");
      for (const item of args.expected) {
        const source = folder(item.folderPath);
        const prefix = `${session}:`;
        const id = item.instanceId.startsWith(prefix) ? Number(item.instanceId.slice(prefix.length)) : NaN;
        const message = Number.isSafeInteger(id) && await messages.get(id).catch(() => null);
        if (!message || message.external || message.folder?.id !== source.id) {
          fail("conflict", "The audited message instance is no longer in the expected folder.");
        }
        targets.push({ message, messageId: item.messageId });
      }
      verify(args.expected, targets);
      if (destinations.some((item, index) => item.id !== targets[index].message.folder.id &&
          withinTrash(item)) && args._allowTrashMove !== true) {
        fail("conflict", "Restoring a message to Trash requires trash permission.");
      }
      const flagChanges = (message, { read, flagged, tags }) =>
        message.read !== read || message.flagged !== flagged ||
        JSON.stringify([...(message.tags ?? [])].sort()) !== JSON.stringify(tags);
      if (args._allowTagRestore !== true && targets.some(({ message }, index) => flagChanges(message, previous[index]))) {
        fail("conflict", "Restoring message flags requires tag permission.");
      }
      signal.throwIfAborted();
      for (const [index, { message }] of targets.entries()) {
        const { read, flagged, tags } = previous[index];
        const target = destinations[index];
        signal.throwIfAborted();
        const current = target.id !== message.folder.id ? await moveTracked(message, target) : message;
        if (flagChanges(current, previous[index])) {
          if (args._allowTagRestore !== true) fail("operation_failed", "Thunderbird changed the flags during the move.");
          await messages.update(current.id, { read, flagged, tags });
        }
      }
      return { success: true, restored: targets.length };
    }
    case "listAccounts":
      return Promise.all(accessible.map(async ({ id, name, type, identities }) => {
        const defaultIdentity = await accounts.getDefaultIdentity(id);
        return { id, name: text(name), type, identities: identities.map(identity => ({
          id: identity.id, email: text(identity.email), name: text(identity.name), isDefault: identity.id === defaultIdentity?.id,
        })) };
      }));
    case "getAccountAccess":
      return { mode: "restricted", accounts: accessible.map(({ id, name, type }) => ({ id, name: text(name), type })) };
    case "listFolders": {
      const parent = args.folderPath && folder(args.folderPath, true);
      const selected = tree.filter(item => !item.isRoot && (!args.accountId || item.accountId === args.accountId) &&
        (!parent || inScope(item, parent)) && (!args.favoritesOnly || item.isFavorite));
      const rows = await Promise.all(selected.map(async item => {
        const info = await folders.getFolderInfo(item.id).catch(() => null);
        const role = item.specialUse?.[0];
        const type = role === "archives" ? "archive" : role === "outbox" ? "queue" : role ?? "folder";
        return { name: text(item.name), path: item.id, type,
          accountId: item.accountId, totalMessages: info?.totalMessageCount ?? null, unreadMessages: info?.unreadMessageCount ?? null,
          depth: parent ? item.depth - parent.depth : item.depth, isFavorite: item.isFavorite };
      }));
      return args.format === "table" ? { columns: FOLDER_COLUMNS, rows: rows.map(row => FOLDER_COLUMNS.map(key => row[key])) } : rows;
    }
    case "searchMessages":
      if (args.searchBody) fail("unsupported_op", "Body search requires the Gloda module.");
      return search(false);
    case "getRecentMessages":
      return search(true);
    case "getMessageHeaders":
      return headers(await find(args.messageId, args.folderPath));
    case "batchGetMessageHeaders": {
      const ids = args.messageIds;
      if (ids.length > 200 || ids.some(id => typeof id !== "string" || !id)) {
        fail("bad_request", "Supply at most 200 non-empty message IDs.");
      }
      if (!ids.length) return { headers: {}, total: 0, failed: 0 };
      folder(args.folderPath);
      const collected = new Map();
      for (const id of new Set(ids)) {
        signal.throwIfAborted();
        try { collected.set(id, await headers(await find(id, args.folderPath))); }
        catch (error) {
          if (signal.aborted) throw error;
          collected.set(id, { error: error.code === "not_found" ? error.message : "Could not read message headers." });
        }
      }
      const failed = [...collected.values()].filter(result => result.error).length;
      return { headers: Object.fromEntries(collected), total: ids.length, failed };
    }
    case "getMessage":
    case "getMessages": {
      if (args.saveAttachments || args.includeInlineImages) fail("unsupported_op", "This attachment option requires the advanced mail module.");
      if (op === "getMessage") return read(await find(args.messageId, args.folderPath), args);
      if (!Array.isArray(args.messages) || args.messages.length < 1 || args.messages.length > 20) fail("bad_request", "Supply 1–20 messages.");
      const results = [];
      for (const [index, ref] of args.messages.entries()) {
        signal.throwIfAborted();
        try { results.push({ index, messageId: ref.messageId, folderPath: ref.folderPath, ...(await read(await find(ref.messageId, ref.folderPath), args)) }); }
        catch (error) {
          signal.throwIfAborted();
          results.push({ index, messageId: ref.messageId, folderPath: ref.folderPath,
            error: error instanceof ToolError ? error.message : "Thunderbird could not read the message." });
        }
      }
      const succeeded = results.filter(result => !result.error).length;
      return { messages: results, requested: results.length, succeeded, failed: results.length - succeeded, max: 20 };
    }
    case "displayMessage": {
      const message = await find(args.messageId, args.folderPath);
      const displayMode = args.displayMode ?? "3pane";
      if (!["3pane", "tab", "window"].includes(displayMode)) fail("bad_request", "Unknown display mode.");
      let subject = text(message.subject);
      if (settings.allowEncrypted !== true) {
        await messageContent(message);
        subject = await outerSubject(message);
      }
      signal.throwIfAborted();
      if (displayMode === "3pane") {
        const mailTabs = await api.mailTabs.query({ lastFocusedWindow: true });
        const mailTab = mailTabs.find(tab => tab.active) ?? mailTabs[0];
        if (!mailTab) fail("operation_failed", "No mail tab is available for 3pane display.");
        signal.throwIfAborted();
        await api.mailTabs.setSelectedMessages(mailTab.id, [message.id]);
        signal.throwIfAborted();
        if (!mailTab.active) await tabs.update(mailTab.id, { active: true });
      } else {
        await api.messageDisplay.open({ messageId: message.id, location: displayMode });
      }
      return { success: true, displayMode, subject };
    }
    case "updateMessage":
    case "deleteMessages": {
      const refs = args.messageIds ?? [args.messageId];
      if (!Array.isArray(refs) || !refs.length || refs.length > 200) fail("bad_request", "Supply 1–200 message IDs.");
      const source = folder(args.folderPath);
      const deleting = op === "deleteMessages";
      const changes = deleting ? {} : args;
      const trash = deleting ? (!!args._trashFolder && !withinTrash(source)) || special(source, "drafts") : changes.trash === true;
      if ((trash && (changes.moveTo || changes.copyTo)) || (changes.moveTo && changes.copyTo)) {
        fail("bad_request", "Move, copy and trash are mutually exclusive.");
      }
      const move = trash ? trashFolder(source.accountId) :
        changes.moveTo ? folder(changes.moveTo) : null;
      const copy = changes.copyTo ? folder(changes.copyTo) : null;
      if (trash && (!move || move.id === source.id)) fail("bad_request", "No separate Trash folder is available for this account.");
      if ([move, copy].some(item => item && special(item, "outbox"))) fail("access_denied", "Messages cannot be moved or copied to Outbox.");
      if (scope && move && !trash && withinTrash(move) && args._allowTrashMove !== true) {
        fail("access_denied", "Moving a message into Trash requires trash permission.");
      }
      const targets = [];
      for (const messageId of new Set(refs)) targets.push({ message: await find(messageId, source.id), messageId });
      verify(args._expected, targets);
      signal.throwIfAborted();
      for (const { message } of targets) {
        signal.throwIfAborted();
        const change = {};
        if (changes.read !== undefined) change.read = changes.read;
        if (changes.flagged !== undefined) change.flagged = changes.flagged;
        if (changes.addTags || changes.removeTags) {
          change.tags = [...new Set([...message.tags, ...(changes.addTags ?? [])])].filter(tag => !changes.removeTags?.includes(tag));
        }
        if (Object.keys(change).length) await messages.update(message.id, change);
      }
      const ids = targets.map(({ message }) => message.id);
      signal.throwIfAborted();
      if (copy) await messages.copy(ids, copy.id);
      signal.throwIfAborted();
      if (move && !scope) await messages.move(ids, move.id);
      if (deleting && !trash) {
        if (!withinTrash(source)) fail("access_denied", "No verified Trash is configured for this account.");
        await messages.delete(ids, { deletePermanently: true, isUserAction: true });
      }
      const result = { success: true, [deleting ? "deleted" : "updated"]: ids.length };
      if (deleting) result.movedToTrash = !!move;
      if (scope && !copy && (!deleting || move)) result._after = await afterWrite(targets, move);
      return result;
    }
    case "saveDraft":
    case "sendMail":
      return composeMessage();
    case "replyToMessage":
    case "forwardMessage":
      return composeMessage(await find(args.messageId, args.folderPath));
    default:
      fail("unsupported_op", "This operation requires an optional module.");
  }
}
