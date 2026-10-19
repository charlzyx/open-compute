import assert from "node:assert/strict";
import test from "node:test";
import {
  compileRuntime,
  importRuntime,
  moduleUrl,
} from "../compiled-runtime.mjs";

globalThis.scheduler = { wait: () => new Promise(() => {}) };

const codec = moduleUrl(await compileRuntime("durable-objects/id-codec.ts"));
const tunnel = moduleUrl(await compileRuntime("sockets/tunnel.ts"));
const cloudflare = moduleUrl(`
  export const connectCalls = [];
  export const operationStarts = [];
  export const background = [];
  export function waitUntil(promise) { background.push(Promise.resolve(promise)); }
`);
const stub = moduleUrl(
  await compileRuntime("durable-objects/stub.ts", {
    "./identity.js": moduleUrl(
      await compileRuntime("durable-objects/identity.ts"),
    ),
    "./errors.js": moduleUrl(await compileRuntime("durable-objects/errors.ts")),
    "../bindings/rpc-disposal.js": moduleUrl(
      await compileRuntime("bindings/rpc-disposal.ts"),
    ),
    "cloudflare:workers": cloudflare,
    "../sockets/tunnel.js": tunnel,
  }),
);
const { createDurableObjectNamespace } = await importRuntime(
  "durable-objects/namespace.ts",
  {
    "./stub.js": stub,
    "./id-codec.js": codec,
  },
);
const cloudflareModule = await import(cloudflare);
const { createDurableObjectStubPolicy, durableObjectChannelCallbacks } =
  await import(stub);

// Explicit Node-only stand-in. The native workerd test owns prototype/receiver validation.
function createId(value, name, jurisdiction) {
  return {
    name,
    jurisdiction,
    toString() {
      return value;
    },
    equals(other) {
      return other.toString() === value;
    },
  };
}

const nativeRpcStubs = new WeakSet();
const nativeFactories = {
  createId,
  createNamespace: (policy) => policy,
  createRpcStub: (policy) => {
    nativeRpcStubs.add(policy);
    return policy;
  },
  isRpcStub: (value) => nativeRpcStubs.has(value),
  createPrivateTransport: (raw) => raw,
  createRpcTransport: (raw) => raw,
  createUserRpcTransport: (raw) => raw,
  disposeRpcResult: (value) =>
    Object.getOwnPropertyDescriptor(value, Symbol.dispose)?.value?.call(value),
  createPublicTransport: (raw) => {
    const fetch = raw.fetch;
    const connect = raw.connect;
    return {
      fetch: (request) => Reflect.apply(fetch, raw, [request]),
      connect: (address, options) =>
        Reflect.apply(connect, raw, [address, options]),
    };
  },
  createStub: (id, _transport, policy) =>
    new Proxy(
      { id, name: id.name },
      {
        get(target, property) {
          return property === "id" || property === "name"
            ? Reflect.get(target, property)
            : Reflect.get(policy, property);
        },
      },
    ),
};

function namespaceBinding(capability, allocate = createId) {
  return createDurableObjectNamespace(capability, {
    ...nativeFactories,
    createId: allocate,
  });
}

function namespace(prefix = "aaaaaaaaaaaaaaaa") {
  const calls = [];
  const prepared = new Map();
  const properties = {
    release: "A",
    "release-label": "A-label",
  };
  const transport = {
    dispatchRpc: (objectId, _channelId, _sequence, method, args) => {
      cloudflareModule.operationStarts.push("rpc:" + method);
      calls.push({ objectId, method, args });
      return nativeResult(Promise.resolve({ echoed: args, objectId, method }));
    },
    getRpcProperty: (_objectId, _channelId, _sequence, property) => {
      cloudflareModule.operationStarts.push("get:" + property);
      return nativeResult(Promise.resolve(properties[property]));
    },
    startRpc(objectId, channelId, sequence, kind, member, args) {
      let value;
      try {
        value =
          kind === "call"
            ? transport.dispatchRpc(objectId, channelId, sequence, member, args)
            : transport.getRpcProperty(objectId, channelId, sequence, member);
      } catch (error) {
        value = nativeResult(Promise.reject(error));
      }
      const holder = { [Symbol.dispose]() {} };
      const resolved = Promise.resolve(holder);
      return {
        then: resolved.then.bind(resolved),
        take() {
          const taken = value;
          value = undefined;
          return taken;
        },
      };
    },
    async cancelOrder() {},
    async prepareConnect(
      objectId,
      _channelId,
      _sequence,
      operationId,
      authority,
    ) {
      cloudflareModule.operationStarts.push(
        "prepare-connect:" + authority.kind,
      );
      prepared.set(operationId, { objectId, authority });
    },
    async cancelConnect(operationId) {
      cloudflareModule.operationStarts.push("cancel-connect:" + operationId);
      prepared.delete(operationId);
    },
    async dispatchFetch(_objectId, _channelId, _sequence, _request) {
      cloudflareModule.operationStarts.push("fetch");
      return new Response("ok");
    },
    fetch(request) {
      const match = /^\/([0-9a-f]{64})\/([0-9a-f]{32})\/([0-9]+)$/.exec(
        new URL(request.url).pathname,
      );
      return transport.dispatchFetch(
        match?.[1],
        match?.[2],
        Number(match?.[3]),
        request,
      );
    },
    connect(tokenAddress, options) {
      cloudflareModule.operationStarts.push("connect");
      if (options?.secureTransport === "invalid") {
        throw new TypeError("native invalid secureTransport");
      }
      const operationId = /^([0-9a-f]{32})\.do-transport\.invalid:1$/.exec(
        tokenAddress,
      )?.[1];
      const socket = {
        address: undefined,
        tokenAddress,
        options,
        opened: undefined,
        closed: undefined,
      };
      const call = { objectId: undefined, address: undefined, options, socket };
      const opened = Promise.resolve().then(async () => {
        for (
          let attempt = 0;
          attempt < 20 && !prepared.has(operationId);
          attempt += 1
        ) {
          await Promise.resolve();
        }
        const pending =
          operationId === undefined ? undefined : prepared.get(operationId);
        const address =
          pending?.authority.kind === "string"
            ? pending.authority.address
            : pending === undefined
              ? undefined
              : {
                  hostname: pending.authority.hostname,
                  port: pending.authority.port,
                };
        socket.address = address;
        call.address = address;
        call.objectId = pending?.objectId;
        if (address === "failed.example:443" || pending === undefined) {
          throw new Error("native connect failed");
        }
        return {};
      });
      socket.opened = opened;
      socket.closed = opened.then(() => undefined);
      cloudflareModule.connectCalls.push(call);
      return socket;
    },
  };
  const ns = namespaceBinding(
    {
      schemaVersion: 1,
      namespacePrefix: prefix,
      namespaceNameKey: Buffer.alloc(32).toString("base64"),
      maxObjectNameBytes: 64,
      transport,
    },
    createId,
  );
  return { ns, calls, transport };
}

