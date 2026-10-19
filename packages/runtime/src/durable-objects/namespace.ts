import { privateWeakMap } from "../private-weak-map.js";
import { base64Bytes, hex, hmacSha256, randomBytes, utf8 } from "./id-codec.js";
import type {
  DoNamespaceCapability,
  DoNativeFactories,
  DoObjectIdentity,
} from "./protocol.js";
import { createStubPolicy, rawTransport } from "./stub.js";

interface NamespaceState {
  factories: DoNativeFactories;
  prefix: string;
  key: Uint8Array;
  maxNameBytes: number;
  raw: object;
  jurisdiction: string | undefined;
}
const namespaceState = privateWeakMap<object, NamespaceState>();
const idState = privateWeakMap<object, DoObjectIdentity>();
const ID = /^[0-9a-f]{64}$/;
const ID_PREFIX_HEX_LENGTH = 16;
const ID_BODY_BYTES = 15;
const ID_TAG_BYTES = 8;
const ID_FORMAT_BASE = 0xa0;
const JURISDICTIONS = new Set(["eu", "fedramp", "fedramp-high", "us"]);
const JURISDICTION_CODES = new Map<string, number>([
  ["eu", 1],
  ["fedramp", 2],
  ["fedramp-high", 3],
  ["us", 4],
]);
const JURISDICTIONS_BY_CODE = new Map<number, string>(
  [...JURISDICTION_CODES].map(([name, code]) => [code, name]),
);
const LOCATION_HINTS = new Set([
  "wnam",
  "enam",
  "sam",
  "weur",
  "eeur",
  "apac",
  "apac-ne",
  "apac-se",
  "oc",
  "afr",
  "me",
]);
const ROUTING_MODES = new Set(["primary-only"]);
function failure(code: string, type: ErrorConstructor = Error) {
  const error = Object.assign(new type(code), { stableCode: code });
  error.stack = `${error.name}: ${code}`;
  return error;
}

function record(value: unknown): value is Record<string, unknown> {
  return value !== null && typeof value === "object" && !Array.isArray(value);
}

function enumOption(
  value: unknown,
  allowed: Set<string>,
  code: string,
): string {
  if (typeof value !== "string" || !allowed.has(value))
    throw failure(code, TypeError);
  return value;
}

function getOptions(options: unknown): {
  locationHint?: string;
  routingMode?: string;
} {
  if (options === undefined) return {};
  if (!record(options)) throw failure("DO_ID_INVALID", TypeError);
  const output: { locationHint?: string; routingMode?: string } = {};
  if (options.locationHint !== undefined) {
    output.locationHint = enumOption(
      options.locationHint,
      LOCATION_HINTS,
      "DO_ID_INVALID",
    );
  }
  if (options.routingMode !== undefined) {
    output.routingMode = enumOption(
      options.routingMode,
      ROUTING_MODES,
      "DO_ID_INVALID",
    );
  }
  return output;
}

function uniqueIdOptions(options: unknown): { jurisdiction?: string } {
  if (options === undefined) return {};
  if (!record(options)) throw failure("DO_ID_INVALID", TypeError);
  if (options.jurisdiction === undefined || options.jurisdiction === null)
    return {};
  return {
    jurisdiction: enumOption(
      options.jurisdiction,
      JURISDICTIONS,
      "DO_ID_INVALID",
    ),
  };
}

function parseJurisdiction(value: unknown): string | undefined {
  if (value === undefined || value === null) return undefined;
  return enumOption(value, JURISDICTIONS, "DO_ID_INVALID");
}

function hexBytes(value: string): Uint8Array {
  const bytes = new Uint8Array(value.length / 2);
  for (let index = 0; index < bytes.length; index += 1) {
    bytes[index] = Number.parseInt(value.slice(index * 2, index * 2 + 2), 16);
  }
  return bytes;
}

function equalBytes(left: Uint8Array, right: Uint8Array): boolean {
  if (left.byteLength !== right.byteLength) return false;
  let difference = 0;
  for (let index = 0; index < left.byteLength; index += 1) {
    difference |= left[index]! ^ right[index]!;
  }
  return difference === 0;
}

