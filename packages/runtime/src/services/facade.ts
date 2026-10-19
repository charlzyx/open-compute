import type { PublicTransport } from "cloudflare-internal:wrapped-binding";
import { privateWeakMap } from "../private-weak-map.js";
import {
  encodeServiceValue,
  serviceCapabilityController,
  serviceMember,
} from "./capabilities.js";
import { currentServiceFrame, type ServiceFrame } from "./scope.js";
import {
  parseServiceWebSocketHandoffs,
  responseWithHeaders,
  SERVICE_WEBSOCKET_HANDOFF_HEADER,
} from "./websocket-handoff.js";

interface NativeServiceTransport {
  holdWebSocketHandoff(
    handles: readonly string[],
  ): Promise<ReadableStream<Uint8Array>>;
  root(scopeId: string): NativeServiceRoot;
  rpc(frame: ServiceFrame, method: string, args: unknown[]): unknown;
  get(frame: ServiceFrame, property: string): unknown;
  completeRoot(scopeId: string): unknown;
  beginCapability(retention: string, frame: ServiceFrame): unknown;
  releaseRetention(retention: string): unknown;
  completeOperation(handle: string): unknown;
  retainCapability(
    handle: string,
    owner: "caller" | "target",
    deadlineAt: number,
  ): unknown;
}

/** Private pipelined controller; no tenant-visible Cloudflare interface is redeclared. */
interface NativeServiceRoot extends Disposable {
  rpc(frame: ServiceFrame, method: string, args: unknown[]): unknown;
  get(frame: ServiceFrame, property: string): unknown;
  ready(): Promise<void>;
}

const NativeProxy = Proxy;
const nativeGet = Reflect.get;
const nativeApply = Reflect.apply;
const nativeBind = Function.prototype.bind;
const nativeFreeze = Object.freeze;
const nativeValues = Object.values;
const nativePush = Array.prototype.push;
const rootLeases = privateWeakMap<
  ServiceFrame,
  {
    transport: NativeServiceTransport;
    lease: NativeServiceRoot;
  }[]
>();

function rootController(
  transport: NativeServiceTransport,
  frame: ServiceFrame,
) {
  if (frame.parentFrame !== null) return undefined;
  let leases = rootLeases.get(frame);
  if (!leases) {
    leases = [];
    rootLeases.set(frame, leases);
  }
  for (let index = 0; index < leases.length; index++) {
    const existing = leases[index]!;
    if (existing.transport === transport) return existing.lease;
  }
  const lease = transport.root(frame.scopeId);
  nativeApply(nativePush, leases, [{ transport, lease }]);
  return lease;
}

const serviceWebSocketHandoffs = privateWeakMap<object, readonly string[]>();
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

/** Hide trusted Service handoff handles while preserving their native WebSocket identity. */
export function captureServiceWebSocketHandoffs(response: Response): Response {
  const raw = response.headers.get(SERVICE_WEBSOCKET_HANDOFF_HEADER);
  if (raw === null) return response;
  const headers = new Headers(response.headers);
  headers.delete(SERVICE_WEBSOCKET_HANDOFF_HEADER);
  if (!response.webSocket) throw failure("SERVICE_UNAVAILABLE");
  serviceWebSocketHandoffs.set(
    response.webSocket,
    nativeFreeze(parseServiceWebSocketHandoffs(raw)),
  );
  return responseWithHeaders(response, headers);
}

/** Replace any tenant header with handles previously captured from a trusted Service response. */
export function attachServiceWebSocketHandoffs(response: Response): Response {
  const handles = response.webSocket
    ? serviceWebSocketHandoffs.get(response.webSocket)
    : undefined;
  if (
    !response.headers.has(SERVICE_WEBSOCKET_HANDOFF_HEADER) &&
    handles === undefined
  )
    return response;
  const headers = new Headers(response.headers);
  headers.delete(SERVICE_WEBSOCKET_HANDOFF_HEADER);
  if (handles !== undefined)
    headers.set(SERVICE_WEBSOCKET_HANDOFF_HEADER, handles.join(","));
  return responseWithHeaders(response, headers);
}

/** Read only the handoff association captured from a trusted Service response. */
export function channelWebSocketHandoffs(
  response: Response,
): readonly string[] {
  return response.webSocket
    ? (serviceWebSocketHandoffs.get(response.webSocket) ?? [])
    : [];
}

/** Consume the association when a private channel takes ownership and hide its header. */
export function detachChannelWebSocketHandoffs(response: Response): Response {
  if (response.webSocket) serviceWebSocketHandoffs.delete(response.webSocket);
  if (!response.headers.has(SERVICE_WEBSOCKET_HANDOFF_HEADER)) return response;
  const headers = new Headers(response.headers);
  headers.delete(SERVICE_WEBSOCKET_HANDOFF_HEADER);
  return responseWithHeaders(response, headers);
}

