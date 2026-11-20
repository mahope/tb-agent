"use strict";

const assert = require("node:assert/strict");
const { test } = require("node:test");
const { setImmediate, setTimeout } = require("node:timers/promises");

const address = { protocol: 2, host: "127.0.0.1", port: 42123, token: "ab".repeat(32) };

test("only accepts protocol v2 connection files with numeric loopback addresses", async () => {
  const { connectionInfo } = await import("../extension/daemon-transport.mjs");
  assert.deepEqual(connectionInfo({ ...address, pid: 123 }), address);
  assert.equal(connectionInfo({ ...address, host: "::1" }).host, "::1");
  for (const host of ["localhost", "127.0.0.1.example.invalid", "127.0.0.256", "0.0.0.0", "127.1", "0x7f.0.0.1"]) {
    assert.throws(() => connectionInfo({ ...address, host }));
  }
  assert.throws(() => connectionInfo({ ...address, token: "wrong" }));
  assert.throws(() => connectionInfo({ ...address, port: 65536 }));
});

test("advertises capabilities, streams Unicode intact and drops cancelled responses", async () => {
  const { connectDaemon } = await import("../extension/daemon-transport.mjs");
  const previous = global.WebSocket;
  const sockets = [];
  class FakeSocket {
    static OPEN = 1;
    readyState = 1;
    bufferedAmount = 0;
    frames = [];
    constructor(url, token) {
      Object.assign(this, { url, token });
      sockets.push(this);
    }
    send(frame) { this.frames.push(JSON.parse(frame)); }
    incoming(frame) { this.onmessage({ data: JSON.stringify(frame) }); }
    close(code) {
      this.readyState = 3;
      this.onclose({ code });
    }
  }
  global.WebSocket = FakeSocket;
  const body = "a".repeat(65_534) + "🦉".repeat(100_000);
  const modules = ["core", "experiment.gloda"];
  let signal;
  let finish;
  const states = [];
  try {
    connectDaemon({
      readConnection: async () => address,
      capabilities: async () => ({ thunderbird: "154.0", extensionVersion: "0.10.0", modules }),
      dispatch(op, _args, controllerSignal) {
        if (op === "getMessage") return body;
        signal = controllerSignal;
        return new Promise(resolve => { finish = resolve; });
      },
      setStatus: status => states.push(status),
    });
    await setImmediate();
    const [socket] = sockets;
    assert.equal(socket.url, `ws://${address.host}:${address.port}/ws`);
    assert.equal(socket.token, `tb.token.${address.token}`);
    socket.onopen();
    assert.deepEqual(socket.frames[0].capabilities.modules, modules);
    socket.incoming({ type: "request", id: "early", op: "getMessage" });
    assert.equal(socket.frames.length, 1);
    socket.incoming({ type: "welcome", protocol: 2 });
    socket.incoming({ type: "request", id: "cancelled", op: "searchMessages" });
    socket.incoming({ type: "cancel", id: "cancelled" });
    assert.equal(signal.aborted, true);
    finish("discarded");
    socket.incoming({ type: "request", id: "live", op: "getMessage" });
    for (let attempts = 0; attempts < 100 && !socket.frames.some(frame => frame.streamed); attempts++) {
      await setTimeout(2);
    }
    const chunks = socket.frames.filter(frame => frame.type === "chunk");
    assert.deepEqual(chunks.map(frame => frame.seq), chunks.map((_, index) => index));
    assert.ok(chunks.every(frame => !/[\uD800-\uDBFF]$/.test(frame.data)));
    assert.equal(JSON.parse(chunks.map(frame => frame.data).join("")), body);
    assert.deepEqual(socket.frames.at(-1), { type: "response", id: "live", ok: true, streamed: true });
    assert.ok(socket.frames.every(frame => frame.id !== "cancelled"));
    assert.deepEqual(states, ["Connecting", "Connected"]);
  } finally {
    for (const socket of sockets) socket.close(1012);
    global.WebSocket = previous;
  }
});

