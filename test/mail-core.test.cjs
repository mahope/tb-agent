"use strict";

const { test } = require("node:test");
const assert = require("node:assert/strict");

function account(id) {
  const children = { id: `${id}://Inbox`, accountId: id, path: "/Inbox", specialUse: ["inbox"] };
  return {
    id, identities: [], rootFolder: {
      id: `${id}://`, accountId: id, path: "/", isRoot: true, subFolders: [children],
    },
  };
}

test("search only queries granted folders and ignores results attributed elsewhere", async () => {
  const { dispatchCore } = await import("../extension/mail-core.mjs");
  const primary = account("primary");
  const privateAccount = account("private");
  const calls = [];
  const api = {
    accounts: { list: async () => [primary, privateAccount] },
    messages: {
      getHeaders: async () => ({ subject: ["report"] }),
      async query(query) {
        calls.push(query);
        return { messages: [primary, privateAccount].map(({ id, rootFolder }) => ({
          headerMessageId: id, folder: rootFolder.subFolders[0], subject: "report", date: new Date(0),
        })) };
      },
    },
  };
  const signal = new AbortController().signal;
  assert.deepEqual(await dispatchCore(api, "searchMessages", { query: "report" }, signal, { allowedAccounts: [] }), []);
  assert.deepEqual(calls, []);
  const visible = await dispatchCore(api, "searchMessages", { query: "report" }, signal, { allowedAccounts: ["primary"] });
  assert.deepEqual(visible.map(message => message.id), ["primary"]);
  assert.deepEqual(calls.map(call => call.folderId), [["primary://Inbox"]]);
});

test("a move to a restricted destination fails before updating the message", async () => {
  const { dispatchCore } = await import("../extension/mail-core.mjs");
  const api = { accounts: { list: async () => [account("primary"), account("private")] } };
  await assert.rejects(
    dispatchCore(api, "updateMessage", {
      messageId: "example", folderPath: "primary://Inbox", moveTo: "private://Inbox", read: true,
    }, new AbortController().signal, { allowedAccounts: ["primary"] }),
    { code: "not_found" },
  );
});

async function mailFixture() {
  const { dispatchCore } = await import("../extension/mail-core.mjs");
  const primary = account("primary");
  const inbox = primary.rootFolder.subFolders[0];
  const archive = { ...inbox, id: "primary://Archive", path: "/Archive", specialUse: ["archives"] };
  primary.rootFolder.subFolders.push(archive);
  const messages = [
    { id: 1, headerMessageId: "one", folder: inbox, subject: "decrypted", date: new Date() },
    { id: 2, headerMessageId: "two", folder: archive, subject: "second", date: new Date() },
    { id: 3, headerMessageId: "one", folder: archive, subject: "decrypted", date: new Date(0) },
  ];
  const api = {
    accounts: { list: async () => [primary] },
    messages: {
      query: async ({ folderId, headerMessageId, fromDate }) => ({
        messages: messages.filter(message => [folderId].flat().includes(message.folder.id) &&
          (!headerMessageId || message.headerMessageId === headerMessageId) && (!fromDate || message.date >= fromDate)),
      }),
      getHeaders: async id => ({ subject: [id === 2 ? "second" : "outer"] }),
      getFull: async () => ({ partName: "1", contentType: "text/html", body: "<b>content</b>" }),
    },
    messengerUtilities: { convertToPlainText: async html => html.replace(/<[^>]+>/g, "") },
  };
  const run = (op, args, allowEncrypted = false) =>
    dispatchCore(api, op, args, new AbortController().signal, { allowedAccounts: ["primary", "secondary"], allowEncrypted });
  return { api, run, primary, inbox, archive, messages };
}

test("search matches and counts outer subjects until encrypted mail is enabled", async () => {
  const { run, archive } = await mailFixture();
  assert.deepEqual(await run("searchMessages", { query: "subject:decrypted", countOnly: true }), { count: 0 });
  const [row] = await run("searchMessages", { query: "subject:outer" });
  assert.equal(row.subject, "outer");
  assert.deepEqual(row.dupLocations, [archive.id]);
  assert.deepEqual(await run("searchMessages", { query: "subject:decrypted", countOnly: true }, true), { count: 1 });
  const recent = await run("getRecentMessages", {});
  assert.deepEqual(recent.map(item => item.subject).sort(), ["outer", "second"]);
});

test("HTML-only reads use messengerUtilities and retain batch entries around a read failure", async () => {
  const { api, run, inbox, archive } = await mailFixture();
  const getFull = api.messages.getFull;
  api.messages.getFull = async id => {
    if (id === 2) throw new Error("Provider internal details");
    return getFull();
  };
  const refs = [
    { messageId: "one", folderPath: inbox.id },
    { messageId: "two", folderPath: archive.id },
    { messageId: "one", folderPath: archive.id },
  ];
  assert.equal((await run("getMessage", refs[0])).body, "content");
  const batch = await run("getMessages", { messages: refs });
  assert.equal(batch.succeeded, 2);
  assert.equal(batch.failed, 1);
  assert.deepEqual(batch.messages.map(row => row.body ?? row.error), ["content", "Thunderbird could not read the message.", "content"]);
});

test("display requires encrypted opt-in and defaults to selection in a mail tab", async () => {
  const { api, run, inbox } = await mailFixture();
  const calls = [];
  api.messages.getFull = async (id, options) => {
    assert.equal(options.decrypt, false);
    return { contentType: "multipart/encrypted" };
  };
  api.messageDisplay = { open: async options => calls.push(options) };
  api.mailTabs = { query: async () => [{ id: 17, active: true }], setSelectedMessages: async (...args) => calls.push(args) };
  api.tabs = { update: async () => {} };
  const ref = { messageId: "one", folderPath: inbox.id };
  await assert.rejects(run("displayMessage", ref), { code: "access_denied" });
  assert.deepEqual(calls, []);
  await run("displayMessage", ref, true);
  await run("displayMessage", { ...ref, displayMode: "window" }, true);
  assert.deepEqual(calls, [[17, [1]], { messageId: 1, location: "window" }]);
});

test("replacing a draft rejects another account and preserves the original if Thunderbird saves elsewhere", async () => {
  const { api, run, primary, inbox, archive, messages } = await mailFixture();
  const secondary = account("secondary");
  secondary.rootFolder.subFolders[0].specialUse = ["drafts"];
  primary.identities = [{ id: "sender1", email: "one@example.invalid" }];
  secondary.identities = [{ id: "sender2", email: "two@example.invalid" }];
  inbox.specialUse = ["drafts"];
  primary.rootFolder.subFolders.push({ ...archive, id: "primary://Trash", path: "/Trash", specialUse: ["trash"] });
  api.accounts.list = async () => [primary, secondary];
  api.accounts.getDefault = async () => primary;
  let opened = 0;
  api.compose = { beginNew: async () => ({ id: ++opened }), saveMessage: async () => ({ messages: [messages[2]] }) };
  api.tabs = { remove: async () => {} };
  api.messages.move = async () => assert.fail("Original draft must be preserved");
  const ref = { replaceMessageId: "one", replaceFolderPath: inbox.id };
  await assert.rejects(run("saveDraft", { ...ref, from: "sender2" }), { code: "bad_request" });
  assert.equal(opened, 0);
  await assert.rejects(run("saveDraft", { ...ref, from: "sender1" }), { code: "operation_failed" });
});
