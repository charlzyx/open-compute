import { createHash } from "node:crypto";
import { lstatSync, readFileSync } from "node:fs";
import { isAbsolute } from "node:path";

export function preparedImage(
  inputPath: string | undefined,
  index: 0 | 1 = 0,
): string {
  if (inputPath === undefined || !isAbsolute(inputPath))
    throw new Error("Containers build requires the prepared P23 input JSON");
  const inputStat = lstatSync(inputPath);
  if (
    !inputStat.isFile() ||
    inputStat.isSymbolicLink() ||
    inputStat.size > 65536
  )
    throw new Error("Containers input JSON must be a bounded regular file");
  const input: unknown = JSON.parse(readFileSync(inputPath, "utf8"));
  if (
    typeof input !== "object" ||
    input === null ||
    !("images" in input) ||
    !Array.isArray(input.images) ||
    input.images.length !== 2
  )
    throw new Error("Containers input must contain prepared OCI images");
  const first: unknown = input.images[index];
  if (
    typeof first !== "object" ||
    first === null ||
    !("manifest" in first) ||
    typeof first.manifest !== "string" ||
    !isAbsolute(first.manifest)
  )
    throw new Error("Containers input must name an absolute OCI manifest");
  const stat = lstatSync(first.manifest);
  if (!stat.isFile() || stat.isSymbolicLink() || stat.size > 1048576)
    throw new Error("Containers manifest must be a bounded regular file");
  const digest = createHash("sha256")
    .update(readFileSync(first.manifest))
    .digest("hex");
  return `p23-images.test/containers-fixture@sha256:${digest}`;
}
