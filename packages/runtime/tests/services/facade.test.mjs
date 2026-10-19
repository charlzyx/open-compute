import assert from "node:assert/strict";
import test from "node:test";
import { compileRuntime, moduleUrl } from "../compiled-runtime.mjs";

globalThis.scheduler = { wait: () => new Promise(() => {}) };

const cloudflare = moduleUrl(`
  export class RpcTarget {}
  export function RpcStub(receiver) {
    const stub = (...args) => receiver(...args);
    Object.setPrototypeOf(stub, RpcStub.prototype);
    stub[Symbol.dispose] = () => {};
    return stub;
  }
  export class WorkerEntrypoint { constructor(ctx, env) { this.ctx = ctx; this.env = env; } }
  export let env = {};
  export const connectCalls = [];
  export const exports = {
    __OpenComputeServiceConnectTransport({ props }) {
      return { connect(address, options) {
        const socket = { address, options };
        connectCalls.push({ props, address, options, socket });
        return socket;
      } };
    },
  };
  export const background = [];
  export function waitUntil(promise) { background.push(Promise.resolve(promise)); }
  export function withEnv(next, action) {
    const previous = env;
    env = next;
    try {
      const result = action();
      if (result && typeof result.then === "function") {
        return Promise.resolve(result).finally(() => { env = previous; });
      }
      env = previous;
      return result;
    } catch (error) { env = previous; throw error; }
  }
`);
const scopeUrl = moduleUrl(
  await compileRuntime("services/scope.ts", {
    "cloudflare:workers": cloudflare,
  }),
);
const wrapped = moduleUrl(`
  import { RpcStub, RpcTarget } from ${JSON.stringify(cloudflare)};
  function createServiceRpcStub(policy) {
    const native = new RpcStub(policy);
    return new Proxy(native, {
      apply(_owner, _receiver, args) {
        if (typeof policy !== "function") throw new TypeError("SERVICE_ENTRYPOINT_NOT_FOUND");
        return policy(...args);
      },
      get(owner, property) {
        if (property === "constructor" || property === "__proto__") return Reflect.get(owner, property);
        if (typeof property === "symbol" && property !== Symbol.dispose) return undefined;
        if (property === "dup") return () => {
          const duplicate = policy.dup();
          const sameKind = typeof policy === "function"
            ? typeof duplicate === "function"
            : duplicate instanceof RpcTarget;
          if (!sameKind) throw new TypeError("native policy duplicate must preserve its receiver kind");
          return createServiceRpcStub(duplicate);
        };
        return Reflect.get(policy, property);
      }
    });
  }
  export default { createServiceRpcStub };
`);
const serviceErrors = moduleUrl(`
  export const bindingError = code => Object.assign(new Error(code), { stableCode: code });
`);
const deadlineUrl = moduleUrl(
  await compileRuntime("services/deadline.ts", {
    "../loader/shared.js": serviceErrors,
  }),
);
const transferUrl = moduleUrl(
  await compileRuntime("services/capability-transfer.ts", {
    "cloudflare:workers": cloudflare,
    "../loader/shared.js": serviceErrors,
    "./deadline.js": deadlineUrl,
  }),
);
const privateRpc = moduleUrl(
  `export const sources = [];
   export function createRpcTransport(value) { sources.push(value); return value; }`,
);
const privateRpcModule = await import(privateRpc);
const capabilitiesUrl = moduleUrl(
  await compileRuntime("services/capabilities.ts", {
    "../bindings/rpc-disposal.js": moduleUrl(
      await compileRuntime("bindings/rpc-disposal.ts"),
    ),
    "cloudflare:workers": cloudflare,
    "./scope.js": scopeUrl,
    "./deadline.js": deadlineUrl,
    "./capability-transfer.js": transferUrl,
    "../bindings/rpc-transport.js": privateRpc,
    "./rpc-member.js": moduleUrl(
      await compileRuntime("services/rpc-member.ts"),
    ),
  }),
);
const handoffUrl = moduleUrl(
  await compileRuntime("services/websocket-handoff.ts"),
);
const facadeUrl = moduleUrl(
  await compileRuntime("services/facade.ts", {
    "./scope.js": scopeUrl,
    "./capabilities.js": capabilitiesUrl,
    "./websocket-handoff.js": handoffUrl,
  }),
);
const cloudflareModule = await import(cloudflare);
const {
  decodeServiceValue: decodePrivateServiceValue,
  encodeServiceValue: encodePrivateServiceValue,
  serviceCapabilityController,
} = await import(capabilitiesUrl);
const {
  ServiceBinding: ServicePolicy,
  attachServiceWebSocketHandoffs,
  completeServiceScope,
  createServiceBinding: createPrivateServiceBinding,
  captureServiceWebSocketHandoffs,
  registerServiceBinding,
  serviceChannelCallbacks,
  channelWebSocketHandoffs,
  detachChannelWebSocketHandoffs,
} = await import(facadeUrl);
const {
  SERVICE_WEBSOCKET_HANDOFF_HEADER,
  appendServiceWebSocketHandoff,
  serviceWebSocketHandoffHandles,
} = await import(handoffUrl);
const { rootServiceFrame, childServiceFrame, withServiceScope } = await import(
  scopeUrl
);

const { default: nativeBinding } = await import(wrapped);
const createStub = nativeBinding.createServiceRpcStub.bind(nativeBinding);
test("remote capability authority keeps native construction local through duplication and disposal", () => {
  const events = [];
  let next = 0;
  let denyDuplicate = false;
  function remote() {
    const identity = next++;
    const methods = {
      beginCapability: (...args) => events.push(["begin", identity, ...args]),
      releaseRetention: (...args) =>
        events.push(["release", identity, ...args]),
      completeOperation: (...args) =>
        events.push(["complete", identity, ...args]),
      retainCapability: (...args) => events.push(["retain", identity, ...args]),
      dup() {
        if (denyDuplicate) throw Error("duplicate refused");
        return remote();
      },
      [Symbol.dispose]: () => events.push(["dispose", identity]),
    };
    return new Proxy(methods, {
      get(owner, property) {
        if (property === "createStub")
          throw Error("native factory cannot cross RPC");
        return Reflect.get(owner, property);
      },
    });
  }
  const localFactory = (policy) => ({ local: policy });
  const controller = serviceCapabilityController(remote(), localFactory);
  const copy = controller.dup();
  const policy = {};
  assert.deepEqual(copy.createStub(policy), { local: policy });
  controller.beginCapability("retention", { scopeId: "scope" });
  copy.retainCapability("operation", "caller", 123);
  copy.completeOperation("operation");
  copy.releaseRetention("retention");
  denyDuplicate = true;
  assert.throws(() => copy.dup(), /duplicate refused/);
  controller[Symbol.dispose]();
  copy[Symbol.dispose]();
  assert.deepEqual(events, [
    ["begin", 0, "retention", { scopeId: "scope" }],
    ["retain", 1, "operation", "caller", 123],
    ["complete", 1, "operation"],
    ["release", 1, "retention"],
    ["dispose", 0],
    ["dispose", 1],
  ]);
});
const nativeApply = Reflect.apply;
function publicTransport(raw) {
  const fetch = raw.fetch;
  const connect = raw.connect;
  return {
    fetch: (request) => nativeApply(fetch, raw, [request]),
    connect: (address, options) =>
      nativeApply(connect, raw, [address, options]),
  };
}
function ServiceBinding(raw) {
  return new ServicePolicy(
    rootTransport(raw),
    createStub,
    () => {},
    publicTransport(raw),
  );
}
function createServiceBinding(env) {
  return createPrivateServiceBinding(
    createStub,
    rootTransport(env.fetcher),
    () => {},
    publicTransport(env.fetcher),
  );
}
function rootTransport(raw) {
  return Object.assign(raw, {
    root() {
      return {
        rpc: raw.rpc.bind(raw),
        get: raw.get.bind(raw),
        ready() {},
        [Symbol.dispose]() {},
      };
    },
  });
}

