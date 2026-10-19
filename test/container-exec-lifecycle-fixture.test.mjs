import assert from "node:assert/strict";
import test from "node:test";
import { execLifecycle } from "./applications/containers/src/exec-lifecycle.ts";

for (const [message, errorCode] of [
  ["Container exec control failed.", "EXEC_CONTROL_FAILED"],
  ["Container exec process could not start.", "EXEC_SPAWN_FAILED"],
  ["private-token/path/internal topology", "RUNTIME_UNKNOWN"],
]) {
  test(`exec lifecycle diagnostics classify ${errorCode} without raw errors`, async () => {
    let destroys = 0;
    const response = await execLifecycle({
      exec: async () => {
        throw new Error(message);
      },
      destroy: async () => {
        destroys++;
      },
    });
    assert.equal(response.status, 500);
    assert.deepEqual(await response.json(), {
      error: "EXEC_LIFECYCLE_FAILED",
      phase: "pid.spawn",
      errorKind: "runtime",
      errorCode,
    });
    assert.equal(destroys, 1);
  });
}

test("exec lifecycle preserves an assertion failure when cleanup also fails", async () => {
  const response = await execLifecycle({
    exec: async () => ({ output: async () => ({ exitCode: 9 }) }),
    destroy: async () => {
      throw new Error("private cleanup information");
    },
  });
  assert.equal(response.status, 500);
  assert.deepEqual(await response.json(), {
    error: "EXEC_LIFECYCLE_FAILED",
    phase: "pid.output",
    errorKind: "fixture-assertion",
    errorCode: "ASSERTION_FAILED",
    cleanup: {
      phase: "cleanup.destroy",
      errorKind: "runtime",
      errorCode: "RUNTIME_UNKNOWN",
    },
  });
});

test("only shell removal selects root; the shellless child selects nobody", async () => {
  const encoder = new TextEncoder();
  const options = [];
  const processes = [
    {
      pid: 123,
      output: async () => ({ exitCode: 0, stdout: encoder.encode("123\n") }),
    },
    {
      stdin: new WritableStream(),
      exitCode: Promise.resolve(7),
      output: async () => ({
        exitCode: 7,
        stdout: encoder.encode("late-out"),
        stderr: encoder.encode("late-err"),
      }),
    },
    {
      stdin: new WritableStream(),
      exitCode: Promise.resolve(7),
      output: async () => {
        throw new Error("Container exec output was interrupted.");
      },
    },
    { output: async () => ({ exitCode: 0 }) },
  ];
  const response = await execLifecycle({
    exec: async (_argv, suppliedOptions) => {
      options.push(suppliedOptions);
      const process = processes.shift();
      if (process) return process;
      throw new Error("Container exec process could not start.");
    },
    destroy: async () => {},
  });
  assert.deepEqual(options, [
    undefined,
    { stdin: "pipe" },
    { stdin: "pipe" },
    { user: "0:0", cwd: "/" },
    { user: "65534:65534", cwd: "/" },
  ]);
  assert.equal(response.status, 500);
  assert.deepEqual(await response.json(), {
    error: "EXEC_LIFECYCLE_FAILED",
    phase: "shellless.spawn",
    errorKind: "runtime",
    errorCode: "EXEC_SPAWN_FAILED",
  });
});
