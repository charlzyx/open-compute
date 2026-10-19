function check(condition: boolean, message: string): asserts condition {
  if (!condition) throw new Error(message);
}

async function beforeDeadline<T>(
  operation: Promise<T>,
  deadline: number,
  phase: string,
): Promise<T> {
  let timer: ReturnType<typeof setTimeout> | null = null;
  try {
    return await Promise.race([
      operation,
      new Promise<never>((_, reject) => {
        timer = setTimeout(
          () => reject(new Error(`PTY probe timed out: ${phase}`)),
          Math.max(0, deadline - Date.now()),
        );
      }),
    ]);
  } finally {
    clearTimeout(timer);
  }
}

const ptyScript = String.raw`
stty -echo || exit 90
resized=0
trap 'resized=1' WINCH
printf 'pid:%s\ninitial:' "$$"
stty size || exit 91
printf 'ready\n'
while :; do
  if [ "$resized" = 1 ]; then
    resized=0
    printf 'winch:'
    stty size || exit 92
  fi
  if IFS= read -r -t 0.05 line; then
    if [ "$line" = finish ]; then
      printf 'stdout-finished\n'
      printf 'stderr-finished\n' >&2
      exit 7
    fi
  fi
done
`;

export async function execPtyProbe(container: Container) {
  check(container.running, "PTY probe requires a running container");
  const deadline = Date.now() + 20_000;
  const cancellation = new AbortController();
  let process: ExecProcess | undefined;
  let completed = false;
  let reader: ReadableStreamDefaultReader | undefined;
  let writer: WritableStreamDefaultWriter | undefined;
  try {
    process = await beforeDeadline(
      container.exec(["/bin/bash", "-c", ptyScript], {
        pty: { cols: 80, rows: 24 },
        stdin: "pipe",
        stdout: "pipe",
        stderr: "combined",
        signal: cancellation.signal,
      }),
      deadline,
      "exec",
    );
    const stderr: unknown = Reflect.get(process, "stderr");
    const stderrKind =
      stderr === null
        ? "null"
        : stderr === undefined
          ? "undefined"
          : stderr instanceof ReadableStream
            ? "stream"
            : "other";
    check(process.isPty && process.pid > 0, "PTY identity is missing");
    check(process.stdout !== null, "PTY stdout is missing");
    check(process.stdin !== null, "PTY stdin is missing");
    reader = process.stdout.getReader();
    writer = process.stdin.getWriter();
    const decoder = new TextDecoder();
    let stdout = "";
    let bytes = 0;
    let eof = false;
    const read = async () => {
      check(reader !== undefined, "PTY reader is missing");
      const chunk = await beforeDeadline(reader.read(), deadline, "stdout");
      if (chunk.done) {
        stdout += decoder.decode();
        eof = true;
        return;
      }
      check(chunk.value instanceof Uint8Array, "Invalid PTY output chunk");
      bytes += chunk.value.byteLength;
      check(bytes <= 4096, "PTY output exceeded its limit");
      stdout += decoder.decode(chunk.value, { stream: true });
    };
    while (!stdout.includes("ready\r\n") && !stdout.includes("ready\n")) {
      await read();
      check(!eof, "PTY exited before readiness");
    }
    const initial = stdout.replaceAll("\r\n", "\n");
    const pid = /^pid:([0-9]+)$/m.exec(initial);
    check(pid !== null && Number(pid[1]) === process.pid, "PTY PID mismatch");
    check(initial.includes("initial:24 80\n"), "Initial PTY geometry mismatch");
    process.resize(100, 30);
    while (!stdout.replaceAll("\r\n", "\n").includes("winch:30 100\n")) {
      await read();
      check(!eof, "PTY exited before SIGWINCH geometry proof");
    }
    await beforeDeadline(
      writer.write(new TextEncoder().encode("finish\n")),
      deadline,
      "stdin-write",
    );
    await beforeDeadline(writer.close(), deadline, "stdin-finish");
    while (!eof) await read();
    const exitCode = await beforeDeadline(process.exitCode, deadline, "exit");
    const merged = stdout.replaceAll("\r\n", "\n");
    check(
      merged.includes("stdout-finished\nstderr-finished\n"),
      "PTY stdout and stderr were not merged",
    );
    check(exitCode === 7, "PTY exit status mismatch");
    completed = true;
    return {
      pid: process.pid,
      shellPid: Number(pid[1]),
      isPty: process.isPty,
      stderrKind,
      initialGeometry: { cols: 80, rows: 24 },
      resizedGeometry: { cols: 100, rows: 30 },
      sigwinchObserved: true,
      stdinFinished: true,
      stdout,
      bytes,
      exitCode,
      eof,
    };
  } finally {
    if (!completed) {
      try {
        process?.kill(9);
      } finally {
        cancellation.abort();
        await beforeDeadline(
          Promise.allSettled([reader?.cancel(), writer?.abort()]),
          Date.now() + 1000,
          "cleanup",
        ).catch(() => {});
      }
    }
    reader?.releaseLock();
    writer?.releaseLock();
  }
}