test("public RPC admission charges siblings and child frames before dispatch while cleanup stays private", async () => {
  let admissions = 0;
  const events = [];
  const admit = () => {
    if (admissions >= 2) throw new Error("Too many subrequests.");
    admissions++;
  };
  function binding(name) {
    const control = {
      rpc() {
        events.push(["rpc", name]);
        return Promise.resolve(name);
      },
      get() {
        events.push(["get", name]);
        return Promise.resolve(7);
      },
      async completeRoot() {
        events.push(["complete", name]);
      },
    };
    const raw = {
      fetch() {
        throw new Error("unexpected HTTP transport use");
      },
      connect() {
        throw new Error("unexpected socket transport use");
      },
    };
    return new ServicePolicy(
      rootTransport(control),
      createStub,
      admit,
      publicTransport(raw),
    );
  }
  const left = binding("left");
  const right = binding("right");
  const env = { LEFT: left, RIGHT: right };
  const frame = rootServiceFrame();
  await withServiceScope(env, frame, async () => {
    assert.equal(await left.echo(), "left");
    assert.equal(await right.version, 7);
    assert.throws(() => left.echo(), /Too many subrequests/);
  });
  await withServiceScope(
    env,
    childServiceFrame(frame.scopeId, crypto.randomUUID()),
    async () => {
      assert.throws(() => right.echo(), /Too many subrequests/);
    },
  );
  await completeServiceScope(env, frame);
  assert.equal(admissions, 2);
  assert.deepEqual(events, [
    ["rpc", "left"],
    ["get", "right"],
    ["complete", "left"],
    ["complete", "right"],
  ]);
});

test("HTTP and CONNECT retain original native dispatch without a second policy admission", async () => {
  const calls = [];
  const raw = {
    async fetch(request) {
      calls.push(["fetch", request]);
      return new Response("ok");
    },
    connect(address, options) {
      calls.push(["connect", address, options]);
      return { opened: Promise.resolve({}) };
    },
  };
  const control = rootTransport({
    rpc() {
      throw new Error("unexpected control RPC");
    },
    get() {
      throw new Error("unexpected control getter");
    },
    async completeRoot() {},
  });
  const service = new ServicePolicy(
    control,
    createStub,
    () => {
      throw new Error("duplicate public admission");
    },
    publicTransport(raw),
  );
  const frame = rootServiceFrame();
  await withServiceScope({ SERVICE: service }, frame, async () => {
    assert.equal(
      await (await service.fetch("https://service.test/")).text(),
      "ok",
    );
    await service.connect("service.test:443", { secureTransport: "on" }).opened;
  });
  await completeServiceScope({ SERVICE: service }, frame);
  assert.equal(
    calls[0][1].headers.get("x-open-compute-service-frame"),
    JSON.stringify(frame),
  );
  assert.deepEqual(calls[1], [
    "connect",
    "service.test:443",
    { secureTransport: "on" },
  ]);
});

test("root controllers are reused per binding, disposed on drain, and excluded from child calls", async () => {
  const acquisitions = [];
  const releases = [];
  const calls = [];
  function transport(name) {
    const raw = {
      rpc(frame) {
        calls.push([name, frame]);
        return Promise.resolve(name);
      },
      get(frame) {
        return raw.rpc(frame);
      },
      fetch() {
        return Promise.resolve(new Response(name));
      },
      connect() {},
      async completeRoot() {
        // Disposal must still happen after a private backend cleanup failure.
        if (name === "second") throw new Error("backend unavailable");
      },
      root(scopeId) {
        acquisitions.push([name, scopeId]);
        return {
          rpc: raw.rpc,
          get: raw.get,
          ready() {},
          [Symbol.dispose]() {
            releases.push([name, scopeId]);
          },
        };
      },
    };
    return new ServicePolicy(raw, createStub, () => {}, publicTransport(raw));
  }
  const first = transport("first");
  const second = transport("second");
  const frame = rootServiceFrame();
  await withServiceScope({}, frame, async () => {
    assert.equal(await first.read(), "first");
    assert.equal(await first.value, "first");
    assert.equal(await (await first.fetch("http://service/")).text(), "first");
    assert.equal(await second.read(), "second");
  });
  assert.deepEqual(acquisitions, [
    ["first", frame.scopeId],
    ["second", frame.scopeId],
  ]);
  assert.deepEqual(releases, []);
  await completeServiceScope({ first, second }, frame);
  assert.deepEqual(releases, acquisitions);
  await completeServiceScope({ first, second }, frame);
  assert.equal(releases.length, 2);
  const child = childServiceFrame(frame.scopeId, crypto.randomUUID());
  await withServiceScope({}, child, async () => {
    assert.equal(await first.read(), "first");
  });
  assert.equal(acquisitions.length, 2);
  assert.equal(calls.at(-1)[1], child);
  assert.throws(
    () =>
      new ServicePolicy({ rpc() {}, get() {} }, createStub, () => {}, {
        fetch() {},
        connect() {},
      }),
    /SERVICE_BINDING_DENIED/,
  );
});
function decodeServiceValue(value) {
  return decodePrivateServiceValue(value, undefined, { createStub });
}
function encodeServiceValue(value, controller) {
  return encodePrivateServiceValue(value, { ...controller, createStub });
}

function activation(events) {
  return {
    async begin() {
      const handle = crypto.randomUUID();
      events.push(["begin", handle]);
      return { handle, frame: crypto.randomUUID(), deadlineMs: 30_000 };
    },
    async complete(handle) {
      events.push(["complete", handle]);
    },
    async release() {
      events.push(["release"]);
    },
  };
}

for (const [operation, member] of [
  ["get", "data"],
  ["call", "arrow"],
  ["get", "ownAccessor"],
  ["call", "shadow"],
  ["get", "absentValue"],
]) {
  test(`Service capability visibility denies ${operation} of class ${member}`, async () => {
    const events = [];
    let effects = 0;
    class Target extends cloudflareModule.RpcTarget {
      data = 9;
      arrow = () => {
        effects++;
        return 9;
      };
      constructor() {
        super();
        Object.defineProperty(this, "ownAccessor", {
          get() {
            effects++;
            return 9;
          },
        });
        this.shadow = () => {
          effects++;
          return 9;
        };
      }
      shadow() {
        return "prototype";
      }
      get absentValue() {
        return undefined;
      }
    }
    const source = encodeServiceValue(new Target(), {});
    source.handle.activate(activation(events));
    try {
      await assert.rejects(
        source.handle.call(rootServiceFrame(), operation, member, []),
        TypeError,
      );
      assert.equal(effects, 0);
      assert.equal(events.filter((event) => event[0] === "complete").length, 1);
    } finally {
      await source.handle.releaseCapability();
    }
  });
}

