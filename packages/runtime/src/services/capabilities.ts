import { env as currentEnv, RpcTarget, waitUntil } from "cloudflare:workers";
import { preserveRpcDisposal } from "../bindings/rpc-disposal.js";
import { createRpcTransport } from "../bindings/rpc-transport.js";
import { privateWeakMap } from "../private-weak-map.js";
import {
  activateServiceCapabilities,
  serviceCapability as capabilityEnvelope,
} from "./capability-transfer.js";
import { serviceDeadline, serviceDeadlineAt } from "./deadline.js";
import { serviceRpcMember } from "./rpc-member.js";
import {
  childServiceFrame,
  currentServiceFrame,
  withServiceScope,
  type ServiceFrame,
} from "./scope.js";

interface NativeCapabilityHandle extends Disposable {
  call(
    frame: ServiceFrame,
    operation: "call" | "get" | "apply",
    method: string,
    args: unknown[],
  ): unknown;
  dup(): NativeCapabilityHandle;
  releaseCapability(): unknown;
}

interface CapabilityAdmission {
  readonly handle: string;
  readonly frame: string;
  readonly deadlineMs: unknown;
}

/** Private authority and local native stub factory for Service capability calls. */
export interface CapabilityController {
  readonly createStub?: (policy: object) => object;
  beginCapability(retention: string, frame: ServiceFrame): unknown;
  releaseRetention(retention: string): unknown;
  completeOperation(handle: string): unknown;
  retainCapability(
    handle: string,
    owner: "caller" | "target",
    deadlineAt: number,
  ): unknown;
  dup?(): CapabilityController;
  [Symbol.dispose]?(): void;
}

interface RetentionController extends Disposable {
  begin(frame: ServiceFrame): unknown;
  complete(handle: string): unknown;
  release(): unknown;
  dup(): RetentionController;
}

const NativeProxy = Proxy;
const nativeGet = Reflect.get;
const nativeApply = Reflect.apply;
const nativeBind = Function.prototype.bind;
const nativeFreeze = Object.freeze;
const nativeEntries = Object.entries;
const nativePrototype = Object.getPrototypeOf;
const nativeCreate = Object.create;
const nativeSetPrototypeOf = Object.setPrototypeOf;
const nativeDefine = Object.defineProperty;
const objectPrototype = Object.prototype;
const RESERVED = new Set([
  "constructor",
  "__proto__",
  "then",
  "dup",
  "__openComputeServiceRpc",
  "__openComputeServiceFetch",
]);

function object(value: unknown): value is object {
  return (
    value !== null && (typeof value === "object" || typeof value === "function")
  );
}

function callable(value: unknown): value is (...args: unknown[]) => unknown {
  return typeof value === "function";
}

function failure(code: string): Error {
  const error = Object.assign(new Error(code), { stableCode: code });
  error.stack = `Error: ${code}`;
  return error;
}

function admission(value: unknown): value is CapabilityAdmission {
  return (
    object(value) &&
    typeof nativeGet(value, "handle") === "string" &&
    typeof nativeGet(value, "frame") === "string"
  );
}

class SourceCapability extends RpcTarget {
  readonly #target: object;
  readonly #controller: CapabilityController;
  readonly #controllerOwned: boolean;
  readonly #environment: Record<string, unknown>;
  #retention: RetentionController | undefined;
  #retentionOwned = false;
  #disposed = false;

  constructor(
    target: object,
    controller: CapabilityController,
    environment: Record<string, unknown>,
  ) {
    super();
    this.#target = target;
    const duplicate = nativeGet(controller, "dup");
    this.#controllerOwned = callable(duplicate);
    this.#controller = this.#controllerOwned
      ? nativeApply(
          duplicate as (...args: unknown[]) => CapabilityController,
          controller,
          [],
        )
      : controller;
    this.#environment = environment;
  }

  activate(retention: unknown): void {
    if (!object(retention)) throw failure("SERVICE_BINDING_DENIED");
    retention = createRpcTransport(retention, "service-retention");
    if (
      !object(retention) ||
      this.#disposed ||
      !callable(nativeGet(retention, "begin")) ||
      !callable(nativeGet(retention, "complete")) ||
      !callable(nativeGet(retention, "release")) ||
      this.#retention !== undefined
    ) {
      throw failure("SERVICE_BINDING_DENIED");
    }
    const duplicate = nativeGet(retention, "dup");
    this.#retentionOwned = callable(duplicate);
    this.#retention = this.#retentionOwned
      ? nativeApply(
          duplicate as (...args: unknown[]) => RetentionController,
          retention,
          [],
        )
      : (retention as RetentionController);
  }

