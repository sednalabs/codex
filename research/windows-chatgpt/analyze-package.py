#!/usr/bin/env python3
"""Read-only, bounded analysis of the official Windows desktop MSIX.

Only package identity, hashes, file-type/module metadata, allowlisted agent
interface identifiers, and JSON schema key names are emitted. No code, prompts,
credentials, or unfiltered strings are written to the workflow log.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import struct
import sys
import zipfile
from collections import Counter
from datetime import datetime, timezone
from pathlib import PurePosixPath
from xml.etree import ElementTree


TOOL_NAMES = {
    "browser_observe",
    "browser_step",
    "browser_inspect",
    "browser_review",
    "browser_tabs",
    "browser_session",
    "computer_use",
}

STATIC_TERMS = [
    "browser_observe",
    "browser_step",
    "browser_inspect",
    "browser_review",
    "browser_tabs",
    "browser_session",
    "computer_use",
    "computer-use",
    "browser_use_full_cdp_access",
    "browser_use_external",
    "@Browser",
    "@Computer",
    "Page.captureScreenshot",
    "DOMSnapshot.captureSnapshot",
    "Accessibility.getFullAXTree",
    "Input.dispatchMouseEvent",
    "Input.dispatchKeyEvent",
    "tab_id",
    "tabId",
    "session_id",
    "sessionId",
    "screenshot",
    "viewport",
    "accessibility",
    "coordinates",
    "WebView2",
    "mcpServers",
]

SCHEMA_KEYS = {
    "action",
    "url",
    "tab_id",
    "tabId",
    "target",
    "selector",
    "x",
    "y",
    "button",
    "key",
    "text",
    "direction",
    "deltaX",
    "deltaY",
    "wait_ms",
    "timeout_ms",
    "screenshot",
    "viewport",
    "include_dom",
    "dom_snapshot",
    "accessibility_tree",
    "application_id",
    "appId",
    "state",
}

TEXT_SUFFIXES = {
    ".css",
    ".html",
    ".js",
    ".json",
    ".mjs",
    ".cjs",
    ".map",
    ".md",
    ".resw",
    ".toml",
    ".txt",
    ".xml",
    ".yaml",
    ".yml",
}
PE_SUFFIXES = {".exe", ".dll", ".node"}
MAX_ENTRY_BYTES_FOR_TEXT = 128 * 1024 * 1024
MAX_SCHEMA_BYTES = 4 * 1024 * 1024
MAX_MODULES_REPORTED = 40
MAX_STRING_MATCHES_REPORTED = 240


def sha256_member(archive: zipfile.ZipFile, name: str) -> str:
    digest = hashlib.sha256()
    with archive.open(name) as stream:
        while chunk := stream.read(1024 * 1024):
            digest.update(chunk)
    return digest.hexdigest()


def parse_pe_header(data: bytes) -> dict[str, object] | None:
    if len(data) < 0x40 or data[:2] != b"MZ":
        return None
    pe_offset = struct.unpack_from("<I", data, 0x3C)[0]
    if pe_offset + 24 > len(data) or data[pe_offset : pe_offset + 4] != b"PE\0\0":
        return {"valid_pe_header": False}
    machine, section_count = struct.unpack_from("<HH", data, pe_offset + 4)
    optional_size = struct.unpack_from("<H", data, pe_offset + 20)[0]
    optional = pe_offset + 24
    if optional + 2 > len(data):
        return {"valid_pe_header": False}
    magic = struct.unpack_from("<H", data, optional)[0]
    machine_name = {
        0x014C: "x86",
        0x8664: "x64",
        0xAA64: "arm64",
        0x01C4: "arm32",
    }.get(machine, f"unknown-0x{machine:04x}")
    directory_base = {0x10B: optional + 96, 0x20B: optional + 112}.get(magic)
    number_offset = {0x10B: optional + 92, 0x20B: optional + 108}.get(magic)
    directory_count = 0
    if number_offset is not None and number_offset + 4 <= len(data):
        directory_count = struct.unpack_from("<I", data, number_offset)[0]

    def data_directory(index: int) -> tuple[int, int]:
        if directory_base is None or directory_count <= index:
            return (0, 0)
        pos = directory_base + index * 8
        if pos + 8 > len(data) or pos + 8 > optional + optional_size:
            return (0, 0)
        return struct.unpack_from("<II", data, pos)

    import_rva, import_size = data_directory(1)
    resource_rva, resource_size = data_directory(2)
    clr_rva, clr_size = data_directory(14)
    section_table = optional + optional_size
    sections: list[tuple[int, int, int, int]] = []
    for index in range(min(section_count, 96)):
        pos = section_table + index * 40
        if pos + 40 > len(data):
            break
        name = data[pos : pos + 8].split(b"\0", 1)[0].decode("ascii", "replace")
        virtual_size, virtual_address, raw_size, raw_pointer = struct.unpack_from(
            "<IIII", data, pos + 8
        )
        sections.append((virtual_address, max(virtual_size, raw_size), raw_pointer, raw_size))

    def rva_to_offset(rva: int) -> int | None:
        for virtual_address, span, raw_pointer, raw_size in sections:
            if virtual_address <= rva < virtual_address + span:
                offset = raw_pointer + rva - virtual_address
                if offset < raw_pointer + raw_size and offset < len(data):
                    return offset
        return None

    imports: list[str] = []
    import_offset = rva_to_offset(import_rva) if import_rva else None
    if import_offset is not None and import_size:
        for index in range(min(import_size // 20, 512)):
            pos = import_offset + index * 20
            if pos + 20 > len(data):
                break
            descriptor = struct.unpack_from("<IIIII", data, pos)
            name_rva = descriptor[3]
            if not name_rva:
                break
            name_offset = rva_to_offset(name_rva)
            if name_offset is None:
                continue
            end = data.find(b"\0", name_offset, min(name_offset + 256, len(data)))
            if end < 0:
                continue
            imports.append(data[name_offset:end].decode("ascii", "replace"))

    section_names = []
    for index in range(min(section_count, 96)):
        pos = section_table + index * 40
        if pos + 8 > len(data):
            break
        section_names.append(data[pos : pos + 8].split(b"\0", 1)[0].decode("ascii", "replace"))

    return {
        "valid_pe_header": True,
        "machine": machine_name,
        "section_count": section_count,
        "sections": section_names[:16],
        "managed_cli_header": bool(clr_rva and clr_size),
        "imports": sorted(set(imports))[:16],
        "resource_directory_present": bool(resource_rva and resource_size),
    }


def walk_json(value: object, path: str = "$", out: list[dict[str, object]] | None = None) -> list[dict[str, object]]:
    if out is None:
        out = []
    if isinstance(value, dict):
        name_value = value.get("name")
        if isinstance(name_value, str) and name_value in TOOL_NAMES:
            parameters = value.get("parameters")
            if not isinstance(parameters, dict):
                parameters = value.get("input_schema")
            parameter_names: set[str] = set()
            required_names: set[str] = set()
            if isinstance(parameters, dict):
                properties = parameters.get("properties")
                if isinstance(properties, dict):
                    parameter_names = set(properties) & SCHEMA_KEYS
                required = parameters.get("required")
                if isinstance(required, list):
                    required_names = {item for item in required if isinstance(item, str) and item in SCHEMA_KEYS}
            out.append(
                {
                    "tool_name": name_value,
                    "json_path": path,
                    "parameter_names_allowlisted": sorted(parameter_names),
                    "required_names_allowlisted": sorted(required_names),
                }
            )
        for key, child in value.items():
            walk_json(child, f"{path}.{key}", out)
    elif isinstance(value, list):
        for index, child in enumerate(value):
            walk_json(child, f"{path}[{index}]", out)
    return out


def find_positions(haystack: bytes, needle: bytes, max_positions: int = 4) -> tuple[int, list[int]]:
    positions: list[int] = []
    cursor = 0
    while len(positions) < max_positions:
        found = haystack.find(needle, cursor)
        if found < 0:
            break
        positions.append(found)
        cursor = found + max(1, len(needle))
    return (len(positions), positions)


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--package", required=True)
    parser.add_argument("--headers", required=True)
    parser.add_argument("--source-url", required=True)
    parser.add_argument("--signature-json", required=True)
    args = parser.parse_args()

    package_path = args.package
    package_hash = hashlib.sha256()
    with open(package_path, "rb") as package_stream:
        while chunk := package_stream.read(1024 * 1024):
            package_hash.update(chunk)
    headers: dict[str, str] = {}
    if os.path.exists(args.headers):
        for line in open(args.headers, encoding="latin-1", errors="replace"):
            if ":" not in line:
                continue
            name, value = line.split(":", 1)
            key = name.strip().lower()
            if key in {"date", "last-modified", "etag", "content-type", "content-length"}:
                headers[key] = value.strip()[:256]

    with zipfile.ZipFile(package_path, "r") as archive:
        infos = [info for info in archive.infolist() if not info.is_dir()]
        names = [info.filename for info in infos]
        manifest_info = next((info for info in infos if info.filename.lower() == "appxmanifest.xml"), None)
        if manifest_info is None:
            raise RuntimeError("MSIX has no root AppxManifest.xml")
        manifest_root = ElementTree.fromstring(archive.read(manifest_info))
        identity = next((item for item in manifest_root if item.tag.endswith("}Identity") or item.tag == "Identity"), None)
        if identity is None:
            raise RuntimeError("MSIX manifest has no Identity element")
        applications = [element for element in manifest_root.iter() if element.tag.endswith("}Application") or element.tag == "Application"]
        app_entries = []
        for application in applications[:12]:
            app_entries.append(
                {
                    "id": application.attrib.get("Id"),
                    "executable": application.attrib.get("Executable"),
                    "entry_point": application.attrib.get("EntryPoint"),
                }
            )

        extension_counts = Counter(PurePosixPath(name).suffix.lower() or "<none>" for name in names)
        top_dirs = Counter(name.split("/", 1)[0] for name in names)
        total_uncompressed = sum(info.file_size for info in infos)
        pe_entries = [info for info in infos if PurePosixPath(info.filename).suffix.lower() in PE_SUFFIXES]
        managed_count = 0
        pe_summary = Counter()
        module_reports: list[dict[str, object]] = []
        static_hits: list[dict[str, object]] = []
        text_schemas: list[dict[str, object]] = []

        keyword_paths = ("chatgpt", "codex", "browser", "computer", "agent", "mcp", "webview", "desktop", "plugin")
        executable_names = {PurePosixPath(entry.get("executable") or "").name.lower() for entry in app_entries}

        for info in pe_entries:
            if info.file_size <= 0:
                continue
            with archive.open(info) as stream:
                header = stream.read(min(info.file_size, 2 * 1024 * 1024))
            pe = parse_pe_header(header)
            if pe is None:
                continue
            if pe.get("valid_pe_header"):
                pe_summary[str(pe.get("machine"))] += 1
                if pe.get("managed_cli_header"):
                    managed_count += 1
            lower_path = info.filename.lower()
            basename = PurePosixPath(lower_path).name
            relevant_name = basename in executable_names or any(key in lower_path for key in keyword_paths)
            if relevant_name and len(module_reports) < MAX_MODULES_REPORTED:
                module_reports.append(
                    {
                        "path": info.filename,
                        "bytes": info.file_size,
                        "sha256": sha256_member(archive, info.filename),
                        "pe": pe,
                    }
                )

        schema_count = 0
        for info in infos:
            suffix = PurePosixPath(info.filename).suffix.lower()
            lower_path = info.filename.lower()
            should_scan_text = suffix in TEXT_SUFFIXES and info.file_size <= MAX_ENTRY_BYTES_FOR_TEXT
            should_scan_binary = suffix in PE_SUFFIXES and (
                PurePosixPath(lower_path).name in executable_names
                or any(key in lower_path for key in keyword_paths)
            )
            if not (should_scan_text or should_scan_binary):
                continue
            try:
                data = archive.read(info)
            except (OSError, zipfile.BadZipFile):
                continue
            file_hash = hashlib.sha256(data).hexdigest()
            if suffix == ".json" and len(data) <= MAX_SCHEMA_BYTES:
                try:
                    value = json.loads(data.decode("utf-8-sig"))
                    for schema in walk_json(value):
                        schema_count += 1
                        if len(text_schemas) < 80:
                            text_schemas.append({"path": info.filename, **schema})
                except (UnicodeDecodeError, json.JSONDecodeError):
                    pass

            lowered = data.lower()
            haystacks = [("ascii", lowered), ("utf16le", lowered)]
            for term in STATIC_TERMS:
                term_hits: list[dict[str, object]] = []
                for encoding, haystack in haystacks:
                    encoding_name = "utf-16le" if encoding == "utf16le" else "utf-8"
                    needle = term.encode(encoding_name).lower()
                    count, positions = find_positions(haystack, needle)
                    if count:
                        term_hits.append({"encoding": encoding, "offsets_first_4": positions})
                if term_hits:
                    static_hits.append(
                        {
                            "path": info.filename,
                            "sha256": file_hash,
                            "identifier": term,
                            "matches": term_hits,
                            "evidence_class": "static_string_only",
                        }
                    )

        runtime_indicators = {
            "managed_cli_assemblies": managed_count,
            "native_pe_modules": sum(pe_summary.values()) - managed_count,
            "pe_machine_counts": dict(sorted(pe_summary.items())),
            "electron_asar_present": any(name.lower().endswith(".asar") for name in names),
            "webview2_named_asset_present": any("webview2" in name.lower() for name in names),
            "javascript_assets": sum(1 for name in names if PurePosixPath(name).suffix.lower() in {".js", ".mjs", ".cjs"}),
            "json_assets": sum(1 for name in names if PurePosixPath(name).suffix.lower() == ".json"),
            "wasm_assets": sum(1 for name in names if PurePosixPath(name).suffix.lower() == ".wasm"),
        }

        report = {
            "research_id": "chatgpt-windows-desktop-agent-surfaces-2026-10-28",
            "retrieved_at_utc": datetime.now(timezone.utc).isoformat(timespec="seconds"),
            "runner": {
                "os": os.environ.get("ImageOS", "windows-latest"),
                "workflow_host_sha": os.environ.get("GITHUB_SHA"),
                "workflow_host_ref": os.environ.get("GITHUB_REF_NAME"),
            },
            "acquisition": {
                "source_url": args.source_url,
                "response_headers_allowlisted": headers,
                "package_file": "ChatGPT-x64.msix",
                "package_bytes": os.path.getsize(package_path),
                "package_sha256": package_hash.hexdigest(),
                "authenticode": json.loads(args.signature_json),
            },
            "package_identity": {
                "name": identity.attrib.get("Name"),
                "publisher": identity.attrib.get("Publisher"),
                "version": identity.attrib.get("Version"),
                "processor_architecture": identity.attrib.get("ProcessorArchitecture"),
                "resource_id": identity.attrib.get("ResourceId"),
                "applications": app_entries,
                "manifest_sha256": sha256_member(archive, manifest_info.filename),
                "signature_file_present": any(name.lower() == "appxsignature.p7x" for name in names),
                "block_map_file_present": any(name.lower() == "appxblockmap.xml" for name in names),
            },
            "inventory": {
                "file_count": len(infos),
                "uncompressed_bytes": total_uncompressed,
                "top_level_entry_counts": dict(sorted(top_dirs.items())),
                "extension_counts": dict(sorted(extension_counts.items())),
                "pe_module_count": len(pe_entries),
                "modules": module_reports,
            },
            "runtime_indicators": runtime_indicators,
            "static_interface_evidence": {
                "tool_name_static_strings": sorted(
                    {hit["identifier"] for hit in static_hits if hit["identifier"] in TOOL_NAMES}
                ),
                "tool_name_static_matches": [hit for hit in static_hits if hit["identifier"] in TOOL_NAMES],
                "string_matches": static_hits[:MAX_STRING_MATCHES_REPORTED],
                "string_matches_truncated": len(static_hits) > MAX_STRING_MATCHES_REPORTED,
                "serialized_tool_schema_count": schema_count,
                "serialized_tool_schemas_allowlisted": text_schemas,
                "interpretation": "String presence and JSON matches are static evidence only; they do not prove registration, dispatch, server availability, or live model exposure.",
            },
            "handling": {
                "installed_or_executed": False,
                "raw_package_uploaded": False,
                "raw_code_or_unfiltered_strings_emitted": False,
                "temporary_package_location": "GitHub-hosted runner temp; removed with runner",
            },
        }
        print(json.dumps(report, sort_keys=True, separators=(",", ":")))
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except Exception as exc:
        # Do not echo archive entry names or raw text in failure output.
        print(json.dumps({"inspection_error_type": type(exc).__name__}), file=sys.stderr)
        raise SystemExit(2)
