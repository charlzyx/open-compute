import assert from "node:assert/strict";
import test from "node:test";
import { importRuntime, moduleUrl } from "../compiled-runtime.mjs";

const metadata = new WeakMap();
globalThis.__openComputeFacetTestMetadata = metadata;
const background = [];
const cloudflare = moduleUrl(`
  export function waitUntil(promise) { globalThis.__openComputeFacetTestBackground.push(Promise.resolve(promise)); }
`);
globalThis.__openComputeFacetTestBackground = background;
const runtime = moduleUrl(`
  export function loopbackDurableObjectMetadata(value) {
    return globalThis.__openComputeFacetTestMetadata.get(value);
  }
`);
const tunnel = moduleUrl(`
  export function socketAuthorityWire(value) { return { kind: "test", value }; }
`);
const privateControls = new WeakMap();
const originalConnects = new WeakMap();
globalThis.__openComputeFacetOriginalConnects = originalConnects;
const issuedDomains = [];
globalThis.__openComputeFacetIssuedDomains = issuedDomains;
globalThis.__openComputeFacetPrivateControls = privateControls;
globalThis.__openComputeFacetAdmission = () => {};
const wrapped = moduleUrl(`
  export default {
    createPrivateTransport(raw) {
      return globalThis.__openComputeFacetPrivateControls.get(raw) ?? raw;
    },
    validatePublicTransport() {},
    connectPublicTransport(raw, address, options) {
      const connect = globalThis.__openComputeFacetOriginalConnects.get(raw) ?? raw.connect.bind(raw);
      return connect(address, options);
    },
    admitSubrequest() { globalThis.__openComputeFacetAdmission(); }
  };
`);
const rpcTransport = moduleUrl(`
  export function createRpcTransport(raw, domain) {
    globalThis.__openComputeFacetIssuedDomains.push(domain);
    return raw;
  }
`);
const { TenantFacets } = await importRuntime("durable-objects/facets.ts", {
  "cloudflare-internal:wrapped-binding": wrapped,
  "../bindings/rpc-transport.js": rpcTransport,
  "cloudflare:workers": cloudflare,
  "../loader/wrappers/runtime.js": runtime,
  "../sockets/tunnel.js": tunnel,
});

const authority = Object.freeze({
  instanceId: "019c0000000070008000000000000001",
  workerId: "019c0000-0000-7000-8000-000000000002",
  versionId: "019c0000-0000-7000-8000-000000000003",
  workerCodeSha256: "a".repeat(64),
  objectId: "b".repeat(64),
  namespaceResourceId: "019c0000-0000-7000-8000-000000000004",
  objectGeneration: 1,
  routeGeneration: 1,
  className: "Root",
});

function fixture() {
  const calls = [];
  const nativeClasses = new WeakSet();
  const nativeFacets = {
    create(name, depth, id, actorClass) {
      if (!nativeClasses.has(actorClass))
        throw new TypeError("Invalid native class");
      calls.push({ kind: "native-create", name, depth, id, actorClass });
    },
    revoke() {},
  };
  const manager = {
    async __openComputePrepareNativeFacet(_authority, path) {
      calls.push({ kind: "prepare-native", path });
      return "physical-child";
    },
    async __openComputeFacetCall(_authority, path, descriptor, method, args) {
      calls.push({ kind: "call", path, descriptor, method, args });
      return method === "increment" ? 1 : { method, args };
    },
    async __openComputeFacetGet(_authority, path, descriptor, property) {
      calls.push({ kind: "get", path, descriptor, property });
      return property === "label" ? "facet-label" : undefined;
    },
    async __openComputeFacetFetch(_authority, path, descriptor, request) {
      calls.push({ kind: "fetch", path, descriptor, url: request.url });
      return new Response("facet-response");
    },
    async __openComputeFacetAbort(_authority, path, name, reason) {
      calls.push({ kind: "abort", path, name, reason });
    },
    async __openComputeFacetDelete(_authority, path, name) {
      calls.push({ kind: "delete", path, name });
    },
    async __openComputeFacetClone(_authority, path, source, destination) {
      calls.push({ kind: "clone", path, source, destination });
    },
    async __openComputePrepareFacetConnect(
      _authority,
      path,
      descriptor,
      token,
      socket,
    ) {
      calls.push({ kind: "prepare-connect", path, descriptor, token, socket });
    },
    async __openComputeCancelFacetConnect(token) {
      calls.push({ kind: "cancel-connect", token });
    },
    connect(address, options) {
      calls.push({ kind: "connect", address, options });
      return {
        address,
        options,
        opened: Promise.resolve(),
        closed: Promise.resolve(),
      };
    },
  };
  return {
    calls,
    facets: new TenantFacets(manager, authority, [], "root-id", nativeFacets),
    manager,
    nativeClasses,
    nativeFacets,
  };
}