test("Service function named __call does not replace direct invocation", async () => {
  const fn = (number) => number + 3;
  fn.data = 20;
  fn.__call = function (number) {
    return this.data + number;
  };
  const source = encodeServiceValue(fn, {});
  source.handle.activate(activation([]));
  const stub = decodeServiceValue(source);
  try {
    await withServiceScope({}, rootServiceFrame(), async () => {
      assert.equal(await stub(2), 5);
      assert.equal(await stub.__call(2), 22);
    });
  } finally {
    await source.handle.releaseCapability();
  }
});

test("Service target named __call preserves its receiver", async () => {
  class Target extends cloudflareModule.RpcTarget {
    #value = 20;
    __call(number) {
      return this.#value + number;
    }
  }
  const source = encodeServiceValue(new Target(), {});
  source.handle.activate(activation([]));
  const stub = decodeServiceValue(source);
  try {
    await withServiceScope({}, rootServiceFrame(), async () => {
      assert.equal(await stub.__call(2), 22);
      await assert.rejects(
        source.handle.call(rootServiceFrame(), "apply", "__call", [2]),
        TypeError,
      );
    });
  } finally {
    await source.handle.releaseCapability();
  }
});

test("changing a Service target prototype does not expose its instance fields", async () => {
  class Target extends cloudflareModule.RpcTarget {
    data = 9;
    detach() {
      Object.setPrototypeOf(this, Object.prototype);
      return true;
    }
  }
  const source = encodeServiceValue(new Target(), {});
  source.handle.activate(activation([]));
  try {
    assert.equal(
      await source.handle.call(rootServiceFrame(), "call", "detach", []),
      true,
    );
    await assert.rejects(
      source.handle.call(rootServiceFrame(), "get", "data", []),
      TypeError,
    );
  } finally {
    await source.handle.releaseCapability();
  }
});

test("rejected Service members do not retain input callbacks", async () => {
  const events = [];
  let acquisitions = 0;
  let activations = 0;
  class Target extends cloudflareModule.RpcTarget {
    operation = () => {
      throw Error("instance method reached");
    };
  }
  const source = encodeServiceValue(new Target(), {
    retainCapability() {
      acquisitions++;
      throw Error("rejected member retained input");
    },
  });
  source.handle.activate(activation(events));
  const callback = {
    __openComputeServiceCapability: 1,
    kind: "function",
    handle: {
      activate() {
        activations++;
      },
    },
  };
  try {
    await assert.rejects(
      source.handle.call(rootServiceFrame(), "call", "operation", [callback]),
      TypeError,
    );
    assert.equal(acquisitions, 0);
    assert.equal(activations, 0);
    assert.equal(events.filter((event) => event[0] === "complete").length, 1);
  } finally {
    await source.handle.releaseCapability();
  }
});

test("Service capability visibility preserves inherited class methods and function own members", async () => {
  class Base extends cloudflareModule.RpcTarget {
    inherited() {
      return 4;
    }
  }
  class Target extends Base {
    get value() {
      return 3;
    }
  }
  const target = encodeServiceValue(new Target(), {});
  const fn = (number) => number + 3;
  fn.data = 7;
  fn.arrow = () => 8;
  Object.defineProperty(fn, "ownAccessor", {
    get() {
      return 9;
    },
  });
  Object.setPrototypeOf(
    fn,
    Object.assign(Object.create(Object.getPrototypeOf(fn)), {
      inheritedField: 10,
    }),
  );
  const callback = encodeServiceValue(fn, {});
  for (const source of [target, callback])
    source.handle.activate(activation([]));
  try {
    assert.equal(
      await target.handle.call(rootServiceFrame(), "get", "value", []),
      3,
    );
    assert.equal(
      await target.handle.call(rootServiceFrame(), "call", "inherited", []),
      4,
    );
    assert.equal(
      await callback.handle.call(rootServiceFrame(), "apply", "", [2]),
      5,
    );
    assert.equal(
      await callback.handle.call(rootServiceFrame(), "get", "data", []),
      7,
    );
    assert.equal(
      await callback.handle.call(rootServiceFrame(), "call", "arrow", []),
      8,
    );
    assert.equal(
      await callback.handle.call(rootServiceFrame(), "get", "ownAccessor", []),
      9,
    );
    await assert.rejects(
      callback.handle.call(rootServiceFrame(), "get", "inheritedField", []),
      TypeError,
    );
  } finally {
    await target.handle.releaseCapability();
    await callback.handle.releaseCapability();
  }
});

test("decoded Service collections preserve native group disposal and its receiver", () => {
  for (const value of [{ nested: { answer: 42 } }, [{ answer: 42 }]]) {
    let disposed = 0;
    Object.defineProperty(value, Symbol.dispose, {
      value() {
        assert.equal(this, value);
        disposed++;
      },
    });
    value.self = value;
    const decoded = decodeServiceValue(value);
    assert.notEqual(decoded, value);
    if (!Array.isArray(value)) assert.equal(decoded.self, decoded);
    assert.equal(
      Object.getOwnPropertyDescriptor(decoded, Symbol.dispose).enumerable,
      false,
    );
    decoded[Symbol.dispose]();
    assert.equal(disposed, 1);
  }
  assert.equal(decodeServiceValue({ answer: 42 })[Symbol.dispose], undefined);
});

test("private RPC promise disposal uses the native receiver", () => {
  let disposed = 0;
  const value = Promise.resolve(41);
  value[Symbol.dispose] = function () {
    assert.equal(this, value);
    disposed++;
  };
  const service = new ServiceBinding({
    rpc: () => value,
    fetch: () => Promise.resolve(new Response()),
    get() {},
    connect() {},
  });
  withServiceScope({}, rootServiceFrame(), () => {
    service.echo()[Symbol.dispose]();
  });
  assert.equal(disposed, 1);
});

