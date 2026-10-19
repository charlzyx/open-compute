import assert from "node:assert/strict";
import test from "node:test";
import {
  compileRuntime,
  importRuntime,
  moduleUrl,
} from "../compiled-runtime.mjs";

// Native stand-in owns its member table; it never reads the receiver's JavaScript prototype.
const native = moduleUrl(`
  const states = new WeakMap();
  export const resultModes = [];
  const nativeApply = Reflect.apply;
  const nativeThen = Promise.prototype.then;
  export function issue(type, members, fulfilled, invoke, dispose = () => {}) {
    const receiver = type === "promise" || type === "property"
      ? (...args) => invoke(...args) : {};
    states.set(receiver, { type, members, fulfilled: Promise.resolve(fulfilled), dispose });
    return receiver;
  }
  export default {
    rpcTransportKind(value) { return states.get(value)?.type ?? ""; },
    getRpcTransportMember(value, name, resultKind) { resultModes.push([name, resultKind]); return states.get(value).members[name]; },
    thenRpcTransport(value, fulfilled, rejected) {
      return nativeApply(nativeThen, states.get(value).fulfilled, [fulfilled, rejected]);
    },
    disposeRpcTransport(value) { states.get(value).dispose(); },
    duplicateRpcTransport(value) {
      const state = states.get(value);
      return issue(state.type, state.members, undefined, undefined, state.dispose);
    },
  };
`);
const { issue, resultModes } = await import(native);
const { createRpcTransport, disposeRpcResult } = await importRuntime(
  "bindings/rpc-transport.ts",
  {
    "cloudflare-internal:wrapped-binding": native,
    "../private-weak-map.js": moduleUrl(
      await compileRuntime("private-weak-map.ts"),
    ),
  },
);

function poison(receiver, hits) {
  const prototype = {};
  for (const name of [
    "root",
    "rpc",
    "get",
    "ready",
    "take",
    "startRpc",
    "cancelOrder",
    "then",
    "dup",
    Symbol.dispose,
  ])
    Object.defineProperty(prototype, name, {
      get() {
        hits.push([name, this]);
        throw Error("public prototype must not receive private authority");
      },
    });
  Object.setPrototypeOf(receiver, prototype);
}

test("private RPC view retains root pipeline, native then and disposal without prototype getters", async () => {
  const hits = [],
    calls = [];
  let released = 0;
  const holder = issue("stub", {}, undefined, undefined, () => {
    released++;
  });
  const ready = issue("property", {}, undefined, (...args) => {
    calls.push(["ready", ...args]);
    return issue("promise", {}, undefined, () => {});
  });
  const reply = { exact: "payload" };
  const rpc = issue("property", {}, undefined, (...args) => {
    calls.push(["rpc", ...args]);
    return issue("promise", {}, reply, () => {});
  });
  const rootResult = issue(
    "promise",
    { ready, rpc },
    holder,
    () => {},
    () => {
      released++;
    },
  );
  const rootMethod = issue("property", {}, undefined, (scope) => {
    calls.push(["root", scope]);
    return rootResult;
  });
  const raw = issue("fetcher", { root: rootMethod });
  for (const receiver of [raw, rootMethod, rootResult, ready, rpc, holder])
    poison(receiver, hits);
  const transport = createRpcTransport(raw, "transport");
  assert.equal(Object.getPrototypeOf(transport), null);
  assert.equal(createRpcTransport(raw, "transport"), transport);
  assert.equal(transport.then, undefined);
  const lease = transport.root("scope");
  await lease.ready();
  assert.equal(await lease.rpc({ scopeId: "scope" }, "method", [1]), reply);
  disposeRpcResult(await lease);
  lease[Symbol.dispose]();
  assert.equal(released, 2);
  assert.deepEqual(calls, [
    ["root", "scope"],
    ["ready"],
    ["rpc", { scopeId: "scope" }, "method", [1]],
  ]);
  assert.deepEqual(hits, []);
  assert.throws(
    () => createRpcTransport({ rpc() {} }, "transport"),
    /SERVICE_BINDING_DENIED/,
  );
});

