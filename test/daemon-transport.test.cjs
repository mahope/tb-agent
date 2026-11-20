"use strict";

const assert = require("node:assert/strict");
const { test } = require("node:test");
const { setImmediate, setTimeout } = require("node:timers/promises");

const address = { protocol: 1, host: "127.0.0.1", port: 42123, token: "ab".repeat(32) };

test("only accepts protocol v1 connection files with numeric loopback addresses", async () => {
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
    socket.incoming({ type: "welcome", protocol: 1 });
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