test("Service facade preserves methods, getters, callbacks, returned targets, and root completion", async () => {
  const events = [];
  const raw = {
    connect(address, options) {
      if (address === "malformed")
        throw new TypeError("native malformed address");
      const opened =
        address === "failed.example:443"
          ? Promise.reject(new Error("native connect failed"))
          : Promise.resolve({});
      const socket = { address, options, opened };
      events.push(["connect", address, options]);
      return socket;
    },
    async rpc(_frame, method, args) {
      if (method === "add") return args[0] + args[1];
      if (method === "callback") {
        args[0].handle.activate(activation(events));
        const [callback] = decodeServiceValue(args);
        const value = await callback(41);
        callback[Symbol.dispose]();
        return value;
      }
      if (method === "target") {
        class Counter extends cloudflareModule.RpcTarget {
          get version() {
            return 7;
          }
          increment(value) {
            return value + 1;
          }
        }
        const envelope = encodeServiceValue(new Counter(), raw);
        envelope.handle.activate(activation(events));
        return envelope;
      }
      throw new Error("unexpected method");
    },
    async get(_frame, property) {
      if (property === "version") return 3;
      throw new Error("unexpected property");
    },
    async fetch(request) {
      return new Response(request.url);
    },
    async completeRoot(scopeId) {
      events.push(["root", scopeId]);
    },
    async beginCapability() {
      throw new Error("not used");
    },
    async releaseRetention() {},
    async completeOperation() {},
  };
  const service = createServiceBinding({ fetcher: raw });
  const nativeIdentity = {};
  assert.throws(
    () => registerServiceBinding(nativeIdentity, {}),
    /SERVICE_BINDING_DENIED/,
  );
  registerServiceBinding(nativeIdentity, service);
  assert.equal(service.then, undefined);
  assert.throws(() => service.constructor, /SERVICE_BINDING_DENIED/);
  const frame = rootServiceFrame();
  await withServiceScope({ SERVICE: service }, frame, async (scoped) => {
    assert.equal(await service.add(1, 2), 3);
    assert.equal(await service.version, 3);
    assert.equal(await service.callback((value) => value + 1), 42);
    const target = await service.target();
    assert.equal(target.then, undefined);
    assert.equal(target.constructor, cloudflareModule.RpcStub);
    assert.ok(target instanceof cloudflareModule.RpcStub);
    assert.equal(await target.increment(9), 10);
    assert.equal(await target.version, 7);
    target[Symbol.dispose]();
    assert.equal(
      await (await service.fetch("https://example.invalid/path")).text(),
      "https://example.invalid/path",
    );
    const socket = service.connect("example.com:443", { allowHalfOpen: true });
    assert.equal(socket.address, "example.com:443");
    assert.deepEqual(socket.options, { allowHalfOpen: true });
    assert.deepEqual(
      events.find((event) => event[0] === "connect"),
      ["connect", "example.com:443", { allowHalfOpen: true }],
    );
    const ipv6 = { hostname: "2606:4700:4700::1111", port: 443 };
    const ipv6Socket = service.connect(ipv6);
    assert.equal(ipv6Socket.address, ipv6);
    await completeServiceScope({ ...scoped, NATIVE: nativeIdentity }, frame);
  });
  await Promise.allSettled(cloudflareModule.background);
  assert.equal(events.filter((event) => event[0] === "begin").length, 3);
  assert.equal(events.filter((event) => event[0] === "complete").length, 3);
  assert.equal(events.filter((event) => event[0] === "release").length, 2);
  assert.deepEqual(events.at(-1), ["root", frame.scopeId]);
});

test("returned Service stubs preserve frame authority and independently dispose duplicates", async () => {
  for (const kind of ["target", "function"]) {
    const calls = [];
    let releases = 0;
    let disposals = 0;
    let denyDuplicate = true;
    const handle = () => ({
      call(frame, operation, method, args) {
        calls.push([frame, operation, method, args]);
        return { value: args[0] };
      },
      dup() {
        if (denyDuplicate) throw Error("duplicate refused");
        return handle();
      },
      releaseCapability() {
        releases += 1;
      },
      [Symbol.dispose]() {
        disposals += 1;
      },
    });
    const stub = decodeServiceValue({
      __openComputeServiceCapability: 1,
      kind,
      handle: handle(),
    });
    assert.equal(stub.constructor, cloudflareModule.RpcStub);
    assert.ok(stub instanceof cloudflareModule.RpcStub);
    assert.equal(stub.__proto__, cloudflareModule.RpcStub.prototype);
    assert.equal(stub[Symbol.iterator], undefined);
    assert.equal(typeof stub.prototype, "function");
    assert.throws(() => stub.__openComputeServiceRpc, /SERVICE_BINDING_DENIED/);
    const call = kind === "function" ? stub : stub.echo;
    assert.throws(() => call(1), /SERVICE_BINDING_DENIED/);
    assert.throws(() => stub.dup(), /duplicate refused/);
    denyDuplicate = false;
    const copy = stub.dup();
    const duplicate = stub.dup;
    const frame = rootServiceFrame();
    await withServiceScope({}, frame, async () => {
      assert.deepEqual(await call(1), { value: 1 });
      if (kind === "target")
        assert.throws(() => stub(1), {
          name: "TypeError",
          message: "SERVICE_ENTRYPOINT_NOT_FOUND",
        });
      stub[Symbol.dispose]();
      stub[Symbol.dispose]();
      assert.throws(() => call(2), /SERVICE_BINDING_DENIED/);
      assert.throws(() => duplicate(), /SERVICE_BINDING_DENIED/);
      assert.equal(releases, 0);
      const callCopy = kind === "function" ? copy : copy.echo;
      assert.deepEqual(await callCopy(2), { value: 2 });
      copy[Symbol.dispose]();
      copy[Symbol.dispose]();
      assert.throws(() => copy.dup(), /SERVICE_BINDING_DENIED/);
    });
    await Promise.allSettled(cloudflareModule.background);
    assert.equal(releases, 1);
    assert.equal(disposals, 2);
    assert.deepEqual(calls, [
      [
        frame,
        kind === "function" ? "apply" : "call",
        kind === "function" ? "" : "echo",
        [1],
      ],
      [
        frame,
        kind === "function" ? "apply" : "call",
        kind === "function" ? "" : "echo",
        [2],
      ],
    ]);
  }
});

test("callback admission refuses invalid deadlines before execution and cannot revive a released target", async () => {
  let calls = 0;
  const events = [];
  class Target extends cloudflareModule.RpcTarget {
    echo() {
      calls += 1;
      return 1;
    }
    [Symbol.dispose]() {
      events.push("target-dispose");
    }
  }
  const controller = {
    dup() {
      return this;
    },
    [Symbol.dispose]() {
      events.push("controller-dispose");
    },
  };
  for (const deadlineMs of [0, 30_001, 1.5, "1000"]) {
    const encoded = encodeServiceValue(new Target(), controller);
    const retention = {
      dup() {
        return this;
      },
      async begin() {
        return {
          handle: crypto.randomUUID(),
          frame: crypto.randomUUID(),
          deadlineMs,
        };
      },
      async complete() {
        events.push("complete");
      },
      async release() {
        events.push("release");
        throw Error("release failed");
      },
      [Symbol.dispose]() {
        events.push("retention-dispose");
      },
    };
    assert.throws(() => encoded.handle.activate({}), /SERVICE_BINDING_DENIED/);
    encoded.handle.activate(retention);
    assert.throws(
      () => encoded.handle.activate(retention),
      /SERVICE_BINDING_DENIED/,
    );
    await assert.rejects(
      encoded.handle.call(rootServiceFrame(), "call", "echo", []),
      /SERVICE_UNAVAILABLE/,
    );
    await assert.rejects(encoded.handle.releaseCapability(), /release failed/);
    await encoded.handle.releaseCapability();
    assert.throws(
      () => encoded.handle.activate(retention),
      /SERVICE_BINDING_DENIED/,
    );
    await assert.rejects(
      encoded.handle.call(rootServiceFrame(), "call", "echo", []),
      /SERVICE_BINDING_DENIED/,
    );
  }
  assert.equal(calls, 0);
  assert.deepEqual(
    events,
    Array.from({ length: 4 }, () => [
      "complete",
      "retention-dispose",
      "release",
      "retention-dispose",
      "controller-dispose",
      "target-dispose",
    ]).flat(),
  );
});