/** Private Service policy shared by the native Fetcher and root lifecycle. */
export class ServiceBinding {
  readonly #transport: NativeServiceTransport;
  readonly #httpTransport: PublicTransport;
  readonly #createStub: (policy: object) => object;
  readonly #admitOperation: () => void;

  constructor(
    control: unknown,
    createStub: (policy: object) => object,
    admitOperation: () => void,
    httpTransport: PublicTransport,
  ) {
    if (
      !object(control) ||
      !callable(nativeGet(control, "rpc")) ||
      !callable(nativeGet(control, "get")) ||
      !callable(nativeGet(control, "root")) ||
      !callable(admitOperation) ||
      !object(httpTransport) ||
      !callable(nativeGet(httpTransport, "fetch")) ||
      !callable(nativeGet(httpTransport, "connect"))
    ) {
      throw failure("SERVICE_BINDING_DENIED");
    }
    this.#transport = control as NativeServiceTransport;
    this.#httpTransport = httpTransport;
    this.#createStub = createStub;
    this.#admitOperation = admitOperation;
    const proxy = new NativeProxy(this, {
      get(owner, property, receiver) {
        if (property === "then") return undefined;
        if (typeof property === "string" && RESERVED.has(property)) {
          throw failure("SERVICE_BINDING_DENIED");
        }
        const own = nativeGet(owner, property, receiver);
        if (own !== undefined || typeof property !== "string") {
          return callable(own) ? nativeApply(nativeBind, own, [owner]) : own;
        }
        const callbackController = serviceCapabilityController(
          owner.#transport,
          owner.#createStub,
        );
        return serviceMember((operation, args) => {
          const frame = currentServiceFrame();
          owner.#admitOperation();
          const transport =
            rootController(owner.#transport, frame) ?? owner.#transport;
          return operation === "get"
            ? transport.get(frame, property)
            : transport.rpc(
                frame,
                property,
                encodeServiceValue(args, callbackController) as unknown[],
              );
        }, callbackController);
      },
    });
    transports.set(this, this.#transport);
    transports.set(proxy, this.#transport);
    const callbacks = {
      fetch: (request: Request) =>
        nativeApply(nativeServiceFetch, this, [request]),
      connect: (address: string) =>
        nativeApply(nativeServiceConnect, this, [address]),
      hold: (handles: readonly string[]) =>
        this.#transport.holdWebSocketHandoff(handles),
    };
    channelCallbacks.set(this, callbacks);
    channelCallbacks.set(proxy, callbacks);
    return proxy;
  }

  async fetch(input: RequestInfo | URL, init?: RequestInit): Promise<Response> {
    const frame = currentServiceFrame();
    const request = new Request(input, init);
    request.headers.set("x-open-compute-service-frame", JSON.stringify(frame));
    const lease = rootController(this.#transport, frame);
    if (lease) await lease.ready();
    // Fetcher owns the HTTP/WebSocket capability; RPC cannot serialize WebSockets.
    return this.#httpTransport
      .fetch(request)
      .then(captureServiceWebSocketHandoffs);
  }

  connect(address: SocketAddress | string, options?: SocketOptions): Socket {
    return this.#httpTransport.connect(address, options);
  }
}

const transports = privateWeakMap<object, NativeServiceTransport>();
const nativeServiceFetch = ServiceBinding.prototype.fetch;
const nativeServiceConnect = ServiceBinding.prototype.connect;
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

/** Return HTTP and TCP dispatch only for a policy issued by this Service authority. */
export function serviceChannelCallbacks(policy: object) {
  return channelCallbacks.get(policy) ?? null;
}

/** Construct the private Service policy for a native Fetcher. */
export function createServiceBinding(
  createStub: (policy: object) => object,
  control: unknown,
  admitOperation: () => void,
  httpTransport: PublicTransport,
): ServiceBinding {
  return new ServiceBinding(control, createStub, admitOperation, httpTransport);
}

/** Associate the native public identity with its existing root lifecycle owner. */
export function registerServiceBinding(
  publicBinding: object,
  privateBinding: object,
): void {
  const transport = transports.get(privateBinding);
  if (!transport) throw failure("SERVICE_BINDING_DENIED");
  transports.set(publicBinding, transport);
}

/** Complete every raw controller participating in one drained root event. */
export async function completeServiceScope(
  userEnv: Record<string, unknown>,
  frame: ServiceFrame,
): Promise<void> {
  const leases = rootLeases.get(frame);
  rootLeases.delete(frame);
  const unique = new Set<NativeServiceTransport>();
  for (const value of nativeValues(userEnv)) {
    if (object(value)) {
      const transport = transports.get(value);
      if (transport) unique.add(transport);
    }
  }
  try {
    await Promise.allSettled(
      [...unique].map((transport) =>
        Promise.resolve(transport.completeRoot(frame.scopeId)),
      ),
    );
  } finally {
    if (leases)
      for (let index = 0; index < leases.length; index++)
        leases[index]!.lease[Symbol.dispose]();
  }
}