function nativeResult(promise) {
  const target = () => undefined;
  return new Proxy(target, {
    get(_owner, property) {
      if (property === "then") return promise.then.bind(promise);
      if (typeof property === "symbol") return undefined;
      const child = promise.then((value) =>
        Reflect.get(value, property, value),
      );
      return nativeMember(promise, property, child);
    },
  });
}

function nativeMember(parent, property, child) {
  const target = (...args) =>
    nativeResult(
      parent.then((value) =>
        Reflect.apply(Reflect.get(value, property, value), value, args),
      ),
    );
  return new Proxy(target, {
    get(_owner, nested) {
      if (nested === "then") return child.then.bind(child);
      return Reflect.get(nativeResult(child), nested);
    },
  });
}

function nativeStub(target, lifecycle = { disposed: 0, duplicated: 0 }) {
  const stub = new Proxy(Object.create(null), {
    get(_owner, property) {
      if (property === "then") return undefined;
      if (property === "dup")
        return () => {
          lifecycle.duplicated += 1;
          return nativeStub(target, lifecycle);
        };
      if (property === Symbol.dispose)
        return () => {
          lifecycle.disposed += 1;
        };
      const value = Reflect.get(target, property, target);
      if (typeof value !== "function")
        return nativeResult(Promise.resolve(value));
      return (...args) =>
        nativeResult(
          Promise.resolve().then(() => Reflect.apply(value, target, args)),
        );
    },
  });
  nativeRpcStubs.add(stub);
  return stub;
}

test("jurisdiction and placement options are accepted with stable local semantics", () => {
  const { ns } = namespace();
  const eu = ns.jurisdiction("eu");
  const named = eu.idFromName("alpha");
  assert.equal(named.jurisdiction, "eu");
  assert.equal(named.toString(), eu.idFromName("alpha").toString());
  assert.notEqual(named.toString(), ns.idFromName("alpha").toString());
  const unique = eu.newUniqueId({ jurisdiction: "eu" });
  assert.equal(unique.jurisdiction, "eu");
  assert.equal(ns.idFromString(unique.toString()).jurisdiction, "eu");
  const stub = eu.get(named, {
    locationHint: "enam",
    routingMode: "primary-only",
  });
  assert.equal(stub.id.toString(), named.toString());
  assert.equal(ns.get(named).id.toString(), named.toString());
  eu.getByName("alpha", { locationHint: "wnam" });
  ns.getByName("alpha", { locationHint: "wnam", extra: true });
  assert.equal(ns.jurisdiction(null).newUniqueId().jurisdiction, undefined);
  assert.equal(
    ns.newUniqueId({ jurisdiction: null, extra: true }).jurisdiction,
    undefined,
  );
  assert.throws(() => ns.jurisdiction("mars"), /DO_ID_INVALID/);
  assert.throws(
    () => ns.getByName("alpha", { locationHint: "eu" }),
    /DO_ID_INVALID/,
  );
  assert.throws(
    () => ns.getByName("alpha", { routingMode: "nearest" }),
    /DO_ID_INVALID/,
  );
  assert.throws(() => ns.jurisdiction("us").get(unique), /DO_ID_INVALID/);
});

test("ID round-trip and namespace isolation stay exact", () => {
  const { ns } = namespace();
  const other = namespace("bbbbbbbbbbbbbbbb").ns;
  const named = ns.idFromName("alpha");
  const parsed = ns.idFromString(named.toString());
  assert.equal(parsed.toString(), named.toString());
  assert.equal(parsed.name, undefined);
  assert.equal(named.equals(parsed), true);
  assert.throws(() => other.idFromString(named.toString()), /DO_ID_INVALID/);
  assert.throws(
    () => ns.idFromString(named.toString().toUpperCase()),
    /DO_ID_INVALID/,
  );
  const forged = `${named.toString().slice(0, -1)}${named.toString().endsWith("0") ? "1" : "0"}`;
  assert.throws(() => ns.idFromString(forged), /DO_ID_INVALID/);
});

