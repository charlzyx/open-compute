type NativeContainer = NonNullable<DurableObjectState["container"]>;

type Diagnostic = { phase: string };
type Failure = { phase: string; errorKind: string; errorCode: string };

class FixtureAssertionError extends Error {}

function failure(phase: string, error: unknown): Failure {
  let errorCode = "RUNTIME_UNKNOWN";
  if (error instanceof FixtureAssertionError) errorCode = "ASSERTION_FAILED";
  else if (error instanceof Error) {
    if (error.message === "Container exec control failed.")
      errorCode = "EXEC_CONTROL_FAILED";
    if (error.message === "Container exec process could not start.")
      errorCode = "EXEC_SPAWN_FAILED";
  }
  return {
    phase,
    errorKind:
      error instanceof FixtureAssertionError ? "fixture-assertion" : "runtime",
    errorCode,
  };
}

function failedResponse(original: Failure, cleanup?: Failure): Response {
  return Response.json(
    {
      error: "EXEC_LIFECYCLE_FAILED",
      ...original,
      ...(cleanup ? { cleanup } : {}),
    },
    { status: 500 },
  );
}

function check(condition: boolean, message: string): asserts condition {
  if (!condition) throw new FixtureAssertionError(message);
}

async function interrupted(output: Promise<ExecOutput>): Promise<true> {
  try {
    await output;
  } catch (error) {
    if (
      !(error instanceof Error) ||
      error.message !== "Container exec output was interrupted."
    )
      throw error;
    return true;
  }
  throw new FixtureAssertionError(
    "exec output must reject interrupted delivery",
  );
}

