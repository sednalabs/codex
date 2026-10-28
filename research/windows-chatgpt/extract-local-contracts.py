#!/usr/bin/env python3
"""Summarize only the frozen package's API document and Windows type interfaces."""

import argparse
import hashlib
import json
import re
import zipfile


API_SUFFIX = "environment-docs/codex-app/api.json"
API_HASH = "afc25cc80c0abb56fff443642791f2b797113209fb435f686ac4a1dd3654f725"
DECLARATIONS = {
    "computer_use_client.d.ts": "e057add5e0b0d1043a40b4655570a1b4467d0e765a8c44d913e5deb8c4dd4774",
    "computer_use_client_base.d.ts": "6afb8fbcd3f554e715f1b2d7d80b7237dcc6dfe48725deda730903da35e302eb",
    "helper_transport.d.ts": "f3e75b0fbf6f16f0d8a422394c70b0b207032e8130f8fd847d475a8b930b474e",
}
TARGET_DIR = "app/resources/cua_node/bin/node_modules/%40oai/cua/dist/project/cua/sky_js/src/targets/windows/internal/"
IDENT = re.compile(r"[A-Za-z_$][A-Za-z0-9_$./:-]{0,100}\Z")
TYPE = re.compile(r"[\w\s.$<>{}\[\]|&?:,'\"(),=;\-]+\Z")


def matching_end(text, start):
    pairs = {"(": ")", "[": "]", "{": "}", "<": ">"}
    stack = []
    quote = None
    escaped = False
    for i in range(start, len(text)):
        char = text[i]
        if quote:
            if escaped:
                escaped = False
            elif char == "\\":
                escaped = True
            elif char == quote:
                quote = None
        elif char in "'\"`":
            quote = char
        elif char in pairs:
            stack.append(pairs[char])
        elif stack and char == stack[-1]:
            stack.pop()
            if not stack:
                return i
    return None


def top_level_statements(text):
    statements = []
    start = 0
    i = 0
    while i < len(text):
        if text[i] in "([{<":
            end = matching_end(text, i)
            if end is None:
                return statements, False
            i = end
        elif text[i] in ";\n":
            if text[start:i].strip():
                statements.append(text[start:i].strip())
            start = i + 1
        i += 1
    if text[start:].strip():
        statements.append(text[start:].strip())
    return statements, True


def object_fields(text):
    start = text.find("{")
    if start < 0:
        return [], False
    end = matching_end(text, start)
    if end is None:
        return [], False
    statements, complete = top_level_statements(text[start + 1:end])
    fields = []
    for statement in statements:
        match = re.fullmatch(r"\s*([A-Za-z_$][\w$]*)(\?)?\s*:\s*(.+)", statement, re.S)
        if match:
            field_type = clean_type(match.group(3))
            if field_type is not None:
                fields.append({"name": match.group(1), "optional": bool(match.group(2)), "type": field_type})
            else:
                complete = False
        else:
            complete = False
    return fields, complete


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
    matches = re.finditer(r"(?m)^\s*(?:(?:public|protected|private|abstract|static|readonly|async)\s+)*([A-Za-z_$][\w$]*)(?:\s*<[^;\n]+?>)?\s*\(([^;]*?)\)\s*(?::\s*([^;]+))?;", text)
    methods = []
    for match in matches:
        preceding = [owner for owner in owners if owner.start() < match.start()]
        params = " ".join(match.group(2).split())
        result = " ".join((match.group(3) or "").split())
        if len(params) <= 1000 and len(result) <= 500 and (not params or TYPE.fullmatch(params)) and (not result or TYPE.fullmatch(result)):
            methods.append({"owner_candidate": preceding[-1].group(1) if preceding else None, "method": match.group(1), "parameters": params, "return_type": result or None})
    for match in re.finditer(r"(?m)^\s*(?:(?:public|protected|private|abstract|static|readonly)\s+)*([A-Za-z_$][\w$]*)\??\s*:\s*\(([^;]*?)\)\s*=>\s*([^;]+);", text):
        preceding = [owner for owner in owners if owner.start() < match.start()]
        params, result = clean_type(match.group(2)), clean_type(match.group(3))
        if params is not None and result is not None:
            methods.append({"owner_candidate": preceding[-1].group(1) if preceding else None, "method": match.group(1), "parameters": params, "return_type": result, "style": "function_property"})
    aliases = []
    for alias in re.finditer(r"\b(?:export\s+)?type\s+([A-Za-z_$][\w$]*)\s*=\s*\{", text):
        fields, complete = object_fields(text[alias.start():])
        aliases.append({"name": alias.group(1), "fields": fields, "complete": complete})
    return {"declared_types": [m.group(1) for m in owners], "method_signatures": methods, "object_type_aliases": aliases,
            "limitations": "method regex may omit generic or property-style methods"}