test("callback timeout retains the admitted operation until execution settles", async (t) => {
  let now = 0;
  t.mock.method(Date, "now", () => now);
  const previous = globalThis.scheduler;
  const deadline = Promise.withResolvers();
  const started = Promise.withResolvers();
  const execution = Promise.withResolvers();
  const events = [];
  const backgroundStart = cloudflareModule.background.length;
  globalThis.scheduler = {
    wait(ms) {
      assert.equal(ms, 10);
      return deadline.promise;
    },
  };
  try {
    const encoded = encodeServiceValue(() => {
      started.resolve();
      return execution.promise;
    }, {});
    const retention = activation(events);
    const begin = retention.begin;
    retention.begin = async () => ({ ...(await begin()), deadlineMs: 10 });
    encoded.handle.activate(retention);
    const operation = encoded.handle.call(rootServiceFrame(), "apply", "", []);
    await started.promise;
    now = 10;
    deadline.resolve();
    await assert.rejects(operation, /SERVICE_TIMEOUT/);
    assert.deepEqual(
      events.map((event) => event[0]),
      ["begin"],
    );
    execution.resolve(42);
    await Promise.all(cloudflareModule.background.slice(backgroundStart));
    assert.deepEqual(
      events.map((event) => event[0]),
      ["begin", "complete"],
    );
    assert.equal(events[0][1], events[1][1]);
    await encoded.handle.releaseCapability();
    assert.equal(events.at(-1)[0], "release");
  } finally {
    globalThis.scheduler = previous;
  }
});

function capabilityTimers(t) {
  const timers = [];
  t.mock.method(globalThis.scheduler, "wait", (delay, options) => {
    const timer = Promise.withResolvers();
    timers.push({ delay, options, ...timer });
    options?.signal.addEventListener(
      "abort",
      () => timer.reject(Error("cancelled deadline")),
      { once: true },
    );
    return timer.promise;
  });
  return timers;
}

for (const phase of ["execution", "handoff"]) {
  test(`capability disposal preserves private ownership until late ${phase} cleanup`, async (t) => {
    let now = 0;
    t.mock.method(Date, "now", () => now);
    const timers = capabilityTimers(t);
    const entered = Promise.withResolvers();
    const result = Promise.withResolvers();
    const backgroundStart = cloudflareModule.background.length;
    const events = [];
    let released = false;
    function retention() {
      let disposed = false;
      const check = () => {
        if (disposed) throw Error("retention stub disposed");
      };
      return {
        dup() {
          check();
          return retention();
        },
        async begin() {
          check();
          if (released) throw Error("capability released");
          return {
            handle: crypto.randomUUID(),
            frame: crypto.randomUUID(),
            deadlineMs: 10,
          };
        },
        async complete() {
          check();
          events.push("complete");
        },
        async release() {
          check();
          released = true;
          events.push("revoke");
        },
        [Symbol.dispose]() {
          disposed = true;
          events.push("retention-dispose");
        },
      };
    }
    function controller() {
      let disposed = false;
      return {
        dup() {
          if (disposed) throw Error("controller disposed");
          return controller();
        },
        async retainCapability() {
          entered.resolve();
          await result.promise;
          if (disposed) throw Error("receipt hop disposed");
          return {
            release() {
              events.push("late-release");
            },
          };
        },
        [Symbol.dispose]() {
          disposed = true;
          events.push("controller-dispose");
        },
      };
    }
    const source = encodeServiceValue(async () => {
      entered.resolve();
      return await result.promise;
    }, controller());
    source.handle.activate(retention());
    const nested = {
      __openComputeServiceCapability: 1,
      kind: "function",
      handle: {
        activate() {
          throw Error("late receipt activated");
        },
      },
    };
    const observed = source.handle
      .call(
        rootServiceFrame(),
        "apply",
        "",
        phase === "handoff" ? [nested] : [],
      )
      .catch((error) => error);
    await entered.promise;
    now = 10;
    for (const timer of timers)
      if (!timer.options.signal.aborted) timer.resolve();
    assert.match((await observed).message, /SERVICE_TIMEOUT/);
    await source.handle.releaseCapability();
    result.resolve(42);
    const background = await Promise.allSettled(
      cloudflareModule.background.slice(backgroundStart),
    );
    assert.ok(background.every((result) => result.status === "fulfilled"));
    assert.equal(events.filter((event) => event === "complete").length, 1);
    assert.equal(
      events.filter((event) => event === "retention-dispose").length,
      2,
    );
    assert.equal(
      events.filter((event) => event === "controller-dispose").length,
      phase === "handoff" ? 2 : 1,
    );
    assert.equal(
      events.filter((event) => event === "late-release").length,
      phase === "handoff" ? 1 : 0,
    );
  });
}

test("capability success and failure cancel the admitted deadline timer", async (t) => {
  t.mock.method(Date, "now", () => 0);
  const timers = capabilityTimers(t);
  const events = [];
  const capability = encodeServiceValue((value) => {
    if (value === "fail") throw Error("target rejected");
    return 42;
  }, {});
  capability.handle.activate(activation(events));
  assert.equal(
    await capability.handle.call(rootServiceFrame(), "apply", "", []),
    42,
  );
  await assert.rejects(
    capability.handle.call(rootServiceFrame(), "apply", "", ["fail"]),
    /target rejected/,
  );
  assert.ok(timers.length >= 2);
  assert.ok(timers.every((timer) => timer.options?.signal.aborted));
  assert.equal(events.filter((event) => event[0] === "complete").length, 2);
  await capability.handle.releaseCapability();
});

for (const operation of ["call", "get"]) {
  test(`capability ${operation} does not export a late result after its deadline`, async (t) => {
    let now = 0;
    t.mock.method(Date, "now", () => now);
    const timers = capabilityTimers(t);
    const entered = Promise.withResolvers();
    const result = Promise.withResolvers();
    const backgroundStart = cloudflareModule.background.length;
    const events = [];
    let duplicates = 0;
    const controller = {
      dup() {
        duplicates++;
        return this;
      },
      [Symbol.dispose]() {},
      retainCapability() {
        throw Error("expired result acquired retention");
      },
    };
    class Target extends cloudflareModule.RpcTarget {
      invoke() {
        entered.resolve();
        return result.promise;
      }
      get value() {
        return this.invoke();
      }
    }
    const source = encodeServiceValue(new Target(), controller);
    const root = activation(events);
    const begin = root.begin;
    root.begin = async () => ({ ...(await begin()), deadlineMs: 10 });
    source.handle.activate(root);
    const outcome = source.handle
      .call(
        rootServiceFrame(),
        operation,
        operation === "call" ? "invoke" : "value",
        [],
      )
      .catch((error) => error);
    await entered.promise;
    now = 10;
    timers.find((timer) => !timer.options?.signal.aborted).resolve();
    assert.match((await outcome).message, /SERVICE_TIMEOUT/);
    result.resolve(new Target());
    await Promise.allSettled(
      cloudflareModule.background.slice(backgroundStart),
    );
    assert.equal(
      duplicates,
      1,
      "late result must not create another authority-owning SourceCapability",
    );
    assert.equal(events.filter((event) => event[0] === "complete").length, 1);
    await source.handle.releaseCapability();
  });
}

test("capability rollback releases the whole batch after a synchronous cleanup failure", async (t) => {
  t.mock.method(Date, "now", () => 0);
  const timers = capabilityTimers(t);
  const events = [];
  let acquired = 0;
  const source = encodeServiceValue(
    () => {
      throw Error("must not dispatch");
    },
    {
      async retainCapability() {
        const index = acquired++;
        return {
          release() {
            events.push(["release", index]);
            if (index === 0) throw Error("private cleanup detail");
          },
        };
      },
    },
  );
  source.handle.activate(activation(events));
  const nested = (index) => ({
    __openComputeServiceCapability: 1,
    kind: "function",
    handle: {
      activate() {
        if (index === 1) throw Error("activation rejected");
      },
    },
  });
  await assert.rejects(
    source.handle.call(rootServiceFrame(), "apply", "", [nested(0), nested(1)]),
    (error) => {
      assert.equal(error.message, "SERVICE_UNAVAILABLE");
      return true;
    },
  );
  assert.deepEqual(
    events.filter((event) => event[0] === "release"),
    [
      ["release", 0],
      ["release", 1],
    ],
  );
  assert.equal(events.filter((event) => event[0] === "complete").length, 1);
  assert.ok(timers.every((timer) => timer.options.signal.aborted));
  await source.handle.releaseCapability();
});

