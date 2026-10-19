# AGENTS – open-compute

This file applies repository-wide. Nested `AGENTS.md` files add rules for their directories. Read linked references only when the task touches their subject; historical plans and reports are evidence, not new instructions.

## Operating Contract

- Answer, review, diagnosis and planning requests call for evidence and a report. Change, build and fix requests authorize the in-scope local edits and non-destructive checks needed to finish the work; do not stop at a plan or ask again for those actions.
- Use reasonable assumptions for routine choices. Ask only when missing information changes the result, authority or scope; continue independent authorized work while waiting. Later corrections steer the current task unless the user cancels it.
- External writes, destructive actions, privileged commands, runtime downloads, release packaging and material scope expansion need explicit authorization. Reuse authorization already given for that action and scope. Prepare the reviewable local result before requesting any remaining approval; available credentials alone do not authorize mutations.
- User instructions take precedence over skill guidelines. If a repository or skill instruction blocks work, link the exact file, quote the rule and explain the conflict; do not turn a recommendation into an approval requirement.
- For research/validation, inspect the root `.env` and authenticated CLI sessions before declaring credentials unavailable. Load only required variables; never print, log, persist or pass unrelated secrets. Cloudflare research uses the repository-pinned cf under Node.js 22.18+, with `cf auth whoami` and explicit target-account verification; OAuth can satisfy authentication without `CLOUDFLARE_API_TOKEN`.
- Preserve unrelated user changes. Do not edit the parent Lynx OS project, reset local data, rewrite Git history or clean unrelated files without explicit scope. For a request to synchronize and push `main`, merge remote `main` and push without force or tag changes; report branch-protection failures rather than inventing another branch/PR. Do not create `codex/` branches.
- Conversational replies are Chinese, concise and result-first: state what changed or was found, evidence, and material limitations. Omit optional commentary. Keep commit messages and GitHub-facing text in English; preserve surrounding document language.
- For coding and design tasks, proactively read and apply `ponytail:ponytail` in full mode unless the user selects another level. Choose the smallest complete solution. Reuse existing code and remove obsolete code. Do not add speculative abstractions, options, dependencies or features. Preserve security, data integrity and required validation.
- Before presenting plans or writing/editing documentation, read and apply `hai-simplified-technical` from the installed skills directory. Use each document's language rules and project terminology. Run its `scripts/check.py` on changed documents and review each finding.

## Day1 Architecture

- **Design for current requirements with one authoritative implementation.** Update affected producers, consumers, schemas, fixtures, tests, build paths and docs together; remove superseded code. Earlier open-compute APIs, configs, development data, snapshots and byte formats do not justify aliases, defaults, backfills, dual paths or compatibility shims.
- The compatibility exception is observable official Cloudflare behavior within declared support. Document the source, exact behavior, date/flag or version range, relevant pinned workerd constraints and regression coverage before retaining a branch.
- Platform SQL migrations included in a published release are immutable: preserve filenames, order and bytes; append the next contiguous migration. Unreleased definitions may be consolidated. This differs from user D1 migrations. Pre-1.0 model breaks must be explicit and fail closed; they do not permit rewriting released migrations or silently repairing data.
- Optimize for a self-hosted single machine serving small and medium businesses. Add abstractions only for ownership, real duplication, security or a material reduction in complexity. Defer low-impact long-tail gaps with their scope, impact and evidence; do not weaken declared contracts, security, integrity, required Gates or reproducible common-path behavior.

## Repository and Ownership

