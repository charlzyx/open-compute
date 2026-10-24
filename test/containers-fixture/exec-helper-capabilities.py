#!/usr/bin/env python3
"""Exercise the credential boundary in an owned scratch container without host root or downloads."""
from pathlib import Path
import hashlib
import io
import json
import os
import re
import shutil
import stat
import subprocess
import sys
import tarfile
import tempfile
import uuid


def digest(path):
    with path.open("rb") as source:
        return hashlib.file_digest(source, "sha256").hexdigest()


def identity(info):
    return tuple(getattr(info, name) for name in (
        "st_dev", "st_ino", "st_uid", "st_gid", "st_mode", "st_nlink",
        "st_size", "st_mtime_ns", "st_ctime_ns",
    ))


def docker(*arguments, body=None, required=True):
    result = subprocess.run(
        ["docker", *arguments], input=body, capture_output=True, timeout=45
    )
    if len(result.stdout) > 1 << 20 or len(result.stderr) > 1 << 20:
        raise RuntimeError("EXEC_CREDENTIAL_OUTPUT_LIMIT")
    if required and result.returncode:
        raise RuntimeError("EXEC_CREDENTIAL_ENGINE_" + arguments[0].upper())
    return result


def main():
    if len(sys.argv) != 3 or os.geteuid() == 0:
        raise RuntimeError("EXEC_CREDENTIAL_ORDINARY_BUILD_USER_REQUIRED")
    binary = Path(sys.argv[1])
    coverage = Path(sys.argv[2]).resolve(strict=True)
    original = binary.lstat()
    if not binary.is_absolute() or not stat.S_ISREG(original.st_mode) or not 0 < original.st_size <= 32 << 20 or not original.st_mode & 0o111:
        raise RuntimeError("EXEC_CREDENTIAL_BINARY_REJECTED")
    binary_digest = digest(binary)
    binary_fd = os.open(binary, os.O_RDONLY | os.O_NOFOLLOW | os.O_CLOEXEC)
    if identity(os.fstat(binary_fd)) != identity(original):
        os.close(binary_fd)
        raise RuntimeError("EXEC_CREDENTIAL_BINARY_CHANGED")
    output = Path(tempfile.mkdtemp(prefix="exec-capabilities-", dir=coverage.parent))
    helper_source = output / "helper-source"
    helper_source.mkdir(mode=0o700)
    shutil.copyfile(binary, helper_source / "helper")
    (helper_source / "helper").chmod(0o555)
    helper_source.chmod(0o555)
    profiles = output / "profiles"
    profiles.mkdir(mode=0o777)
    profiles.chmod(0o777)
    token = uuid.uuid4().hex
    marker = "OC_EXEC_ROOT_PROBE=" + token
    label = "dev.open-compute.exec-credential-probe"
    image = container = None
    report = {"success": False, "hostRootUsed": False, "downloads": False}

    def image_identity(expected):
        value = json.loads(docker("image", "inspect", image).stdout)[0]
        markers = [item for item in value["Config"].get("Env", []) if item.startswith("OC_EXEC_ROOT_PROBE=")]
        if value["Id"] != image or markers != [expected]:
            raise RuntimeError("EXEC_CREDENTIAL_IMAGE_IDENTITY")
        return value

    def container_identity(expected):
        value = json.loads(docker("inspect", container).stdout)[0]
        if value["Id"] != container or value["Image"] != image or value["Config"]["Labels"].get(label) != expected:
            raise RuntimeError("EXEC_CREDENTIAL_CONTAINER_IDENTITY")
        return value

    try:
        archive = io.BytesIO()
        with tarfile.open(fileobj=archive, mode="w") as target:
            entry = tarfile.TarInfo("exec-tests")
            entry.mode, entry.size = 0o555, original.st_size
            with os.fdopen(os.dup(binary_fd), "rb") as source:
                target.addfile(entry, source)
        image = docker("image", "import", "--change", "ENV " + marker, "-", body=archive.getvalue()).stdout.decode().strip()
        if not re.fullmatch(r"sha256:[0-9a-f]{64}", image):
            raise RuntimeError("EXEC_CREDENTIAL_IMAGE_RESULT")
        image_identity(marker)
        try:
            image_identity(marker + "-wrong")
        except RuntimeError:
            report["wrongImageMarkerRejected"] = True
        else:
            raise RuntimeError("EXEC_CREDENTIAL_IMAGE_GUARD_FAILED")
        container = docker(
            "create", "--label", label + "=" + token, "--network", "none",
            "--user", "0:0", "--cpus", "0.0625", "--memory", "256m",
            "--memory-swap", "256m", "--pids-limit", "64", "--cap-drop", "NET_RAW",
            "--security-opt", "no-new-privileges:true", "--log-driver", "none",
            "--mount", "type=bind,src=" + str(profiles) + ",dst=/tmp",
            "--mount", "type=bind,src=" + str(helper_source) + ",dst=/.open-compute-exec,readonly",
            "--env", "GOCOVERDIR=/tmp", "--env", "OC_EXEC_TEST_PAYLOAD=root-credentials",
            image, "/.open-compute-exec/helper", "-test.run=^TestExecSupervisorPayload$", "-test.v", "-test.gocoverdir=/tmp",
        ).stdout.decode().strip()
        if not re.fullmatch(r"[0-9a-f]{64}", container):
            raise RuntimeError("EXEC_CREDENTIAL_CONTAINER_RESULT")
        configuration = container_identity(token)
        host = configuration["HostConfig"]
        if host["Privileged"] or host["CapAdd"] or host["NetworkMode"] != "none" or host["CapDrop"] not in (["NET_RAW"], ["CAP_NET_RAW"]):
            raise RuntimeError("EXEC_CREDENTIAL_CONTAINER_POLICY")
        try:
            container_identity(token + "-wrong")
        except RuntimeError:
            report["wrongContainerMarkerRejected"] = True
        else:
            raise RuntimeError("EXEC_CREDENTIAL_CONTAINER_GUARD_FAILED")
        result = docker("start", "--attach", container)
        (output / "unit.log").write_bytes(result.stdout + result.stderr)
        after = container_identity(token)
        if after["State"]["Running"] or after["State"]["ExitCode"] != 0 or b"ROOT_CREDENTIAL_BOUND_PASSED" not in result.stdout or b"--- PASS: TestExecSupervisorPayload" not in result.stdout:
            raise RuntimeError("EXEC_CREDENTIAL_BOUNDARY_FAILED")
        if not list(profiles.glob("covmeta.*")) or not list(profiles.glob("covcounters.*")):
            raise RuntimeError("EXEC_CREDENTIAL_COVERAGE_MISSING")
        if digest(binary) != binary_digest or identity(binary.lstat()) != identity(original) or identity(os.fstat(binary_fd)) != identity(original):
            raise RuntimeError("EXEC_CREDENTIAL_BINARY_CHANGED")
        report.update(success=True, binarySha256=binary_digest, profiles=str(profiles), isolatedRootPayloadPassed=True)
    except BaseException as error:
        report["failure"] = type(error).__name__
        raise
    finally:
        errors = []
        try:
            if container:
                container_identity(token)
                docker("rm", "--force", container)
                report["ownedContainerRemoved"] = docker("inspect", container, required=False).returncode != 0
        except BaseException as error:
            errors.append("container:" + type(error).__name__)
        try:
            if image:
                image_identity(marker)
                docker("image", "rm", image)
                report["ownedImageRemoved"] = docker("image", "inspect", image, required=False).returncode != 0
        except BaseException as error:
            errors.append("image:" + type(error).__name__)
        os.close(binary_fd)
        if errors:
            report.update(success=False, cleanupFailures=errors)
        (output / "result.json").write_text(json.dumps(report, indent=2) + "\n")
        if errors:
            raise RuntimeError("EXEC_CREDENTIAL_CLEANUP_FAILED")
    if not report.get("ownedContainerRemoved") or not report.get("ownedImageRemoved"):
        raise RuntimeError("EXEC_CREDENTIAL_CLEANUP_FAILED")
    print(profiles)


if __name__ == "__main__":
    main()
