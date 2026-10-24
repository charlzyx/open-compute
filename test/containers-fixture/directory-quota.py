#!/usr/bin/env python3
"""Prove directory mounts share the rootfs quota through the trusted helper."""
from pathlib import Path
import hashlib
import http.client
import io
import json
import os
import re
import shutil
import socket
import sys
import tarfile
import time
import uuid


class Engine(http.client.HTTPConnection):
    def connect(self):
        host = os.environ.get("DOCKER_HOST", "unix:///var/run/docker.sock")
        if not host.startswith("unix://"):
            raise RuntimeError("DIRECTORY_QUOTA_ENGINE_SOCKET_REQUIRED")
        self.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.sock.settimeout(30)
        self.sock.connect(host.removeprefix("unix://"))


def request(method, path, value=None, raw=None, accepted=(200,)):
    connection = Engine("localhost", timeout=30)
    try:
        body = raw
        headers = {"Content-Type": "application/x-tar"} if raw is not None else {}
        if value is not None:
            body = json.dumps(value).encode()
            headers["Content-Type"] = "application/json"
        connection.request(method, path, body, headers)
        response = connection.getresponse()
        data = response.read((1 << 20) + 1)
        if response.status not in accepted or len(data) > 1 << 20:
            raise RuntimeError("DIRECTORY_QUOTA_ENGINE_RESPONSE")
        return json.loads(data) if data else None
    finally:
        connection.close()


def inventory():
    return {
        "containers": sorted(row["Id"] for row in request("GET", "/containers/json?all=true")),
        "volumes": sorted(row["Name"] for row in request("GET", "/volumes")["Volumes"] or []),
        "engine": request("GET", "/info")["ID"],
    }