test("protocol v2 background enforces daemon scope before accessing Thunderbird", async () => {
  const globals = { WebSocket: global.WebSocket, browser: global.browser, crypto: global.crypto };
  const sockets = [];
  class Socket {
    static OPEN = 1;
    readyState = 1;
    frames = [];
    constructor() { sockets.push(this); }
    send(frame) { this.frames.push(JSON.parse(frame)); }
    incoming(frame) { this.onmessage({ data: JSON.stringify(frame) }); }
    close() { this.onclose({ code: 1012 }); }
  }
  const account = id => ({
    id, identities: [{ id: `identity-${id}`, email: `${id}@example.invalid` }],
    rootFolder: { id: `${id}://`, accountId: id, isRoot: true, subFolders: [
      { id: `${id}://Inbox`, accountId: id, path: "/Inbox", specialUse: ["inbox"] },
      { id: `${id}://Trash`, accountId: id, path: "/Trash", specialUse: ["trash"] },
      { id: `${id}://Drafts`, accountId: id, path: "/Drafts", specialUse: ["drafts"] },
    ] },
  });
  const work = account("work");
  const personal = account("personal");
  let message = { id: 42, headerMessageId: "message", folder: work.rootFolder.subFolders[0],
    read: false, flagged: false, tags: [] };
  const events = [];
  const moveListeners = new Set();
  const listeners = { addListener() {} };
  if (!globals.crypto) global.crypto = require("node:crypto").webcrypto;
  Object.assign(global, {
    WebSocket: Socket,
    browser: {
      runtime: { getBrowserInfo: async () => ({ version: "154.0" }), getManifest: () => ({ version: "0.10.0" }), onMessage: listeners },
      storage: { local: { get: async () => ({ daemonConnection: address, daemonSettings: { allowedAccounts: ["work", "personal"] } }) },
        onChanged: listeners },
      accounts: { list: async () => [work, personal], getDefault: async () => work,
        getDefaultIdentity: async () => work.identities[0] },
      folders: {},
      messages: {
        query: async ({ folderId }) => ({ messages: message.folder.id === folderId ? [message] : [] }),
        get: async () => message,
        update: async (_id, changes) => { events.push("mark"); message = { ...message, ...changes }; },
        onMoved: { addListener: fn => moveListeners.add(fn), removeListener: fn => moveListeners.delete(fn) },
        move: async (_ids, target) => {
          events.push("move");
          const source = message;
          message = { ...message, id: message.id + 1, folder: work.rootFolder.subFolders.find(folder => folder.id === target) };
          for (const fn of moveListeners) fn({ messages: [source] }, { messages: [message] });
        },
        delete: async (_ids, options) => { events.push(options.deletePermanently ? "permanent delete" : "delete"); },
      },
      compose: {
        beginNew: async details => { events.push(details.identityId); return { id: 5 }; },
        saveMessage: async () => {
          events.push("draft");
          return { messages: [{ ...message, folder: work.rootFolder.subFolders[2] }] };
        },
      },
      tabs: { remove: async () => {} },
    },
  });
  try {
    await import("../extension/daemon-background.mjs");
    await setImmediate();
    const [socket] = sockets;
    socket.onopen();
    assert.equal(socket.frames[0].protocol, 2);
    assert.equal(socket.frames[0].capabilities.inventory.folders["work://Trash"], "work");
    assert.equal(socket.frames[0].capabilities.inventory.defaultIdentity, "identity-work");
    socket.incoming({ type: "welcome", protocol: 2 });
    let nextId = 0;
    async function request(op, params) {
      const id = String(++nextId);
      socket.incoming({ type: "request", id, op, params });
      await setImmediate();
      const response = socket.frames.find(frame => frame.id === id);
      assert.ok(response, `missing response for ${op}`);
      return response;
    }
    const _scope = { accounts: ["work"], headers: [] };
    const ref = { folderPath: "work://Inbox", messageId: "message" };
    const denied = { ...ref, folderPath: "personal://Inbox", _scope };
    for (const [op, args] of [
      ["sendMail", { from: "identity-personal", _identityAccount: "personal", _scope }],
      ["saveDraft", { from: "identity-work", _identityAccount: "work", _draftFolder: "work://Drafts",
        _draftAccounts: ["work"], _scope }],
      ["updateMessage", { ...denied, read: true }],
      ["updateMessage", { ...denied, trash: true }],
      ["deleteMessages", { ...denied, messageIds: ["message"] }],
      ["updateMessage", { ...ref, moveTo: "work://Trash", _scope }],
      ["updateMessage", { ...ref, read: true, _scope }],
    ]) assert.equal((await request(op, args)).ok, false, op);
    assert.deepEqual(events, []);
    const snap = async folderPath =>
      (await request("snapshotMessages", { folderPath, messageIds: ["message"], _scope })).result;
    const before = await snap(ref.folderPath);
    const marked = await request("updateMessage", { ...ref, read: true, _scope, _expected: before });
    assert.equal(marked.result._after[0].read, true);
    assert.equal((await request("updateMessage", { ...ref, read: false, _scope, _expected: before })).error.code, "conflict");
    const trashed = await request("updateMessage", { ...ref, trash: true, _trashFolder: "work://Trash", _scope,
      _expected: marked.result._after });
    assert.equal(trashed.result._after[0].folderPath, "work://Trash");
    assert.notEqual(trashed.result._after[0].instanceId, marked.result._after[0].instanceId);
    const restored = await request("restoreMessages", { expected: trashed.result._after, restore: before, _scope, _allowTagRestore: true });
    assert.equal(restored.result.restored, 1);
    assert.equal(message.read, false);
    const deleted = await request("deleteMessages", { folderPath: ref.folderPath, messageIds: ["message"],
      _scope, _expected: await snap(ref.folderPath), _trashFolder: "work://Trash" });
    assert.equal(deleted.result.movedToTrash, true);
    assert.equal((await request("deleteMessages", { folderPath: "work://Trash", messageIds: ["message"],
      _scope, _expected: deleted.result._after, _trashFolder: "work://Trash" })).result.movedToTrash, false);
    const identity = { from: "identity-work", _identityAccount: "work", _scope };
    assert.equal((await request("sendMail", identity)).result.mode, "review");
    assert.equal((await request("saveDraft", { ...identity, _draftFolder: "work://Drafts",
      _draftAccounts: ["work", "personal"] })).result.folderPath, "work://Drafts");
    assert.deepEqual(events, ["mark", "move", "move", "mark", "move", "permanent delete", "identity-work", "identity-work", "draft"]);
  } finally {
    for (const socket of sockets) socket.close();
    Object.assign(global, { WebSocket: globals.WebSocket, browser: globals.browser });
    if (!globals.crypto) delete global.crypto;
  }
});
