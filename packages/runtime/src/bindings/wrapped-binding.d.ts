/** Internal native base used only by config-owned wrapped-binding extensions. */
declare module "cloudflare-internal:wrapped-binding" {
  export interface PublicTransport {
    fetch(request: Request): Promise<Response>;
    connect(address: SocketAddress | string, options?: SocketOptions): Socket;
  }
  class WrappedBinding {
    constructor(fetcher: unknown);
  }
  const binding: {
    WrappedBinding: typeof WrappedBinding;
    createPrivateTransport(transport: unknown): object;
    validatePublicTransport(transport: unknown): void;
    fetchPublicTransport(
      transport: unknown,
      request: Request,
    ): Promise<Response>;
    connectPublicTransport(
      transport: unknown,
      address: SocketAddress | string,
      options?: SocketOptions,
    ): Socket;
    rpcTransportKind(
      value: unknown,
    ): "" | "fetcher" | "stub" | "promise" | "property";
    getRpcTransportMember(
      receiver: object,
      name: string,
      resultKind: "value" | "stub" | "stream" | "data",
    ): unknown;
    thenRpcTransport(
      receiver: object,
      fulfilled: (value: unknown) => unknown,
      rejected?: (error: unknown) => unknown,
    ): unknown;
    disposeRpcTransport(receiver: object): void;
    duplicateRpcTransport(
      receiver: object,
      exposure: "private" | "value",
    ): object;
    admitSubrequest(): void;
    createServiceRpcStub(policy: object): object;
    isRpcStub(value: unknown): boolean;
    createDurableObjectId(
      value: string,
      name?: string,
      jurisdiction?: string,
    ): DurableObjectId;
    createDurableObjectNamespace(policy: object): DurableObjectNamespace;
    createDurableObjectStub(
      id: DurableObjectId,
      transport: object,
      policy: object,
    ): DurableObjectStub;
    installQueuePolicy(queue: object, policy: object): void;
    decodeQueueV8(bytes: Uint8Array): unknown;
  };
  export default binding;
}