def main():
    binary = Path(sys.argv[1]).resolve(strict=True)
    image = sys.argv[2]
    quota_tests = Path(sys.argv[3]).resolve(strict=True)
    if not re.fullmatch(r"sha256:[0-9a-f]{64}", image):
        raise RuntimeError("DIRECTORY_QUOTA_IMMUTABLE_IMAGE_REQUIRED")
    fields = ("st_dev", "st_ino", "st_uid", "st_gid", "st_mode", "st_nlink", "st_size", "st_mtime_ns", "st_ctime_ns")
    identity = lambda: tuple(getattr(binary.stat(), field) for field in fields)
    original = identity()
    digest = hashlib.sha256(binary.read_bytes()).hexdigest()
    output = binary.parent / ("directory-quota-evidence-" + uuid.uuid4().hex)
    output.mkdir(mode=0o700)
    helper_source = output / "helper-source"
    helper_source.mkdir(mode=0o700)
    shutil.copyfile(binary, helper_source / "helper")
    (helper_source / "helper").chmod(0o555)
    helper_source.chmod(0o555)
    name = "oc-directory-quota-" + uuid.uuid4().hex
    labels = {"dev.open-compute.directory-proof": name}
    containers = []
    volume = False
    report = {"success": False, "checks": {}, "cleanup": {}, "helperSha256": digest}
    baseline = inventory()
    inspected_image = request("GET", "/images/" + image + "/json")
    assert inspected_image["Id"] == image
    expected_labels = dict(inspected_image["Config"].get("Labels") or {})
    expected_labels.update(labels)

    def owned(identifier):
        value = request("GET", "/containers/" + identifier + "/json")
        assert value["Id"] == identifier and value["Config"]["Labels"] == expected_labels
        assert value["Image"] == image
        return value

    def create(suffix, command, mounts, readonly=False, environment=None):
        value = request("POST", "/containers/create?name=" + name + suffix, {
            "Image": image, "User": "0", "Entrypoint": command[:1], "Cmd": command[1:],
            "Labels": labels, "Env": environment or [], "HostConfig": {
                "NetworkMode": "none", "Memory": 67_108_864, "MemorySwap": 67_108_864,
                "NanoCpus": 62_500_000, "PidsLimit": 16, "StorageOpt": {"size": "33554432"},
                "LogConfig": {"Type": "none"}, "SecurityOpt": ["no-new-privileges:true"],
                "ReadonlyRootfs": readonly, "CapDrop": ["ALL"] if readonly else ["NET_RAW"],
                "CapAdd": [], "Mounts": mounts,
            },
        }, accepted=(201,))
        identifier = value["Id"]
        assert re.fullmatch(r"[0-9a-f]{64}", identifier)
        containers.append(identifier)
        owned(identifier)
        return identifier

    def run(identifier, expected):
        request("POST", "/containers/" + identifier + "/start", accepted=(204,))
        deadline = time.monotonic() + 30
        while True:
            value = owned(identifier)
            if not value["State"]["Running"]:
                report.setdefault("helperRuns", []).append({"container": identifier, "expectedExit": expected, "actualExit": value["State"]["ExitCode"]})
                assert value["State"]["ExitCode"] == expected
                return
            assert time.monotonic() < deadline
            time.sleep(0.05)

    try:
        value = request("POST", "/volumes/create", {"Name": name, "Driver": "local", "Labels": labels}, accepted=(201,))
        volume = True
        assert value["Name"] == name and value["Labels"] == labels
        command = "set -eu; if mv /.open-compute-exec /.open-compute-exec-moved; then exit 73; fi; test \"$(cat /restore/saved)\" = directory-restored; test \"$(stat -c %a /restore/saved)\" = 640; test \"$(stat -c %u:%g /restore/saved)\" = 1234:2345; test \"$(stat -c %i /restore/saved)\" = \"$(stat -c %i /restore/same)\"; test \"$(readlink /restore/relative)\" = saved; dd if=/dev/zero of=/root-fill bs=1048576 count=12 2>/dev/null; if output=$(dd if=/dev/zero of=/restore/volume-fill bs=1048576 count=24 2>&1); then exit 71; fi; case \"$output\" in *\"No space left on device\"*) ;; *) exit 72;; esac; bytes=$(wc -c < /restore/volume-fill); test \"$bytes\" -lt 25165824; test \"$bytes\" -gt 8388608; : > /root-fill; dd if=/dev/zero of=/restore/reclaimed bs=1048576 count=8 2>/dev/null; printf ROOT-SNAPSHOT-MARKER > /root-marker; : > /restore/volume-fill; : > /restore/reclaimed"
        app = create("", ["/.open-compute-exec/helper", "--open-compute-main", "0:0", "/bin/sh", "-c", command], [{"Type": "volume", "Source": name, "Target": "/restore", "VolumeOptions": {"NoCopy": True}}, {"Type": "bind", "Source": str(helper_source), "Target": "/.open-compute-exec", "ReadOnly": True}])
        inspection = owned(app)
        upper = Path(inspection["GraphDriver"]["Data"]["UpperDir"])
        root = Path(request("GET", "/info")["DockerRootDir"])
        assert upper.is_relative_to(root / "overlay2") and upper.name == "diff"
        mounts = [
            {"Type": "bind", "Source": str(helper_source), "Target": "/.open-compute-exec", "ReadOnly": True},
            {"Type": "bind", "Source": str(upper), "Target": "/open-compute-rootfs", "ReadOnly": True},
            {"Type": "volume", "Source": name, "Target": "/open-compute-directories/0", "VolumeOptions": {"NoCopy": True}},
        ]
        invalid = create("-extra-argument", ["/.open-compute-exec/helper", "--open-compute-directory-quota", "extra"], mounts, readonly=True)
        run(invalid, 125)
        preparer = create("-prepare", ["/.open-compute-exec/helper", "--open-compute-directory-quota"], mounts, readonly=True)
        run(preparer, 0)
        report["checks"].update(extraArgumentRejected=True, helperHasNoAddedCapabilities=True, helperPathCannotBeRebound=True, trustedHelperPreparedEmptyDirectory=True)
        mutation_mounts = [
            {"Type": "bind", "Source": str(helper_source), "Target": "/.open-compute-exec", "ReadOnly": True},
            {"Type": "bind", "Source": str(quota_tests), "Target": "/quota-tests", "ReadOnly": True},
            {"Type": "volume", "Source": name, "Target": "/restore", "VolumeOptions": {"NoCopy": True}},
        ]
        for mode in ["unfiltered-control", "filtered"]:
            command = ["/quota-tests", "-test.run=^TestContainerQuotaMutationPayload$"]
            if mode == "filtered":
                command = ["/.open-compute-exec/helper", "--open-compute-main", "0:0"] + command
            mutation = create("-" + mode, command, mutation_mounts,
                              environment=["OC_QUOTA_MUTATION_PAYLOAD=" + mode])
            run(mutation, 0)
        report["checks"]["tenantCannotRemoveQuotaButUnfilteredOwnerCan"] = True
        payload = io.BytesIO()
        with tarfile.open(fileobj=payload, mode="w") as archive:
            for filename, kind, data, link in [("saved", tarfile.REGTYPE, b"directory-restored\n", ""), ("same", tarfile.LNKTYPE, b"", "saved"), ("relative", tarfile.SYMTYPE, b"", "saved")]:
                entry = tarfile.TarInfo(filename)
                entry.type, entry.mode, entry.uid, entry.gid = kind, 0o640, 1234, 2345
                entry.size, entry.linkname = len(data), link
                archive.addfile(entry, io.BytesIO(data))
        request("PUT", "/containers/" + app + "/archive?path=%2Frestore", raw=payload.getvalue())
        dirty = create("-nonempty", ["/.open-compute-exec/helper", "--open-compute-directory-quota"], mounts, readonly=True)
        run(dirty, 125)
        report["checks"]["nonemptyDirectoryRejected"] = True
        run(app, 0)
        report["checks"].update(exactContentUidGidMode=True, hardlinkAndSymlinkPreserved=True, combinedQuotaEnforced=True, rootTruncationReleasesDirectoryCapacity=True)
        connection = Engine("localhost", timeout=30)
        try:
            connection.request("GET", "/containers/" + app + "/export")
            response = connection.getresponse()
            assert response.status == 200
            root_seen = False
            with tarfile.open(fileobj=response, mode="r|") as archive:
                for entry in archive:
                    path = entry.name.removeprefix("./")
                    assert path not in ["restore/saved", "restore/same", "restore/relative"]
                    if path == "root-marker":
                        assert archive.extractfile(entry).read() == b"ROOT-SNAPSHOT-MARKER"
                        root_seen = True
            assert root_seen
            report["checks"]["wholeSnapshotExcludesDirectoryMount"] = True
        finally:
            connection.close()
        assert identity() == original and hashlib.sha256(binary.read_bytes()).hexdigest() == digest
        report["success"] = True
    except BaseException as error:
        report["failure"] = type(error).__name__
    finally:
        cleanup_errors = []
        for identifier in reversed(containers):
            try:
                owned(identifier)
                request("DELETE", "/containers/" + identifier + "?force=true", accepted=(204,))
                request("GET", "/containers/" + identifier + "/json", accepted=(404,))
            except BaseException as error:
                cleanup_errors.append(type(error).__name__)
        if volume:
            try:
                value = request("GET", "/volumes/" + name)
                assert value["Labels"] == labels and value["Name"] == name
                request("DELETE", "/volumes/" + name, accepted=(204,))
                request("GET", "/volumes/" + name, accepted=(404,))
            except BaseException as error:
                cleanup_errors.append(type(error).__name__)
        try:
            assert inventory() == baseline
        except BaseException as error:
            cleanup_errors.append(type(error).__name__)
        report["cleanup"] = {"containers": len(containers), "volume": volume, "failures": cleanup_errors}
        report["success"] = report["success"] and not cleanup_errors
        (output / "result.json").write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps({"success": report["success"], "result": str(output / "result.json"), "checks": report["checks"], "failure": report.get("failure"), "cleanup": report["cleanup"]}))
    if not report["success"]:
        raise SystemExit(1)


if __name__ == "__main__":
    main()
