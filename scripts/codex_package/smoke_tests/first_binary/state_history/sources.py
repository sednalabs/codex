"""Independent historical SQL identity and version mapping for hosted fixtures.

Only SQL bytes from the frozen U/F sources may generate a profile. The reviewed
bridge manifest is pinned by digest, then each referenced SQL file is checked
against its Git blob or SHA-384 identity before any fixture database is made.
"""

from __future__ import annotations

import hashlib
from dataclasses import dataclass
from pathlib import Path

UPSTREAM_SHA = "76a6e55d5ac69e3ba6b0481f8f3b94256f46dfa7"
UPSTREAM_TREE = "01cd0e4b1c5aa80b5befbc26c37df47f93129bf8"
HISTORICAL_FORK_SHA = "def481c8fef0ca460b997238f1edbf39e7f33ed9"
BRIDGE_MANIFEST_SHA256 = (
    "3ae1c53a4fb44b051f297c7e12d55c6f1fd45ea94729ab807a11e78bbb23c564"
)
FORK_BASE = 8_000_000_000_000_000
OLD_FORK_TO_NEW = {
    56: FORK_BASE,
    57: FORK_BASE + 1,
    58: FORK_BASE + 2,
    9000: FORK_BASE + 3,
    9001: FORK_BASE + 4,
    9002: FORK_BASE + 5,
    9003: FORK_BASE + 6,
}

# Exact deployed 24-50 sequence from F's migration_repair.rs. Values refer
# to frozen U versions or to the seven new fork namespaced versions above.
SHIFTED_TO_CANONICAL = {
    24: FORK_BASE + 3,
    25: 24,
    26: 25,
    27: FORK_BASE + 4,
    28: FORK_BASE + 5,
    29: 26,
    30: 27,
    31: 28,
    32: 29,
    33: 30,
    34: 31,
    35: 32,
    36: 33,
    37: 34,
    38: FORK_BASE,
    39: 35,
    40: 40,
    41: 41,
    42: 42,
    43: 39,
    44: 36,
    45: FORK_BASE + 1,
    46: 37,
    47: 38,
    48: 43,
    49: 44,
    50: FORK_BASE + 6,
}

EXPECTED_FORK_SQL_SHA384 = {
    56: "c021f9cc67108fe6dfbbc58a40d20ffeb716724f1283681eb23e6f639b48a1739a7eb1ea222ed3679941d48f72101284",
    57: "dd42ba9960d4e01c554bfac6a27c67f3c21e80bdc8e0dd928a90d208e1e680976a3ea356a9f9360f9760a5ca42659053",
    58: "448d0f4ab51b856fc182d97e5e408861630fc387d7316473426771c298581881016018a34d45145e0e503179dc7efd84",
    9000: "f0392a5480c11466f48752cdad6e1bcd05fafa3c17ba0e54f04b02dbd9b5bf9976c107d8159abc225e1a4dc0191065bf",
    9001: "d391b088bc785fcc6d8e61abbf2702e65175ec0e2d30df51e4ff6e51d1ed5659839058ef82d046683ebb8009c5ba1317",
    9002: "f1db36e9fbf40560e028c5c606f821cbf6040932a7a37d3bf47da118c7622c97789a25636376b9ed1da38e3ba8b3856d",
    9003: "e3dcfd70c67f1ec352fd635f7e633d246e1d8ad70668eb482ca201c5688b435028b6bbe603d5eb23c4397c6f2184b7d6",
}
EXPECTED_U_56_58_SHA384 = {
    56: "5de44fac5505249be66fcd640e9ac3d36550854c0db1272eb52cbb6c3d18b207db4cb782686d81089e77ebb57f6819b1",
    57: "729562c57bfa8b43d5ae3476c99cef6e81a54709a74283057ce8d2d6f55cf8eae2594fdb7cff4238ebe1e481e7c049e6",
    58: "68467af3ce1ff09777c3f7167d522e11d88586fbb80a1588c631cf643a7d4985b221a6c8e8623997b37d44605b7e1091",
}


@dataclass(frozen=True)
class HistoricalSql:
    canonical_version: int
    description: str
    content: bytes
    checksum: bytes
    filename: str
    provenance: str


def _git_blob_sha1(content: bytes) -> str:
    prefix = f"blob {len(content)}\0".encode()
    return hashlib.sha1(prefix + content).hexdigest()


class HistoricalSources:
    def __init__(self, source_root: Path):
        self.source_root = source_root.resolve()
        state_root = self.source_root / "codex-rs" / "state"
        manifest = state_root / "migration_namespace_manifest.txt"
        manifest_bytes = manifest.read_bytes()
        actual_manifest = hashlib.sha256(manifest_bytes).hexdigest()
        if actual_manifest != BRIDGE_MANIFEST_SHA256:
            raise AssertionError(
                f"bridge migration manifest changed: {actual_manifest}"
            )
        self.migrations_root = state_root / "migrations"
        self.by_version: dict[int, HistoricalSql] = {}
        for raw_line in manifest_bytes.decode().splitlines():
            line = raw_line.strip()
            if not line or line.startswith("#"):
                continue
            kind, filename, expected_digest = line.split()
            version = int(filename.split("_", 1)[0])
            if version in self.by_version:
                raise AssertionError(f"duplicate migration version {version}")
            content = (self.migrations_root / filename).read_bytes()
            digest = (
                _git_blob_sha1(content)
                if kind == "u"
                else hashlib.sha384(content).hexdigest()
            )
            if digest != expected_digest:
                raise AssertionError(f"historical SQL changed: {filename}")
            description = filename.split("_", 1)[1].removesuffix(".sql").replace("_", " ")
            self.by_version[version] = HistoricalSql(
                canonical_version=version,
                description=description,
                content=content,
                checksum=hashlib.sha384(content).digest(),
                filename=filename,
                provenance=UPSTREAM_SHA if kind == "u" else HISTORICAL_FORK_SHA,
            )
        if set(self.by_version) != set(range(1, 59)) | set(
            range(FORK_BASE, FORK_BASE + 7)
        ):
            raise AssertionError("historical SQL source set is incomplete")
        for old, canonical in OLD_FORK_TO_NEW.items():
            checksum = hashlib.sha384(self.by_version[canonical].content).hexdigest()
            if checksum != EXPECTED_FORK_SQL_SHA384[old]:
                raise AssertionError(f"fork migration {old} checksum changed")
        for version, expected in EXPECTED_U_56_58_SHA384.items():
            if hashlib.sha384(self.by_version[version].content).hexdigest() != expected:
                raise AssertionError(f"upstream migration {version} checksum changed")

    def script(self, canonical_version: int) -> HistoricalSql:
        return self.by_version[canonical_version]

    def old_fork_script(self, old_version: int) -> HistoricalSql:
        return self.script(OLD_FORK_TO_NEW[old_version])