test("namespace and ID validation reject malformed authority before native allocation", () => {
  const { ns, transport } = namespace();
  const capability = {
    schemaVersion: 1,
    namespacePrefix: "a".repeat(16),
    namespaceNameKey: Buffer.alloc(32).toString("base64"),
    maxObjectNameBytes: 64,
    transport,
  };
  let allocated = 0;
  const allocate = (...args) => {
    allocated += 1;
    return createId(...args);
  };
  for (const invalid of [
    null,
    undefined,
    {},
    [],
    "namespace",
    { ...capability, schemaVersion: 2 },
    { ...capability, namespacePrefix: "A".repeat(16) },
    { ...capability, namespaceNameKey: Buffer.alloc(31).toString("base64") },
    { ...capability, maxObjectNameBytes: 0 },
    { ...capability, maxObjectNameBytes: 1025 },
    { ...capability, maxObjectNameBytes: 1.5 },
    { ...capability, transport: null },
    { ...capability, transport: { ...transport, startRpc: undefined } },
  ]) {
    assert.throws(
      () => namespaceBinding(invalid, allocate),
      /DO_NAMESPACE_NOT_FOUND/,
    );
  }
  const checked = namespaceBinding(capability, allocate);
  const eu = checked.jurisdiction("eu");
  for (const rejected of [
    () => checked.idFromName(42),
    () => checked.idFromName("🙂".repeat(17)),
    () => checked.newUniqueId([]),
    () => checked.newUniqueId({ jurisdiction: "unknown" }),
    () => eu.newUniqueId({ jurisdiction: "us" }),
    () => eu.idFromString(ns.idFromName("plain").toString()),
    () => checked.idFromString("a".repeat(16) + "00".repeat(24)),
    () => checked.get({ toString: () => ns.idFromName("plain").toString() }),
    () => checked.get({}, null),
  ])
    assert.throws(rejected, /DO_ID_INVALID/);
  assert.equal(allocated, 0);
  const boundary = checked.idFromName("🙂".repeat(16));
  assert.equal(boundary.name, "🙂".repeat(16));
  assert.equal(allocated, 1);
});

test("tenant ID method overrides never alter RPC, fetch or connect authority", async () => {
  const { ns, transport, calls } = namespace();
  const id = ns.idFromName("alpha");
  const value = id.toString();
  const fetches = [];
  transport.dispatchFetch = (objectId) => {
    fetches.push(objectId);
    return Promise.resolve(new Response("ok"));
  };
  id.toString = () => {
    throw Error("tenant override must not be invoked");
  };
  const stub = ns.get(id);
  assert.equal(stub.name, "alpha");
  await stub.echo(1);
  await stub.fetch("https://test/");
  const socket = stub.connect("example.com:443");
  await socket.opened;
  assert.equal(calls[0].objectId, value);
  assert.deepEqual(fetches, [value]);
  assert.equal(cloudflareModule.connectCalls.at(-1).objectId, value);
});

test("a reused allocator result cannot replace an existing ID authority", async () => {
  const { ns, transport, calls } = namespace();
  const id = ns.idFromName("alpha");
  const value = id.toString();
  const capability = {
    schemaVersion: 1,
    namespacePrefix: "a".repeat(16),
    namespaceNameKey: Buffer.alloc(32).toString("base64"),
    maxObjectNameBytes: 64,
    transport,
  };
  const reused = namespaceBinding(capability, () => id);
  assert.throws(() => reused.idFromName("different"), /DO_ID_INVALID/);
  const invalid = namespaceBinding(capability, () => null);
  assert.throws(() => invalid.idFromName("different"), /DO_ID_INVALID/);
  const stub = ns.get(id);
  assert.equal(stub.name, "alpha");
  await stub.echo(1);
  assert.equal(calls[0].objectId, value);
});

test("transferred Fetcher policy retains object authority and routes id/name as RPC", async () => {
  const { ns, transport, calls } = namespace();
  const id = ns.idFromName("transferred").toString();
  const policy = createDurableObjectStubPolicy(
    { fetcher: transport, id },
    nativeFactories,
  );
  await policy.echo("payload");
  await policy.id();
  await policy.name();
  assert.deepEqual(
    calls.map((call) => call.objectId),
    [id, id, id],
  );
  assert.deepEqual(
    calls.map((call) => call.method),
    ["echo", "id", "name"],
  );
  assert.equal(await (await policy.fetch("https://object/")).text(), "ok");
  const socket = policy.connect("example.com:443");
  await socket.opened;
  assert.equal(cloudflareModule.connectCalls.at(-1).objectId, id);
  assert.equal(policy.then, undefined);
  assert.throws(() => policy.__openComputeInternal, /DO_RPC_UNSUPPORTED/);
  for (const input of [
    { fetcher: transport, id: "A".repeat(64) },
    { fetcher: transport, id: "a".repeat(63) },
    { fetcher: {}, id },
    { fetcher: null, id },
    { fetcher: { ...transport, cancelOrder: null }, id },
  ])
    assert.throws(
      () => createDurableObjectStubPolicy(input, nativeFactories),
      /DO_ID_INVALID/,
    );
});

test("RPC forwards native values and connect returns a native bridge Socket synchronously", async () => {
  const { ns, calls } = namespace();
  const stub = ns.getByName("rpc");
  const when = new Date("2026-08-30T00:00:00.000Z");
  const result = await stub.echo({ when, nested: new Map([["a", 1]]) });
  assert.deepEqual(calls[0].args[0].when, when);
  assert.equal(calls[0].args[0].nested.get("a"), 1);
  assert.equal(result.method, "echo");
  const socket = stub.connect("example.com:443", { allowHalfOpen: true });
  await socket.opened;
  assert.equal(socket.address, "example.com:443");
  assert.deepEqual(socket.options, { allowHalfOpen: true });
  assert.equal(
    cloudflareModule.connectCalls.at(-1).objectId,
    stub.id.toString(),
  );
  assert.match(socket.tokenAddress, /^[0-9a-f]{32}\.do-transport\.invalid:1$/);
  const ipv6 = { hostname: "2606:4700:4700::1111", port: 443 };
  const ipv6Socket = stub.connect(ipv6);
  await ipv6Socket.opened;
  assert.deepEqual(ipv6Socket.address, ipv6);
  assert.equal(
    cloudflareModule.operationStarts.at(-1),
    "prepare-connect:record",
  );
});

