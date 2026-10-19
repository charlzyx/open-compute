import type { PublicTransport } from "cloudflare-internal:wrapped-binding";
import { waitUntil } from "cloudflare:workers";
import { preserveRpcDisposal } from "../bindings/rpc-disposal.js";
import { privateWeakMap } from "../private-weak-map.js";
import { socketAuthorityWire } from "../sockets/tunnel.js";
import { sanitizeDoError } from "./errors.js";
import { encodeObjectIdentity, OBJECT_IDENTITY_HEADER } from "./identity.js";
import type {
  DoNativeFactories,
  DoObjectIdentity,
  DoRawTransport,
  DoRpcResultProvider,
} from "./protocol.js";

interface StubState {
  identity: DoObjectIdentity;
  createRpcStub: (policy: object) => object;
  isRpcStub: (value: unknown) => boolean;
  httpTransport: PublicTransport;
  rpcTransport: DoRawTransport;
  disposeRpcResult: (value: object) => void;
  createUserRpcTransport: (value: object) => object;
  control: DoRawTransport;
  order: StubOrder;
  rpcWrappers: WeakMap<object, object>;
  failed?: Error;
  pending: Set<(error: Error) => void>;
}
interface StubOrder {
  channelId: string;
  next: number;
  inFlight: number;
  starting: number;
  startTail: Promise<void>;
}
const stubState = privateWeakMap<object, StubState>();
const channelCallbacks = privateWeakMap<
  object,
  {
    readonly fetch: (request: Request) => Promise<Response>;
    readonly connect: (address: string) => Socket;
    readonly hold: (
      handles: readonly string[],
    ) => Promise<ReadableStream<Uint8Array>>;
  }
>();
const nativeChannelApply = Reflect.apply;
const nativeChannelGet = Reflect.get;
const FORBIDDEN_RPC = new Set([
  "constructor",
  "prototype",
  "__proto__",
  "then",
  "dup",
  "fetch",
  "connect",
  "alarm",
  "webSocketMessage",
  "webSocketClose",
  "webSocketError",
]);
const LOCAL_STUB_MEMBERS = new Set(["fetch", "connect"]);
const ID = /^[0-9a-f]{64}$/;

function failure(code: string, type: ErrorConstructor = Error) {
  const error = Object.assign(new type(code), { stableCode: code });
  error.stack = `${error.name}: ${code}`;
  return error;
}

function record(value: unknown): value is Record<string, unknown> {
  return value !== null && typeof value === "object" && !Array.isArray(value);
}

function noteFailure(state: StubState, error: unknown): Error {
  const safe = sanitizeDoError(error);
  if (safe.durableObjectReset && !state.failed) {
    state.failed = safe;
    for (const reject of state.pending) reject(safe);
    state.pending.clear();
  }
  return state.failed ?? safe;
}

function assertActive(state: StubState): void {
  if (state.failed) throw state.failed;
}

function observe<T>(state: StubState, value: PromiseLike<T>): Promise<T> {
  return new Promise((resolve, reject) => {
    state.pending.add(reject);
    Promise.resolve(value).then(
      (result) => {
        state.pending.delete(reject);
        if (state.failed) reject(state.failed);
        else resolve(result);
      },
      (error: unknown) => {
        state.pending.delete(reject);
        reject(noteFailure(state, error));
      },
    );
    if (state.failed) {
      state.pending.delete(reject);
      reject(state.failed);
    }
  });
}

function object(value: unknown): value is object {
  return (
    value !== null && (typeof value === "object" || typeof value === "function")
  );
}

function callable(value: unknown): value is (...args: unknown[]) => unknown {
  return typeof value === "function";
}

