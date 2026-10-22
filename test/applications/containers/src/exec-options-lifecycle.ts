type NativeContainer = NonNullable<DurableObjectState["container"]>;

function check(condition: boolean, message: string): asserts condition {
  if (!condition) throw new Error(message);
}

export async function collect(
  native: NativeContainer,
  script: string,
  options: ContainerExecOptions,
  validate: (process: ExecProcess) => void,
  deadline: number,
): Promise<{ process: ExecProcess; output: ExecOutput }> {
  check(
    Date.now() < deadline,
    "exec options cannot start after their deadline",
  );
  const abort = new AbortController();
  let exited = false;
  let timer: ReturnType<typeof setTimeout> | null = null;
  try {
    return await Promise.race([
      (async () => {
        const child = await native.exec(["python3", "-c", script], {
          ...options,
          signal: abort.signal,
        });
        void child.exitCode.then(
          () => {
            exited = true;
          },
          () => {},
        );
        validate(child);
        const output = await child.output();
        exited = true;
        return { process: child, output };
      })(),
      new Promise<never>((_resolve, reject) => {
        timer = setTimeout(
          () => reject(new Error("exec options did not complete")),
          Math.max(0, deadline - Date.now()),
        );
      }),
    ]);
  } finally {
    clearTimeout(timer);
    // A settled child must never receive a cleanup signal.
    if (!exited) abort.abort();
  }
}

export async function execOptionsLifecycle(
  native: NativeContainer,
  diagnostic: { phase: string },
): Promise<Response> {
  diagnostic.phase = "exec-options-running-check";
  check(native.running, "exec options require an already running main");
  const deadline = Date.now() + 60000;
  const encoder = new TextEncoder();
  const decoder = new TextDecoder();
  const input = new ReadableStream<Uint8Array>({
    start(controller) {
      controller.enqueue(encoder.encode("first:"));
      controller.enqueue(encoder.encode("second"));
      controller.close();
    },
  });
  diagnostic.phase = "exec-options-stdin-collect";
  const stdin = await collect(
    native,
    "import os,sys; data=sys.stdin.buffer.read(); assert data==b'first:second'; os.write(1,b'stdin-eof:'+data); os.write(2,b'stdin-err')",
    { stdin: input },
    () => {},
    deadline,
  );
  diagnostic.phase = "exec-options-stdin-check";
  check(
    stdin.output.exitCode === 0 &&
      decoder.decode(stdin.output.stdout) === "stdin-eof:first:second" &&
      decoder.decode(stdin.output.stderr) === "stdin-err",
    "ReadableStream stdin must deliver all bytes and real EOF",
  );

  diagnostic.phase = "exec-options-stdout-ignored-collect";
  const stdoutIgnored = await collect(
    native,
    "import os; os.write(1,b'ignored-out'); os.write(2,b'kept-err')",
    { stdout: "ignore" },
    (process) => {
      check(
        Reflect.get(process, "stdout") == null,
        "ignored stdout must not expose a readable stream",
      );
    },
    deadline,
  );
  diagnostic.phase = "exec-options-stdout-ignored-check";
  check(
    stdoutIgnored.output.exitCode === 0 &&
      stdoutIgnored.output.stdout.byteLength === 0 &&
      decoder.decode(stdoutIgnored.output.stderr) === "kept-err",
    "stdout ignore must discard only stdout",
  );

  diagnostic.phase = "exec-options-stderr-ignored-collect";
  const stderrIgnored = await collect(
    native,
    "import os; os.write(1,b'kept-out'); os.write(2,b'ignored-err')",
    { stderr: "ignore" },
    (process) => {
      check(
        Reflect.get(process, "stderr") == null,
        "ignored stderr must not expose a readable stream",
      );
    },
    deadline,
  );
  diagnostic.phase = "exec-options-stderr-ignored-check";
  check(
    stderrIgnored.output.exitCode === 0 &&
      decoder.decode(stderrIgnored.output.stdout) === "kept-out" &&
      stderrIgnored.output.stderr.byteLength === 0,
    "stderr ignore must discard only stderr",
  );

  diagnostic.phase = "exec-options-combined-collect";
  const combined = await collect(
    native,
    "import os; os.write(1,b'combined-out\\n'); os.write(2,b'combined-err\\n')",
    { stderr: "combined" },
    (process) => {
      check(
        !process.isPty &&
          Reflect.get(process, "stdout") != null &&
          Reflect.get(process, "stderr") == null,
        "non-PTY combined output must expose only stdout",
      );
    },
    deadline,
  );
  // Independent stdout/stderr pipes do not define an order between their writes.
  diagnostic.phase = "exec-options-combined-lines-check";
  const combinedLines = decoder.decode(combined.output.stdout).split("\n");
  check(
    combinedLines.pop() === "",
    "combined output must preserve both newlines",
  );
  combinedLines.sort();
  check(
    combined.output.exitCode === 0 &&
      combinedLines.join("\n") === "combined-err\ncombined-out" &&
      combined.output.stderr.byteLength === 0,
    "combined stdout must contain both sources and stderr must be empty",
  );
  let secondOutputName: string | null = null;
  diagnostic.phase = "exec-options-second-output";
  try {
    await combined.process.output();
  } catch (error) {
    if (!(error instanceof TypeError)) throw error;
    secondOutputName = error.name;
  }
  diagnostic.phase = "exec-options-second-output-check";
  check(secondOutputName === "TypeError", "output must reject a second call");
  diagnostic.phase = "exec-options-running-after-check";
  check(native.running, "exec options must preserve their running main");
  return Response.json({
    stdin: {
      stdout: "stdin-eof:first:second",
      stderr: "stdin-err",
      exitCode: 0,
    },
    stdoutIgnored: {
      absent: true,
      stdoutBytes: 0,
      stderr: "kept-err",
      exitCode: 0,
    },
    stderrIgnored: {
      absent: true,
      stdout: "kept-out",
      stderrBytes: 0,
      exitCode: 0,
    },
    combined: {
      isPty: false,
      stderrAbsent: true,
      stdoutLines: combinedLines,
      stderrBytes: 0,
      exitCode: 0,
    },
    secondOutputName,
  });
}