test("connect preserves native option errors and cancels failed authorities", async () => {
  const { ns } = namespace();
  const stub = ns.getByName("connect-validation");
  const start = cloudflareModule.operationStarts.length;
  assert.throws(
    () => stub.connect("example.com:443", { secureTransport: "invalid" }),
    (error) =>
      error instanceof TypeError &&
      error.message === "native invalid secureTransport",
  );
  const asynchronousFailure = stub.connect("failed.example:443");
  await asynchronousFailure.opened.catch(() => undefined);
  const successful = stub.connect("ok.example:443");
  await successful.opened;
  await Promise.allSettled(cloudflareModule.background);
  const operations = cloudflareModule.operationStarts.slice(start);
  assert.equal(
    operations.filter((operation) => operation.startsWith("prepare-connect:"))
      .length,
    3,
  );
  assert.equal(
    operations.filter((operation) => operation.startsWith("cancel-connect:"))
      .length,
    2,
  );
  assert.equal(successful.address, "ok.example:443");
});

test("namespace and transferred stubs keep CONNECT preparation and failed-operation cleanup outside public quota", async () => {
  for (const transferred of [false, true]) {
    const { transport } = namespace();
    const events = [];
    let publicCalls = 0;
    const admit = () => {
      if (publicCalls >= 2) throw Error("Too many subrequests.");
      publicCalls++;
    };
    transport.fetch = () => {
      admit();
      return Promise.reject(Error("DO_EXECUTION_LIMIT"));
    };
    transport.connect = () => {
      admit();
      throw new TypeError("native invalid secureTransport");
    };
    transport.startRpc = () => {
      admit();
      throw Error("unexpected RPC dispatch");
    };
    for (const name of ["prepareConnect", "cancelConnect", "cancelOrder"])
      transport[name] = async () => {
        throw Error("unexpected public control call");
      };
    const control = {
      ...transport,
      async prepareConnect() {
        events.push("prepare");
      },
      async cancelConnect() {
        events.push("cancel-connect");
      },
      async cancelOrder() {
        events.push("cancel-order");
      },
    };
    let copies = 0;
    const factories = {
      ...nativeFactories,
      createPrivateTransport(raw) {
        assert.equal(raw, transport);
        copies++;
        return control;
      },
    };
    const ns = createDurableObjectNamespace(
      {
        schemaVersion: 1,
        namespacePrefix: "aaaaaaaaaaaaaaaa",
        namespaceNameKey: btoa("k".repeat(32)),
        maxObjectNameBytes: 1024,
        transport,
      },
      factories,
    );
    const stub = transferred
      ? createDurableObjectStubPolicy(
          { fetcher: transport, id: ns.idFromName("quota").toString() },
          factories,
        )
      : ns.getByName("quota");
    await assert.rejects(stub.fetch("https://object/"), {
      message: "DO_EXECUTION_LIMIT",
    });
    assert.throws(() => stub.connect("example.com:443"), {
      message: "native invalid secureTransport",
    });
    await Promise.allSettled(cloudflareModule.background);
    assert.equal(copies, 1);
    assert.equal(publicCalls, 2);
    assert.equal(events.filter((event) => event === "prepare").length, 1);
    assert.equal(
      events.filter((event) => event === "cancel-connect").length,
      1,
    );
    assert.equal(events.filter((event) => event === "cancel-order").length, 2);
    assert.throws(() => stub.echo(), { message: "DO_RUNTIME_EXCEPTION" });
    assert.equal(publicCalls, 2);
  }
});

test("CONNECT cancellation waits for late preparation before releasing its authority", async () => {
  for (const synchronous of [true, false]) {
    const { transport } = namespace();
    let release;
    let started;
    const barrier = new Promise((resolve) => {
      release = resolve;
    });
    const preparing = new Promise((resolve) => {
      started = resolve;
    });
    let registered = false;
    const events = [];
    const control = {
      ...transport,
      async prepareConnect() {
        started();
        await barrier;
        registered = true;
        events.push("prepare");
      },
      async cancelConnect() {
        registered = false;
        events.push("cancel");
      },
      async cancelOrder() {
        events.push("order");
      },
    };
    transport.connect = () => {
      const error = Error("native CONNECT rejected");
      if (synchronous) throw error;
      return {
        opened: Promise.reject(error),
        closed: Promise.reject(error),
      };
    };
    const stub = createDurableObjectStubPolicy(
      { fetcher: transport, id: "a".repeat(64) },
      { ...nativeFactories, createPrivateTransport: () => control },
    );
    if (synchronous)
      assert.throws(() => stub.connect("example.com:443"), {
        message: "native CONNECT rejected",
      });
    else await stub.connect("example.com:443").opened.catch(() => {});
    await preparing;
    release();
    await Promise.allSettled(cloudflareModule.background);
    assert.equal(registered, false);
    assert.deepEqual(events, ["prepare", "cancel", "order"]);
  }
});

test("native clone failures are opaque and do not masquerade as unsupported RPC", async () => {
  const { ns, transport } = namespace();
  transport.dispatchRpc = () =>
    nativeResult(Promise.reject(new TypeError("DataCloneError secret")));
  const stub = ns.getByName("rpc");
  let caught;
  try {
    await stub.echo(new WeakMap());
  } catch (error) {
    caught = error;
  }
  assert.equal(caught?.message, "DO_RUNTIME_EXCEPTION");
  assert.equal(String(caught).includes("secret"), false);
});

