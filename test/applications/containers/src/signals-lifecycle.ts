type NativeContainer = NonNullable<DurableObjectState["container"]>;

function check(condition: boolean, message: string): asserts condition {
  if (!condition) throw new Error(message);
}

const mainScript = [
  "import os,signal,socket,sys",
  "signo = int(sys.argv[1])",
  "code = int(sys.argv[2])",
  "signal.signal(signo,lambda *_: os._exit(code))",
  "server = socket.socket(socket.AF_UNIX)",
  "server.bind('/tmp/p23-signal-ready')",
  "server.listen(1)",
  "while True:",
  " connection,_ = server.accept()",
  " with connection:",
  "  with connection.makefile('rb') as stream:",
  "   assert stream.readline(7) == b'ready\\n'",
  "  connection.sendall(b'ready\\n')",
].join("\n");

async function ready(native: NativeContainer): Promise<void> {
  const probe = await native.exec([
    "python3",
    "-c",
    [
      "import socket,time",
      "deadline = time.monotonic()+30",
      "while True:",
      " connection = socket.socket(socket.AF_UNIX)",
      " connection.settimeout(5)",
      " try: connection.connect('/tmp/p23-signal-ready'); break",
      " except (FileNotFoundError,ConnectionRefusedError):",
      "  connection.close()",
      "  if time.monotonic() >= deadline: raise",
      "  time.sleep(0.01)",
      "connection.sendall(b'ready\\n')",
      "with connection.makefile('rb') as stream:",
      " assert stream.readline(7) == b'ready\\n'",
      "connection.close()",
      "print('ready',end='')",
    ].join("\n"),
  ]);
  const output = await probe.output();
  check(
    output.exitCode === 0 &&
      output.stderr.byteLength === 0 &&
      new TextDecoder().decode(output.stdout) === "ready",
    "main signal handler must acknowledge readiness",
  );
}

async function startMain(
  native: NativeContainer,
  image: string,
  signo: number,
  exitCode: number,
  requireActive: () => void,
): Promise<void> {
  requireActive();
  check(!native.running, "signal lifecycle must start without a running main");
  native.start({
    image,
    enableInternet: false,
    entrypoint: ["python3", "-c", mainScript, String(signo), String(exitCode)],
  });
  await ready(native);
  requireActive();
  check(native.running, "main must be running after its readiness response");
}

async function signalledMain(
  native: NativeContainer,
  image: string,
  signo: number,
  exitCode: number,
  requireActive: () => void,
): Promise<number> {
  await startMain(native, image, signo, exitCode, requireActive);
  const monitored = native.monitor().then(
    () => {
      check(exitCode === 0, "nonzero monitor must reject");
      return { code: 0 };
    },
    (error: unknown) => {
      check(exitCode !== 0, "zero exit monitor must resolve");
      check(error instanceof Error, "nonzero monitor must reject an Error");
      const code: unknown = Reflect.get(error, "exitCode");
      check(code === exitCode, "monitor must preserve the main exit code");
      return { code: exitCode };
    },
  );
  native.signal(signo);
  const result = await monitored;
  check(result.code === exitCode, "signal must reach the prepared main trap");
  check(!native.running, "monitor settlement must clear running");
  await native.destroy();
  return result.code;
}

async function exercise(
  native: NativeContainer,
  image: string,
  requireActive: () => void,
): Promise<Response> {
  const mainSignalExitCode = await signalledMain(
    native,
    image,
    10,
    23,
    requireActive,
  );
  const mainRealtimeSignalExitCode = await signalledMain(
    native,
    image,
    34,
    23,
    requireActive,
  );
  const monitorZero =
    (await signalledMain(native, image, 10, 0, requireActive)) === 0;
  const monitorNonzeroExitCode = await signalledMain(
    native,
    image,
    10,
    7,
    requireActive,
  );

  await startMain(native, image, 10, 23, requireActive);
  const reason = new Error("requested signal lifecycle termination");
  const forcedMonitor = native.monitor().then(
    () => false,
    (error: unknown) => error === reason,
  );
  await native.destroy(reason);
  const destroyReasonIdentity = await forcedMonitor;
  check(
    destroyReasonIdentity,
    "destroy must preserve the exact monitor reason",
  );
  check(!native.running, "destroy must clear running");

  await startMain(native, image, 10, 23, requireActive);
  const process = await native.exec(
    [
      "python3",
      "-c",
      "import sys; print('ready',flush=True); sys.stdin.buffer.read()",
    ],
    { stdin: "pipe" },
  );
  check(
    !process.isPty &&
      process.stdin !== null &&
      process.stdout !== null &&
      process.stderr !== null,
    "signal target requires separate process streams",
  );
  const reader = process.stdout.getReader();
  const writer = process.stdin.getWriter();
  const stderr = new Response(process.stderr).text();
  let stdout = "";
  let execKillExitCode: number;
  try {
    const decoder = new TextDecoder();
    while (!stdout.endsWith("\n")) {
      const chunk = await reader.read();
      check(!chunk.done, "signal target must report readiness before exiting");
      stdout += decoder.decode(chunk.value, { stream: true });
      check(stdout.length <= 6, "signal target readiness must be bounded");
    }
    check(stdout === "ready\n", "signal target readiness must match");
    process.kill(15);
    execKillExitCode = await process.exitCode;
    check(
      execKillExitCode === 143,
      "SIGTERM must terminate the actual exec child",
    );
    check((await reader.read()).done, "killed child stdout must reach EOF");
    check((await stderr) === "", "killed child stderr must reach empty EOF");
    check(native.running, "exec kill must preserve the main process");
    await ready(native);
  } finally {
    writer.releaseLock();
    reader.releaseLock();
  }
  return Response.json({
    mainSignalExitCode,
    mainRealtimeSignalExitCode,
    monitorZero,
    monitorNonzeroExitCode,
    destroyReasonIdentity,
    execKillExitCode,
    execKillStdout: stdout,
    execKillStderr: "",
  });
}

export async function signalsLifecycle(
  native: NativeContainer,
  image: string,
): Promise<Response> {
  let deadline: ReturnType<typeof setTimeout> | null = null;
  let active = true;
  const requireActive = () =>
    check(active, "signal lifecycle cannot continue after its deadline");
  try {
    return await Promise.race([
      exercise(native, image, requireActive),
      new Promise<never>((_resolve, reject) => {
        deadline = setTimeout(() => {
          active = false;
          reject(new Error("signal lifecycle did not complete"));
        }, 60000);
      }),
    ]);
  } finally {
    active = false;
    clearTimeout(deadline);
    await native.destroy();
  }
}