async function exercise(
  native: NativeContainer,
  diagnostic: Diagnostic,
): Promise<Response> {
  const decoder = new TextDecoder();
  const encoder = new TextEncoder();
  diagnostic.phase = "pid.spawn";
  const pid = await native.exec([
    "python3",
    "-c",
    "import os; print(os.getpid())",
  ]);
  diagnostic.phase = "pid.output";
  const pidOutput = await pid.output();
  check(pidOutput.exitCode === 0, "PID probe must succeed");
  check(
    Number(decoder.decode(pidOutput.stdout).trim()) === pid.pid,
    "exec PID must identify the application child",
  );

  diagnostic.phase = "late.spawn";
  const late = await native.exec(
    [
      "python3",
      "-c",
      [
        "import os,sys",
        "if os.fork(): os._exit(7)",
        "if sys.stdin.buffer.read() != b'r': os._exit(90)",
        "os.write(1,b'late-out')",
        "os.write(2,b'late-err')",
        "os._exit(0)",
      ].join("\n"),
    ],
    { stdin: "pipe" },
  );
  check(late.stdin !== null, "late output requires stdin");
  const lateWriter = late.stdin.getWriter();
  diagnostic.phase = "late.exit";
  const lateExit = await late.exitCode;
  check(lateExit === 7, "child exit must precede descendant release");
  diagnostic.phase = "late.write";
  await lateWriter.write(encoder.encode("r"));
  diagnostic.phase = "late.close";
  await lateWriter.close();
  diagnostic.phase = "late.output";
  const lateOutput = await late.output();
  check(
    lateOutput.exitCode === 7 &&
      decoder.decode(lateOutput.stdout) === "late-out" &&
      decoder.decode(lateOutput.stderr) === "late-err",
    "descendant output must survive the application child exit",
  );
  diagnostic.phase = "late.exit-repeat";
  check((await late.exitCode) === 7, "late output must preserve exit code");

  diagnostic.phase = "truncated.spawn";
  const truncated = await native.exec(
    [
      "python3",
      "-c",
      [
        "import os,signal,sys",
        "helper = os.getppid()",
        "if os.fork(): os._exit(7)",
        "if sys.stdin.buffer.read() != b'r': os._exit(90)",
        "os.kill(helper,signal.SIGKILL)",
        "os._exit(0)",
      ].join("\n"),
    ],
    { stdin: "pipe" },
  );
  check(truncated.stdin !== null, "truncation requires stdin");
  const truncatedWriter = truncated.stdin.getWriter();
  diagnostic.phase = "truncated.exit";
  const truncatedExit = await truncated.exitCode;
  check(truncatedExit === 7, "truncation must follow a trusted child exit");
  const truncatedOutput = interrupted(truncated.output());
  diagnostic.phase = "truncated.write";
  await truncatedWriter.write(encoder.encode("r"));
  diagnostic.phase = "truncated.close";
  await truncatedWriter.close();
  diagnostic.phase = "truncated.output";
  const outputInterrupted = await truncatedOutput;
  diagnostic.phase = "truncated.exit-repeat";
  check(
    (await truncated.exitCode) === 7,
    "helper death must preserve the trusted child exit code",
  );

  diagnostic.phase = "remove-shell.spawn";
  const removeShell = await native.exec(
    [
      "python3",
      "-c",
      "import os; os.unlink('/bin/sh'); assert not os.path.lexists('/bin/sh')",
    ],
    { user: "0:0", cwd: "/" },
  );
  diagnostic.phase = "remove-shell.output";
  check(
    (await removeShell.output()).exitCode === 0,
    "shell removal must succeed",
  );
  diagnostic.phase = "shellless.spawn";
  const shellless = await native.exec(
    [
      "python3",
      "-c",
      [
        "import os",
        "assert os.geteuid() == 65534 and os.getegid() == 65534 and os.getcwd() == '/'",
        "assert not os.path.lexists('/bin/sh')",
        "try: fd = os.open('/p23-exec-write',os.O_WRONLY|os.O_CREAT|os.O_EXCL,0o600)",
        "except PermissionError: pass",
        "else:",
        " os.close(fd)",
        " os.unlink('/p23-exec-write')",
        " raise AssertionError('working directory is writable')",
        "print('shellless-readonly',end='')",
      ].join("\n"),
    ],
    { user: "65534:65534", cwd: "/" },
  );
  diagnostic.phase = "shellless.output";
  const shelllessOutput = await shellless.output();
  check(
    shelllessOutput.exitCode === 0 &&
      decoder.decode(shelllessOutput.stdout) === "shellless-readonly",
    "exec must work without a shell in a non-writable working directory",
  );

  diagnostic.phase = "owner-loss.spawn";
  const active = await native.exec(
    ["python3", "-c", "import sys; sys.stdin.buffer.read()"],
    { stdin: "pipe" },
  );
  check(active.stdin !== null, "active exec requires stdin");
  const activeWriter = active.stdin.getWriter();
  const ownerOutput = interrupted(active.output());
  diagnostic.phase = "owner-loss.destroy";
  await native.destroy();
  diagnostic.phase = "owner-loss.output";
  const ownerInterrupted = await ownerOutput;
  activeWriter.releaseLock();
  check(!native.running, "destroy must stop the active execution owner");
  return Response.json({
    pidMatches: true,
    late: { stdout: "late-out", stderr: "late-err", exitCode: lateExit },
    truncated: { outputInterrupted, exitCode: truncatedExit },
    shellless: {
      stdout: "shellless-readonly",
      exitCode: shelllessOutput.exitCode,
    },
    ownerLoss: { outputInterrupted: ownerInterrupted, running: native.running },
  });
}

export async function execLifecycle(
  native: NativeContainer,
): Promise<Response> {
  let deadline: ReturnType<typeof setTimeout> | null = null;
  const diagnostic: Diagnostic = { phase: "initial" };
  let originalFailure: Failure | undefined;
  let response: Response;
  try {
    response = await Promise.race([
      exercise(native, diagnostic),
      new Promise<never>((_resolve, reject) => {
        deadline = setTimeout(
          () => reject(new Error("exec lifecycle did not complete")),
          60000,
        );
      }),
    ]);
  } catch (error) {
    originalFailure = failure(diagnostic.phase, error);
    response = failedResponse(originalFailure);
  } finally {
    clearTimeout(deadline);
  }
  try {
    await native.destroy();
  } catch (error) {
    const cleanup = failure("cleanup.destroy", error);
    return failedResponse(
      originalFailure ?? cleanup,
      originalFailure ? cleanup : undefined,
    );
  }
  return response;
}