test("RPC method reflection stays local until call or property await", async () => {
  const { ns, calls } = namespace();
  const start = cloudflareModule.operationStarts.length;
  const echo = ns.getByName("reflection").echo;
  assert.equal(echo.constructor, Function);
  assert.equal(typeof echo.name, "string");
  assert.equal(typeof echo.length, "number");
  assert.equal(typeof echo.call, "function");
  assert.equal(typeof echo.then, "function");
  assert.equal(echo[Symbol.toStringTag], undefined);
  assert.equal(echo[Symbol.iterator], undefined);
  assert.equal(typeof echo.next, "function");
  assert.equal(typeof echo.get, "function");
  assert.equal(typeof echo.next.then, "function");
  assert.deepEqual(cloudflareModule.operationStarts.slice(start), []);
  assert.equal(calls.length, 0);
  assert.deepEqual((await echo("value")).echoed, ["value"]);
  assert.equal(calls.length, 1);
});

test("repeated RPC property reads observe current remote values", async () => {
  const { ns, transport } = namespace();
  let revision = 0;
  transport.getRpcProperty = () => nativeResult(Promise.resolve(++revision));
  const property = ns.getByName("revision").revision;
  assert.equal(await property, 1);
  assert.equal(await property, 2);
  assert.equal(revision, 2);
  transport.getRpcProperty = () =>
    nativeResult(Promise.resolve({ revision: ++revision }));
  const nested = ns.getByName("revision").record.revision;
  assert.equal(await nested, 3);
  assert.equal(await nested, 4);
  assert.equal(revision, 4);
});

test("decoded DO collections retain native group disposal and its receiver", async () => {
  const { ns, transport } = namespace();
  const stub = ns.getByName("disposable-results");
  for (const value of [
    { answer: 42 },
    [42],
    new Map([["answer", 42]]),
    new Set([42]),
  ]) {
    let disposed = 0;
    Object.defineProperty(value, Symbol.dispose, {
      value() {
        assert.equal(this, value);
        disposed++;
      },
    });
    transport.dispatchRpc = () => nativeResult(Promise.resolve(value));
    const decoded = await stub.result();
    assert.notEqual(decoded, value);
    assert.equal(
      Object.getOwnPropertyDescriptor(decoded, Symbol.dispose).enumerable,
      false,
    );
    decoded[Symbol.dispose]();
    assert.equal(disposed, 1);
  }
});

test("dynamic properties, punctuation, and native promise pipelines stay intact", async () => {
  const { ns, transport } = namespace();
  const lifecycle = { disposed: 0, duplicated: 0 };
  const capability = nativeStub(
    {
      label: "A-capability",
      echo(value) {
        return `A:${value}`;
      },
      fail() {
        throw new Error("tenant-capability-secret");
      },
    },
    lifecycle,
  );
  const model = {
    release: "A",
    "release-label": "A-label",
    get failingProperty() {
      throw new Error("tenant-property-secret");
    },
    "echo-value"(value) {
      return `A:${value}`;
    },
    capabilityValue() {
      return capability;
    },
    capabilityEnvelope() {
      return { target: capability };
    },
    callbackValue(callback, value) {
      return callback(value);
    },
  };
  transport.dispatchRpc = (_objectId, _channelId, _sequence, method, args) =>
    nativeResult(
      Promise.resolve().then(() => Reflect.apply(model[method], model, args)),
    );
  transport.getRpcProperty = (_objectId, _channelId, _sequence, property) =>
    nativeResult(
      Promise.resolve().then(() => Reflect.get(model, property, model)),
    );
  const stub = ns.getByName("rpc");
  assert.equal(await stub.release, "A");
  assert.equal(await stub["release-label"], "A-label");
  assert.equal(await stub["echo-value"]("punctuation"), "A:punctuation");
  let caught;
  try {
    await stub.failingProperty;
  } catch (error) {
    caught = error;
  }
  assert.equal(caught?.message, "DO_RUNTIME_EXCEPTION");
  assert.equal(String(caught).includes("tenant-property-secret"), false);
  assert.equal(await stub.capabilityValue().echo("pipelined"), "A:pipelined");
  assert.equal(await stub.capabilityValue().label, "A-capability");
  assert.equal(
    await stub.callbackValue((value) => `callback:${value}`, "ok"),
    "callback:ok",
  );
  const held = await stub.capabilityValue();
  assert.equal(await held.echo("held"), "A:held");
  const duplicate = held.dup();
  assert.equal(await duplicate.echo("duplicate"), "A:duplicate");
  duplicate[Symbol.dispose]();
  held[Symbol.dispose]();
  assert.deepEqual(lifecycle, { disposed: 2, duplicated: 1 });
  caught = undefined;
  try {
    await held.fail();
  } catch (error) {
    caught = error;
  }
  assert.equal(caught?.message, "DO_RUNTIME_EXCEPTION");
  assert.equal(String(caught).includes("tenant-capability-secret"), false);
  const envelope = await stub.capabilityEnvelope();
  caught = undefined;
  try {
    await envelope.target.fail();
  } catch (error) {
    caught = error;
  }
  assert.equal(caught?.message, "DO_RUNTIME_EXCEPTION");
  assert.equal(String(caught).includes("tenant-capability-secret"), false);
});

test("the direct native transport preserves cross-surface start order without poisoning later calls", async () => {
  const { ns, transport } = namespace();
  const start = cloudflareModule.operationStarts.length;
  transport.dispatchRpc = (_objectId, _channelId, _sequence, method) => {
    cloudflareModule.operationStarts.push(`rpc:${method}`);
    return method === "first"
      ? nativeResult(Promise.reject(new Error("first failed")))
      : nativeResult(Promise.resolve(method));
  };
  transport.dispatchFetch = async () => {
    cloudflareModule.operationStarts.push("fetch");
    return new Response("fetch-ok");
  };
  const stub = ns.getByName("rpc");
  const first = Promise.resolve(stub.first()).then(
    () => false,
    (error) => error?.message === "DO_RUNTIME_EXCEPTION",
  );
  const fetched = stub.fetch("https://object.invalid/");
  const socket = stub.connect("example.com:443");
  const second = stub.second();
  assert.equal(await first, true);
  assert.equal(await (await fetched).text(), "fetch-ok");
  await socket.opened;
  assert.equal(socket.address, "example.com:443");
  assert.equal(await second, "second");
  assert.deepEqual(cloudflareModule.operationStarts.slice(start), [
    "rpc:first",
    "connect",
    "fetch",
    "prepare-connect:string",
    "rpc:second",
  ]);
});

