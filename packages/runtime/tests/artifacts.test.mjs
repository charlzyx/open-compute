import assert from "node:assert/strict";
import test from "node:test";
import { importRuntime } from "./compiled-runtime.mjs";

const { ArtifactError, ArtifactsBinding } = await importRuntime(
  "artifacts/facade.ts",
);

const info = {
  id: "repo-id",
  name: "source",
  description: null,
  defaultBranch: "main",
  createdAt: "2026-09-10T00:00:00Z",
  updatedAt: "2026-09-10T00:00:00Z",
  lastPushAt: null,
  source: null,
  readOnly: false,
  remote: `https://artifacts.example.test/git/${"0".repeat(32)}/apps/source.git`,
};
const created = {
  id: "repo-id",
  name: "source",
  description: null,
  defaultBranch: "main",
  remote: info.remote,
  token: "once",
};

test("Artifacts facade exposes the pinned namespace and repository methods", async () => {
  const calls = [];
  const binding = new ArtifactsBinding({
    async call(operation, input) {
      calls.push({ operation, input });
      if (operation === "get") return info;
      if (operation === "list") {
        const { remote: _, ...listed } = info;
        return { repos: [listed], total: 1, cursor: "next" };
      }
      if (operation === "create-token") {
        return {
          id: "token-id",
          plaintext: "once",
          scope: "read",
          expiresAt: "2026-09-10T00:01:00Z",
        };
      }
      if (operation === "list-tokens") {
        return {
          tokens: [
            {
              id: "token-id",
              scope: "read",
              state: "active",
              createdAt: "2026-09-10T00:00:00Z",
              expiresAt: "2026-09-10T00:01:00Z",
            },
          ],
          total: 1,
        };
      }
      if (operation === "revoke-token" || operation === "delete") return true;
      return created;
    },
  });

  assert.deepEqual(await binding.create("source"), created);
  const repository = await binding.get("source");
  assert.deepEqual(Object.keys(binding), []);
  assert.deepEqual(Object.getOwnPropertySymbols(binding), []);
  assert.deepEqual(Object.keys(repository), []);
  assert.deepEqual(Object.getOwnPropertySymbols(repository), []);
  assert.deepEqual(await repository.info(), info);
  assert.deepEqual(await repository.info(), info);
  assert.equal((await repository.createToken("read", 60)).scope, "read");
  assert.equal((await repository.listTokens()).total, 1);
  assert.equal(await repository.revokeToken("token-id"), true);
  assert.deepEqual(await repository.fork("target"), created);
  assert.deepEqual(
    await binding.import({
      source: { url: "https://example.com/source.git" },
      target: { name: "imported" },
    }),
    created,
  );
  const { remote: _, ...listedInfo } = info;
  assert.deepEqual(await binding.list({ limit: 1 }), {
    repos: [listedInfo],
    total: 1,
    cursor: "next",
  });
  assert.equal(await binding.delete("source"), true);
  assert.deepEqual(
    calls.map(({ operation }) => operation),
    [
      "create",
      "get",
      "get",
      "get",
      "create-token",
      "list-tokens",
      "revoke-token",
      "fork",
      "import",
      "list",
      "delete",
    ],
  );
});

test("Artifacts calls keep the transport private under tenant prototype edits", async () => {
  const raw = {
    call: async (operation) => (operation === "get" ? info : true),
  };
  const original = Reflect.apply;
  const originalGet = Reflect.get;
  const originalIsArray = Array.isArray;
  Reflect.apply = (fn, receiver, args) => {
    assert.notEqual(receiver, raw, "transport leaked through a patched global");
    return original(fn, receiver, args);
  };
  Reflect.get = (target, ...args) => {
    assert.notEqual(
      target,
      raw,
      "transport leaked through patched Reflect.get",
    );
    return originalGet(target, ...args);
  };
  Array.isArray = (value) => {
    assert.notEqual(
      value,
      raw,
      "transport leaked through patched Array.isArray",
    );
    return originalIsArray(value);
  };
  try {
    const binding = new ArtifactsBinding(raw);
    const repository = await binding.get("source");
    assert.equal(repository.raw, undefined);
    assert.equal(await repository.revokeToken("token-id"), true);
  } finally {
    Reflect.apply = original;
    Reflect.get = originalGet;
    Array.isArray = originalIsArray;
  }
});

test("Artifacts repository info performs a fresh lookup and preserves deletion errors", async () => {
  let current = { ...info };
  const binding = new ArtifactsBinding({
    async call(operation, input) {
      assert.equal(operation, "get");
      assert.deepEqual(input, { name: "source" });
      if (current === null) throw new ArtifactError("NOT_FOUND", 10200);
      return current;
    },
  });
  const repository = await binding.get("source");
  current = {
    ...info,
    description: "updated",
    updatedAt: "2026-09-11T00:00:00Z",
  };
  assert.deepEqual(await repository.info(), current);
  assert.equal(repository.description, undefined);
  current = null;
  await assert.rejects(repository.info(), {
    name: "ArtifactsError",
    code: "NOT_FOUND",
  });
});

test("Artifacts facade preserves stable errors and rejects malformed success", async () => {
  const expected = new ArtifactError("INVALID_TTL", 10103);
  const failed = new ArtifactsBinding({
    call() {
      throw expected;
    },
  });
  await assert.rejects(failed.create("repo"), (error) => {
    assert.equal(error.name, "ArtifactsError");
    assert.equal(error.code, "INVALID_TTL");
    assert.equal(error.numericCode, 10103);
    return true;
  });
  const malformed = new ArtifactsBinding({ call: async () => ({ total: 1 }) });
  await assert.rejects(malformed.list(), {
    name: "ArtifactsError",
    message: "INTERNAL_ERROR",
  });
});