function idPayload(
  key: Uint8Array,
  body: Uint8Array,
  requested: string | undefined,
): Uint8Array {
  if (body.byteLength !== ID_BODY_BYTES)
    throw failure("DO_ID_INVALID", TypeError);
  const payload = new Uint8Array(1 + ID_BODY_BYTES + ID_TAG_BYTES);
  payload[0] =
    ID_FORMAT_BASE +
    (requested === undefined ? 0 : JURISDICTION_CODES.get(requested)!);
  payload.set(body, 1);
  payload.set(
    hmacSha256(key, payload.subarray(0, 1 + ID_BODY_BYTES)).subarray(
      0,
      ID_TAG_BYTES,
    ),
    1 + ID_BODY_BYTES,
  );
  return payload;
}

function namedBody(
  key: Uint8Array,
  name: Uint8Array,
  requested: string | undefined,
): Uint8Array {
  const input = new Uint8Array(2 + name.byteLength);
  input[0] = 0x6e;
  input[1] = requested === undefined ? 0 : JURISDICTION_CODES.get(requested)!;
  input.set(name, 2);
  return hmacSha256(key, input).subarray(0, ID_BODY_BYTES);
}

function decodeId(
  state: Pick<NamespaceState, "prefix" | "key" | "jurisdiction">,
  value: unknown,
): { value: string; jurisdiction: string | undefined } {
  if (
    typeof value !== "string" ||
    !ID.test(value) ||
    !value.startsWith(state.prefix)
  ) {
    throw failure("DO_ID_INVALID", TypeError);
  }
  const payload = hexBytes(value.slice(ID_PREFIX_HEX_LENGTH));
  const code = payload[0]! - ID_FORMAT_BASE;
  const decodedJurisdiction =
    code === 0 ? undefined : JURISDICTIONS_BY_CODE.get(code);
  if (
    code < 0 ||
    code > JURISDICTION_CODES.size ||
    (code !== 0 && decodedJurisdiction === undefined)
  ) {
    throw failure("DO_ID_INVALID", TypeError);
  }
  const content = payload.subarray(0, 1 + ID_BODY_BYTES);
  const expected = hmacSha256(state.key, content).subarray(0, ID_TAG_BYTES);
  if (!equalBytes(expected, payload.subarray(1 + ID_BODY_BYTES))) {
    throw failure("DO_ID_INVALID", TypeError);
  }
  if (
    state.jurisdiction !== undefined &&
    state.jurisdiction !== decodedJurisdiction
  ) {
    throw failure("DO_ID_INVALID", TypeError);
  }
  return { value, jurisdiction: decodedJurisdiction };
}

function assertName(name: unknown, maxBytes: number): Uint8Array {
  if (typeof name !== "string") throw failure("DO_ID_INVALID", TypeError);
  const bytes = utf8(name);
  if (bytes.byteLength > maxBytes) throw failure("DO_ID_INVALID", TypeError);
  return bytes;
}

function makeId(
  state: NamespaceState,
  value: string,
  name: string | undefined,
  jurisdiction: string | undefined,
) {
  const id = state.factories.createId(value, name, jurisdiction);
  if (id === null || typeof id !== "object" || idState.has(id))
    throw failure("DO_ID_INVALID", TypeError);
  idState.set(id, Object.freeze({ value, name, jurisdiction }));
  return id;
}

class NamespacePolicy {
  constructor(
    composite: unknown,
    factories: DoNativeFactories,
    requestedJurisdiction?: unknown,
  ) {
    if (composite instanceof NamespacePolicy) {
      const parent = namespaceState.get(composite);
      if (!parent) throw failure("DO_NAMESPACE_NOT_FOUND");
      const scoped = parseJurisdiction(requestedJurisdiction);
      namespaceState.set(
        this,
        Object.freeze({ ...parent, jurisdiction: scoped }),
      );
      Object.freeze(this);
      return;
    }
    if (!namespaceCapability(composite, factories)) {
      throw failure("DO_NAMESPACE_NOT_FOUND");
    }
    const key = base64Bytes(composite.namespaceNameKey);
    if (key.byteLength !== 32) throw failure("DO_NAMESPACE_NOT_FOUND");
    namespaceState.set(
      this,
      Object.freeze({
        factories,
        prefix: composite.namespacePrefix,
        key,
        maxNameBytes: composite.maxObjectNameBytes,
        raw: composite.transport,
        jurisdiction: undefined,
      }),
    );
    Object.freeze(this);
  }

