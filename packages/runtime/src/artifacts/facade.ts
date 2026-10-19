import wrappedBinding from "cloudflare-internal:wrapped-binding";

interface ArtifactsRawTransport {
  call(operation: string, input: unknown): Promise<unknown>;
}

const ERROR_CODES = new Set<ArtifactsErrorCode>([
  "ALREADY_EXISTS",
  "NOT_FOUND",
  "IMPORT_IN_PROGRESS",
  "FORK_IN_PROGRESS",
  "INVALID_INPUT",
  "INVALID_REPO_NAME",
  "INVALID_TTL",
  "INVALID_URL",
  "REMOTE_AUTH_REQUIRED",
  "UPSTREAM_UNAVAILABLE",
  "MEMORY_LIMIT",
  "INTERNAL_ERROR",
]);

export class ArtifactError extends Error implements ArtifactsError {
  readonly name = "ArtifactsError";
  constructor(
    readonly code: ArtifactsErrorCode,
    readonly numericCode: number,
  ) {
    super(code);
  }
}

function record(value: unknown): value is Record<string, unknown> {
  return value !== null && typeof value === "object" && !Array.isArray(value);
}

function protocolError(): never {
  throw new ArtifactError("INTERNAL_ERROR", 10400);
}

function isTransport(value: unknown): value is ArtifactsRawTransport {
  return (
    value !== null &&
    typeof value === "object" &&
    typeof (value as Partial<ArtifactsRawTransport>).call === "function"
  );
}

function error(value: unknown): ArtifactError {
  if (record(value)) {
    const code = value.code;
    const numericCode = value.numericCode;
    if (
      typeof code === "string" &&
      ERROR_CODES.has(code as ArtifactsErrorCode) &&
      Number.isSafeInteger(numericCode)
    ) {
      return new ArtifactError(
        code as ArtifactsErrorCode,
        numericCode as number,
      );
    }
  }
  return new ArtifactError("INTERNAL_ERROR", 10400);
}

async function call(
  raw: ArtifactsRawTransport,
  operation: string,
  input: unknown,
): Promise<unknown> {
  try {
    return await raw.call(operation, input);
  } catch (cause) {
    throw error(cause);
  }
}

function string(value: unknown): string {
  if (typeof value !== "string") protocolError();
  return value;
}

function nullableString(value: unknown): string | null {
  if (value !== null && typeof value !== "string") protocolError();
  return value;
}

function repoInfo(value: unknown): ArtifactsRepoInfo {
  if (!record(value)) protocolError();
  return {
    id: string(value.id),
    name: string(value.name),
    description: nullableString(value.description),
    defaultBranch: string(value.defaultBranch),
    createdAt: string(value.createdAt),
    updatedAt: string(value.updatedAt),
    lastPushAt: nullableString(value.lastPushAt),
    source: nullableString(value.source),
    readOnly:
      typeof value.readOnly === "boolean" ? value.readOnly : protocolError(),
    remote: string(value.remote),
  };
}

function created(value: unknown): ArtifactsCreateRepoResult {
  if (!record(value)) protocolError();
  return {
    id: string(value.id),
    name: string(value.name),
    description: nullableString(value.description),
    defaultBranch: string(value.defaultBranch),
    remote: string(value.remote),
    token: string(value.token),
  };
}

function createdToken(value: unknown): ArtifactsCreateTokenResult {
  if (!record(value)) protocolError();
  const scope = value.scope;
  if (scope !== "read" && scope !== "write") protocolError();
  return {
    id: string(value.id),
    plaintext: string(value.plaintext),
    scope,
    expiresAt: string(value.expiresAt),
  };
}

function listedTokens(value: unknown): ArtifactsTokenListResult {
  if (!record(value) || !Array.isArray(value.tokens)) protocolError();
  const tokens = value.tokens.map((raw): ArtifactsTokenInfo => {
    if (!record(raw)) protocolError();
    const scope = raw.scope;
    const state = raw.state;
    if (
      (scope !== "read" && scope !== "write") ||
      (state !== "active" && state !== "expired" && state !== "revoked")
    )
      protocolError();
    return {
      id: string(raw.id),
      scope,
      state,
      createdAt: string(raw.createdAt),
      expiresAt: string(raw.expiresAt),
    };
  });
  if (!Number.isSafeInteger(value.total) || value.total !== tokens.length)
    protocolError();
  return { tokens, total: value.total as number };
}

