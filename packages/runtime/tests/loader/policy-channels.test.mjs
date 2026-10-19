import assert from "node:assert/strict";
import test from "node:test";
import { compileRuntime, moduleUrl } from "../compiled-runtime.mjs";

const cloudflare = moduleUrl(`
  export let env = {};
  export const pending = [];
  export function withEnv(value, action) {
    const before = env;
    env = value;
    try {
      const result = action();
      if (result && typeof result.then === "function")
        return Promise.resolve(result).finally(() => { env = before; });
      env = before;
      return result;
    } catch (error) { env = before; throw error; }
  }
  export function waitUntil(promise) { pending.push(promise); promise.catch(() => {}); }
  export const exports = {};
  export function withExports(_value, action) { return action(); }
  export class WorkerEntrypoint {}
`);
const capabilities = moduleUrl(`
  export const completions = [];
  export const associations = new WeakMap();
  export function channelWebSocketHandoffs(response) { return associations.get(response) ?? []; }
  export function detachChannelWebSocketHandoffs(response) {
    associations.delete(response);
    response.headers.delete("x-open-compute-service-websocket-handoffs");
    return response;
  }
  export function completeServiceScope(_env, frame) { completions.push(frame.scopeId); }
  export const attachServiceWebSocketHandoffs = value => value;
  export const decodeServiceValue = value => value;
  export const encodeServiceValue = value => value;
  export const serviceCapabilityController = () => {};
  export const serviceMember = action => action;
`);
const scopeUrl = moduleUrl(
  await compileRuntime("services/scope.ts", {
    "cloudflare:workers": cloudflare,
  }),
);
const completion = moduleUrl(
  await compileRuntime("loader/wrappers/completion.ts", {
    "../../services/facade.js": capabilities,
  }),
);
const runtimeUrl = moduleUrl(
  await compileRuntime("loader/wrappers/runtime.ts", {
    "cloudflare:workers": cloudflare,
    "cloudflare-internal:workers": moduleUrl(
      "export default { withWaitUntilObserver(_observer, action) { return action(); } };",
    ),
    "cloudflare-internal:wrapped-binding": moduleUrl(
      "export default { createServiceRpcStub: value => value }",
    ),
    "../../services/capabilities.js": capabilities,
    "../../services/facade.js": capabilities,
    "../../services/rpc-member.js": moduleUrl(
      await compileRuntime("services/rpc-member.ts"),
    ),
    "../../services/scope.js": scopeUrl,
    "./completion.js": completion,
    "./loopback.js": moduleUrl(
      "export const tenantExports = () => ({}); export const loopbackDurableObjectMetadata = () => {};",
    ),
  }),
);
const { createPolicyChannelCallbacks } = await import(runtimeUrl);
const { currentServiceFrame, rootServiceFrame, withServiceScope } =
  await import(scopeUrl);
const workersMock = await import(cloudflare);
const { pending, withEnv } = workersMock;
const { completions, associations } = await import(capabilities);

function deferred() {
  let resolve;
  let reject;
  const promise = new Promise((done, fail) => {
    resolve = done;
    reject = fail;
  });
  return { promise, resolve, reject };
}

async function drainTasks() {
  const tasks = pending.splice(0);
  await Promise.all(tasks);
}

test("channel registration captures declared env descriptors but not a previous Service frame", async () => {
  const binding = Object.freeze({ declared: true });
  const variables = { BINDING: binding };
  Object.defineProperty(variables, "TOKEN", {
    value: "declared-value",
    enumerable: false,
  });
  const old = rootServiceFrame();
  const seen = [];
  const channel = withServiceScope(variables, old, () =>
    createPolicyChannelCallbacks({
      async fetch() {
        seen.push({
          frame: currentServiceFrame(),
          binding: workersMock.env.BINDING,
          token: Object.getOwnPropertyDescriptor(workersMock.env, "TOKEN"),
          keys: Reflect.ownKeys(workersMock.env),
        });
        return new Response("body");
      },
      connect() {
        throw Error("not called");
      },
    }),
  );
  for (let index = 0; index < 2; index++) {
    const { response, leaseBody, errorCode } = await withEnv(
      { PRIVATE_POLICY: "not captured" },
      () => channel.fetch(new Request("https://public/")),
    );
    assert.equal(leaseBody, null);
    assert.equal(errorCode, null);
    assert.equal(await response.text(), "body");
  }
  await drainTasks();
  assert.equal(seen.length, 2);
  assert.notEqual(seen[0].frame.scopeId, old.scopeId);
  assert.notEqual(seen[1].frame.scopeId, seen[0].frame.scopeId);
  for (const value of seen) {
    assert.equal(value.frame.parentFrame, null);
    assert.equal(value.binding, binding);
    assert.deepEqual(value.token, {
      value: "declared-value",
      writable: false,
      enumerable: false,
      configurable: false,
    });
    assert.equal(value.keys.includes("PRIVATE_POLICY"), false);
    assert.equal(value.keys.filter((key) => typeof key === "symbol").length, 1);
    assert.ok(completions.includes(value.frame.scopeId));
  }
});

