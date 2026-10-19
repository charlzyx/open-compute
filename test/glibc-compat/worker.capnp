using Workerd = import "/workerd/workerd.capnp";

const config :Workerd.Config = (
  services = [
    (name = "main", worker = (
      modules = [(name = "worker", esModule = "export default { fetch() { return new Response('glibc228-worker-ok'); } }; ")],
      compatibilityDate = "2026-09-08",
    )),
  ],
  sockets = [(name = "http", address = "127.0.0.1:8080", http = (), service = "main")],
);
