import { Container, getContainer, switchPort } from "@cloudflare/containers";

/** Official package subclass; deployment/runtime evidence is separate from this compile fixture. */
export class PackageContainer extends Container {
  override defaultPort = 8080;
  override sleepAfter = "5m";
  override envVars = { MODE: "fixture" };
  override entrypoint = ["/app/server"];
  override enableInternet = true;
}

export async function officialContainerPackage(
  ctx: ConstructorParameters<typeof Container>[0],
  env: Cloudflare.Env,
  binding: DurableObjectNamespace<PackageContainer>,
  request: Request,
) {
  const container = new PackageContainer(ctx, env);
  await container.startAndWaitForPorts({
    ports: [8080],
    startOptions: { envVars: { MODE: "fixture" }, enableInternet: true },
    cancellationOptions: {
      abort: new AbortController().signal,
      portReadyTimeoutMS: 2000,
    },
  });
  const response: Response = await container.fetch(request);
  const routed: Response = await getContainer(binding, "instance").fetch(
    switchPort(request, 8080),
  );
  await container.stop("SIGTERM");
  await container.destroy();
  return { response, routed };
}