test("late response cancellation completes a fresh root scope after real body cancellation", async () => {
  const ready = deferred();
  const end = deferred();
  let canceled = false;
  let frame;
  const channel = withEnv({ DECLARED: "ok" }, () =>
    createPolicyChannelCallbacks({
      async fetch() {
        frame = currentServiceFrame();
        ready.resolve();
        await end.promise;
        return new Response(
          new ReadableStream({
            cancel() {
              canceled = true;
            },
          }),
        );
      },
      connect() {
        throw Error("not called");
      },
    }),
  );
  const result = channel.fetch(new Request("https://late/"));
  await ready.promise;
  assert.equal(completions.includes(frame.scopeId), false);
  end.resolve();
  const { response } = await result;
  await response.body.cancel();
  await drainTasks();
  assert.equal(canceled, true);
  assert.ok(completions.includes(frame.scopeId));
});

test("channel TCP completion follows opened and real closed, not the returned Socket handle", async () => {
  const opened = deferred();
  const closed = deferred();
  const socket = { opened: opened.promise, closed: closed.promise };
  let frame;
  const channel = withEnv({ DECLARED: "ok" }, () =>
    createPolicyChannelCallbacks({
      async fetch() {
        throw Error("not called");
      },
      connect(address) {
        assert.equal(address, "public:443");
        frame = currentServiceFrame();
        return socket;
      },
    }),
  );
  let returned = false;
  const result = channel.connect("public:443").then((value) => {
    returned = true;
    return value;
  });
  await Promise.resolve();
  assert.equal(returned, false);
  opened.resolve({});
  assert.equal(await result, socket);
  assert.equal(completions.includes(frame.scopeId), false);
  closed.resolve();
  await drainTasks();
  assert.ok(completions.includes(frame.scopeId));
});

test("channel TCP opened rejection waits closed cleanup and preserves the error", async () => {
  const opened = deferred();
  const closed = deferred();
  let frame;
  const channel = withEnv({}, () =>
    createPolicyChannelCallbacks({
      async fetch() {
        throw Error("not called");
      },
      connect() {
        frame = currentServiceFrame();
        return { opened: opened.promise, closed: closed.promise };
      },
    }),
  );
  const failure = assert.rejects(
    channel.connect("public:443"),
    /connect denied/,
  );
  opened.reject(Error("connect denied"));
  await failure;
  assert.equal(completions.includes(frame.scopeId), false);
  closed.reject(Error("connection closed"));
  await drainTasks();
  assert.ok(completions.includes(frame.scopeId));
});

test("channel handoff activates before root completion and keeps private ownership out of headers", async () => {
  const lease = new ReadableStream({});
  const response = new Response(null, {
    headers: { "x-open-compute-service-websocket-handoffs": "private" },
  });
  Object.defineProperty(response, "webSocket", { value: {} });
  associations.set(response, ["issued-handoff"]);
  const activate = deferred();
  let frame;
  const channel = withEnv({}, () =>
    createPolicyChannelCallbacks({
      async fetch() {
        frame = currentServiceFrame();
        return response;
      },
      connect() {
        throw Error("not called");
      },
      async hold(handles) {
        assert.deepEqual(handles, ["issued-handoff"]);
        assert.equal(completions.includes(frame.scopeId), false);
        await activate.promise;
        return lease;
      },
    }),
  );
  const result = channel.fetch(new Request("https://public/"));
  await Promise.resolve();
  assert.equal(completions.includes(frame.scopeId), false);
  activate.resolve();
  const packet = await result;
  assert.equal(
    packet.response.headers.has("x-open-compute-service-websocket-handoffs"),
    false,
  );
  assert.equal(packet.leaseBody, lease);
  assert.equal(packet.errorCode, null);
  assert.equal(associations.has(response), false);
  await packet.leaseBody.cancel();
  await drainTasks();
});

test("failed channel activation retains the Response in a fixed failure packet for native cleanup", async () => {
  const response = new Response(null);
  Object.defineProperty(response, "webSocket", { value: {} });
  associations.set(response, ["issued-handoff"]);
  const channel = withEnv({}, () =>
    createPolicyChannelCallbacks({
      async fetch() {
        return response;
      },
      connect() {
        throw Error("not called");
      },
      async hold() {
        throw Error("private cause must not cross this boundary");
      },
    }),
  );
  const packet = await channel.fetch(new Request("https://public/"));
  assert.equal(packet.response.webSocket, response.webSocket);
  assert.equal(packet.leaseBody, null);
  assert.equal(packet.errorCode, "SERVICE_UNAVAILABLE");
  assert.equal(associations.has(response), false);
  await drainTasks();
});