test("native reset rejects in-flight and future calls while a fresh stub recovers", async () => {
  for (const surface of ["rpc", "fetch"]) {
    const { ns, transport } = namespace();
    let abort;
    let resolvePending;
    let calls = 0;
    const pending = new Promise((resolve) => {
      resolvePending = resolve;
    });
    const failed = new Promise((_resolve, reject) => {
      abort = reject;
    });
    transport.dispatchRpc = () =>
      nativeResult(
        ++calls === 1
          ? failed
          : calls === 2
            ? pending
            : Promise.resolve("fresh"),
      );
    transport.dispatchFetch = () =>
      ++calls === 1
        ? failed
        : calls === 2
          ? pending
          : Promise.resolve(new Response("fresh"));
    const stub = ns.getByName("reset");
    const invoke = (owner) =>
      surface === "rpc" ? owner.echo() : owner.fetch("https://object/");
    const outcomes = Promise.allSettled([invoke(stub), invoke(stub)]);
    await new Promise((resolve) => setImmediate(resolve));
    assert.equal(calls, 2);
    abort(
      Object.assign(new Error("sensitive fatal failure"), {
        durableObjectReset: true,
      }),
    );
    const results = await outcomes;
    for (const result of results) {
      assert.equal(result.status, "rejected");
      assert.equal(result.reason.message, "DO_RUNTIME_EXCEPTION");
      assert.equal(result.reason.durableObjectReset, true);
      assert.equal(String(result.reason).includes("sensitive"), false);
    }
    const held = stub.echo;
    assert.throws(() => held(), { message: "DO_RUNTIME_EXCEPTION" });
    await assert.rejects(stub.fetch("https://object/"), {
      message: "DO_RUNTIME_EXCEPTION",
    });
    assert.throws(() => stub.connect("example.com:443"), {
      message: "DO_RUNTIME_EXCEPTION",
    });
    assert.equal(calls, 2);
    const fresh = await invoke(ns.getByName("reset"));
    assert.equal(surface === "rpc" ? fresh : await fresh.text(), "fresh");
    const transferred = createDurableObjectStubPolicy(
      { fetcher: transport, id: stub.id.toString() },
      nativeFactories,
    );
    const transferredResult = await invoke(transferred);
    assert.equal(
      surface === "rpc" ? transferredResult : await transferredResult.text(),
      "fresh",
    );
    assert.throws(() => held(), { message: "DO_RUNTIME_EXCEPTION" });
    assert.equal(calls, 4);
    resolvePending(surface === "rpc" ? "late" : new Response("late"));
    await Promise.allSettled(cloudflareModule.background);
  }
});

test("native Socket failure metadata closes its stub without replacing the Socket", async () => {
  for (const event of ["opened", "closed"]) {
    const { ns, transport } = namespace();
    let abort;
    let finish;
    const failure = new Promise((_resolve, reject) => {
      abort = reject;
    });
    const pending = new Promise((resolve) => {
      finish = resolve;
    });
    const socket = {
      opened: event === "opened" ? failure : Promise.resolve({}),
      closed: event === "closed" ? failure : Promise.resolve(),
    };
    transport.connect = () => socket;
    transport.dispatchFetch = () => pending;
    const stub = ns.getByName("socket-reset");
    assert.equal(stub.connect("example.com:443"), socket);
    const observed = assert.rejects(stub.fetch("https://object/"), {
      message: "DO_RUNTIME_EXCEPTION",
    });
    await new Promise((resolve) => setImmediate(resolve));
    abort(
      Object.assign(new Error("DO_RUNTIME_EXCEPTION"), {
        durableObjectReset: true,
      }),
    );
    await observed;
    assert.throws(() => stub.echo(), { message: "DO_RUNTIME_EXCEPTION" });
    finish(new Response());
    await Promise.allSettled(cloudflareModule.background);
  }
});

test("distinct stubs do not share an unresolved dispatch admission", async () => {
  const { ns, transport } = namespace();
  let release;
  const admitted = new Promise((resolve) => {
    release = resolve;
  });
  const channels = [];
  transport.startRpc = (_id, channel) => {
    channels.push(channel);
    const ready =
      channels.length === 1
        ? admitted
        : Promise.resolve({ [Symbol.dispose]() {} });
    return {
      then: ready.then.bind(ready),
      take: () => nativeResult(Promise.resolve("ok")),
    };
  };
  const first = ns.getByName("independent");
  const second = ns.getByName("independent");
  assert.equal(await first.echo(), "ok");
  assert.equal(await second.echo(), "ok");
  assert.equal(channels.length, 2);
  assert.notEqual(channels[0], channels[1]);
  release({ [Symbol.dispose]() {} });
  await Promise.allSettled(cloudflareModule.background);
});