  async call(
    frame: ServiceFrame,
    operation: "call" | "get" | "apply",
    method: string,
    rawArgs: unknown[],
  ): Promise<unknown> {
    const active = this.#retention;
    if (!active) throw failure("SERVICE_BINDING_DENIED");
    // A caller may dispose the public target while this admitted call is still draining.
    const owned = this.#retentionOwned;
    const retention = owned ? active.dup() : active;
    let operationHandle: string | undefined;
    let timedOut = false;
    let execution: Promise<unknown> | undefined;
    try {
      const admitted: unknown = await retention.begin(frame);
      if (!admission(admitted)) throw failure("SERVICE_UNAVAILABLE");
      operationHandle = admitted.handle;
      const deadlineAt = serviceDeadlineAt(admitted.deadlineMs);
      const running = (async () => {
        const encoded = await withServiceScope(
          this.#environment,
          childServiceFrame(frame.scopeId, admitted.frame),
          async () => {
            let value: unknown;
            if (operation === "get") {
              value = serviceRpcMember(this.#target, method, "get");
            } else {
              const fn = serviceRpcMember(this.#target, method, operation);
              await activateNestedCapabilities(
                rawArgs,
                admitted.handle,
                this.#controller,
                "caller",
                deadlineAt,
              );
              const args = decodeServiceValue(
                rawArgs,
                privateWeakMap<object, unknown>(),
                this.#controller,
              );
              if (!Array.isArray(args)) throw failure("SERVICE_BINDING_DENIED");
              value = nativeApply(
                fn,
                operation === "apply" ? undefined : this.#target,
                args,
              );
            }
            const settled = await value;
            if (Date.now() >= deadlineAt) throw failure("SERVICE_TIMEOUT");
            return encodeServiceValue(settled, this.#controller);
          },
        );
        await activateNestedCapabilities(
          encoded,
          admitted.handle,
          this.#controller,
          "target",
          deadlineAt,
        );
        return encoded;
      })();
      execution = running;
      return await serviceDeadline(
        () => running,
        deadlineAt,
        () => {
          timedOut = true;
        },
      );
    } finally {
      const complete = async () => {
        try {
          if (operationHandle !== undefined)
            await retention.complete(operationHandle);
        } finally {
          if (owned) retention[Symbol.dispose]();
        }
      };
      if (timedOut && execution) {
        waitUntil(execution.then(complete, complete));
      } else {
        await complete();
      }
    }
  }

  async releaseCapability(): Promise<void> {
    await this.#release();
  }

  async #release(): Promise<void> {
    if (this.#disposed) return;
    this.#disposed = true;
    const retention = this.#retention;
    this.#retention = undefined;
    const retentionOwned = this.#retentionOwned;
    this.#retentionOwned = false;
    try {
      if (retention) {
        try {
          await retention.release();
        } finally {
          if (retentionOwned) retention[Symbol.dispose]();
        }
      }
    } finally {
      try {
        if (this.#controllerOwned) this.#controller[Symbol.dispose]?.();
      } finally {
        const disposeTarget = nativeGet(this.#target, Symbol.dispose);
        if (callable(disposeTarget))
          nativeApply(disposeTarget, this.#target, []);
      }
    }
  }

  [Symbol.dispose](): void {
    waitUntil(this.#release());
  }
}

async function activateNestedCapabilities(
  value: unknown,
  operationHandle: string,
  controller: CapabilityController,
  owner: "caller" | "target",
  deadlineAt: number,
): Promise<void> {
  await activateServiceCapabilities(
    value,
    async () => {
      // Keep the private hop alive through a late receipt after the public target is disposed.
      const duplicate = nativeGet(controller, "dup");
      const owned = callable(duplicate);
      const transfer = owned
        ? (nativeApply(duplicate, controller, []) as CapabilityController)
        : controller;
      try {
        const retained: unknown = await transfer.retainCapability(
          operationHandle,
          owner,
          deadlineAt,
        );
        if (!object(retained)) throw failure("SERVICE_BINDING_DENIED");
        return createRpcTransport(retained, "service-retention");
      } finally {
        if (owned) transfer[Symbol.dispose]?.();
      }
    },
    deadlineAt,
    (source) =>
      owner === "caller" ? createRpcTransport(source, "capability") : source,
  );
}

function clonableObject(value: object): boolean {
  return (
    Array.isArray(value) ||
    nativePrototype(value) === objectPrototype ||
    nativePrototype(value) === null
  );
}

/** Replace local RpcTarget values with generic native capabilities before an RPC hop. */
export function encodeServiceValue(
  value: unknown,
  controller: CapabilityController,
  seen = privateWeakMap<object, unknown>(),
): unknown {
  if (value instanceof RpcTarget || typeof value === "function") {
    if (!object(currentEnv)) throw failure("SERVICE_BINDING_DENIED");
    return nativeFreeze({
      __openComputeServiceCapability: 1,
      kind: typeof value === "function" ? "function" : "target",
      handle: new SourceCapability(
        value,
        controller,
        currentEnv as Record<string, unknown>,
      ),
    });
  }
  if (!object(value) || !clonableObject(value)) return value;
  const prior = seen.get(value);
  if (prior !== undefined) return prior;
  if (Array.isArray(value)) {
    const output: unknown[] = [];
    seen.set(value, output);
    for (const item of value)
      output.push(encodeServiceValue(item, controller, seen));
    return output;
  }
  const output: Record<string, unknown> = nativeCreate(nativePrototype(value));
  seen.set(value, output);
  for (const [key, item] of nativeEntries(value)) {
    nativeDefine(output, key, {
      value: encodeServiceValue(item, controller, seen),
      enumerable: true,
      writable: true,
      configurable: true,
    });
  }
  return output;
}

/** Wrap a native RPC method or getter while preserving its pipelined result. */
export function serviceMember(
  call: (operation: "call" | "get", args: unknown[]) => unknown,
  callbackController?: CapabilityController,
): (...args: unknown[]) => unknown {
  const member = (...args: unknown[]) =>
    serviceResult(call("call", args), callbackController);
  return new NativeProxy(member, {
    get(target, property, receiver) {
      if (property === "then") {
        return (
          resolved: (value: unknown) => unknown,
          rejected?: (reason: unknown) => unknown,
        ) =>
          Promise.resolve(
            serviceResult(call("get", []), callbackController),
          ).then(resolved, rejected);
      }
      if (typeof property === "string" && RESERVED.has(property)) {
        throw failure("SERVICE_BINDING_DENIED");
      }
      const value: unknown = nativeGet(target, property, receiver);
      return callable(value) ? nativeApply(nativeBind, value, [target]) : value;
    },
  });
}

/** Decode a Service RPC result while preserving native promise disposal. */
export function serviceResult(
  raw: unknown,
  callbackController?: CapabilityController,
): unknown {
  if (!object(raw)) return raw;
  const then = nativeGet(raw, "then");
  if (!callable(then))
    return decodeServiceValue(
      raw,
      privateWeakMap<object, unknown>(),
      callbackController,
    );
  return new NativeProxy(raw, {
    get(target, property, receiver) {
      if (property === Symbol.dispose) {
        const dispose = nativeGet(target, property, target);
        return callable(dispose)
          ? () => nativeApply(dispose, target, [])
          : undefined;
      }
      if (property === "then") {
        return (
          resolved: (value: unknown) => unknown,
          rejected?: (reason: unknown) => unknown,
        ) =>
          nativeApply(then, target, [
            (value: unknown) =>
              resolved(
                decodeServiceValue(
                  value,
                  privateWeakMap<object, unknown>(),
                  callbackController,
                ),
              ),
            rejected,
          ]);
      }
      if (typeof property !== "string" || RESERVED.has(property)) {
        return nativeGet(target, property, receiver);
      }
      return serviceMember((operation, args) => {
        const envelope = nativeGet(target, "handle");
        if (!object(envelope)) throw failure("SERVICE_UNAVAILABLE");
        const call = nativeGet(envelope, "call");
        if (!callable(call)) throw failure("SERVICE_UNAVAILABLE");
        return nativeApply(call, envelope, [
          currentServiceFrame(),
          operation,
          property,
          callbackController
            ? (encodeServiceValue(args, callbackController) as unknown[])
            : args,
        ]);
      }, callbackController);
    },
  });
}

interface CapabilityGroup {
  remaining: number;
  released: boolean;
}

function duplicateCapability(
  handle: NativeCapabilityHandle,
  kind: "function" | "target",
  group: CapabilityGroup,
  callbackController?: CapabilityController,
): object {
  const duplicate = capabilityPolicy(
    handle.dup(),
    kind,
    group,
    callbackController,
  );
  group.remaining += 1;
  return duplicate;
}

function disposeCapability(
  handle: NativeCapabilityHandle,
  group: CapabilityGroup,
): void {
  if (group.released || group.remaining < 1) return;
  group.remaining -= 1;
  if (group.remaining > 0) {
    handle[Symbol.dispose]();
    return;
  }
  group.released = true;
  const release = nativeGet(handle, "releaseCapability");
  if (!callable(release)) {
    handle[Symbol.dispose]();
    return;
  }
  waitUntil(
    Promise.resolve(nativeApply(release, handle, [])).then(
      () => handle[Symbol.dispose](),
      () => handle[Symbol.dispose](),
    ),
  );
}

function capabilityPolicy(
  handle: NativeCapabilityHandle,
  kind: "function" | "target",
  group: CapabilityGroup = { remaining: 1, released: false },
  callbackController?: CapabilityController,
): object {
  let disposed = false;
  const invoke = (
    operation: "call" | "get" | "apply",
    method: string,
    args: unknown[],
  ) => {
    if (disposed) throw failure("SERVICE_BINDING_DENIED");
    return serviceResult(
      handle.call(
        currentServiceFrame(),
        operation,
        method,
        callbackController
          ? (encodeServiceValue(args, callbackController) as unknown[])
          : args,
      ),
      callbackController,
    );
  };
  const receiver =
    kind === "function"
      ? (...args: unknown[]) => invoke("apply", "", args)
      : new (class extends RpcTarget {})();
  return new NativeProxy(receiver, {
    get(_owner, property) {
      if (property === "then") return undefined;
      if (property === Symbol.dispose) {
        return () => {
          if (disposed) return;
          disposed = true;
          disposeCapability(handle, group);
        };
      }
      if (disposed) throw failure("SERVICE_BINDING_DENIED");
      if (property === "dup")
        return () => {
          if (disposed) throw failure("SERVICE_BINDING_DENIED");
          return duplicateCapability(handle, kind, group, callbackController);
        };
      if (typeof property !== "string" || RESERVED.has(property)) {
        throw failure("SERVICE_BINDING_DENIED");
      }
      return serviceMember(
        (operation, args) => invoke(operation, property, args),
        callbackController,
      );
    },
  });
}

function capability(
  handle: NativeCapabilityHandle,
  kind: "function" | "target",
  group: CapabilityGroup = { remaining: 1, released: false },
  callbackController?: CapabilityController,
): object {
  const createStub = callbackController?.createStub;
  if (!createStub) throw failure("SERVICE_BINDING_DENIED");
  return createStub(capabilityPolicy(handle, kind, group, callbackController));
}

/** Recursively restore trusted generic capability envelopes as native-RPC-backed facades. */
export function decodeServiceValue(
  value: unknown,
  seen = privateWeakMap<object, unknown>(),
  callbackController?: CapabilityController,
): unknown {
  if (capabilityEnvelope(value))
    return capability(
      createRpcTransport(value.handle, "capability") as NativeCapabilityHandle,
      value.kind,
      undefined,
      callbackController,
    );
  if (
    !object(value) ||
    value instanceof Date ||
    value instanceof Request ||
    value instanceof Response ||
    value instanceof ReadableStream ||
    value instanceof WritableStream ||
    value instanceof ArrayBuffer ||
    ArrayBuffer.isView(value) ||
    value instanceof Map ||
    value instanceof Set ||
    value instanceof Error ||
    value instanceof RegExp
  ) {
    return value;
  }
  const prior = seen.get(value);
  if (prior !== undefined) return prior;
  if (Array.isArray(value)) {
    const output: unknown[] = [];
    seen.set(value, output);
    for (const item of value)
      output.push(decodeServiceValue(item, seen, callbackController));
    return preserveRpcDisposal(value, output);
  }
  if (
    nativePrototype(value) !== objectPrototype &&
    nativePrototype(value) !== null
  ) {
    return value;
  }
  const output: Record<string, unknown> = nativeCreate(nativePrototype(value));
  seen.set(value, output);
  for (const [key, item] of nativeEntries(value)) {
    nativeDefine(output, key, {
      value: decodeServiceValue(item, seen, callbackController),
      enumerable: true,
      writable: true,
      configurable: true,
    });
  }
  return preserveRpcDisposal(value, output);
}

/** Keep native stub construction local while forwarding private capability authority. */
export function serviceCapabilityController(
  transport: CapabilityController,
  createStub: (policy: object) => object,
): CapabilityController {
  const duplicate = nativeGet(transport, "dup");
  const dispose = nativeGet(transport, Symbol.dispose);
  return nativeSetPrototypeOf(
    {
      createStub,
      beginCapability: (retention, frame) =>
        transport.beginCapability(retention, frame),
      releaseRetention: (retention) => transport.releaseRetention(retention),
      completeOperation: (handle) => transport.completeOperation(handle),
      retainCapability: (handle, owner, deadlineAt) =>
        transport.retainCapability(handle, owner, deadlineAt),
      ...(callable(duplicate)
        ? {
            dup: () =>
              serviceCapabilityController(
                nativeApply(duplicate, transport, []) as CapabilityController,
                createStub,
              ),
          }
        : {}),
      ...(callable(dispose)
        ? { [Symbol.dispose]: () => nativeApply(dispose, transport, []) }
        : {}),
    } satisfies CapabilityController,
    null,
  );
}
