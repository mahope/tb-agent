const CHUNK_SIZE = 64 * 1024;
const SINGLE_FRAME = new Set(["snapshotMessages", "restoreMessages", "updateMessage", "deleteMessages",
  "saveDraft", "sendMail", "replyToMessage", "forwardMessage"]);
const MAX_FRAME = 16 * 1024 * 1024;
const MAX_RESULT = 256 * 1024 * 1024;

export function connectionInfo(value) {
  const host = value?.host;
  const loopback = typeof host === "string" && (host === "::1" ||
    (/^127(?:\.(?:0|[1-9]\d{0,2})){3}$/.test(host) && host.split(".").every(octet => Number(octet) <= 255)));
  if (value?.protocol !== 2 || !loopback ||
      !Number.isInteger(value.port) || value.port < 1 || value.port > 65535 ||
      typeof value.token !== "string" || !/^[0-9a-f]{64}$/.test(value.token)) {
    throw new Error("Select a valid tbd protocol v2 connection file with a loopback address.");
  }
  return { protocol: 2, host: value.host, port: value.port, token: value.token };
}

export class ToolError extends Error {
  constructor(code, message) {
    super(message);
    this.code = code;
  }
}

export function connectDaemon({ readConnection, capabilities, dispatch, setStatus }) {
  let socket;
  let cancelRequests;
  let retry;
  let generation = 0;
  let delay = 500;

  function reconnect() {
    const current = ++generation;
    clearTimeout(retry);
    cancelRequests?.();
    socket?.close(1000);
    socket = undefined;
    delay = 500;
    void connect(current);
  }

  function schedule(current) {
    if (current !== generation) return;
    setStatus("Reconnecting");
    retry = setTimeout(() => void connect(current), delay);
    delay = Math.min(30_000, delay * 2);
  }

  async function connect(current) {
    let info;
    let advertised;
    let ws;
    try {
      const config = await readConnection();
      if (current !== generation) return;
      if (!config) {
        setStatus("Select the daemon connection file");
        return;
      }
      info = connectionInfo(config);
      advertised = await capabilities();
      if (current !== generation) return;
      const host = info.host === "::1" ? "[::1]" : info.host;
      ws = new WebSocket(`ws://${host}:${info.port}/ws`, `tb.token.${info.token}`);
    } catch {
      schedule(current);
      return;
    }

    socket = ws;
    setStatus("Connecting");
    let ready = false;
    let lastPong = 0;
    const requests = new Map();
    function abortRequests() {
      for (const request of requests.values()) request.abort();
    }
    cancelRequests = abortRequests;
    const handshake = setTimeout(() => ws.close(4000), 10_000);
    const heartbeat = setInterval(() => {
      if (!ready) return;
      if (Date.now() - lastPong > 55_000) ws.close(4000);
      else send({ type: "ping" });
    }, 25_000);

    function send(frame) {
      if (ws.readyState === WebSocket.OPEN) ws.send(JSON.stringify(frame));
    }

    async function run({ id, op, params }) {
      const controller = new AbortController();
      requests.set(id, controller);
      const { signal } = controller;
      try {
        const result = await dispatch(op, params ?? {}, signal, advertised.modules);
        if (signal.aborted) return;
        const json = JSON.stringify(result ?? null);
        if (new TextEncoder().encode(json).length > MAX_RESULT) {
          throw new ToolError("payload_too_large", "Result exceeds 256 MiB.");
        }
        if (json.length <= CHUNK_SIZE || SINGLE_FRAME.has(op)) {
          const response = { type: "response", id, ok: true, result: result ?? null };
          if (SINGLE_FRAME.has(op) && new TextEncoder().encode(JSON.stringify(response)).length > MAX_FRAME) {
            throw new ToolError("payload_too_large", "Audit result exceeds a single frame.");
          }
          send(response);
          return;
        }
        let seq = 0;
        for (let start = 0; start < json.length;) {
          if (signal.aborted) return;
          let end = Math.min(start + CHUNK_SIZE, json.length);
          const last = json.charCodeAt(end - 1);
          if (end < json.length && last >= 0xd800 && last <= 0xdbff) end--;
          send({ type: "chunk", id, seq: seq++, data: json.slice(start, end) });
          start = end;
          // Let cancellation/close events run and bound the socket's outgoing queue.
          do {
            await new Promise(resolve => setTimeout(resolve, 0));
          } while (!signal.aborted && ws.bufferedAmount > 1024 * 1024);
        }
        if (!signal.aborted) send({ type: "response", id, ok: true, streamed: true });
      } catch (error) {
        if (!signal.aborted) {
          send({ type: "response", id, ok: false, error: {
            code: error instanceof ToolError ? error.code : "operation_failed",
            message: error instanceof ToolError ? error.message : "Thunderbird could not complete the operation.",
          } });
        }
      } finally {
        requests.delete(id);
      }
    }

    ws.onopen = () => send({
      type: "hello", protocol: 2, role: "extension",
      name: `tb-agent extension ${advertised.extensionVersion}`, capabilities: advertised,
    });
    ws.onmessage = event => {
      if (current !== generation) return;
      let frame;
      try { frame = JSON.parse(event.data); } catch { return; }
      if (!frame || typeof frame !== "object") return;
      if (frame.type === "welcome" && frame.protocol === 2 && !ready) {
        clearTimeout(handshake);
        ready = true;
        lastPong = Date.now();
        delay = 500;
        setStatus("Connected");
      } else if (ready && frame.type === "pong") {
        lastPong = Date.now();
      } else if (ready && frame.type === "ping") {
        send({ type: "pong" });
      } else if (ready && frame.type === "cancel") {
        requests.get(frame.id)?.abort();
      } else if (ready && frame.type === "request" && typeof frame.id === "string" && !requests.has(frame.id)) {
        void run(frame);
      }
    };
    ws.onclose = event => {
      abortRequests();
      clearTimeout(handshake);
      clearInterval(heartbeat);
      if (current !== generation) return;
      if (event.code === 1012) setStatus("Replaced by another Thunderbird extension");
      else schedule(current);
    };
  }

  reconnect();
  return reconnect;
}