test("pending RPC responses retain the channel without serializing response completion", async () => {
  const { ns, transport } = namespace();
  let finish;
  const pending = new Promise((resolve) => {
    finish = resolve;
  });
  const calls = [];
  transport.dispatchRpc = (_id, channel, sequence, method) => {
    calls.push({ channel, sequence });
    return nativeResult(
      method === "hold" ? pending : Promise.resolve("release"),
    );
  };
  const stub = ns.getByName("pending-rpc");
  const first = Promise.resolve(stub.hold());
  try {
    assert.equal(await stub.release(), "release");
    assert.equal(calls.length, 2);
    assert.equal(calls[0].channel, calls[1].channel);
    assert.deepEqual(
      calls.map((call) => call.sequence),
      [0, 1],
    );
  } finally {
    finish("done");
    assert.equal(await first, "done");
  }
});

test("a pending fetch does not block later operations on the same object", async () => {
  const { ns, transport } = namespace();
  const requests = [];
  let finish;
  transport.dispatchFetch = async (_objectId, channelId, sequence) => {
    requests.push({ channelId, sequence });
    if (sequence === 0)
      await new Promise((resolve) => {
        finish = resolve;
      });
    return new Response(String(sequence));
  };
  const stub = ns.getByName("pending-fetch");
  const first = stub.fetch("https://object.invalid/long-poll");
  const second = stub.fetch("https://object.invalid/release");
  await new Promise((resolve) => setImmediate(resolve));
  try {
    assert.equal(
      requests.length,
      2,
      "dispatch order must not serialize response completion",
    );
    assert.equal(requests[0].channelId, requests[1].channelId);
    assert.deepEqual(
      requests.map((request) => request.sequence),
      [0, 1],
    );
    assert.equal(await (await second).text(), "1");
  } finally {
    finish?.();
    await Promise.allSettled([first, second]);
  }
});

test("a quiescent stub starts a fresh channel after its host can hibernate", async () => {
  const { ns, transport } = namespace();
  const requests = [];
  transport.dispatchFetch = async (_objectId, channelId, sequence) => {
    requests.push({ channelId, sequence });
    return new Response("ok");
  };
  const stub = ns.getByName("hibernate");
  await (await stub.fetch("https://object.invalid/first")).text();
  await (await stub.fetch("https://object.invalid/after-idle")).text();
  assert.deepEqual(
    requests.map((request) => request.sequence),
    [0, 0],
  );
  assert.notEqual(requests[0].channelId, requests[1].channelId);
});

test("a pending RPC dispatch acknowledgement keeps the burst channel", async () => {
  const { ns, transport } = namespace();
  const requests = [];
  let acknowledge;
  const acknowledged = new Promise((resolve) => {
    acknowledge = resolve;
  });
  transport.startRpc = (_objectId, channelId, sequence) => {
    requests.push({ channelId, sequence });
    return {
      then: acknowledged.then.bind(acknowledged),
      take: () => nativeResult(Promise.resolve("ok")),
    };
  };
  const stub = ns.getByName("rpc-ack");
  const first = stub.first();
  const second = stub.second();
  assert.equal(requests.length, 1);
  acknowledge({ [Symbol.dispose]() {} });
  await Promise.all([first, second]);
  assert.equal(requests.length, 2);
  assert.equal(requests[0].channelId, requests[1].channelId);
  assert.deepEqual(
    requests.map((request) => request.sequence),
    [0, 1],
  );
});

test("stock RPC serializable values are forwarded without a local allowlist", async () => {
  const { ns, transport } = namespace();
  transport.dispatchRpc = (_objectId, _channelId, _sequence, _method, args) =>
    nativeResult(Promise.resolve(args[0]));
  const stub = ns.getByName("rpc");
  const error = new TypeError("returned value");
  const input = {
    bigint: 12n,
    map: new Map([["key", new Set([1, 2])]]),
    regexp: /native/giu,
    error,
    data: new DataView(Uint8Array.from([3, 4]).buffer),
    headers: new Headers({ "x-value": "ok" }),
  };
  const output = await stub.echo(input);
  assert.equal(output.bigint, 12n);
  assert.deepEqual([...output.map.get("key")], [1, 2]);
  assert.equal(output.regexp.source, "native");
  assert.equal(output.error, error);
  assert.equal(output.data.getUint8(1), 4);
  assert.equal(output.headers.get("x-value"), "ok");
});

test("resolved graphs retain cycles, native values and repeated capability identity", async () => {
  const { ns, transport } = namespace();
  const capability = nativeStub({ echo: (value) => value });
  const graph = Object.create(null);
  graph.self = graph;
  graph.targets = [capability, capability];
  graph.date = new Date(1000);
  graph.bytes = new ArrayBuffer(2);
  graph.request = new Request("https://test/");
  graph.response = new Response("ok");
  graph.stream = new ReadableStream({
    start: (controller) => controller.close(),
  });
  graph.record = JSON.parse(
    '{"__proto__":{"scope":"remote"},"constructor":"value"}',
  );
  transport.dispatchRpc = () => nativeResult(Promise.resolve(graph));
  const output = await ns.getByName("graph").read();
  assert.equal(Object.getPrototypeOf(output), null);
  assert.equal(output.self, output);
  assert.equal(output.targets[0], output.targets[1]);
  assert.equal(await output.targets[0].echo("ok"), "ok");
  assert.equal(Object.getPrototypeOf(output.record), Object.prototype);
  assert.equal(Object.hasOwn(output.record, "__proto__"), true);
  assert.deepEqual(output.record.__proto__, { scope: "remote" });
  assert.equal(output.record.constructor, "value");
  for (const key of ["date", "bytes", "request", "response", "stream"])
    assert.equal(output[key], graph[key]);
});