function loopback(entrypoint, props) {
  const value = Object.freeze({});
  metadata.set(value, { entrypoint, props });
  return value;
}

test("public facet calls share native admission while startup and lifecycle stay private", async () => {
  const { calls, manager, nativeClasses, nativeFacets } = fixture();
  const control = { ...manager };
  privateControls.set(manager, control);
  for (const name of [
    "__openComputePrepareNativeFacet",
    "__openComputeFacetCall",
    "__openComputeFacetGet",
    "__openComputeFacetFetch",
    "__openComputeFacetAbort",
    "__openComputeFacetDelete",
    "__openComputeFacetClone",
  ])
    manager[name] = () => {
      throw Error("unexpected public control dispatch");
    };
  let admitted = 0;
  globalThis.__openComputeFacetAdmission = () => {
    if (admitted >= 2) throw Error("Too many subrequests.");
    admitted++;
  };
  try {
    const facets = new TenantFacets(
      manager,
      authority,
      [],
      "root-id",
      nativeFacets,
    );
    const actorClass = Object.freeze({});
    nativeClasses.add(actorClass);
    const stub = facets.get("dynamic", () => ({ class: actorClass }));
    assert.equal(await stub.increment(), 1);
    assert.equal(await stub.label, "facet-label");
    assert.throws(() => stub.increment(), /Too many subrequests/);
    facets.clone("dynamic", "copy");
    facets.delete("copy");
    facets.abort("dynamic", "stop");
    await Promise.all(background.splice(0));
    assert.equal(admitted, 2);
    assert.deepEqual(
      calls.map((call) => call.kind),
      [
        "prepare-native",
        "native-create",
        "call",
        "prepare-native",
        "native-create",
        "get",
        "clone",
        "delete",
        "abort",
      ],
    );
  } finally {
    globalThis.__openComputeFacetAdmission = () => {};
  }
});

test("logical facets forward methods, properties, fetch, props, and inherited ids", async () => {
  const { calls, facets } = fixture();
  let startups = 0;
  const stub = facets.get("child", () => {
    startups += 1;
    return { class: loopback("Child", { marker: "value" }) };
  });
  assert.equal(await stub.increment(), 1);
  assert.equal(await stub.label, "facet-label");
  assert.equal(
    await (await stub.fetch("https://facet.invalid/path")).text(),
    "facet-response",
  );
  assert.equal(startups, 1);
  assert.deepEqual(
    calls.map((call) => [call.kind, call.path]),
    [
      ["call", ["child"]],
      ["get", ["child"]],
      ["fetch", ["child"]],
    ],
  );
  assert.deepEqual(calls[0].descriptor, {
    entrypoint: "Child",
    id: "root-id",
    props: { marker: "value" },
  });
});

