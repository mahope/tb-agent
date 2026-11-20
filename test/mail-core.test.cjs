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