test("capability cleanup keeps private retention away from replaced global helpers", async (t) => {
  t.mock.method(Date, "now", () => 0);
  capabilityTimers(t);
  const events = [];
  const source = encodeServiceValue(() => 42, {
    retainCapability: async () => ({
      release() {
        events.push("release");
      },
    }),
  });
  source.handle.activate(activation(events));
  const nested = {
    __openComputeServiceCapability: 1,
    kind: "function",
    handle: {
      activate() {
        throw Error("activation rejected");
      },
    },
  };
  const deny = () => {
    throw Error("private helper intercepted");
  };
  t.mock.method(globalThis, "WeakSet", deny);
  t.mock.method(globalThis, "AbortController", deny);
  t.mock.method(Promise, "race", deny);
  const originalMap = Array.prototype.map;
  Array.prototype.map = deny;
  let error;
  try {
    await source.handle.call(rootServiceFrame(), "apply", "", [nested]);
  } catch (failure) {
    error = failure;
  } finally {
    Array.prototype.map = originalMap;
    t.mock.restoreAll();
  }
  assert.match(error.message, /activation rejected/);
  assert.ok(events.includes("release"));
  assert.equal(
    events.filter((event) => Array.isArray(event) && event[0] === "complete")
      .length,
    1,
  );
  await source.handle.releaseCapability();
});

for (const owner of ["caller", "target"]) {
  test(`capability call releases a late ${owner} handoff without activating it`, async (t) => {
    let now = 0;
    t.mock.method(Date, "now", () => now);
    const timers = capabilityTimers(t);
    const entered = Promise.withResolvers();
    const receipt = Promise.withResolvers();
    const released = Promise.withResolvers();
    const events = [];
    const backgroundStart = cloudflareModule.background.length;
    let acquisitions = 0,
      activations = 0,
      dispatches = 0;
    const controller = {
      async retainCapability(_handle, retainedOwner) {
        assert.equal(retainedOwner, owner);
        acquisitions++;
        entered.resolve();
        return await receipt.promise;
      },
    };
    class Target extends cloudflareModule.RpcTarget {
      invoke() {
        dispatches++;
        return owner === "target" ? new Target() : 42;
      }
    }
    const source = encodeServiceValue(new Target(), controller);
    const root = activation(events);
    const begin = root.begin;
    root.begin = async () => ({ ...(await begin()), deadlineMs: 10 });
    source.handle.activate(root);
    const nested = {
      __openComputeServiceCapability: 1,
      kind: "function",
      handle: {
        activate() {
          activations++;
        },
      },
    };
    const outcome = source.handle.call(
      rootServiceFrame(),
      "call",
      "invoke",
      owner === "caller" ? [nested, nested] : [],
    );
    const observed = outcome.catch((error) => error);
    await entered.promise;
    now = 10;
    const pendingTimers = timers.filter(
      (timer) => !timer.options?.signal.aborted,
    );
    assert.ok(pendingTimers.length > 0);
    pendingTimers[0].resolve();
    assert.match((await observed).message, /SERVICE_TIMEOUT/);
    receipt.resolve({
      begin() {},
      complete() {},
      release() {
        released.resolve();
      },
    });
    // Own the continuation explicitly so a failed assertion cannot strand the deferred receipt.
    await Promise.allSettled(
      cloudflareModule.background.slice(backgroundStart),
    );
    assert.equal(activations, 0);
    assert.equal(acquisitions, 1);
    assert.equal(dispatches, owner === "target" ? 1 : 0);
    assert.equal(events.filter((event) => event[0] === "complete").length, 1);
    assert.ok(timers.every((timer) => timer.options?.signal.aborted));
    await Promise.race([
      released.promise,
      Promise.resolve().then(() => {
        throw Error("late retention was not released");
      }),
    ]);
    await source.handle.releaseCapability();
  });
}

test("nested callbacks and returned targets retain both owners and preserve structured values", async () => {
  const events = [];
  const sources = [];
  const controller = {
    async retainCapability(handle, owner) {
      events.push([owner, handle]);
      return activation([]);
    },
  };
  class Target extends cloudflareModule.RpcTarget {
    async invoke(callback) {
      return { target: new Target(), value: await callback(41) };
    }
    get version() {
      return 7;
    }
  }
  const target = encodeServiceValue(new Target(), controller);
  sources.push(target.handle);
  target.handle.activate(activation([]));
  const callback = encodeServiceValue((value) => value + 1, controller);
  sources.push(callback.handle);
  const encoded = await target.handle.call(
    rootServiceFrame(),
    "call",
    "invoke",
    [callback],
  );
  sources.push(encoded.target.handle);
  assert.equal(privateRpcModule.sources.includes(encoded.target.handle), false);
  assert.equal(encoded.value, 42);
  assert.deepEqual(
    events.map((event) => event[0]),
    ["caller", "target"],
  );
  assert.equal(
    await target.handle.call(rootServiceFrame(), "get", "version", []),
    7,
  );
  await assert.rejects(
    target.handle.call(rootServiceFrame(), "call", "version", []),
    /SERVICE_ENTRYPOINT_NOT_FOUND/,
  );
  await assert.rejects(
    target.handle.call(rootServiceFrame(), "call", "invoke", {}),
    /SERVICE_BINDING_DENIED/,
  );
  const value = Object.assign(Object.create(null), {
    values: [new Date(0), new Uint8Array([1]), new Map([[1, 2]])],
  });
  value.self = value;
  const roundTrip = decodeServiceValue(encodeServiceValue(value, controller));
  assert.equal(Object.getPrototypeOf(roundTrip), null);
  assert.equal(roundTrip.self, roundTrip);
  assert.deepEqual(roundTrip.values, value.values);
  await Promise.all(sources.map((source) => source.releaseCapability()));
});

test("Service connect preserves native address forms and errors", async () => {
  const events = [];
  const raw = {
    connect(address) {
      events.push(["connect", address]);
      if (address === "malformed")
        throw new TypeError("native malformed address");
      return {
        address,
        opened:
          address === "failed.example:443"
            ? Promise.reject(new Error("native connect failed"))
            : Promise.resolve({}),
      };
    },
    async fetch() {
      throw new Error("not used");
    },
    async rpc() {
      throw new Error("not used");
    },
    async get() {
      throw new Error("not used");
    },
  };
  const service = new ServiceBinding(raw);

  assert.throws(
    () => service.connect("malformed"),
    (error) =>
      error instanceof TypeError &&
      error.message === "native malformed address",
  );
  const failed = service.connect("failed.example:443");
  await failed.opened.catch(() => undefined);
  const successful = service.connect("ok.example:443");
  await successful.opened;
  assert.deepEqual(
    events.filter((event) => event[0] === "connect").map((event) => event[1]),
    ["malformed", "failed.example:443", "ok.example:443"],
  );
});