for (const failure of ["throw", "opened", "closed"]) {
  test(`facet CONNECT ${failure} cleans up after late private preparation`, async () => {
    const { facets, manager } = fixture();
    const entered = Promise.withResolvers();
    const released = Promise.withResolvers();
    const pending = new Set();
    const events = [];
    manager.__openComputePrepareFacetConnect = async (
      _authority,
      _path,
      _descriptor,
      token,
    ) => {
      entered.resolve();
      await released.promise;
      pending.add(token);
      events.push("prepared");
    };
    manager.__openComputeCancelFacetConnect = async (token) => {
      pending.delete(token);
      events.push("cancelled");
    };
    const error = new TypeError("native socket option failure");
    const socket = {
      opened: failure === "opened" ? Promise.reject(error) : Promise.resolve(),
      closed: failure === "opened" ? Promise.reject(error) : Promise.resolve(),
    };
    manager.connect = () => {
      if (failure === "throw") throw error;
      return socket;
    };
    const stub = facets.get("socket", () => ({ class: loopback("Child") }));
    if (failure === "throw")
      assert.throws(
        () => stub.connect("example.com:443"),
        (value) => value === error,
      );
    else assert.equal(stub.connect("example.com:443"), socket);
    await entered.promise;
    assert.deepEqual(events, []);
    released.resolve();
    await Promise.all(background.splice(0));
    assert.deepEqual(events, ["prepared", "cancelled"]);
    assert.equal(pending.size, 0);
  });
}

test("facet startup callbacks receive no arguments", async () => {
  const { facets } = fixture();
  const facet = facets.get("child", function (...args) {
    assert.deepEqual(args, []);
    return { class: loopback("Child") };
  });
  assert.equal(await facet.increment(), 1);
});

test("clone and delete are ordered before destination startup and clear cached callbacks", async () => {
  const { calls, facets, manager } = fixture();
  let releaseClone;
  manager.__openComputeFacetClone = async (...args) => {
    calls.push({ kind: "clone", args });
    await new Promise((resolve) => {
      releaseClone = resolve;
    });
  };
  let startups = 0;
  const sourceClass = loopback("Child", undefined);
  await facets.get("source", () => ({ class: sourceClass })).increment();
  facets.clone("source", "destination");
  const pending = facets
    .get("destination", () => {
      startups += 1;
      return { class: sourceClass, id: "destination-id" };
    })
    .increment();
  while (releaseClone === undefined) await Promise.resolve();
  assert.equal(startups, 0);
  releaseClone();
  assert.equal(await pending, 1);
  assert.equal(startups, 1);
  facets.delete("destination");
  const fresh = facets
    .get("destination", () => {
      startups += 1;
      return { class: sourceClass, id: "fresh-id" };
    })
    .increment();
  assert.equal(await fresh, 1);
  assert.equal(startups, 2);
  await Promise.all(background.splice(0));
  assert.deepEqual(calls.filter((call) => call.kind === "clone").length, 1);
  assert.deepEqual(calls.filter((call) => call.kind === "delete").length, 1);
});

test("abort rejects existing stubs with the caller reason and permits a fresh startup", async () => {
  const { calls, facets } = fixture();
  const childClass = loopback("Child", undefined);
  const stale = facets.get("child", () => ({ class: childClass, id: "first" }));
  assert.equal(await stale.increment(), 1);
  const reason = new Error("facet-aborted");
  facets.abort("child", reason);
  await assert.rejects(stale.increment(), (error) => error === reason);
  const fresh = facets.get("child", () => ({
    class: childClass,
    id: "second",
  }));
  assert.equal(await fresh.increment(), 1);
  await Promise.all(background.splice(0));
  assert.equal(calls.filter((call) => call.kind === "abort").length, 1);
  assert.equal(calls.filter((call) => call.kind === "call").length, 2);
});

test("facet validation and exposed surface match the Cloudflare contract", async () => {
  const { calls, facets } = fixture();
  assert.deepEqual(Reflect.ownKeys(facets), []);
  assert.deepEqual(
    Object.getOwnPropertyNames(Object.getPrototypeOf(facets)).sort(),
    ["abort", "clone", "constructor", "delete", "get"],
  );
  assert.throws(
    () => facets.get("x".repeat(257), () => ({ class: loopback("Child") })),
    /too long/,
  );
  assert.throws(
    () =>
      new TenantFacets({}, authority, ["a", "b", "c"], "id").get("d", () => ({
        class: loopback("Child"),
      })),
    /depth limit/,
  );
  await assert.rejects(
    facets.get("invalid", () => ({ class: {} })).increment(),
    /DO_RUNTIME_EXCEPTION/,
  );
  const connected = facets
    .get("socket", () => ({ class: loopback("Child") }))
    .connect("example.com:443", { allowHalfOpen: true });
  assert.match(connected.address, /^[0-9a-f]{32}\.facet-connect\.invalid:1$/);
  await Promise.all(background.splice(0));
  assert.equal(
    calls.some((call) => call.kind === "prepare-connect"),
    true,
  );
});