function beginOperation(order: StubOrder) {
  // A quiescent host may hibernate and forget its in-memory sequence cursor.
  // Completed dispatches need no ordering relationship with the next burst.
  if (order.inFlight === 0 && order.starting === 0) {
    order.channelId = crypto.randomUUID().replaceAll("-", "");
    order.next = 0;
    order.startTail = Promise.resolve();
  }
  if (!Number.isSafeInteger(order.next)) throw failure("DO_STORAGE_LIMIT");
  const call = { channelId: order.channelId, sequence: order.next };
  const immediate = order.inFlight === 0 && order.starting === 0;
  const predecessor = order.startTail;
  let releaseStart: () => void = () => {};
  const startGate = new Promise<void>((resolve) => {
    releaseStart = resolve;
  });
  order.startTail = predecessor.then(() => startGate);
  order.next += 1;
  order.inFlight += 1;
  order.starting += 1;
  let finished = false;
  let startReleased = false;
  const started = (value?: PromiseLike<unknown>) => {
    if (startReleased) return;
    startReleased = true;
    const release = () => {
      order.starting -= 1;
      releaseStart();
    };
    if (value === undefined) {
      release();
      return;
    }
    Promise.resolve(value).then(release, release);
  };
  return {
    ...call,
    immediate,
    predecessor,
    started,
    rollback() {
      if (
        finished ||
        order.channelId !== call.channelId ||
        order.next !== call.sequence + 1
      ) {
        return false;
      }
      finished = true;
      started();
      order.next -= 1;
      order.inFlight -= 1;
      return true;
    },
    done() {
      if (finished) return;
      finished = true;
      started();
      order.inFlight -= 1;
    },
  };
}

interface DeferredRpcValue {
  value: unknown;
}

function deferredRpcProvider(
  state: StubState,
  launch: () => Promise<DeferredRpcValue>,
): unknown {
  const target = () => undefined;
  return new Proxy(target, {
    get(_owner, property) {
      if (property === "then") {
        return (fulfilled?: unknown, rejected?: unknown) =>
          observe(
            state,
            launch().then((holder) => holder.value),
          ).then(
            callable(fulfilled)
              ? (resolved: unknown) =>
                  fulfilled(sanitizeResolved(state, resolved))
              : undefined,
            (error: unknown) => {
              const safe = noteFailure(state, error);
              if (callable(rejected)) return rejected(safe);
              throw safe;
            },
          );
      }
      if (property === Symbol.dispose) {
        return () =>
          waitUntil(
            launch()
              .then((holder) => {
                if (!object(holder.value)) return;
                const dispose: unknown = Reflect.get(
                  holder.value,
                  Symbol.dispose,
                  holder.value,
                );
                if (callable(dispose)) Reflect.apply(dispose, holder.value, []);
              })
              .catch(() => undefined),
          );
      }
      if (typeof property !== "string" || Reflect.has(target, property))
        return Reflect.get(target, property);
      const child = () =>
        launch().then((holder) => {
          if (!object(holder.value)) return { value: undefined };
          return { value: Reflect.get(holder.value, property, holder.value) };
        });
      return deferredRpcProvider(state, child);
    },
    apply(_owner, _receiver, args) {
      const child = launch().then((holder) => {
        if (!callable(holder.value)) throw failure("DO_RUNTIME_EXCEPTION");
        return { value: Reflect.apply(holder.value, holder.value, args) };
      });
      return deferredRpcProvider(state, () => child);
    },
  });
}

function sanitizeResolved(
  state: StubState,
  value: unknown,
  seen = new WeakMap<object, object>(),
): unknown {
  if (!object(value)) return value;
  if (state.isRpcStub(value)) return protectProvider(state, value);
  if (
    value instanceof Date ||
    value instanceof Error ||
    value instanceof RegExp ||
    value instanceof ArrayBuffer ||
    ArrayBuffer.isView(value) ||
    value instanceof Headers ||
    value instanceof Request ||
    value instanceof Response ||
    value instanceof ReadableStream ||
    value instanceof WritableStream
  )
    return value;
  const prior = seen.get(value);
  if (prior) return prior;
  if (Array.isArray(value)) {
    const output: unknown[] = [];
    seen.set(value, output);
    for (const item of value) output.push(sanitizeResolved(state, item, seen));
    return preserveRpcDisposal(value, output);
  }
  if (value instanceof Map) {
    const output = new Map<unknown, unknown>();
    seen.set(value, output);
    for (const [key, item] of value) {
      output.set(
        sanitizeResolved(state, key, seen),
        sanitizeResolved(state, item, seen),
      );
    }
    return preserveRpcDisposal(value, output);
  }
  if (value instanceof Set) {
    const output = new Set<unknown>();
    seen.set(value, output);
    for (const item of value) output.add(sanitizeResolved(state, item, seen));
    return preserveRpcDisposal(value, output);
  }
  const prototype = Object.getPrototypeOf(value);
  if (prototype !== Object.prototype && prototype !== null) return value;
  const output = Object.create(prototype) as Record<string, unknown>;
  seen.set(value, output);
  for (const [key, item] of Object.entries(value))
    Object.defineProperty(output, key, {
      value: sanitizeResolved(state, item, seen),
      enumerable: true,
      writable: true,
      configurable: true,
    });
  return preserveRpcDisposal(value, output);
}