test("WebSocket fetch uses native transport and replaces caller-supplied frame headers", async () => {
  let forwarded;
  const service = new ServiceBinding({
    async fetch(request) {
      forwarded = request;
      return new Response(null, { status: 204 });
    },
    rpc() {},
    get() {},
    connect() {},
  });
  const frame = rootServiceFrame();
  await withServiceScope({ SERVICE: service }, frame, async () => {
    const response = await service.fetch(
      new Request("https://example.invalid/socket", {
        headers: {
          Upgrade: "websocket",
          "x-open-compute-service-frame": "forged",
        },
      }),
    );
    assert.equal(response.status, 204);
  });
  assert.equal(forwarded.url, "https://example.invalid/socket");
  assert.equal(forwarded.headers.get("upgrade"), "websocket");
  assert.deepEqual(
    JSON.parse(forwarded.headers.get("x-open-compute-service-frame")),
    frame,
  );
});

test("Service WebSocket handoff handles stay private and follow the native socket", async () => {
  const NativeResponse = globalThis.Response;
  globalThis.Response = class CloudflareResponse extends NativeResponse {
    constructor(body, init) {
      super(body, init);
      if (init?.webSocket)
        Object.defineProperty(this, "webSocket", { value: init.webSocket });
    }
  };
  const handle = "01991ec0-9d85-7abc-8def-0123456789ab";
  const socket = new EventTarget();
  try {
    const service = new ServiceBinding({
      async fetch() {
        const response = new Response(null, {
          status: 200,
          headers: { [SERVICE_WEBSOCKET_HANDOFF_HEADER]: handle },
          webSocket: socket,
        });
        return response;
      },
      rpc() {},
      get() {},
      connect() {},
    });
    const response = await withServiceScope(
      { SERVICE: service },
      rootServiceFrame(),
      () => service.fetch("https://example.invalid/socket"),
    );
    assert.equal(response.webSocket, socket);
    assert.equal(response.headers.has(SERVICE_WEBSOCKET_HANDOFF_HEADER), false);

    const returned = attachServiceWebSocketHandoffs(response);
    assert.equal(returned.webSocket, socket);
    assert.equal(
      returned.headers.get(SERVICE_WEBSOCKET_HANDOFF_HEADER),
      handle,
    );

    const forged = new Response(null, {
      headers: { [SERVICE_WEBSOCKET_HANDOFF_HEADER]: handle },
    });
    assert.equal(
      attachServiceWebSocketHandoffs(forged).headers.has(
        SERVICE_WEBSOCKET_HANDOFF_HEADER,
      ),
      false,
    );
  } finally {
    globalThis.Response = NativeResponse;
  }
});

test("Service WebSocket handoffs reject invalid handles and preserve the trusted chain", () => {
  const previous = globalThis.Response;
  globalThis.Response = class extends previous {
    constructor(body, init) {
      super(body, init);
      if (init?.webSocket)
        Object.defineProperty(this, "webSocket", { value: init.webSocket });
    }
  };
  const first = "01991ec0-9d85-7abc-8def-0123456789ab";
  const second = "01991ec0-9d85-7abc-8def-0123456789ac";
  try {
    const socket = new EventTarget();
    const response = new Response(null, { webSocket: socket });
    const chained = appendServiceWebSocketHandoff(
      appendServiceWebSocketHandoff(response, first),
      second,
    );
    assert.deepEqual(serviceWebSocketHandoffHandles(chained), [first, second]);
    assert.throws(
      () => appendServiceWebSocketHandoff(chained, first),
      /SERVICE_UNAVAILABLE/,
    );
    assert.throws(
      () => appendServiceWebSocketHandoff(response, "invalid"),
      /SERVICE_UNAVAILABLE/,
    );
    assert.throws(
      () => appendServiceWebSocketHandoff(new Response(null), first),
      /SERVICE_UNAVAILABLE/,
    );
    for (const raw of [
      "",
      "invalid",
      `${first},${first}`,
      Array(17).fill(first).join(","),
    ]) {
      const invalid = new Response(null, {
        webSocket: socket,
        headers: { [SERVICE_WEBSOCKET_HANDOFF_HEADER]: raw },
      });
      assert.throws(
        () => captureServiceWebSocketHandoffs(invalid),
        /SERVICE_UNAVAILABLE/,
      );
      assert.throws(
        () => serviceWebSocketHandoffHandles(invalid),
        /SERVICE_UNAVAILABLE/,
      );
    }
    const forged = new Response(null, {
      headers: { [SERVICE_WEBSOCKET_HANDOFF_HEADER]: first },
    });
    assert.throws(
      () => captureServiceWebSocketHandoffs(forged),
      /SERVICE_UNAVAILABLE/,
    );
    assert.throws(
      () => serviceWebSocketHandoffHandles(forged),
      /SERVICE_UNAVAILABLE/,
    );
    assert.deepEqual(channelWebSocketHandoffs(chained), []);
    const captured = captureServiceWebSocketHandoffs(chained);
    assert.equal(captured.webSocket, socket);
    assert.equal(captured.headers.has(SERVICE_WEBSOCKET_HANDOFF_HEADER), false);
    assert.deepEqual(
      serviceWebSocketHandoffHandles(attachServiceWebSocketHandoffs(captured)),
      [first, second],
    );
    assert.deepEqual(channelWebSocketHandoffs(captured), [first, second]);
    const transferred = detachChannelWebSocketHandoffs(
      attachServiceWebSocketHandoffs(captured),
    );
    assert.equal(transferred.webSocket, socket);
    assert.equal(
      transferred.headers.has(SERVICE_WEBSOCKET_HANDOFF_HEADER),
      false,
    );
    assert.deepEqual(channelWebSocketHandoffs(captured), []);
    assert.deepEqual(
      serviceWebSocketHandoffHandles(attachServiceWebSocketHandoffs(captured)),
      [],
    );
    assert.deepEqual(serviceWebSocketHandoffHandles(response), []);
  } finally {
    globalThis.Response = previous;
  }
});

test("Service fetch preserves streaming request bodies and explicit headers", async () => {
  const service = new ServiceBinding({
    async fetch(request) {
      assert.equal(request.method, "POST");
      assert.equal(request.headers.get("content-type"), "application/json");
      return new Response(await request.text());
    },
    rpc() {},
    get() {},
    connect() {},
  });
  await withServiceScope({ SERVICE: service }, rootServiceFrame(), async () => {
    const body = new ReadableStream({
      start(controller) {
        controller.enqueue(new TextEncoder().encode('{"workspace":'));
        controller.enqueue(new TextEncoder().encode('"/workspace"}'));
        controller.close();
      },
    });
    const response = await service.fetch("https://service.invalid/create", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body,
      duplex: "half",
    });
    assert.equal(await response.text(), '{"workspace":"/workspace"}');
  });
});