test("dynamic facet classes stay local while only the descriptor crosses RPC", async () => {
  const { calls, facets, nativeClasses } = fixture();
  const actorClass = Object.freeze({});
  nativeClasses.add(actorClass);
  assert.equal(
    await facets.get("dynamic", () => ({ class: actorClass })).increment(),
    1,
  );
  assert.deepEqual(
    calls.map((call) => call.kind),
    ["prepare-native", "native-create", "call"],
  );
  assert.equal(calls[1].actorClass, actorClass);
  assert.equal(calls[1].depth, 1);
  assert.deepEqual(calls[2].descriptor, { native: true, id: "root-id" });
});

test("abort during native facet preparation prevents a stale creation", async () => {
  const { calls, facets, manager, nativeClasses } = fixture();
  const actorClass = Object.freeze({});
  nativeClasses.add(actorClass);
  const entered = Promise.withResolvers();
  const released = Promise.withResolvers();
  manager.__openComputePrepareNativeFacet = async () => {
    entered.resolve();
    await released.promise;
    return "physical-child";
  };
  const pending = facets
    .get("dynamic", () => ({ class: actorClass }))
    .increment();
  await entered.promise;
  const reason = new Error("aborted-during-prepare");
  facets.abort("dynamic", reason);
  released.resolve();
  await assert.rejects(pending, (error) => error === reason);
  assert.equal(
    calls.some((call) => call.kind === "native-create" || call.kind === "call"),
    false,
  );
});

test("invalid dynamic facet ids fail before registration or native creation", async () => {
  const { calls, facets, nativeClasses } = fixture();
  const actorClass = Object.freeze({});
  nativeClasses.add(actorClass);
  await assert.rejects(
    facets
      .get("dynamic", () => ({ class: actorClass, id: "x".repeat(2049) }))
      .increment(),
    /DO_RUNTIME_EXCEPTION/,
  );
  assert.deepEqual(calls, []);
});

test("facet issuance retains private control and original native CONNECT when the raw prototype is poisoned", async () => {
  const { calls, manager, nativeFacets } = fixture();
  const control = { ...manager };
  privateControls.set(manager, control);
  originalConnects.set(manager, manager.connect.bind(manager));
  let hits = 0;
  const prototype = {};
  for (const name of [
    "connect",
    "__openComputeFacetCall",
    "__openComputeFacetFetch",
    "__openComputePrepareFacetConnect",
  ]) {
    delete manager[name];
    Object.defineProperty(prototype, name, {
      get() {
        hits++;
        throw Error("private facet receiver reached a public getter");
      },
    });
  }
  Object.setPrototypeOf(manager, prototype);
  const facets = new TenantFacets(
    manager,
    authority,
    [],
    "root-id",
    nativeFacets,
  );
  const stub = facets.get("child", () => ({ class: loopback("Child") }));
  assert.equal(await stub.increment(), 1);
  assert.equal(
    await (await stub.fetch("https://facet.invalid/path")).text(),
    "facet-response",
  );
  const socket = stub.connect("example.com:443", { allowHalfOpen: true });
  await socket.opened;
  await Promise.all(background.splice(0));
  assert.equal(issuedDomains.at(-1), "facet");
  assert.equal(hits, 0);
  assert.deepEqual(
    calls.filter((call) => call.kind === "call").map((call) => call.path),
    [["child"]],
  );
  assert.equal(calls.filter((call) => call.kind === "connect").length, 1);
});