  jurisdiction(value: string) {
    const factories = namespaceState.get(this)!.factories;
    return nativeNamespace(
      new NamespacePolicy(this, factories, value),
      factories,
    );
  }

  idFromName(name: string) {
    const state = namespaceState.get(this)!;
    const bytes = assertName(name, state.maxNameBytes);
    const payload = idPayload(
      state.key,
      namedBody(state.key, bytes, state.jurisdiction),
      state.jurisdiction,
    );
    return makeId(state, state.prefix + hex(payload), name, state.jurisdiction);
  }

  newUniqueId(options?: unknown) {
    const requested = uniqueIdOptions(options);
    const state = namespaceState.get(this)!;
    const jurisdiction = requested.jurisdiction ?? state.jurisdiction;
    if (
      requested.jurisdiction !== undefined &&
      state.jurisdiction !== undefined &&
      requested.jurisdiction !== state.jurisdiction
    ) {
      throw failure("DO_ID_INVALID", TypeError);
    }
    return makeId(
      state,
      state.prefix +
        hex(idPayload(state.key, randomBytes(ID_BODY_BYTES), jurisdiction)),
      undefined,
      jurisdiction,
    );
  }

  idFromString(value: string) {
    const state = namespaceState.get(this)!;
    const decoded = decodeId(state, value);
    return makeId(state, decoded.value, undefined, decoded.jurisdiction);
  }

  get(id: DurableObjectId, options?: unknown) {
    getOptions(options);
    if (!idState.has(id)) throw failure("DO_ID_INVALID", TypeError);
    const state = namespaceState.get(this)!;
    const identity = idState.get(id)!;
    if (!identity.value.startsWith(state.prefix))
      throw failure("DO_ID_INVALID", TypeError);
    if (
      state.jurisdiction !== undefined &&
      identity.jurisdiction !== state.jurisdiction
    ) {
      throw failure("DO_ID_INVALID", TypeError);
    }
    return state.factories.createStub(
      id,
      state.raw,
      createStubPolicy(identity, state.raw, state.factories),
    );
  }

  getByName(name: string, options?: unknown) {
    return this.get(this.idFromName(name), options);
  }
}

function nativeNamespace(
  policy: NamespacePolicy,
  factories: DoNativeFactories,
): DurableObjectNamespace {
  return factories.createNamespace({
    newUniqueId: policy.newUniqueId.bind(policy),
    idFromName: policy.idFromName.bind(policy),
    idFromString: policy.idFromString.bind(policy),
    get: policy.get.bind(policy),
    getByName: policy.getByName.bind(policy),
    jurisdiction: policy.jurisdiction.bind(policy),
  });
}

/** Materialize the native namespace around the sole current identity/router policy. */
export function createDurableObjectNamespace(
  composite: unknown,
  factories: DoNativeFactories,
): DurableObjectNamespace {
  return nativeNamespace(new NamespacePolicy(composite, factories), factories);
}

function namespaceCapability(
  value: unknown,
  factories: DoNativeFactories,
): value is DoNamespaceCapability {
  if (
    value === null ||
    typeof value !== "object" ||
    !("schemaVersion" in value) ||
    value.schemaVersion !== 1 ||
    !("namespacePrefix" in value) ||
    typeof value.namespacePrefix !== "string" ||
    !/^[0-9a-f]{16}$/.test(value.namespacePrefix) ||
    !("namespaceNameKey" in value) ||
    typeof value.namespaceNameKey !== "string" ||
    !("maxObjectNameBytes" in value) ||
    typeof value.maxObjectNameBytes !== "number" ||
    !Number.isSafeInteger(value.maxObjectNameBytes) ||
    value.maxObjectNameBytes < 1 ||
    value.maxObjectNameBytes > 1024 ||
    !("transport" in value) ||
    value.transport === null ||
    typeof value.transport !== "object"
  )
    return false;
  return rawTransport(factories.createRpcTransport(value.transport));
}
