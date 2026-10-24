# glibc 2.28 qualification

This pipeline builds Linux x64 workerd from the source revision in
`packages/runtime/workerd.lock.json`. The compatibility build uses system malloc
and the pinned Rust/Tokio I/O implementation. It does not change the native source.

The qualification checks ELF symbol versions and dependencies, the runtime
version, the private Container protocol, the compatibility catalog, allocator
symbols, and a real Worker in CentOS 8.2 userspace. The ocd workflow also runs
conformance checks, the complete runtime and storage library suites, and the
Worker, KV, assets-only Cron and restart smoke test.

CentOS containers share the GitHub runner's modern Linux kernel. These checks do
not qualify an old CentOS kernel, the complete Container product, workspace
coverage, or every release platform. Artifact creation does not publish a Release
or deploy a server.

## Runtime identity and retention

`workerd-glibc228.lock.json` keeps two identities for a retained artifact:

- `compatibilityWorkflowRun`, `compatibilityArtifactId`, and
  `compatibilityProducerCommit` identify the current artifact transport.
- `compatibilityBuildWorkflowRun`, `compatibilityBuildArtifactId`, and
  `compatibilityBuildProducerCommit` identify the original native build.
- `compatibilityBuildProvenanceSha256` fixes the original build proof stored in
  `build-provenance.json` at the artifact root.

The retention workflow first verifies the current artifact against live GitHub
metadata. It then copies the verified files without changing the workerd binary
or its gzip archive. The outer artifact ZIP changes because it includes the build
proof. The verifier checks the current transport online and the original build
through the fixed proof. It does not require expired original metadata to remain
online.

Actions artifacts have finite retention. The retention workflow requests 90 days,
which is the public-repository limit. Read the new artifact's actual `expires_at`
before changing the pin; a repository limit can shorten that period. The source
rebuild procedure below remains necessary if the current artifact expires.

## Retain an artifact before it expires

1. Run `centos8-retain-runtime.yml` on `centos8-compat` in this fork. Use an explicit
   repository and ref when GitHub accepts dispatch for the registered workflow:
   `gh workflow run centos8-retain-runtime.yml --repo charlzyx/open-compute --ref centos8-compat`.
   If GitHub rejects dispatch because the workflow is absent from the default
   branch, an audited push changing this workflow on `centos8-compat` also starts
   the retention job. Do not change `main`, permissions, or the sync schedule to
   bypass that restriction.
2. Wait for the retention run to succeed. Read its artifact ID, producer commit,
   actual expiration, and the SHA-256 of `build-provenance.json`.
3. Update the transport fields and `targets.linux-x64.archiveUrl` in the
   compatibility pin. Copy the original build identity from the verified proof.
   Keep the binary and gzip SHA-256 values unchanged.
4. Run the artifact verifier tests and verify the downloaded retained artifact.
   Refresh the conformance source digest with `sourceIdentity()` from
   `test/conformance/checks/context.ts`.
5. Push the pin update to the validation branch. Wait for `centos8-ocd.yml` to pass
   before merging the update into `centos8-compat`.

A copy is not a new native build. Do not replace the original build identity with
the retention run or regenerate the original proof during a later copy.

## Rebuild from pinned source

1. Use the isolated validation branch, or add a new validation branch to the
   producer and ocd workflow push filters. Keep the formal workerd lock and
   submodule revision consistent.
2. Run `centos8-system-malloc.yml` for that branch in `charlzyx/open-compute`.
   Use an explicit `--repo charlzyx/open-compute` and `--ref` for dispatch. If
   dispatch is unavailable, use the workflow's declared push branches and paths.
   Do not substitute a binary from an older runtime revision.
3. Wait for the native build, ELF checks and CentOS Worker smoke to pass. Download
   the successful artifact and verify its exact source revision, producer commit,
   Dockerfile and build-script digests, protocol, catalog, binary and archive.
4. Derive a new compatibility pin from the current formal metadata and measured
   artifact values. For a fresh native build, omit the four retained-build fields
   listed above. Keep `compatibilityBuildScriptSha256`; it binds the actual build
   script and is not a retention field.
5. Retain the verified native artifact and record its original build proof. Update
   the transport pin, refresh the conformance digest, and rerun the complete ocd
   compatibility workflow before merging.

Do not treat a source rebuild as byte-identical without checking its hashes. A
changed hash alone is not qualification evidence. Missing or expired inputs fail
closed; the packaging workflow does not silently substitute another runtime.