test("tenant reflection cannot read native Service transport or retention controller", async () => {
  const events = [];
  const raw = {
    async fetch() {
      return new Response(null);
    },
    async rpc(_frame, _method, args) {
      return args;
    },
    async get() {
      return 1;
    },
    connect() {},
    async completeRoot() {},
  };
  const retention = activation(events);
  const controller = {
    dup() {
      return this;
    },
    [Symbol.dispose]() {},
  };
  const secrets = new Set([raw, retention, controller]);
  const observed = [];
  const get = Reflect.get;
  const apply = Reflect.apply;
  const ProxyConstructor = globalThis.Proxy;
  const original = new cloudflareModule.RpcTarget();
  try {
    Reflect.get = (target, ...args) => {
      if (secrets.has(target)) observed.push("get");
      return get(target, ...args);
    };
    Reflect.apply = (method, receiver, args) => {
      if (secrets.has(receiver)) observed.push("apply");
      return apply(method, receiver, args);
    };
    globalThis.Proxy = function (target, handler) {
      if (secrets.has(target)) observed.push("Proxy");
      return new ProxyConstructor(target, handler);
    };
    const service = new ServiceBinding(raw);
    await withServiceScope(
      { SERVICE: service },
      rootServiceFrame(),
      async () => {
        assert.equal(
          await service
            .fetch("https://service.test/")
            .then((response) => response.status),
          200,
        );
      },
    );
    const encoded = encodeServiceValue(original, controller);
    encoded.handle.activate(retention);
    encoded.handle[Symbol.dispose]();
  } finally {
    Reflect.get = get;
    Reflect.apply = apply;
    globalThis.Proxy = ProxyConstructor;
  }
  assert.deepEqual(observed, []);
});

test("Service capability methods preserve arbitrary native property names", async () => {
  const names = ["中文", "with-hyphen", "", "x".repeat(129), "prototype"];
  class Target extends cloudflareModule.RpcTarget {
    #value = 20;
    value(number) {
      return this.#value + number;
    }
  }
  for (const name of names)
    Object.defineProperty(Target.prototype, name, {
      value: Target.prototype.value,
    });
  const fn = (number) => number + 3;
  for (const name of names) fn[name] = (number) => number + 20;
  for (const value of [new Target(), fn]) {
    const source = encodeServiceValue(value, {});
    source.handle.activate(activation([]));
    const stub = decodeServiceValue(source);
    try {
      await withServiceScope({}, rootServiceFrame(), async () => {
        for (const name of names) assert.equal(await stub[name](2), 22);
      });
    } finally {
      await source.handle.releaseCapability();
    }
  }
});

test("Service facade forwards arbitrary native method and getter property names", async () => {
  const calls = [];
  const service = new ServiceBinding({
    rpc(_frame, name, args) {
      calls.push(["call", name, args]);
      return Promise.resolve(22);
    },
    get(_frame, name) {
      calls.push(["get", name]);
      return Promise.resolve(20);
    },
    fetch: () => Promise.resolve(new Response()),
    connect() {},
    completeRoot() {},
    beginCapability() {},
    releaseRetention() {},
    completeOperation() {},
  });
  await withServiceScope({}, rootServiceFrame(), async () => {
    for (const name of [
      "中文",
      "with-hyphen",
      "",
      "x".repeat(129),
      "prototype",
    ]) {
      assert.equal(await service[name](2), 22);
      assert.equal(await service[name], 20);
      assert.deepEqual(calls.splice(0), [
        ["call", name, [2]],
        ["get", name],
      ]);
    }
  });
});

test("issued channel callbacks retain the canonical Service receiver and fresh caller authority", async () => {
  const calls = [];
  const raw = {
    async fetch(request) {
      calls.push(["fetch", request.url]);
      return new Response("issued");
    },
    connect(address) {
      calls.push(["connect", address]);
      return { opened: Promise.resolve({}) };
    },
  };
  const control = rootTransport({
    rpc() {},
    get() {},
    async completeRoot() {},
  });
  const policy = new ServicePolicy(
    control,
    createStub,
    () => {},
    publicTransport(raw),
  );
  const callbacks = serviceChannelCallbacks(policy);
  assert.ok(callbacks);
  assert.equal(serviceChannelCallbacks({ fetch: raw.fetch }), null);
  const originalFetch = ServicePolicy.prototype.fetch;
  const originalConnect = ServicePolicy.prototype.connect;
  try {
    ServicePolicy.prototype.fetch = () => {
      throw Error("poisoned public fetch");
    };
    ServicePolicy.prototype.connect = () => {
      throw Error("poisoned public connect");
    };
    await withServiceScope(
      { SERVICE: policy },
      rootServiceFrame(),
      async () => {
        assert.equal(
          await (
            await callbacks.fetch(new Request("https://issued.test/"))
          ).text(),
          "issued",
        );
        await callbacks.connect("issued.test:443").opened;
      },
    );
  } finally {
    ServicePolicy.prototype.fetch = originalFetch;
    ServicePolicy.prototype.connect = originalConnect;
  }
  assert.deepEqual(calls, [
    ["fetch", "https://issued.test/"],
    ["connect", "issued.test:443"],
  ]);
  await assert.rejects(
    callbacks.fetch(new Request("https://issued.test/")),
    /SERVICE_BINDING_DENIED/,
  );
});

test("issued Service channels retain canonical raw fetch and connect receivers after prototype changes", async () => {
  const calls = [];
  const socket = { opened: Promise.resolve({}), closed: Promise.resolve() };
  const prototype = {
    async rpc(_frame, _method, args) {
      return args;
    },
    async get() {
      return 1;
    },
    async completeRoot() {},
    async fetch(request) {
      calls.push(["fetch", this, request]);
      return new Response("canonical");
    },
    connect(address, options) {
      calls.push(["connect", this, address, options]);
      return socket;
    },
  };
  const raw = Object.create(prototype);
  const policy = createServiceBinding({ fetcher: raw });
  prototype.fetch = () => {
    throw Error("mutable prototype fetch must not dispatch");
  };
  prototype.connect = () => {
    throw Error("mutable prototype connect must not dispatch");
  };
  const frame = rootServiceFrame();
  try {
    await withServiceScope({ SERVICE: policy }, frame, async () => {
      const channel = serviceChannelCallbacks(policy);
      assert.ok(channel);
      assert.equal(
        await (
          await channel.fetch(new Request("https://service.test/"))
        ).text(),
        "canonical",
      );
      assert.equal(channel.connect("service.test:443"), socket);
    });
    assert.equal(calls.length, 2);
    assert.equal(calls[0][1], raw);
    assert.equal(calls[1][1], raw);
    assert.ok(calls[0][2].headers.has("x-open-compute-service-frame"));
    assert.equal(calls[1][2], "service.test:443");
  } finally {
    await completeServiceScope({ SERVICE: policy }, frame);
  }
});

test("private capability controller has no inherited optional authority getters", () => {
  const prototype = Object.prototype;
  const names = ["dup", Symbol.dispose];
  const before = names.map((name) =>
    Object.getOwnPropertyDescriptor(prototype, name),
  );
  let inherited = 0;
  try {
    for (const name of names)
      Object.defineProperty(prototype, name, {
        configurable: true,
        get() {
          inherited++;
          throw Error("private controller must not inherit authority");
        },
      });
    const transport = Object.create(null);
    Object.assign(transport, {
      beginCapability() {},
      releaseRetention() {},
      completeOperation() {},
      retainCapability() {},
    });
    const controller = serviceCapabilityController(transport, createStub);
    assert.equal(Object.getPrototypeOf(controller), null);
    assert.equal(controller.dup, undefined);
    assert.equal(controller[Symbol.dispose], undefined);
    assert.equal(inherited, 0);
  } finally {
    for (let index = 0; index < names.length; index++) {
      if (before[index])
        Object.defineProperty(prototype, names[index], before[index]);
      else delete prototype[names[index]];
    }
  }
});