function protectProvider(state: StubState, value: unknown): unknown {
  if (!object(value)) return value;
  const prior = state.rpcWrappers.get(value);
  if (prior) return prior;
  const target = state.isRpcStub(value)
    ? state.createUserRpcTransport(value)
    : value;
  const wrapper = new Proxy(target, {
    get(target, property) {
      try {
        const member: unknown = Reflect.get(target, property, target);
        if (property === "then") {
          if (!callable(member)) return undefined;
          return (fulfilled?: unknown, rejected?: unknown) =>
            observe(
              state,
              new Promise<unknown>((resolve, reject) => {
                Reflect.apply(member, target, [resolve, reject]);
              }),
            ).then(
              callable(fulfilled)
                ? (resolved: unknown) =>
                    fulfilled(sanitizeResolved(state, resolved))
                : undefined,
              (error: unknown) => {
                const safe = noteFailure(state, error);
                if (callable(rejected)) return rejected(safe);
                throw safe;
              },
            );
        }
        if (property === "dup" && callable(member)) {
          return () => {
            assertActive(state);
            return protectProvider(state, Reflect.apply(member, target, []));
          };
        }
        if (property === Symbol.dispose && callable(member)) {
          return () => Reflect.apply(member, target, []);
        }
        assertActive(state);
        return protectProvider(state, member);
      } catch (error) {
        throw noteFailure(state, error);
      }
    },
    apply(target, _receiver, args) {
      try {
        assertActive(state);
        return protectProvider(
          state,
          Reflect.apply(
            target as (...args: unknown[]) => unknown,
            target,
            args,
          ),
        );
      } catch (error) {
        throw noteFailure(state, error);
      }
    },
  });
  const result = state.isRpcStub(value)
    ? state.createRpcStub(wrapper)
    : wrapper;
  state.rpcWrappers.set(value, result);
  return result;
}

function rpcOperation(
  state: StubState,
  kind: "call" | "get",
  member: string,
  args: unknown[],
): unknown {
  assertActive(state);
  const operation = beginOperation(state.order);
  const launch = (): DeferredRpcValue => {
    let started: DoRpcResultProvider;
    try {
      assertActive(state);
      started = state.rpcTransport.startRpc(
        state.identity.value,
        operation.channelId,
        operation.sequence,
        kind,
        member,
        args,
        state.identity,
      );
    } catch (error) {
      if (!operation.rollback()) operation.done();
      throw noteFailure(state, error);
    }
    operation.started(started);
    let result: unknown;
    try {
      result = started.take();
    } catch (error) {
      operation.done();
      throw noteFailure(state, error);
    }
    if (object(result)) {
      waitUntil(Promise.resolve(result).then(operation.done, operation.done));
    } else {
      operation.done();
    }
    waitUntil(
      started.then(
        (holder) => state.disposeRpcResult(holder),
        (error: unknown) => {
          noteFailure(state, error);
          return state.control
            .cancelOrder(
              state.identity.value,
              operation.channelId,
              operation.sequence,
            )
            .then(
              () => undefined,
              () => undefined,
            );
        },
      ),
    );
    return { value: result };
  };
  if (!operation.immediate) {
    const launched = operation.predecessor.then(launch);
    return deferredRpcProvider(state, () => launched);
  }
  return protectProvider(state, launch().value);
}