def strings(value):
    if isinstance(value, str):
        yield value
    elif isinstance(value, list):
        for child in value:
            yield from strings(child)
    elif isinstance(value, dict):
        for key, child in value.items():
            if key not in {"description", "documentation", "comment", "examples"}:
                yield from strings(child)


def clean_type(value):
    value = " ".join(value.split())
    if len(value) > 800 or (value and not TYPE.fullmatch(value)):
        return None
    index = 0
    while index < len(value):
        if value[index] in "([{<":
            end = matching_end(value, index)
            if end is None:
                return None
            index = end
        index += 1
    return value


def api_contract(root):
    interfaces = root.get("interfaces", {})
    types = root.get("types", {})
    if not isinstance(interfaces, dict) or not isinstance(types, dict):
        raise ValueError("unexpected API representation")
    records = []
    for owner, members in interfaces.items():
        if not IDENT.fullmatch(owner) or not isinstance(members, dict):
            continue
        for member, entry in members.items():
            if not IDENT.fullmatch(member) or not isinstance(entry, dict):
                continue
            found = []
            for text in strings(entry.get("declarations", [])):
                text = re.sub(r"/\*.*?\*/|//[^\n]*", "", text, flags=re.S)
                pattern = rf"\b{re.escape(member)}\s*\((.*?)\)\s*:\s*([^;\n]+)"
                for match in re.finditer(pattern, text, re.S):
                    params, result = clean_type(match.group(1)), clean_type(match.group(2))
                    if params is not None and result is not None:
                        found.append({"parameters": params, "return_type": result})
            unsupported = entry.get("unsupportedByDefaultIn", [])
            records.append({"interface": owner, "member": member, "signatures": found,
                            "declaration_kind": type(entry.get("declarations")).__name__,
                            "unsupported_by_default_in": [x for x in unsupported if isinstance(x, str) and IDENT.fullmatch(x)] if isinstance(unsupported, list) else []})
    type_records = []
    for name, entry in types.items():
        if not IDENT.fullmatch(name) or not isinstance(entry, dict):
            continue
        text = entry.get("text", "")
        fields, complete = [], False
        if isinstance(text, str):
            text = re.sub(r"/\*.*?\*/|//[^\n]*", "", text, flags=re.S)
            fields, complete = object_fields(text)
        refs = entry.get("references", [])
        type_records.append({"name": name, "fields": fields, "object_field_parse_complete": complete, "references": [x for x in refs if isinstance(x, str) and IDENT.fullmatch(x)] if isinstance(refs, list) else []})
    samples = {}
    for owner, member in (("CUAAPI", "click"), ("Tab", "screenshot"), ("Browsers", "get")):
        entry = interfaces.get(owner, {}).get(member, {})
        samples[owner + "." + member] = shape(entry.get("declarations"))[:25] if isinstance(entry, dict) else []
    return {"members": records, "types": type_records, "declaration_structure_samples": samples,
            "limitations": "static declarations; regex extraction may omit complex TypeScript"}


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
    report = {"package_sha256": package_hash.hexdigest(), "api": None, "declarations": []}
    with zipfile.ZipFile(args.package) as package:
        paths = package.namelist()
        api_entries = [path for path in paths if path.endswith(API_SUFFIX)]
        if len(api_entries) != 1:
            raise ValueError(f"expected one API asset, found {len(api_entries)}")
        path = api_entries[0]
        data = package.read(path)
        if digest(data) != API_HASH:
            raise ValueError("API asset hash mismatch")
        api = {"path": path, "sha256": digest(data), "bytes": len(data)}
        try:
            root = json.loads(data.decode("utf-8-sig"))
            api.update({"parse": "success", "root_kind": type(root).__name__, "root_keys": list(root) if isinstance(root, dict) else None, "contract": api_contract(root) if isinstance(root, dict) else None})
        except UnicodeDecodeError as error:
            api.update({"parse": "decode_error", "encoding": error.encoding, "start": error.start, "end": error.end, "reason": error.reason})
        except json.JSONDecodeError as error:
            api.update({"parse": "json_error", "message": error.msg, "line": error.lineno, "column": error.colno, "position": error.pos})
        report["api"] = api
        print(json.dumps({"package_sha256": package_hash.hexdigest(), "api": api}, sort_keys=True), flush=True)
        for name, expected in DECLARATIONS.items():
            matches = [p for p in paths if p == TARGET_DIR + name]
            if len(matches) != 1:
                raise ValueError(f"expected one declaration for {name}, found {len(matches)}")
            data = package.read(matches[0])
            if digest(data) != expected:
                raise ValueError(f"declaration hash mismatch: {name}")
            report["declarations"].append({"path": matches[0], "sha256": digest(data), **declarations(data)})
    print(json.dumps(report, sort_keys=True))


if __name__ == "__main__":
    main()
