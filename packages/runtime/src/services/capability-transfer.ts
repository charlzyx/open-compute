import { waitUntil } from "cloudflare:workers";
import { bindingError } from "../loader/shared.js";
import { serviceDeadline } from "./deadline.js";

/** Private native-RPC envelope shared by Service dispatch and capability calls. */
export interface ServiceCapabilityEnvelope {
  readonly __openComputeServiceCapability: 1;
  readonly kind: "function" | "target";
  readonly handle: object;
}

interface Retention {
  release(): unknown;
}

const nativeGet = Reflect.get;
const nativeApply = Reflect.apply;
const nativePrototype = Object.getPrototypeOf;
const nativeValues = Object.values;
const nativeArray = Array.isArray;
const nativePush = Array.prototype.push;
const nativeMap = Array.prototype.map;
const nativeAllSettled = Promise.allSettled.bind(Promise);
const NativeWeakSet = WeakSet;
const objectPrototype = Object.prototype;

function object(value: unknown): value is object {
  return (
    value !== null && (typeof value === "object" || typeof value === "function")
  );
}

/** Recognize the private envelope before traversing ordinary structured values. */
export function serviceCapability(
  value: unknown,
): value is ServiceCapabilityEnvelope {
  if (
    !object(value) ||
    nativeGet(value, "__openComputeServiceCapability") !== 1
  )
    return false;
  const kind = nativeGet(value, "kind");
  return (
    (kind === "function" || kind === "target") &&
    object(nativeGet(value, "handle"))
  );
}

/** Transfer one batch; failed and late receipts revoke ownership instead of activating it. */
export async function activateServiceCapabilities(
  value: unknown,
  acquire: () => Promise<unknown>,
  deadlineAt: number,
  sourceTransport: (source: object) => object,
): Promise<void> {
  const seen = new NativeWeakSet<object>();
  const retentions: Retention[] = [];
  let failed = false;
  let timedOut = false;
  let pending: Promise<void> | undefined;
  async function visit(item: unknown): Promise<void> {
    if (failed || Date.now() >= deadlineAt)
      throw bindingError("SERVICE_TIMEOUT");
    if (!object(item) || seen.has(item)) return;
    seen.add(item);
    if (serviceCapability(item)) {
      const retained = await acquire();
      if (
        !object(retained) ||
        typeof nativeGet(retained, "release") !== "function"
      ) {
        throw bindingError("SERVICE_UNAVAILABLE");
      }
      const retention = retained as Retention;
      if (failed) {
        await retention.release();
        throw bindingError("SERVICE_TIMEOUT");
      }
      nativeApply(nativePush, retentions, [retention]);
      if (Date.now() >= deadlineAt) throw bindingError("SERVICE_TIMEOUT");
      const source = sourceTransport(item.handle);
      const activate = nativeGet(source, "activate");
      if (typeof activate !== "function")
        throw bindingError("SERVICE_BINDING_DENIED");
      await nativeApply(activate, source, [retained]);
      return;
    }
    if (nativeArray(item)) {
      for (let index = 0; index < item.length; index++)
        await visit(item[index]);
      return;
    }
    if (
      nativePrototype(item) !== objectPrototype &&
      nativePrototype(item) !== null
    )
      return;
    const children = nativeValues(item);
    for (let index = 0; index < children.length; index++)
      await visit(children[index]);
  }
  try {
    await serviceDeadline(
      () => {
        pending = visit(value);
        return pending;
      },
      deadlineAt,
      () => {
        timedOut = true;
      },
    );
  } catch (error) {
    failed = true;
    // A native RPC receipt can arrive after the caller observes timeout. Keep its release alive.
    if (timedOut && pending) {
      waitUntil(
        pending.catch((lateError) => {
          if (
            lateError instanceof Error &&
            nativeGet(lateError, "stableCode") === "SERVICE_TIMEOUT"
          )
            return;
          throw bindingError("SERVICE_UNAVAILABLE");
        }),
      );
    }
    const releaseTasks = nativeApply(nativeMap, retentions, [
      async (retention: Retention) => retention.release(),
    ]) as unknown[];
    const releases = await nativeAllSettled(releaseTasks);
    for (let index = 0; index < releases.length; index++) {
      if (releases[index]!.status === "rejected")
        throw bindingError("SERVICE_UNAVAILABLE");
    }
    throw error;
  }
}