test("unassociated DO or multihop response headers cannot mint a second WebSocket ownership", async () => {
  const response = new Response(null, {
    headers: {
      "x-open-compute-service-websocket-handoffs": "untrusted-header",
    },
  });
  Object.defineProperty(response, "webSocket", { value: {} });
  let activations = 0;
  const channel = withEnv({ DECLARED: "ok" }, () =>
    createPolicyChannelCallbacks({
      async fetch() {
        return response;
      },
      connect() {
        throw Error("not called");
      },
      async hold() {
        activations++;
        throw Error("must not activate");
      },
    }),
  );
  const packet = await channel.fetch(new Request("https://public/"));
  assert.equal(packet.response.webSocket, response.webSocket);
  assert.equal(
    packet.response.headers.has("x-open-compute-service-websocket-handoffs"),
    false,
  );
  assert.equal(packet.leaseBody, null);
  assert.equal(packet.errorCode, null);
  assert.equal(activations, 0);
  await drainTasks();
});

test("issued Service channel owns the normal WebSocket handoff and completes its fresh scope", async () => {
  const facadeUrl = moduleUrl(
    await compileRuntime("services/facade.ts", {
      "./websocket-handoff.js": moduleUrl(
        await compileRuntime("services/websocket-handoff.ts"),
      ),
      "./scope.js": scopeUrl,
      "./capabilities.js": capabilities,
    }),
  );
  const completionUrl = moduleUrl(
    await compileRuntime("loader/wrappers/completion.ts", {
      "../../services/facade.js": facadeUrl,
    }),
  );
  const actualRuntimeUrl = moduleUrl(
    await compileRuntime("loader/wrappers/runtime.ts", {
      "cloudflare:workers": cloudflare,
      "cloudflare-internal:workers": moduleUrl(
        "export default { withWaitUntilObserver(_observer, action) { return action(); } };",
      ),
      "cloudflare-internal:wrapped-binding": moduleUrl(
        "export default { createServiceRpcStub: value => value }",
      ),
      "../../services/capabilities.js": capabilities,
      "../../services/facade.js": facadeUrl,
      "../../services/rpc-member.js": moduleUrl(
        await compileRuntime("services/rpc-member.ts"),
      ),
      "../../services/scope.js": scopeUrl,
      "./completion.js": completionUrl,
      "./loopback.js": moduleUrl(
        "export const tenantExports = () => ({}); export const loopbackDurableObjectMetadata = () => {};",
      ),
    }),
  );
  const {
    createServiceBinding,
    serviceChannelCallbacks,
    channelWebSocketHandoffs,
  } = await import(facadeUrl);
  const { createPolicyChannelCallbacks: createIssuedChannel } = await import(
    actualRuntimeUrl
  );
  const OriginalResponse = Response;
  class WebSocketResponse extends OriginalResponse {
    constructor(body, init = {}) {
      super(body, { ...init, status: init.status === 101 ? 200 : init.status });
      if (init.status === 101)
        Object.defineProperty(this, "status", { value: 101 });
      if (init.webSocket)
        Object.defineProperty(this, "webSocket", { value: init.webSocket });
    }
  }
  globalThis.Response = WebSocketResponse;
  const socket = {};
  const handle = "0194fb2c-1d20-7000-8000-000000000001";
  const frames = [];
  const completed = [];
  let disposed = 0;
  let activations = 0;
  let dropped = false;
  const control = {
    rpc() {},
    get() {},
    root(scopeId) {
      frames.push(scopeId);
      return {
        ready: async () => {},
        [Symbol.dispose]() {
          disposed++;
        },
      };
    },
    async completeRoot(scopeId) {
      completed.push(scopeId);
    },
    async holdWebSocketHandoff(handles) {
      assert.deepEqual(handles, [handle]);
      assert.equal(completed.length, 0);
      activations++;
      return new ReadableStream({
        cancel() {
          dropped = true;
        },
      });
    },
  };
  try {
    const policyRaw = {
      async fetch(request) {
        assert.ok(request.headers.has("x-open-compute-service-frame"));
        return new Response(null, {
          status: 101,
          webSocket: socket,
          headers: { "x-open-compute-service-websocket-handoffs": handle },
        });
      },
      connect() {
        throw Error("not called");
      },
    };
    const policy = createServiceBinding(
      (value) => value,
      control,
      () => {},
      {
        fetch: (request) => policyRaw.fetch(request),
        connect: (address, options) => policyRaw.connect(address, options),
      },
    );
    const channel = withEnv({ BINDING: policy, DECLARED: "ok" }, () =>
      createIssuedChannel(serviceChannelCallbacks(policy)),
    );
    const packet = await channel.fetch(new Request("https://public/"));
    assert.equal(packet.response.webSocket, socket);
    assert.equal(
      packet.response.headers.has("x-open-compute-service-websocket-handoffs"),
      false,
    );
    assert.deepEqual(channelWebSocketHandoffs(packet.response), []);
    assert.equal(packet.errorCode, null);
    assert.equal(activations, 1);
    assert.equal(dropped, false);
    await packet.leaseBody.cancel();
    await drainTasks();
    assert.equal(dropped, true);
    assert.deepEqual(completed, frames);
    assert.equal(disposed, 1);
  } finally {
    globalThis.Response = OriginalResponse;
  }
});
