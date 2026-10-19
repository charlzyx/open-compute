"""Declared Artifacts extension via its shared native JavaScript binding.

The official SDK leaves this extension as JsProxy. Both entrypoint env and
explicit raw env therefore use FFI conversion; no replacement SDK is involved.
"""

import re

import js
from pyodide.ffi import JsException, to_js


def native(value):
    return to_js(value, dict_converter=js.Object.fromEntries, create_pyproxies=False)


def issued(result, field):
    value = result.to_py()
    secret = value.pop(field)
    if not isinstance(secret, str) or not re.fullmatch(r"art_v1_[0-9a-f]{40}\?expires=[0-9]+", secret):
        raise ValueError("invalid issued Artifacts token")
    value["tokenShapeValid"] = True
    return value


async def artifact_case(env, payload):
    binding = env.ARTIFACTS
    operation = payload["operation"]
    name = payload.get("name", "python-sdk")
    if operation == "create":
        return issued(await binding.create(name, native({"description": payload.get("description", "Python binding parity")})), "token")
    if operation == "list":
        return (await binding.list(native(payload.get("options", {"limit": 50})))).to_py()
    if operation == "delete":
        return await binding.delete(name)
    if operation == "errors":
        rejected = {}
        for failure in ("duplicate", "missing", "name", "ttl", "import"):
            try:
                if failure == "duplicate":
                    await binding.create("python-sdk")
                elif failure == "missing":
                    await binding.get("missing-repository")
                elif failure == "name":
                    await binding.create("../invalid")
                elif failure == "ttl":
                    repo = await binding.get("python-sdk")
                    await repo.createToken("read", 0)
                else:
                    await getattr(binding, "import")(native({"source": {"url": "http://127.0.0.1/repo.git"}, "target": {"name": "unsafe"}}))
            except JsException as error:
                rejected[failure] = {"code": error.code, "numericCode": error.numericCode, "message": error.message}
            else:
                rejected[failure] = None
        return {"rejected": rejected}
    repo = await binding.get(name)
    if operation == "inspect":
        return {
            "repo": (await repo.info()).to_py(),
            "tokens": (await repo.listTokens()).to_py(),
        }
    if operation == "token":
        return issued(await repo.createToken(payload["scope"], 3600), "plaintext")
    if operation == "revoke":
        return await repo.revokeToken(payload["tokenId"])
    if operation == "fork":
        return issued(await repo.fork(payload["target"], native({"defaultBranchOnly": True})), "token")
    raise ValueError("unsupported test operation")