class ArtifactRepoHandle implements Pick<
  ArtifactsRepo,
  "info" | "createToken" | "listTokens" | "revokeToken" | "fork"
> {
  readonly #raw: ArtifactsRawTransport;
  readonly #name: string;

  constructor(raw: ArtifactsRawTransport, info: ArtifactsRepoInfo) {
    this.#raw = raw;
    this.#name = info.name;
  }

  async info(): Promise<ArtifactsRepoInfo> {
    return repoInfo(await call(this.#raw, "get", { name: this.#name }));
  }

  async createToken(scope?: "write" | "read", ttl?: number) {
    return createdToken(
      await call(this.#raw, "create-token", {
        repository: this.#name,
        scope,
        ttl,
      }),
    );
  }

  async listTokens() {
    return listedTokens(
      await call(this.#raw, "list-tokens", { repository: this.#name }),
    );
  }

  async revokeToken(tokenOrId: string) {
    const value = await call(this.#raw, "revoke-token", {
      repository: this.#name,
      tokenOrId,
    });
    return typeof value === "boolean" ? value : protocolError();
  }

  async fork(
    name: string,
    opts?: {
      description?: string;
      readOnly?: boolean;
      defaultBranchOnly?: boolean;
    },
  ) {
    return created(
      await call(this.#raw, "fork", {
        repository: this.#name,
        name,
        opts,
      }),
    );
  }
}

/** Supported Cloudflare Artifacts operations over a private namespace transport. */
export class ArtifactsBinding
  extends wrappedBinding.WrappedBinding
  implements Omit<Artifacts, "get">
{
  readonly #raw: ArtifactsRawTransport;
  constructor(raw: unknown) {
    super(raw);
    if (!isTransport(raw)) protocolError();
    this.#raw = raw;
  }

  async create(
    name: string,
    opts?: {
      readOnly?: boolean;
      description?: string;
      setDefaultBranch?: string;
    },
  ) {
    return created(await call(this.#raw, "create", { name, opts }));
  }

  async get(name: string): Promise<ArtifactRepoHandle> {
    return new ArtifactRepoHandle(
      this.#raw,
      repoInfo(await call(this.#raw, "get", { name })),
    );
  }

  async import(params: {
    source: { url: string; branch?: string; depth?: number };
    target: {
      name: string;
      opts?: { description?: string; readOnly?: boolean };
    };
  }) {
    return created(await call(this.#raw, "import", params));
  }

  async list(opts?: { limit?: number; cursor?: string }) {
    const value = await call(this.#raw, "list", opts ?? {});
    if (!record(value) || !Array.isArray(value.repos)) protocolError();
    const repos = value.repos.map((raw) => {
      if (!record(raw)) protocolError();
      const info = repoInfo({ ...raw, remote: "" });
      return {
        id: info.id,
        name: info.name,
        description: info.description,
        defaultBranch: info.defaultBranch,
        createdAt: info.createdAt,
        updatedAt: info.updatedAt,
        lastPushAt: info.lastPushAt,
        source: info.source,
        readOnly: info.readOnly,
      };
    });
    if (!Number.isSafeInteger(value.total)) protocolError();
    const result: ArtifactsRepoListResult = {
      repos,
      total: value.total as number,
    };
    if (value.cursor !== undefined) result.cursor = string(value.cursor);
    return result;
  }

  async delete(name: string) {
    const value = await call(this.#raw, "delete", { name });
    return typeof value === "boolean" ? value : protocolError();
  }
}

/** Construct the config-owned Artifacts binding from its scoped native Fetcher. */
export default function artifactsBinding(env: {
  fetcher: unknown;
}): ArtifactsBinding {
  return new ArtifactsBinding(env.fetcher);
}
