"""One-use hosted preflight for the two PNG fixtures in the a89 Android test."""

from __future__ import annotations

import ast
import base64
import binascii
import json
import os
import re
import struct
import zlib
from pathlib import Path


TARGET_SHA = "a89b02855334af7c0521882271cb97b818981d1a"
BASE_SHA = "e135df532669849b682dfac712c3f59aded039ba"
HOST_TEST_PATH = ".github/scripts/test_d0_android_protocol_acceptance.py"
SOURCE_FIXTURE_PATH = "codex-rs/core/tests/suite/compact_remote.rs"


class FixtureProblem(Exception):
    def __init__(self, category: str) -> None:
        super().__init__(category)
        self.category = category


def _extract_host_png(path: Path) -> str:
    try:
        tree = ast.parse(path.read_text(encoding="utf-8"))
    except (OSError, UnicodeError, SyntaxError) as error:
        raise FixtureProblem("host_fixture_source_unreadable") from error
    values = [
        node.value.value
        for node in ast.walk(tree)
        if isinstance(node, ast.Assign)
        and any(isinstance(target, ast.Name) and target.id == "_PNG" for target in node.targets)
        and isinstance(node.value, ast.Constant)
        and isinstance(node.value.value, str)
    ]
    if len(values) != 1:
        raise FixtureProblem("host_fixture_assignment_cardinality")
    return values[0]


def _extract_source_png(path: Path) -> str:
    try:
        source = path.read_text(encoding="utf-8")
    except (OSError, UnicodeError) as error:
        raise FixtureProblem("source_fixture_unreadable") from error
    matches = re.findall(
        r'let image_url = "data:image/png;base64,([A-Za-z0-9+/=]+)";', source
    )
    if len(matches) != 1:
        raise FixtureProblem("source_fixture_assignment_cardinality")
    return matches[0]


def _validate_one_pixel_grayscale_alpha_png(encoded: str) -> dict[str, int | str]:
    try:
        payload = base64.b64decode(encoded, validate=True)
    except (ValueError, binascii.Error) as error:
        raise FixtureProblem("base64_invalid") from error

    if not payload.startswith(b"\x89PNG\r\n\x1a\n"):
        raise FixtureProblem("png_signature_invalid")

    offset = 8
    chunk_types: list[bytes] = []
    image_data = bytearray()
    header: tuple[int, int, int, int, int, int, int] | None = None
    ended = False

    while offset < len(payload):
        if len(payload) - offset < 12:
            raise FixtureProblem("png_chunk_header_truncated")
        length = struct.unpack_from(">I", payload, offset)[0]
        end = offset + 12 + length
        if end > len(payload):
            raise FixtureProblem("png_chunk_length_invalid")
        chunk_type = payload[offset + 4 : offset + 8]
        chunk_data = payload[offset + 8 : offset + 8 + length]
        stored_crc = struct.unpack_from(">I", payload, offset + 8 + length)[0]
        if (binascii.crc32(chunk_type + chunk_data) & 0xFFFFFFFF) != stored_crc:
            raise FixtureProblem("png_chunk_crc_mismatch")
        chunk_types.append(chunk_type)

        if chunk_type == b"IHDR":
            if header is not None or length != 13:
                raise FixtureProblem("png_header_shape_invalid")
            header = struct.unpack(">IIBBBBB", chunk_data)
        elif chunk_type == b"IDAT":
            image_data.extend(chunk_data)
        elif chunk_type == b"IEND":
            if length != 0 or end != len(payload):
                raise FixtureProblem("png_end_shape_invalid")
            ended = True
            offset = end
            break
        else:
            raise FixtureProblem("png_unexpected_chunk")
        offset = end

    if not ended or header is None or chunk_types != [b"IHDR", b"IDAT", b"IEND"]:
        raise FixtureProblem("png_chunk_sequence_invalid")
    if header != (1, 1, 8, 4, 0, 0, 0):
        raise FixtureProblem("png_header_parameters_invalid")

    decoder = zlib.decompressobj()
    try:
        scanline = decoder.decompress(bytes(image_data)) + decoder.flush()
    except zlib.error as error:
        raise FixtureProblem("png_zlib_invalid") from error
    if not decoder.eof or decoder.unused_data or decoder.unconsumed_tail:
        raise FixtureProblem("png_zlib_stream_invalid")
    # A 1x1, 8-bit grayscale-alpha row is one filter byte plus two pixel bytes.
    if len(scanline) != 3 or scanline[0] != 0:
        raise FixtureProblem("png_decoded_scanline_invalid")
    return {"width": 1, "height": 1, "chunk_count": 3, "decoded_scanline_bytes": 3}


def main() -> int:
    workspace = Path(os.environ.get("GITHUB_WORKSPACE", "."))
    target_dir = Path(os.environ.get("VALIDATION_TARGET_DIR", ""))
    host_dir = Path(os.environ.get("VALIDATION_HELPER_DIR", ""))
    target_sha = os.environ.get("VALIDATION_TARGET_SHA", "")
    base_sha = os.environ.get("VALIDATION_BASE_SHA", "")
    harness_sha = os.environ.get("VALIDATION_HARNESS_SHA", "")

    result: dict[str, object] = {
        "schema_version": "a89-android-png-preflight-v1",
        "identity": {
            "target_sha": target_sha,
            "base_sha": base_sha,
            "harness_sha": harness_sha,
        },
        "fixtures": [],
    }
    output_path = workspace / "android-png-fixture-preflight-results.json"
    if target_sha != TARGET_SHA or base_sha != BASE_SHA or not harness_sha:
        result.update({"status": "failure", "failure_code": "identity_mismatch"})
        output_path.write_text(json.dumps(result, indent=2, sort_keys=True) + "\n", encoding="utf-8")
        print(json.dumps({"status": result["status"], "failure_code": result["failure_code"]}, sort_keys=True))
        return 1

    try:
        host_png = _extract_host_png(host_dir / HOST_TEST_PATH)
        source_png = _extract_source_png(target_dir / SOURCE_FIXTURE_PATH)
        fixture_results = []
        for name, encoded in (
            ("synthetic_android_screenshot", host_png),
            ("source_compact_remote_fixture", source_png),
        ):
            try:
                details = _validate_one_pixel_grayscale_alpha_png(encoded)
            except FixtureProblem as error:
                fixture_results.append({
                    "name": name,
                    "status": "invalid",
                    "failure_code": error.category,
                })
            else:
                fixture_results.append({"name": name, "status": "valid", **details})
        result["fixtures"] = fixture_results
        invalid_fixtures = [item for item in fixture_results if item["status"] != "valid"]
        if invalid_fixtures:
            result.update({
                "status": "failure",
                "failure_code": "fixture_invalid",
            })
        else:
            result.update({"status": "success", "failure_code": ""})
    except FixtureProblem as error:
        result.update({
            "status": "failure",
            "failure_code": error.category,
        })

    output_path.write_text(json.dumps(result, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    print(json.dumps({
        "status": result["status"],
        "failure_code": result["failure_code"],
        "fixtures": result["fixtures"],
    }, sort_keys=True))
    return 0 if result["status"] == "success" else 1


if __name__ == "__main__":
    raise SystemExit(main())