function rpcMember(
  state: StubState,
  property: string,
): (...args: unknown[]) => unknown {
  const getProperty = () => rpcOperation(state, "get", property, []);
  const method = (...args: unknown[]) =>
    rpcOperation(state, "call", property, args);
  return new Proxy(method, {
    get(target, nested, receiver) {
      if (typeof nested !== "string" || Reflect.has(target, nested))
        return Reflect.get(target, nested, receiver);
      if (nested === "then")
        return (
          fulfilled?: (value: unknown) => unknown,
          rejected?: (error: unknown) => unknown,
        ) => Promise.resolve(getProperty()).then(fulfilled, rejected);
      return deferredRpcProvider(state, () => {
        const result = getProperty();
        return Promise.resolve({
          value: object(result) ? Reflect.get(result, nested) : undefined,
        });
      });
    },
  });
}

export function createStubPolicy(
  identity: DoObjectIdentity,
  raw: object,
  factories: Pick<
    DoNativeFactories,
    | "createRpcStub"
    | "isRpcStub"
    | "createPrivateTransport"
    | "createPublicTransport"
    | "createRpcTransport"
    | "createUserRpcTransport"
    | "disposeRpcResult"
  >,
): object {
  const control = factories.createPrivateTransport(raw);
  const rpcTransport = factories.createRpcTransport(raw);
  if (!rawTransport(rpcTransport) || !rawTransport(control))
    throw failure("DO_INTERNAL_PROTOCOL_ERROR");
  const target = new StubPolicy();
  const state: StubState = {
    identity,
    createRpcStub: factories.createRpcStub,
    isRpcStub: factories.isRpcStub,
    httpTransport: factories.createPublicTransport(raw),
    rpcTransport,
    disposeRpcResult: factories.disposeRpcResult,
    createUserRpcTransport: factories.createUserRpcTransport,
    control,
    order: {
      channelId: crypto.randomUUID().replaceAll("-", ""),
      next: 0,
      inFlight: 0,
      starting: 0,
      startTail: Promise.resolve(),
    },
    rpcWrappers: new WeakMap<object, object>(),
    pending: new Set(),
  };
  stubState.set(target, state);
  const proxy = new Proxy(target, {
    get(owner, property, receiver) {
      if (property === "then") return undefined;
      if (typeof property !== "string")
        return Reflect.get(owner, property, receiver);
      if (LOCAL_STUB_MEMBERS.has(property)) {
        const method = Reflect.get(owner, property, owner);
        return method.bind(owner);
      }
      if (FORBIDDEN_RPC.has(property) || property.startsWith("__openCompute")) {
        throw failure("DO_RPC_UNSUPPORTED", TypeError);
      }
      return rpcMember(state, property);
    },
  });
  stubState.set(proxy, state);
  const callbacks = {
    fetch: (request: Request) =>
      nativeChannelApply(nativeStubFetch, target, [request]),
    connect: (address: string) =>
      nativeChannelApply(nativeStubConnect, target, [address]),
    hold: (handles: readonly string[]) => control.holdWebSocketHandoff(handles),
  };
  channelCallbacks.set(target, callbacks);
  channelCallbacks.set(proxy, callbacks);
  return proxy;
}

