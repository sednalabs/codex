#!/usr/bin/env python3
"""Summarize only the frozen package's API document and Windows type interfaces."""

import argparse
import hashlib
import importlib.util
import json
import re
import zipfile
from pathlib import Path


API_SUFFIX = "environment-docs/codex-app/api.json"
API_HASH = "afc25cc80c0abb56fff443642791f2b797113209fb435f686ac4a1dd3654f725"
DECLARATIONS = {
    "computer_use_client.d.ts": "e057add5e0b0d1043a40b4655570a1b4467d0e765a8c44d913e5deb8c4dd4774",
    "computer_use_client_base.d.ts": "6afb8fbcd3f554e715f1b2d7d80b7237dcc6dfe48725deda730903da35e302eb",
    "helper_transport.d.ts": "f3e75b0fbf6f16f0d8a422394c70b0b207032e8130f8fd847d475a8b930b474e",
}
TARGET_DIR = "/dist/project/cua/sky_js/src/targets/windows/internal/"
IDENT = re.compile(r"[A-Za-z_$][A-Za-z0-9_$./:-]{0,100}\Z")
TYPE = re.compile(r"[\w\s.$<>{}\[\]|&?:,'\"(),=;\-]+\Z")


def digest(data):
    return hashlib.sha256(data).hexdigest()


def shape(value, path="$", depth=0, records=None):
    if records is None:
        records = []
    if len(records) >= 300:
        return records
    if isinstance(value, dict):
        keys = [k for k in value if isinstance(k, str) and IDENT.fullmatch(k)]
        records.append({"path": path, "kind": "object", "keys": keys[:100], "key_count": len(value)})
        if depth < 3:
            for key in keys[:100]:
                if isinstance(value[key], (dict, list)):
                    shape(value[key], path + "." + key, depth + 1, records)
    elif isinstance(value, list):
        records.append({"path": path, "kind": "array", "length": len(value), "element_kinds": sorted({type(x).__name__ for x in value})})
        if depth < 3:
            for index, item in enumerate(value[:2]):
                if isinstance(item, (dict, list)):
                    shape(item, f"{path}[{index}]", depth + 1, records)
    else:
        records.append({"path": path, "kind": type(value).__name__})
    return records


def declarations(data):
    text = data.decode("utf-8-sig")
    text = re.sub(r"/\*.*?\*/|//[^\n]*", "", text, flags=re.S)
    owners = list(re.finditer(r"\b(?:class|interface)\s+([A-Za-z_$][\w$]*)", text))
    matches = re.finditer(r"(?m)^\s*(?:(?:public|protected|private|abstract|static|readonly|async)\s+)*([A-Za-z_$][\w$]*)\s*\(([^;]*?)\)\s*(?::\s*([^;]+))?;", text)
    methods = []
    for match in matches:
        preceding = [owner for owner in owners if owner.start() < match.start()]
        params = " ".join(match.group(2).split())
        result = " ".join((match.group(3) or "").split())
        if len(params) <= 1000 and len(result) <= 500 and (not params or TYPE.fullmatch(params)) and (not result or TYPE.fullmatch(result)):
            methods.append({"owner_candidate": preceding[-1].group(1) if preceding else None, "method": match.group(1), "parameters": params, "return_type": result or None})
    return {"declared_types": [m.group(1) for m in owners], "method_signatures": methods}


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--package", required=True)
    parser.add_argument("--expected-sha256", required=True)
    args = parser.parse_args()
    package_hash = hashlib.sha256()
    with open(args.package, "rb") as stream:
        while chunk := stream.read(1024 * 1024):
            package_hash.update(chunk)
    if package_hash.hexdigest() != args.expected_sha256:
        raise ValueError("package hash mismatch")
    spec = importlib.util.spec_from_file_location("asar_reader", Path(__file__).with_name("analyze-bundled-js.py"))
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    report = {"package_sha256": package_hash.hexdigest(), "api": None, "declarations": []}
    with zipfile.ZipFile(args.package) as package:
        asar = package.read("app/resources/app.asar")
        header, offset = module.parse_asar(asar)
        api_entries = [(path, entry) for path, entry in module.walk_asar(header) if path.endswith(API_SUFFIX)]
        if len(api_entries) != 1:
            raise ValueError(f"expected one API asset, found {len(api_entries)}")
        path, entry = api_entries[0]
        start = offset + int(entry["offset"])
        data = asar[start:start + entry["size"]]
        if digest(data) != API_HASH:
            raise ValueError("API asset hash mismatch")
        api = {"path": path, "sha256": digest(data), "bytes": len(data)}
        try:
            root = json.loads(data.decode("utf-8-sig"))
            api.update({"parse": "success", "root_kind": type(root).__name__, "structure": shape(root), "structure_record_limit": 300})
        except UnicodeDecodeError as error:
            api.update({"parse": "decode_error", "encoding": error.encoding, "start": error.start, "end": error.end, "reason": error.reason})
        except json.JSONDecodeError as error:
            api.update({"parse": "json_error", "message": error.msg, "line": error.lineno, "column": error.colno, "position": error.pos})
        report["api"] = api
        paths = package.namelist()
        for name, expected in DECLARATIONS.items():
            matches = [p for p in paths if TARGET_DIR in p and p.endswith("/" + name)]
            if len(matches) != 1:
                raise ValueError(f"expected one declaration for {name}, found {len(matches)}")
            data = package.read(matches[0])
            if digest(data) != expected:
                raise ValueError(f"declaration hash mismatch: {name}")
            report["declarations"].append({"path": matches[0], "sha256": digest(data), **declarations(data)})
    print(json.dumps(report, sort_keys=True))


if __name__ == "__main__":
    main()
