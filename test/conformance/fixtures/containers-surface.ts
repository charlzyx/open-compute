/** Compile the complete fixed stable Container declarations without a local API facade. */
export async function containersSurface(
  ctx: DurableObjectState,
  options: ContainerStartupOptions,
  execOptions: ContainerExecOptions,
  binding: Fetcher,
  signal: number,
  port: number,
) {
  const container = ctx.container;
  if (container === undefined) throw new Error("Container capability absent");
  const running: boolean = container.running;
  container.start(options);
  const monitor: Promise<void> = container.monitor();
  container.signal(signal);
  const tcpPort: Fetcher = container.getTcpPort(port);
  const process: ExecProcess = await container.exec(["/bin/true"], execOptions);
  const pid: number = process.pid;
  const isPty: boolean = process.isPty;
  const stdin: WritableStream | null = process.stdin;
  const stdout: ReadableStream | null = process.stdout;
  const stderr: ReadableStream | null = process.stderr;
  const exitCode: Promise<number> = process.exitCode;
  process.kill(signal);
  process.resize(80, 24);
  const output: ExecOutput = await process.output();
  const images: Record<string, string> = container.images;
  const info: ContainerInfo | null = await container.inspect();
  const containerSnapshot: ContainerSnapshot =
    await container.snapshotContainer({ name: "snapshot" });
  await container.setInactivityTimeout(1000n);
  await container.interceptOutboundHttp("example.com", binding);
  await container.interceptAllOutboundHttp(binding);
  await container.interceptOutboundHttps("example.com", binding);
  await container.destroy("requested termination");
  return {
    running,
    monitor,
    tcpPort,
    pid,
    isPty,
    stdin,
    stdout,
    stderr,
    exitCode,
    output,
    images,
    info,
    containerSnapshot,
  };
}
