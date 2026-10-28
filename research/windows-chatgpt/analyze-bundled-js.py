#!/usr/bin/env python3
"""Inspect selected unpacked MSIX/ASAR JavaScript without emitting source text."""

from __future__ import annotations

import argparse
import hashlib
import json
import re
import struct
import sys
import zipfile
from collections import Counter
from pathlib import PurePosixPath


EXPECTED_TOOLS = {
    "browser_observe",
    "browser_step",
    "browser_inspect",
    "browser_review",
    "browser_tabs",
    "browser_session",
    "computer_use",
}

CDP_METHODS = [
    "Page.captureScreenshot",
    "Page.getLayoutMetrics",
    "Page.navigate",
    "DOMSnapshot.captureSnapshot",
    "Accessibility.getFullAXTree",
    "DOM.getDocument",
    "DOM.querySelector",
    "Runtime.evaluate",
    "Input.dispatchMouseEvent",
    "Input.dispatchKeyEvent",
    "Target.getTargets",
    "Target.attachToTarget",
]

FEATURE_TERMS = [
    "browser_use_full_cdp_access",
    "browser_use_external",
    "browser_session",
    "browser_tabs",
    "computer_use",
    "screenshot",
    "viewport",
    "accessibility",
    "coordinates",
    "observation_id",
    "observationId",
    "page_id",
    "pageId",
    "tab_id",
    "tabId",
    "session_id",
    "sessionId",
]

SAFE_KEYS = {
    "action",
    "actions",
    "app",
    "application",
    "url",
    "id",
    "name",
    "title",
    "kind",
    "operation",
    "method",
    "command",
    "page_id",
    "pageId",
    "tab_id",
    "tabId",
    "target_id",
    "targetId",
    "target",
    "selector",
    "role",
    "x",
    "y",
    "coordinates",
    "button",
    "key",
    "keys",
    "modifiers",
    "clickCount",
    "text",
    "direction",
    "deltaX",
    "deltaY",
    "scrollX",
    "scrollY",
    "wait_ms",
    "timeout_ms",
    "timeout",
    "screenshot",
    "viewport",
    "include_dom",
    "includeDom",
    "includeScreenshot",
    "includeSnapshot",
    "dom_snapshot",
    "accessibility_tree",
    "accessibility",
    "application_id",
    "appId",
    "state",
    "window",
    "window_id",
    "windowId",
    "session",
    "width",
    "height",
    "scroll_y",
    "settle_ms",
    "observation_id",
    "observationId",
    "session_id",
    "sessionId",
    "content",
    "type",
    "data",
    "mimeType",
    "text",
    "success",
    "error",
    "isError",
    "structuredContent",
    "result",
}

REGISTRATION_MARKERS = [
    "registerTool",
    "register_tool",
    "server.tool(",
    "mcpServer.tool(",
    "addTool(",
    "tools/call",
    "CallToolRequestSchema",
]

MAX_TEXT_MEMBER = 32 * 1024 * 1024
MAX_MODULE_RECORDS = 100
MAX_REFERENCE_RECORDS = 500
MAX_REGISTRATION_RECORDS = 100
MAX_RESULT_RECORDS = 100
MAX_SURFACE_MODULE_RECORDS = 100
MAX_TOOL_SITE_RECORDS = 400

SURFACE_PATH_MARKERS = (
    "browser-service.mjs",
    "%40oai/cua/dist/lib/js/oai_js_browser/dist/skill/",
    "%40oai/cua/dist/lib/js/oai_js_cua/src/",
    "%40oai/cua/dist/project/cua/sky_js/src/targets/windows/",
    ".vite/build/browser-page-preload",
    ".vite/build/browser-background-switches",
    ".vite/build/mcp-annotation-runtime",
    "plugins/openai-bundled/plugins/chrome/",
    "plugins/openai-bundled/plugins/browser/",
    "playwright/lib/mcp/browser/tools/",
    "playwright/lib/mcp/browser/response.js",
)

