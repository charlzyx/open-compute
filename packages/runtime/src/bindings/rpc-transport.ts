import wrapped from "cloudflare-internal:wrapped-binding";
import { privateWeakMap } from "../private-weak-map.js";

export type RpcTransportDomain =
  | "transport"
  | "service-root"
  | "service-retention"
  | "do-holder"
  | "capability"
  | "facet"
  | "user";
type ResultKind = "value" | "stub" | "stream" | "data";
interface ResultContract {
  readonly kind: ResultKind;
  readonly domain: RpcTransportDomain;
}
const NativeProxy = Proxy;
const nativeSetPrototypeOf = Object.setPrototypeOf;
const nativeApply = Reflect.apply;
const nativeMap = Array.prototype.map;
const kind = wrapped.rpcTransportKind.bind(wrapped);
const member = wrapped.getRpcTransportMember.bind(wrapped);
const nativeThen = wrapped.thenRpcTransport.bind(wrapped);
const dispose = wrapped.disposeRpcTransport.bind(wrapped);
const duplicate = wrapped.duplicateRpcTransport.bind(wrapped);
const issued = privateWeakMap<
  object,
  { view: object; domain: RpcTransportDomain; result: ResultContract }
>();
const receivers = privateWeakMap<object, object>();
const DATA: ResultContract = { kind: "data", domain: "transport" };
const VALUE: ResultContract = { kind: "value", domain: "user" };

function object(value: unknown): value is object {
  return (
    value !== null && (typeof value === "object" || typeof value === "function")
  );
}
function contract(domain: RpcTransportDomain, name: string): ResultContract {
  if (domain === "user") return VALUE;
  if (
    domain === "facet" &&
    (name === "__openComputeFacetCall" ||
      name === "__openComputeFacetGet" ||
      name === "__openComputeFacetFetch")
  )
    return VALUE;
  if (domain === "transport") {
    if (name === "root") return { kind: "stub", domain: "service-root" };
    if (name === "startRpc") return { kind: "stub", domain: "do-holder" };
    if (name === "retainCapability")
      return { kind: "stub", domain: "service-retention" };
    if (name === "holdWebSocketHandoff")
      return { kind: "stream", domain: "transport" };
    if (name === "rpc" || name === "get") return VALUE;
  }
  if (
    (domain === "service-root" && (name === "rpc" || name === "get")) ||
    (domain === "do-holder" && name === "take") ||
    (domain === "capability" && name === "call")
  )
    return VALUE;
  return DATA;
}

/** Issue a private view with its existing protocol's result contract. */
export function createRpcTransport(
  receiver: object,
  domain: RpcTransportDomain,
): object {
  return viewProvider(receivers.get(receiver) ?? receiver, domain, DATA);
}
function viewProvider(
  receiver: object,
  domain: RpcTransportDomain,
  resultContract: ResultContract,
): object {
  const type = kind(receiver);
  if (type === "") throw new TypeError("SERVICE_BINDING_DENIED");
  const existing = issued.get(receiver);
  if (existing) {
    if (
      existing.domain !== domain ||
      existing.result.kind !== resultContract.kind ||
      existing.result.domain !== resultContract.domain
    )
      throw new TypeError("SERVICE_BINDING_DENIED");
    return existing.view;
  }
  const target = nativeSetPrototypeOf(() => undefined, null);
  const result = new NativeProxy(target, {
    get(_target, property) {
      if (property === "then") {
        if (type !== "promise" && type !== "property") return undefined;
        return (
          fulfilled?: (value: unknown) => unknown,
          rejected?: (error: unknown) => unknown,
        ) =>
          nativeThen(
            receiver,
            (value: unknown) => {
              const delivered =
                resultContract.kind === "stub"
                  ? privateStub(value, resultContract.domain)
                  : value;
              return fulfilled ? fulfilled(delivered) : delivered;
            },
            rejected,
          );
      }
      if (property === Symbol.dispose)
        return type === "stub" || type === "promise"
          ? () => dispose(receiver)
          : undefined;
      if (property === "dup" && (type === "stub" || domain !== "user")) {
        if (type !== "stub") return undefined;
        return () =>
          viewProvider(
            duplicate(receiver, domain === "user" ? "value" : "private"),
            domain,
            resultContract,
          );
      }
      if (typeof property !== "string") return undefined;
      const next = contract(domain, property);
      return view(member(receiver, property, next.kind), domain, next);
    },
    apply(_target, _this, args) {
      if (
        (type !== "promise" && type !== "property" && type !== "stub") ||
        typeof receiver !== "function"
      )
        throw new TypeError("SERVICE_BINDING_DENIED");
      // Only issued top-level arguments are restored; user objects are never traversed.
      const originalArgs = nativeApply(nativeMap, args, [
        (value: unknown) =>
          object(value) ? (receivers.get(value) ?? value) : value,
      ]);
      return view(
        nativeApply(receiver, undefined, originalArgs),
        resultContract.domain,
        resultContract,
      );
    },
  });
  issued.set(receiver, { view: result, domain, result: resultContract });
  receivers.set(result, receiver);
  return result;
}
function view(
  value: unknown,
  domain: RpcTransportDomain,
  result: ResultContract,
): unknown {
  return object(value) && kind(value) !== ""
    ? viewProvider(value, domain, result)
    : value;
}
function privateStub(value: unknown, domain: RpcTransportDomain): object {
  if (!object(value) || kind(value) !== "stub")
    throw new TypeError("SERVICE_BINDING_DENIED");
  return createRpcTransport(value, domain);
}

/** Release only the issued native result, without consulting a tenant Symbol getter. */
export function disposeRpcResult(value: object): void {
  dispose(receivers.get(value) ?? value);
}
