import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { mkdtempSync, rmSync, symlinkSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";
import { preparedImage } from "./build-input.ts";

test("prepared image reference uses actual immutable manifest bytes and rejects unbounded inputs", () => {
  const directory = mkdtempSync(join(tmpdir(), "p23-build-input-"));
  try {
    const manifest = join(directory, "manifest.json");
    const input = join(directory, "input.json");
    const bytes = '{"schemaVersion":2}';
    writeFileSync(manifest, bytes);
    const images = [{ manifest }, { manifest }];
    writeFileSync(input, JSON.stringify({ images }));
    assert.equal(
      preparedImage(input),
      `p23-images.test/containers-fixture@sha256:${createHash("sha256").update(bytes).digest("hex")}`,
    );
    for (const path of [undefined, "relative.json"])
      assert.throws(() => preparedImage(path));
    for (const value of [
      null,
      {},
      { images: [] },
      { images: [null, null] },
      { images: [{ manifest: "relative" }, { manifest }] },
    ]) {
      writeFileSync(input, JSON.stringify(value));
      assert.throws(() => preparedImage(input));
    }
    writeFileSync(input, "invalid JSON");
    assert.throws(() => preparedImage(input));
    writeFileSync(input, " ".repeat(65537));
    assert.throws(() => preparedImage(input));
    const link = join(directory, "manifest-link");
    symlinkSync(manifest, link);
    writeFileSync(
      input,
      JSON.stringify({ images: [{ manifest: link }, { manifest }] }),
    );
    assert.throws(() => preparedImage(input));
    writeFileSync(input, JSON.stringify({ images }));
    writeFileSync(manifest, " ".repeat(1048577));
    assert.throws(() => preparedImage(input));
  } finally {
    rmSync(directory, { recursive: true, force: true });
  }
});