TOOL_SITE_PATH_MARKERS = (
    "browser-service.mjs",
    "%40oai/cua/dist/project/cua/sky_js/src/targets/windows/",
    "playwright/lib/mcp/browser/tools/",
    "plugins/openai-bundled/plugins/browser/scripts/browser-service.mjs",
    "plugins/openai-bundled/plugins/chrome/scripts/browser-service.mjs",
)

SCHEMA_CUES = (
    "inputSchema",
    "input_schema",
    "parameters",
    "schema",
    "z.object",
    "Type.Object",
)

DISPATCH_CUES = (
    "registerTool",
    "register_tool",
    "server.tool(",
    "addTool(",
    "tools/call",
    "CallToolRequestSchema",
    "dispatch",
    "handler",
)


def sha256(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def parse_asar(data: bytes) -> tuple[dict[str, object], int]:
    if len(data) < 16:
        raise ValueError("asar header too small")
    # ASAR uses a Chromium Pickle containing the size of a second Pickle,
    # followed by that Pickle's serialized JSON string. Each Pickle starts
    # with its payload byte count; the string length and bytes are 4-byte
    # aligned inside the second Pickle.
    outer_payload_size, header_pickle_size = struct.unpack_from("<II", data, 0)
    if outer_payload_size != 4:
        raise ValueError("asar outer pickle is not a single uint32")
    if header_pickle_size < 8 or header_pickle_size % 4:
        raise ValueError("asar header pickle size is invalid")
    header_payload_size, json_size = struct.unpack_from("<II", data, 8)
    if header_payload_size != header_pickle_size - 4:
        raise ValueError("asar header pickle payload size is inconsistent")
    header_json_start = 16
    data_start = 8 + header_pickle_size
    padded_json_size = (json_size + 3) & ~3
    if (
        json_size > len(data)
        or header_json_start + padded_json_size > data_start
        or header_json_start + json_size > len(data)
        or data_start > len(data)
    ):
        raise ValueError("asar header offsets are outside the archive")
    header = json.loads(data[header_json_start : header_json_start + json_size].decode("utf-8"))
    if not isinstance(header, dict) or not isinstance(header.get("files"), dict):
        raise ValueError("asar header has no file tree")
    return header, data_start


def walk_asar(node: dict[str, object], prefix: str = ""):
    files = node.get("files")
    if not isinstance(files, dict):
        return
    for name, entry in files.items():
        if not isinstance(name, str) or not isinstance(entry, dict):
            continue
        path = f"{prefix}/{name}" if prefix else name
        nested = entry.get("files")
        if isinstance(nested, dict):
            yield from walk_asar(entry, path)
        else:
            yield path, entry


def balanced_object(text: str, open_index: int) -> str | None:
    start = text.find("{", open_index)
    if start < 0:
        return None
    depth = 0
    quote = ""
    escaped = False
    line_comment = False
    block_comment = False
    index = start
    while index < len(text):
        ch = text[index]
        nxt = text[index + 1] if index + 1 < len(text) else ""
        if line_comment:
            if ch in "\r\n":
                line_comment = False
        elif block_comment:
            if ch == "*" and nxt == "/":
                block_comment = False
                index += 1
        elif quote:
            if escaped:
                escaped = False
            elif ch == "\\":
                escaped = True
            elif ch == quote:
                quote = ""
        elif ch in "\"'`":
            quote = ch
        elif ch == "/" and nxt == "/":
            line_comment = True
            index += 1
        elif ch == "/" and nxt == "*":
            block_comment = True
            index += 1
        elif ch == "{":
            depth += 1
        elif ch == "}":
            depth -= 1
            if depth == 0:
                return text[start : index + 1]
        index += 1
    return None


def keys_from_object_text(source: str) -> set[str]:
    # This is intentionally only a key-name inventory, not a JavaScript parser.
    keys = set(re.findall(r"(?:^|[,\{])\s*(?:['\"]([A-Za-z_$][\w$.-]*)['\"]|([A-Za-z_$][\w$.-]*))\s*:", source))
    flattened = {first or second for first, second in keys}
    return flattened & SAFE_KEYS


def byte_offset(text: str, character_offset: int) -> int:
    return len(text[:character_offset].encode("utf-8"))


def identifier_literal_matches(text: str):
    pattern = re.compile(r"[\"'`]((?:browser|computer|screen|desktop)(?:[._-][A-Za-z0-9]+){1,4})[\"'`]", re.IGNORECASE)
    for match in pattern.finditer(text):
        yield match.group(1), match.start(1)


def is_surface_path(path: str) -> bool:
    lowered = path.lower()
    # Keep the selected product/skill modules, not vendored dependencies
    # nested below those module roots.
    if lowered.count("/node_modules/") > 1:
        return False
    if "%40oai/browser-desktop/environment-docs/codex-app/" in lowered:
        basename = PurePosixPath(lowered).name
        if basename in {
            "accessibility.md",
            "api-use-behavior.md",
            "api.json",
            "browser-control-interruption.md",
            "browser-safety.md",
            "browser-troubleshooting.md",
            "confirmations.md",
            "documents.json",
            "screenshots.md",
            "session-naming.md",
            "webmcp.md",
        } or "/capabilities/" in lowered:
            return True
    return any(marker in lowered for marker in SURFACE_PATH_MARKERS)


def is_tool_site_path(path: str) -> bool:
    lowered = path.lower()
    return any(marker in lowered for marker in TOOL_SITE_PATH_MARKERS)


def analyze_api_declarations(path: str, data: bytes) -> dict[str, object] | None:
    if not is_surface_path(path):
        return None
    text = data.decode("utf-8-sig", errors="ignore")
    exported: set[str] = set()
    methods: set[str] = set()
    declaration_pattern = re.compile(
        r"\bexport\s+(?:declare\s+)?(?:async\s+)?(?:function|class|const|let|var|interface|type|enum)\s+([A-Za-z_$][\w$]*)"
    )
    exported.update(match.group(1) for match in declaration_pattern.finditer(text))
    for match in re.finditer(r"\bexport\s*\{([^}]{1,1024})\}", text):
        for item in match.group(1).split(","):
            token = item.strip().split(" as ", 1)[0].strip()
            if re.fullmatch(r"[A-Za-z_$][\w$]*", token):
                exported.add(token)
    if PurePosixPath(path).suffix.lower() == ".ts":
        method_pattern = re.compile(r"\b([A-Za-z_$][\w$]*)\??\s*\([^)]{0,512}\)\s*:")
        for match in method_pattern.finditer(text):
            name = match.group(1)
            if re.search(
                r"browser|computer|screen|tab|session|click|mouse|keyboard|key|type|scroll|wait|navigate|page|access|focus|viewport|shot|window|tool|action",
                name,
                re.I,
            ):
                methods.add(name)
    case_labels = sorted(
        {
            match.group(1)
            for match in re.finditer(r"\bcase\s+['\"]([A-Za-z0-9_.-]{1,64})['\"]\s*:", text)
            if re.search(
                r"browser|computer|screen|tab|session|click|mouse|keyboard|key|type|scroll|wait|navigate|page|access|focus|viewport|shot|window|action",
                match.group(1),
                re.I,
            )
        }
    )[:100]
    if not exported and not methods and not case_labels:
        return None
    return {
        "path": path,
        "sha256": sha256(data),
        "exported_declaration_name_candidates": sorted(exported)[:100],
        "typed_agent_api_method_name_candidates": sorted(methods)[:100],
        "dispatch_case_label_candidates": case_labels,
        "evidence_class": "static_export_or_type_or_switch_candidate_not_runtime_proof",
    }


def analyze_tool_name_sites(path: str, data: bytes) -> list[dict[str, object]]:
    if not is_tool_site_path(path):
        return []
    text = data.decode("utf-8-sig", errors="ignore")
    file_hash = sha256(data)
    sites: list[dict[str, object]] = []
    for tool_name in sorted(EXPECTED_TOOLS):
        pattern = re.compile(rf"(?<![A-Za-z0-9_$]){re.escape(tool_name)}(?![A-Za-z0-9_$])")
        for match in list(pattern.finditer(text))[:20]:
            position = match.start()
            window_start = max(0, position - 1024)
            window_end = min(len(text), position + 8192)
            context = text[window_start:window_end]
            schema_keys: set[str] = set()
            schema_cues_found: set[str] = set()
            for cue in SCHEMA_CUES:
                cursor = 0
                while True:
                    cue_position = context.find(cue, cursor)
                    if cue_position < 0:
                        break
                    schema_cues_found.add(cue)
                    object_text = balanced_object(context, cue_position)
                    if object_text:
                        schema_keys |= keys_from_object_text(object_text)
                    cursor = cue_position + len(cue)
            dispatch_cues_found = [cue for cue in DISPATCH_CUES if cue in context]
            if len(sites) < MAX_TOOL_SITE_RECORDS:
                sites.append(
                    {
                        "path": path,
                        "sha256": file_hash,
                        "static_name": tool_name,
                        "offset_utf8_bytes": byte_offset(text, position),
                        "nearby_schema_cues": sorted(schema_cues_found),
                        "nearby_allowlisted_parameter_key_candidates": sorted(schema_keys),
                        "nearby_dispatch_cues": dispatch_cues_found,
                        "evidence_class": "static_neighborhood_candidate_not_exact_schema_or_execution_proof",
                    }
                )
    return sites


def analyze_text_member(path: str, data: bytes) -> tuple[list[dict[str, object]], list[dict[str, object]], list[dict[str, object]], list[dict[str, object]]]:
    try:
        text = data.decode("utf-8-sig", errors="ignore")
    except Exception:
        return [], [], [], []
    file_hash = sha256(data)
    references: list[dict[str, object]] = []
    registrations: list[dict[str, object]] = []
    results: list[dict[str, object]] = []
    serialized: list[dict[str, object]] = []

    for method in CDP_METHODS + FEATURE_TERMS:
        count = 0
        offsets: list[int] = []
        cursor = 0
        while count < 4:
            position = text.lower().find(method.lower(), cursor)
            if position < 0:
                break
            count += 1
            offsets.append(byte_offset(text, position))
            cursor = position + len(method)
        if count:
            references.append(
                {
                    "path": path,
                    "sha256": file_hash,
                    "identifier": method,
                    "offsets_first_4_utf8_bytes": offsets,
                    "evidence_class": "static_reference_only",
                }
            )

    marker_positions: list[tuple[str, int]] = []
    for marker in REGISTRATION_MARKERS:
        cursor = 0
        while True:
            position = text.find(marker, cursor)
            if position < 0:
                break
            marker_positions.append((marker, position))
            cursor = position + len(marker)

    for marker, position in marker_positions[:MAX_REGISTRATION_RECORDS]:
        window_start = max(0, position - 768)
        window_end = min(len(text), position + 3072)
        window = text[window_start:window_end]
        candidate_names = sorted(
            {
                name
                for name, name_pos in identifier_literal_matches(window)
                if name_pos >= max(0, position - window_start - 256)
            }
        )[:20]
        schema_keys: set[str] = set()
        for schema_marker in ("inputSchema", "input_schema", "parameters", "schema"):
            schema_cursor = position
            for _ in range(4):
                schema_pos = text.find(schema_marker, schema_cursor, window_end)
                if schema_pos < 0:
                    break
                object_text = balanced_object(text, schema_pos)
                if object_text:
                    schema_keys |= keys_from_object_text(object_text)
                schema_cursor = schema_pos + len(schema_marker)
        if candidate_names or schema_keys:
            registrations.append(
                {
                    "path": path,
                    "sha256": file_hash,
                    "registration_marker": marker,
                    "offset_utf8_bytes": byte_offset(text, position),
                    "candidate_names_near_registration": candidate_names,
                    "allowlisted_schema_keys_near_registration": sorted(schema_keys),
                    "evidence_class": "static_registration_pattern_candidate_not_runtime_proof",
                }
            )

    for literal, position in identifier_literal_matches(text):
        context = text[max(0, position - 384) : min(len(text), position + 384)]
        has_registration_marker = any(marker in context for marker in REGISTRATION_MARKERS)
        if has_registration_marker:
            continue
        if literal in EXPECTED_TOOLS and len(serialized) < 100:
            serialized.append(
                {
                    "path": path,
                    "sha256": file_hash,
                    "name_literal": literal,
                    "offset_utf8_bytes": byte_offset(text, position),
                    "evidence_class": "static_name_literal_only",
                }
            )

    result_pattern = re.compile(r"(?:type\s*:\s*['\"](image|text)['\"]|mimeType\s*:\s*['\"](image/[^'\"]+)['\"])")
    for match in result_pattern.finditer(text):
        fragment_start = max(0, match.start() - 320)
        fragment_end = min(len(text), match.end() + 320)
        fragment = text[fragment_start:fragment_end]
        keys = keys_from_object_text("{" + fragment + "}")
        kind = match.group(1) or match.group(2)
        results.append(
            {
                "path": path,
                "sha256": file_hash,
                "content_kind_literal": kind,
                "offset_utf8_bytes": byte_offset(text, match.start()),
                "allowlisted_nearby_result_keys": sorted(keys),
                "evidence_class": "static_result_shape_candidate",
            }
        )
        if len(results) >= MAX_RESULT_RECORDS:
            break

    if PurePosixPath(path).suffix.lower() == ".json" and len(data) <= 4 * 1024 * 1024:
        try:
            value = json.loads(data.decode("utf-8-sig"))
            walk_json_schemas(value, path, file_hash, serialized)
        except (UnicodeDecodeError, json.JSONDecodeError):
            pass

    return references, registrations, results, serialized


def walk_json_schemas(value: object, path: str, file_hash: str, out: list[dict[str, object]], json_path: str = "$", limit: int = 100) -> None:
    if len(out) >= limit:
        return
    if isinstance(value, dict):
        name = value.get("name")
        if isinstance(name, str) and (name in EXPECTED_TOOLS or name.lower().startswith(("browser", "computer"))):
            schema = value.get("inputSchema", value.get("parameters", value.get("input_schema")))
            keys: set[str] = set()
            if isinstance(schema, dict):
                properties = schema.get("properties")
                if isinstance(properties, dict):
                    keys = set(properties) & SAFE_KEYS
            out.append(
                {
                    "path": path,
                    "sha256": file_hash,
                    "tool_name": name,
                    "json_path": json_path,
                    "allowlisted_parameter_keys": sorted(keys),
                    "evidence_class": "serialized_json_schema",
                }
            )
        for key, child in value.items():
            walk_json_schemas(child, path, file_hash, out, f"{json_path}.{key}", limit)
    elif isinstance(value, list):
        for index, child in enumerate(value):
            walk_json_schemas(child, path, file_hash, out, f"{json_path}[{index}]", limit)


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--package", required=True)
    parser.add_argument("--expected-sha256", required=True)
    args = parser.parse_args()

    package_hash = hashlib.sha256()
    with open(args.package, "rb") as stream:
        while chunk := stream.read(1024 * 1024):
            package_hash.update(chunk)
    package_sha = package_hash.hexdigest()
    if package_sha.lower() != args.expected_sha256.lower():
        raise ValueError("package SHA-256 does not match the frozen candidate")

    asar_reports: list[dict[str, object]] = []
    module_records: list[dict[str, object]] = []
    all_references: list[dict[str, object]] = []
    all_registrations: list[dict[str, object]] = []
    all_results: list[dict[str, object]] = []
    all_serialized: list[dict[str, object]] = []
    surface_modules: list[dict[str, object]] = []
    surface_tool_sites: list[dict[str, object]] = []
    surface_references: list[dict[str, object]] = []
    surface_registrations: list[dict[str, object]] = []
    surface_results: list[dict[str, object]] = []
    surface_serialized: list[dict[str, object]] = []
    surface_api_declarations: list[dict[str, object]] = []

    with zipfile.ZipFile(args.package, "r") as package:
        infos = [info for info in package.infolist() if not info.is_dir()]
        for info in infos:
            path = info.filename
            suffix = PurePosixPath(path).suffix.lower()
            relevant = any(token in path.lower() for token in ("browser", "computer", "mcp", "cua", "codex"))
            if suffix == ".asar":
                data = package.read(info)
                header, data_start = parse_asar(data)
                entries = list(walk_asar(header))
                extension_counts = Counter(PurePosixPath(name).suffix.lower() or "<none>" for name, _ in entries)
                top_dirs = Counter(name.split("/", 1)[0] for name, _ in entries)
                archive_hash = sha256(data)
                asar_reports.append(
                    {
                        "path": path,
                        "bytes": len(data),
                        "sha256": archive_hash,
                        "header_data_offset": data_start,
                        "entry_count": len(entries),
                        "top_level_entry_counts": dict(sorted(top_dirs.items())),
                        "extension_counts": dict(sorted(extension_counts.items())),
                    }
                )
                for entry_path, entry in entries:
                    if not isinstance(entry, dict) or entry.get("unpacked"):
                        continue
                    entry_suffix = PurePosixPath(entry_path).suffix.lower()
                    entry_size = entry.get("size")
                    if not isinstance(entry_size, int) or entry_size < 0 or entry_size > MAX_TEXT_MEMBER:
                        continue
                    if entry_suffix not in {".js", ".mjs", ".cjs", ".json", ".html", ".ts", ".md", ".map"}:
                        continue
                    offset_value = entry.get("offset")
                    try:
                        offset = int(offset_value) if offset_value is not None else None
                    except (TypeError, ValueError):
                        offset = None
                    if offset is None or offset < 0:
                        continue
                    start = data_start + offset
                    end = start + entry_size
                    if end > len(data):
                        continue
                    member_data = data[start:end]
                    member_hash = sha256(member_data)
                    if is_surface_path(entry_path):
                        if len(surface_modules) < MAX_SURFACE_MODULE_RECORDS:
                            surface_modules.append(
                                {
                                    "path": f"{path}!/{entry_path}",
                                    "bytes": entry_size,
                                    "sha256": member_hash,
                                }
                            )
                        surface_tool_sites.extend(analyze_tool_name_sites(entry_path, member_data))
                        api_summary = analyze_api_declarations(entry_path, member_data)
                        if api_summary:
                            surface_api_declarations.append({"archive": path, **api_summary})
                    if len(module_records) < MAX_MODULE_RECORDS and any(
                        token in entry_path.lower()
                        for token in ("browser", "computer", "mcp", "agent", "tool", "plugin")
                    ):
                        module_records.append(
                            {
                                "path": f"{path}!/{entry_path}",
                                "bytes": entry_size,
                                "sha256": member_hash,
                            }
                        )
                    refs, regs, result_shapes, serialized = analyze_text_member(entry_path, member_data)
                    if refs:
                        all_references.extend({"archive": path, **item} for item in refs[:5000 - len(all_references)])
                        if is_surface_path(entry_path):
                            surface_references.extend({"archive": path, **item} for item in refs)
                    if regs:
                        all_registrations.extend({"archive": path, **item} for item in regs[:2000 - len(all_registrations)])
                        if is_surface_path(entry_path):
                            surface_registrations.extend({"archive": path, **item} for item in regs)
                    if result_shapes:
                        all_results.extend({"archive": path, **item} for item in result_shapes[:2000 - len(all_results)])
                        if is_surface_path(entry_path):
                            surface_results.extend({"archive": path, **item} for item in result_shapes)
                    if serialized:
                        all_serialized.extend({"archive": path, **item} for item in serialized[:2000 - len(all_serialized)])
                        if is_surface_path(entry_path):
                            surface_serialized.extend({"archive": path, **item} for item in serialized)
                continue

            if suffix not in {".js", ".mjs", ".cjs", ".json", ".html", ".ts", ".md", ".map"}:
                continue
            if not relevant or info.file_size > MAX_TEXT_MEMBER:
                continue
            try:
                member_data = package.read(info)
            except (OSError, zipfile.BadZipFile):
                continue
            refs, regs, result_shapes, serialized = analyze_text_member(path, member_data)
            if is_surface_path(path):
                if len(surface_modules) < MAX_SURFACE_MODULE_RECORDS:
                    surface_modules.append({"path": path, "bytes": info.file_size, "sha256": sha256(member_data)})
                surface_tool_sites.extend(analyze_tool_name_sites(path, member_data))
                api_summary = analyze_api_declarations(path, member_data)
                if api_summary:
                    surface_api_declarations.append(api_summary)
                surface_references.extend(refs)
                surface_registrations.extend(regs)
                surface_results.extend(result_shapes)
                surface_serialized.extend(serialized)
            if refs:
                all_references.extend(refs)
            if regs:
                all_registrations.extend(regs)
            if result_shapes:
                all_results.extend(result_shapes)
            if serialized:
                all_serialized.extend(serialized)
            if len(module_records) < MAX_MODULE_RECORDS and (
                "browser-service" in path.lower()
                or "computer_use" in path.lower()
                or "computer-use" in path.lower()
                or "browser_desktop" in path.lower()
            ):
                module_records.append({"path": path, "bytes": info.file_size, "sha256": sha256(member_data)})

    report = {
        "research_id": "chatgpt-windows-bundled-js-2026-10-28",
        "package_sha256_binding": package_sha,
        "asar_archives": asar_reports,
        "selected_modules": module_records[:MAX_MODULE_RECORDS],
        "focused_agent_surface_modules": surface_modules[:MAX_SURFACE_MODULE_RECORDS],
        "focused_tool_name_sites": surface_tool_sites[:MAX_TOOL_SITE_RECORDS],
        "focused_api_declarations": surface_api_declarations[:MAX_SURFACE_MODULE_RECORDS],
        "focused_registration_candidates": surface_registrations[:MAX_REGISTRATION_RECORDS],
        "focused_static_call_site_references": surface_references[:MAX_REFERENCE_RECORDS],
        "focused_result_shape_candidates": surface_results[:MAX_RESULT_RECORDS],
        "focused_serialized_schema_evidence": surface_serialized[:MAX_REGISTRATION_RECORDS],
        "static_call_site_references": all_references[:MAX_REFERENCE_RECORDS],
        "registration_pattern_candidates": all_registrations[:MAX_REGISTRATION_RECORDS],
        "result_shape_candidates": all_results[:MAX_RESULT_RECORDS],
        "serialized_schema_evidence": all_serialized[:MAX_REGISTRATION_RECORDS],
        "truncated": {
            "references": len(all_references) > MAX_REFERENCE_RECORDS,
            "registration_candidates": len(all_registrations) > MAX_REGISTRATION_RECORDS,
            "result_shapes": len(all_results) > MAX_RESULT_RECORDS,
            "serialized_schemas": len(all_serialized) > MAX_REGISTRATION_RECORDS,
        },
        "limits": {
            "raw_source_emitted": False,
            "tool_string_or_CDP_method_presence_does_not_by_itself_prove_live_registration_or_remote_availability": True,
            "JavaScript_registration_patterns_are_candidates_not_execution_proof": True,
        },
    }
    print(json.dumps(report, sort_keys=True, separators=(",", ":")))
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except Exception as exc:
        print(json.dumps({"bundled_js_analysis_error_type": type(exc).__name__}), file=sys.stderr)
        raise SystemExit(2)