test("dispatch admission and fetch failures stay opaque and release pending order", async () => {
  for (const stage of ["start", "take"]) {
    const { ns, transport } = namespace();
    transport.startRpc = () => {
      if (stage === "start") throw Error("secret admission failure");
      return {
        then: Promise.resolve({ [Symbol.dispose]() {} }).then.bind(
          Promise.resolve(),
        ),
        take() {
          throw Error("secret result failure");
        },
      };
    };
    assert.throws(() => ns.getByName(stage).echo(), {
      message: "DO_RUNTIME_EXCEPTION",
    });
  }
  for (const synchronous of [true, false]) {
    const { ns, transport } = namespace();
    let cancellations = 0;
    transport.fetch = () => {
      const error = Error("secret upstream DO_EXECUTION_LIMIT");
      if (synchronous) throw error;
      return Promise.reject(error);
    };
    transport.cancelOrder = async () => {
      cancellations++;
      throw Error("secret cleanup failure");
    };
    const stub = ns.getByName("fetch-failure");
    await assert.rejects(stub.fetch("invalid-url"), {
      message: "DO_RPC_UNSUPPORTED",
    });
    await assert.rejects(stub.fetch("https://test/"), {
      message: "DO_EXECUTION_LIMIT",
    });
    assert.equal(cancellations, synchronous ? 0 : 1);
  }
});

test("queued RPC pipelines and disposal wait for admission without exposing errors", async () => {
  for (const rejected of [false, true]) {
    const { ns, transport } = namespace();
    let acknowledge;
    const admitted = new Promise((resolve) => {
      acknowledge = resolve;
    });
    const lifecycle = { disposed: 0, duplicated: 0 };
    let echoedCalls = 0;
    const capability = nativeStub(
      {
        echo: (value) => {
          echoedCalls++;
          return value;
        },
      },
      lifecycle,
    );
    let starts = 0;
    transport.startRpc = () => {
      starts++;
      if (starts === 1)
        return { then: admitted.then.bind(admitted), take: () => 1 };
      if (rejected) throw Error("secret queued failure");
      const holder = { [Symbol.dispose]() {} };
      const ready = Promise.resolve(holder);
      return { then: ready.then.bind(ready), take: () => capability };
    };
    const stub = ns.getByName("queued");
    assert.equal(stub.first(), 1);
    const pending = stub.second();
    const pipeline = pending.echo("value");
    const echoed = Promise.resolve(pipeline);
    pending[Symbol.dispose]();
    acknowledge({ [Symbol.dispose]() {} });
    if (rejected)
      await assert.rejects(echoed, { message: "DO_RUNTIME_EXCEPTION" });
    else {
      assert.equal(await echoed, "value");
      assert.equal(await pipeline, "value");
      pipeline[Symbol.dispose]();
      await Promise.allSettled(cloudflareModule.background);
      assert.equal(lifecycle.disposed, 1);
      assert.equal(echoedCalls, 1);
    }
    assert.equal(starts, 2);
  }
});

test("reserved Durable Object handlers never become RPC methods", () => {
  const { ns } = namespace();
  const stub = ns.getByName("rpc");
  for (const method of [
    "dup",
    "alarm",
    "webSocketMessage",
    "webSocketClose",
    "webSocketError",
  ]) {
    assert.throws(() => stub[method], /DO_RPC_UNSUPPORTED/);
  }
});

test("issued channel callbacks preserve transferred DO identity and canonical dispatch", async () => {
  const { ns, transport } = namespace();
  const id = ns.idFromName("channel-owner").toString();
  const policy = createDurableObjectStubPolicy(
    { fetcher: transport, id },
    nativeFactories,
  );
  const callbacks = durableObjectChannelCallbacks(policy);
  assert.ok(callbacks);
  assert.equal(durableObjectChannelCallbacks({ fetch: transport.fetch }), null);
  const prototype = Object.getPrototypeOf(policy);
  const originalFetch = prototype.fetch;
  const originalConnect = prototype.connect;
  try {
    prototype.fetch = () => {
      throw Error("poisoned public fetch");
    };
    prototype.connect = () => {
      throw Error("poisoned public connect");
    };
    assert.equal(
      await (await callbacks.fetch(new Request("https://object.test/"))).text(),
      "ok",
    );
    const socket = callbacks.connect("object.test:443");
    await socket.opened;
    assert.equal(cloudflareModule.connectCalls.at(-1).objectId, id);
    await socket.closed;
  } finally {
    prototype.fetch = originalFetch;
    prototype.connect = originalConnect;
  }
});

test("issued DO channels retain canonical raw dispatch after prototype changes with identity and ordering", async () => {
  const { transport } = namespace();
  const socket = { opened: Promise.resolve({}), closed: Promise.resolve() };
  const received = [];
  // Issue a fresh stub from a receiver with inherited native stand-ins.
  const prototype = {
    ...transport,
    async fetch(request) {
      received.push(["fetch", this, request]);
      return new Response("canonical");
    },
    connect(address, options) {
      received.push(["connect", this, address, options]);
      return socket;
    },
  };
  const raw = Object.create(prototype);
  const policy = createDurableObjectStubPolicy(
    { fetcher: raw, id: "b".repeat(64) },
    nativeFactories,
  );
  const channel = durableObjectChannelCallbacks(policy);
  prototype.fetch = () => {
    throw Error("mutable prototype fetch must not dispatch");
  };
  prototype.connect = () => {
    throw Error("mutable prototype connect must not dispatch");
  };
  assert.equal(
    await (await channel.fetch(new Request("https://public.test/"))).text(),
    "canonical",
  );
  assert.equal(channel.connect("public.test:443"), socket);
  await Promise.allSettled(cloudflareModule.background);
  assert.equal(received.length, 2);
  assert.equal(received[0][1], raw);
  assert.equal(received[1][1], raw);
  assert.match(received[0][2].url, /^https:\/\/do-transport\.invalid\/bbbb/);
  assert.ok(received[0][2].headers.has("x-open-compute-object-identity"));
  assert.match(received[1][2], /^[0-9a-f]{32}\.do-transport\.invalid:1$/);
});