test("private DO pipeline preserves raw versus control receiver and native holder disposal", async () => {
  const calls = [],
    hits = [];
  let disposed = 0;
  const holder = issue("stub", {}, undefined, undefined, () => {
    disposed++;
  });
  const payload = { typed: "result" };
  const taken = issue("promise", {}, payload, () => {});
  const take = issue("property", {}, undefined, () => taken);
  const provider = issue("promise", { take }, holder, () => {});
  const start = issue("property", {}, undefined, (...args) => {
    calls.push(["public", ...args]);
    return provider;
  });
  const cancel = issue("property", {}, undefined, (...args) => {
    calls.push(["control", ...args]);
    return issue("promise", {}, undefined, () => {});
  });
  const raw = issue("fetcher", { startRpc: start });
  const control = issue("fetcher", { cancelOrder: cancel });
  for (const receiver of [
    raw,
    control,
    provider,
    holder,
    start,
    cancel,
    take,
    taken,
  ])
    poison(receiver, hits);
  const started = createRpcTransport(raw, "transport").startRpc(
    "object",
    "channel",
    0,
    "call",
    "method",
    [],
    { value: "object" },
  );
  assert.equal(await started.take(), payload);
  disposeRpcResult(await started);
  await createRpcTransport(control, "transport").cancelOrder(
    "object",
    "channel",
    0,
  );
  assert.equal(disposed, 1);
  assert.equal(calls[0][0], "public");
  assert.deepEqual(calls[1], ["control", "object", "channel", 0]);
  assert.deepEqual(hits, []);
});

test("private retention activation restores only issued top-level native arguments", async () => {
  let observed;
  const retained = issue("stub", {}, undefined, undefined);
  const user = { exact: "user-data" };
  const activate = issue("property", {}, undefined, (...args) => {
    observed = args;
    return issue("promise", {}, undefined, () => {});
  });
  const source = issue("stub", { activate }, undefined, undefined);
  await createRpcTransport(source, "capability").activate(
    createRpcTransport(retained, "service-retention"),
    user,
  );
  assert.equal(observed[0], retained);
  assert.equal(observed[1], user);
});

test("private dup is issued only for native stubs while user named dup keeps RPC semantics", async () => {
  const calls = [];
  const remoteDup = issue("property", {}, undefined, () => {
    calls.push("remote-dup");
    return issue("promise", {}, "user-dup-result", () => {});
  });
  const fetcher = issue("fetcher", { dup: remoteDup });
  const pending = issue("promise", { dup: remoteDup }, undefined, () => {});
  assert.equal(createRpcTransport(fetcher, "transport").dup, undefined);
  assert.equal(createRpcTransport(pending, "service-root").dup, undefined);
  assert.deepEqual(calls, []);
  const userPending = issue("promise", { dup: remoteDup }, undefined, () => {});
  assert.equal(
    await createRpcTransport(userPending, "user").dup(),
    "user-dup-result",
  );
  const stub = issue("stub", {});
  const privateStub = createRpcTransport(stub, "service-retention");
  const duplicate = privateStub.dup();
  assert.notEqual(duplicate, privateStub);
  assert.equal(Object.getPrototypeOf(duplicate), null);
  duplicate[Symbol.dispose]();
  assert.deepEqual(calls, ["remote-dup"]);
});

test("only already-issued views can be reissued in the same authority domain", () => {
  const raw = issue("stub", {});
  const view = createRpcTransport(raw, "service-retention");
  assert.equal(createRpcTransport(view, "service-retention"), view);
  assert.throws(
    () => createRpcTransport(view, "service-root"),
    /SERVICE_BINDING_DENIED/,
  );
  assert.throws(
    () => createRpcTransport(new Proxy({}, {}), "service-retention"),
    /SERVICE_BINDING_DENIED/,
  );
});

test("facet RPC preserves user values while lifecycle results use the private data contract", async () => {
  const response = new Response("original-facet-response");
  const payload = { original: "user-payload" };
  const entries = {
    __openComputeFacetCall: payload,
    __openComputeFacetGet: payload,
    __openComputeFacetFetch: response,
    __openComputePrepareNativeFacet: "physical-child",
    __openComputeFacetDelete: undefined,
  };
  const members = Object.fromEntries(
    Object.entries(entries).map(([name, value]) => [
      name,
      issue("property", {}, undefined, () =>
        issue("promise", {}, value, () => {}),
      ),
    ]),
  );
  const manager = createRpcTransport(issue("fetcher", members), "facet");
  const before = resultModes.length;
  for (const [name, expected] of Object.entries(entries))
    assert.equal(await manager[name](), expected);
  assert.deepEqual(resultModes.slice(before), [
    ["__openComputeFacetCall", "value"],
    ["__openComputeFacetGet", "value"],
    ["__openComputeFacetFetch", "value"],
    ["__openComputePrepareNativeFacet", "data"],
    ["__openComputeFacetDelete", "data"],
  ]);
});