class StubPolicy {
  connect(address: SocketAddress | string, options?: SocketOptions): Socket {
    const state = stubState.get(this)!;
    assertActive(state);
    const ordered = beginOperation(state.order);
    const operationId = crypto.randomUUID().replaceAll("-", "");
    const prepared = ordered.predecessor.then(() => {
      assertActive(state);
      return state.control.prepareConnect(
        state.identity.value,
        ordered.channelId,
        ordered.sequence,
        operationId,
        socketAuthorityWire(address),
        state.identity,
      );
    });
    waitUntil(
      prepared.then(
        () => undefined,
        (error: unknown) => {
          noteFailure(state, error);
        },
      ),
    );
    const cancel = async () => {
      await prepared.catch(() => undefined);
      try {
        await state.control.cancelConnect(operationId);
      } finally {
        await state.control.cancelOrder(
          state.identity.value,
          ordered.channelId,
          ordered.sequence,
        );
      }
    };
    let socket: Socket;
    try {
      socket = state.httpTransport.connect(
        `${operationId}.do-transport.invalid:1`,
        options,
      );
    } catch (error) {
      noteFailure(state, error);
      waitUntil(cancel().then(ordered.done, ordered.done));
      throw error;
    }
    waitUntil(
      socket.opened.then(
        () => undefined,
        (error: unknown) => {
          noteFailure(state, error);
          return cancel().then(ordered.done, ordered.done);
        },
      ),
    );
    ordered.started(socket.opened);
    waitUntil(
      socket.closed.then(ordered.done, (error: unknown) => {
        noteFailure(state, error);
        ordered.done();
      }),
    );
    return socket;
  }

  async fetch(input: RequestInfo | URL, init?: RequestInit) {
    const state = stubState.get(this)!;
    assertActive(state);
    let request;
    try {
      request =
        input instanceof Request && init === undefined
          ? input
          : new Request(input, init);
    } catch {
      throw failure("DO_RPC_UNSUPPORTED", TypeError);
    }
    try {
      const headers = new Headers(request.headers);
      headers.set(OBJECT_IDENTITY_HEADER, encodeObjectIdentity(state.identity));
      headers.set("x-open-compute-do-method", request.method);
      headers.set("x-open-compute-do-url", request.url);
      const transport: RequestInit = {
        method: request.method,
        headers,
        body: request.body,
        redirect: "manual",
      };
      if (request.method === "GET" || request.method === "HEAD")
        delete transport.body;
      const operation = beginOperation(state.order);
      const outbound = new Request(
        `https://do-transport.invalid/${state.identity.value}/${operation.channelId}/${operation.sequence}`,
        transport,
      );
      let pending: Promise<Response>;
      try {
        await operation.predecessor;
        assertActive(state);
        pending = state.httpTransport.fetch(outbound);
        // The host enforces sequence order. A response can depend on a later call.
        operation.started();
      } catch (error) {
        operation.rollback();
        throw error;
      }
      try {
        return await observe(state, pending);
      } catch (error) {
        await state.control
          .cancelOrder(
            state.identity.value,
            operation.channelId,
            operation.sequence,
          )
          .catch(() => undefined);
        throw error;
      } finally {
        operation.done();
      }
    } catch (error) {
      throw noteFailure(state, error);
    }
  }
}

const nativeStubFetch = StubPolicy.prototype.fetch;
const nativeStubConnect = StubPolicy.prototype.connect;

/** Return ordered HTTP and TCP dispatch only for a policy issued by this DO authority. */
export function durableObjectChannelCallbacks(policy: object) {
  return channelCallbacks.get(policy) ?? null;
}

/** Rebuild only dispatch policy after a native stub transfers as a Fetcher. */
export function createDurableObjectStubPolicy(
  env: { fetcher: unknown; id: unknown },
  factories: Pick<
    DoNativeFactories,
    | "createRpcStub"
    | "isRpcStub"
    | "createPrivateTransport"
    | "createPublicTransport"
    | "createRpcTransport"
    | "createUserRpcTransport"
    | "disposeRpcResult"
  >,
): object {
  if (
    typeof env.id !== "string" ||
    !ID.test(env.id) ||
    !record(env.fetcher) ||
    !rawTransport(factories.createRpcTransport(env.fetcher))
  )
    throw failure("DO_ID_INVALID", TypeError);
  return createStubPolicy(
    { value: env.id, name: undefined, jurisdiction: undefined },
    env.fetcher,
    factories,
  );
}

export function rawTransport(value: unknown): value is DoRawTransport {
  if (!object(value)) return false;
  return ["startRpc", "cancelOrder", "prepareConnect", "cancelConnect"].every(
    (method) => typeof nativeChannelGet(value, method) === "function",
  );
}