- The canonical origin is `https://open-compute.dev`; service identifiers use `dev.open-compute`.
- Production sources/tooling/assets belong in `apps/`, `crates/`, `packages/`, `share/`; repository Gates, fixtures and fuzz tooling in `test/`; operator/release surfaces in `scripts/` and `examples/`. Keep local tests beside their owning app/crate/package.
- Put repository-local disposable caches, logs and run evidence under `.temp/<purpose>/` using the existing ignore rule. Keep standard outputs in `target/`, `node_modules/` and generated `dist/`; `.data/` is persistent development data. Do not hand-edit generated output. Cleanup and storage budgets are defined in [testing](docs/references/testing.md#验证磁盘预算).
- One `ocd` owns the selected OCD scope, shared listeners/Gateway and scope lock. Each registered instance owns its data-dir lock, SQLite authority, master key, object authority/cache and supervised pinned workerd child.

| Crate | Ownership |
| --- | --- |
| `core` | Config, errors, IDs, secrets, health, clocks |
| `search` | Pure search contracts, filtering, chunking, ranking |
| `images` | Bounded image decode, transform, encode |
| `document-parser` | Format detection/extraction; bounded preprocessing through `images` |
| `storage` | Data directory, locks, SQLite, migrations, identity, secret crypto |
| `artifacts` | S3 artifact storage, preflight, verified cache |
| `runtime` | workerd verification/configuration, process ownership, supervision |
| `workers` | Immutable bundles/deployments, routing pins, runtime-source snapshots |
| `service` | CLI and production control/data-plane composition |

Follow the DAG enforced by `test/check-boundaries.sh`: `workers` may use `core`, `storage`, `artifacts`, but not `runtime`; `service` is the composition root. Keep HTTP/CLI handlers thin. Normalize and validate once at the authority boundary, pass structured values onward, and keep workflows in their owning crate. Verify assumptions against source, tests or upstream evidence; never repair untrusted or persisted values during reads.

## Source Rules

- Rust: MSRV 1.98, edition 2024, workspace lints, `#![deny(missing_docs)]`, `unsafe_code = "forbid"`. Public APIs need useful English rustdoc; identifiers, code comments, error codes and log fields are English.
- Reuse canonical types and explicit ownership. Propagate errors with `?` and stable `PlatformError`; transform at semantic, cleanup, process or external-response boundaries. Avoid `unwrap`/`expect` outside tests or compile-time invariants.
- Production Rust files/functions are limited to 800/300 lines; test files/functions to 2000/800. Split by ownership before crossing limits; avoid growing existing oversized files.
- Root `Cargo.toml` owns shared versions/lints; each crate declares its imports, normally with `workspace = true`. Default features stay empty unless production requires otherwise; preserve no-default-features builds. Commit intentional `Cargo.lock` updates generated by the tool, never by hand.
- Use static imports. Dynamic assembly is confined to the data-driven tenant bundle/`workerLoader` boundary. Test hooks use `#[cfg(any(test, feature = "test-support"))]`.
- Maintained functional JS-family code is `.ts`/`.tsx`, checked by pinned TypeScript 7 in strict mode. JS is limited to tests, fixtures, mocks, disposable investigation evidence and reproducibly generated output. Do not hide functional code in excluded paths.
- Use the root Bun workspace, catalog, `workspace:*` and one `bun.lock`; do not introduce other package-manager lockfiles. No `any`, suppression directives, unchecked double assertions, catch-all ambient modules or relaxed compiler flags; validate `unknown` at untrusted boundaries. Public types must reflect supported capabilities and exclude internal credentials.
- Runtime TS and behavioral tests live by domain in `packages/runtime/`; protocol types stay with their domain, genuinely shared binding types stay shared, and generated assets preserve that layout. Official cf/Vite plugin v2 build applications; Rolldown builds system assets. Application CI must typecheck and build before deployment; the daemon only validates uploaded modules.
- `packages/runtime/dist/` is untracked. CI builds from source, verifies manifest consistency/reproducibility and prepares assets before Rust consumes them. Compilation and dependency installation belong to build tooling, never startup or request handling.

## Runtime and Supply Chain

- Keep the workerd fork minimal: identify the upstream gap before adding a native capability or host interface. Platform policy, identity, routing and persistence stay in open-compute. On upstream updates, replace covered patches and redundant glue with the upstream implementation while preserving behavior and regressions.
- Native sources live in the independent `third_party/workerd/` (`elliothux/workerd`) and `third_party/caddy/` (`elliothux/open-compute-caddy`) submodules. Follow their build rules, commit source there and record the gitlink here. Preserve their local state. A gitlink update is not a formal binary pin update; see [workerd maintenance](docs/workerd/README.md).
- The only runtime pins are `packages/runtime/workerd.lock.json` and `packages/runtime/caddy.lock.json`. Explicit preparation verifies the selected immutable Release asset, target, archive/binary digests, version, compatibility metadata, Caddy module and process flags before embedding. Never commit native binaries or substitute an unverified development binary.
- Pin updates coordinate locks, assets, consumers, fresh compatibility evidence, affected real-runtime Gates and supported-host package validation. Record fork revision, upstream base, build inputs and digests; historical G0 or stock-workerd results do not qualify a new pin.
- Production startup is offline: no fetching, auto-upgrade, PATH search or JS toolchain. Compile checked-in Cap'n Proto with verified workerd; never interpolate tenant input into source. Build preparation and single-file distribution are defined in [single-binary](docs/references/single-binary.md).
- Internal listeners are loopback-only and capability-scoped. Generation tokens stay out of argv, environment, logs, status, metrics, errors and tenant responses, and become invalid after restart.
- `ocd` owns readiness, process groups, bounded output capture, graceful/forced stop, reaping, backoff and secret-free orphan recovery. Validate process start identity and binary digest before signalling a PID. Readiness requires control-fd listen evidence and an HTTP probe; `/health/ready` is admission state, not a restart signal; `/health/live` is liveness.

## Persistence and Worker Security

- `control.sqlite` owns platform/Worker/deployment/route/secret metadata; memory and loader caches are disposable. SQL migrations are contiguous, checksummed and transactional. Update checksum wiring, versions/dispatch, invariants and fault/restart tests with schema changes; reject unsupported/corrupt state and downgrade attempts.
- Keep SQLite foreign keys enabled and transaction callbacks synchronous; do filesystem, S3, process and other async I/O outside transactions. Preserve locks, atomic writes, fsync, permissions, symlink checks and path containment.
- Named Unix sockets derive from `<OCD_DIR>/run/`, distinguished by InstanceId. Reject non-absolute paths and paths over 103 encoded bytes before bind/connect/config rendering. Tests use bounded roots and retain a long-path regression.
- Artifacts and ready deployments are immutable and content-addressed; verify digests before cache admission/execution. Promotion/rollback changes a pointer. Secrets are validated env/file references or ciphertext using the existing AEAD context; never expose plaintext through GET APIs, artifacts, caches, diagnostics, logs, metrics, argv or errors.
- `ocd` is the sole public listener and creates trusted request/deployment identity, stripping or overwriting conflicting external headers. Tenant loader keys identify immutable deployments; resolve modules, vars, secrets, compatibility metadata and bindings from persisted authority, not request scope or host-memory registries.
- Tenant `env` contains only declared vars, secrets and supported bindings; never expose RuntimeSource, S3, SQLite, internal fetchers/tokens or control services. Sanitize tenant/control errors and platform logs: no raw upstream exceptions, loader keys, paths, module source, authorization/cookies, signed URLs, ciphertext or internal topology.
- Tenant outbound shares one `Network(allow = ["network", "local"], deny = ["unix", "unix-abstract"])` IP capability across `fetch`, sockets, `node:net` and `node:tls`. It permits host-allowed public/private/loopback/link-local/metadata IP access; Unix sockets remain denied. Operators own host firewall/network isolation; no per-Worker/instance isolation is claimed. Undelegated paths keep `globalOutbound = null`; declared Service/DO `Fetcher.connect()` is a separate tunnel. Every platform listener authenticates independently of source address. Do not add pre-resolution checks, JS proxies, another outbound gateway, port ACLs or automatic firewall management.
- Client disconnect does not guarantee cancellation. Preserve product cancellation, stream interruption and recovery coverage without restoring the retired G0 probe.

## Validation

Follow [testing](docs/references/testing.md) for scope, commands, discovery, disk budgets and evidence. These rules are completion criteria, not a request to run every check for every edit:

- Documentation/policy-only edits: `git diff --check`, documentation checks, and verify changed commands, links and generated-file claims. No Rust build or Gate. JavaScript-only changes use the owning package checks; production Rust/runtime changes retain the full static, coverage and final acceptance requirements in the reference.
- Use globally installed `mbx` for repository Rust commands, with its shared user configuration/store; no direct Cargo fallback, project wrapper or second compiler cache. Disable build-script execution caching, target views and hardlink restoration. Prepare formal host runtime inputs explicitly before `bun run build`; real-runtime tests require verified `OPEN_COMPUTE_TEST_WORKERD`.
- Inspect `./test/gate.py <targets> --list`, then run affected targets once with `--keep-going`. Collect the complete diagnostics, fix as a batch, and rerun only failed/directly affected targets. Apply the same batch rule to canonical Clippy. No intermediate workspace Gates or unchanged coverage/static reruns.
- After implementation, review, fixes, static checks and coverage, freeze source and run `./test/gate.py --workspace --final` once. Coverage must be at least 90.00% and cannot substitute for this uninstrumented Gate. Reuse evidence only for verified identical measured sources/configuration/inputs; diagnose failed finals before another attempt. The runner rejects duplicate full final inputs. Repetition, soak/load/fuzz and release packaging require their separate scope.
- Test public behavior and invariants. Behavior changes need focused success/failure coverage; security, persistence, protocol and process lifecycle include relevant restart/crash regressions. Do not add tests that merely mirror trivial reversible edits. Missing/mismatched workerd, ignored cases, incomplete discovery/counts or leaked processes/listeners/files/secrets are failures, never skips. Required Gates use real processes, SQLite and documented SigV4/network fixtures with the formal pin.
- **Anti-cheating is a repository invariant.** Never place case IDs, fixture identities/URLs, expected counts/outcomes or fault branches in production sources, config, prompts or operator scripts to satisfy tests. Production behavior derives from validated input and persisted authority. Keep fakes and scenario data in tests/test-support; never hide production logic in coverage exclusions, add coverage-only branches, weaken assertions or replace required runtime paths with mocks/Miniflare.
- Completed upstream POCs are disposable; migrate still-uncovered current regressions into maintained tests and remove duplicates during scoped cleanup. Record whether each removal is an obsolete model, completed investigation or equivalent coverage. Preserve `docs/implemented/g0-results.md` exactly; historical counts do not define current requirements.
- Report a check as passing only after successful exit. When blocked, state the exact missing input/reason and available evidence; never claim full acceptance from a focused check.

## Task References

| When the task concerns | Read |
| --- | --- |
| Tests, coverage, build caches, diagnostics | [Testing](docs/references/testing.md) |
| Runtime input preparation, offline packaging, deployment layout | [Single binary](docs/references/single-binary.md) |
| Versioning, release qualification, publishing or recovery | [Releasing](docs/references/releasing.md) |
| CI topology or build performance | [CI build performance](docs/references/ci-build-performance.md) |
| Cloudflare API support or upstream refresh | [Compatibility](docs/references/cloudflare-compatibility.md), [upstream refresh](docs/references/cloudflare-upstream-refresh.md) |
| Native fork/source updates | [workerd](docs/workerd/README.md) |
| Writing, moving or closing documentation | [Documentation lifecycle](docs/references/README.md) |

Before handoff, check the diff for scope, direct ownership, preserved security/data boundaries and unnecessary abstractions or fallback paths. Documentation maintenance rules live in the linked reference rather than being repeated here.
