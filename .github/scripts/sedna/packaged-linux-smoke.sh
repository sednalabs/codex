#!/usr/bin/env bash
set -euo pipefail

required=(GH_TOKEN GITHUB_REPOSITORY PRODUCT_SHA WORKFLOW_SHA PLATFORM TARGET PACKAGE_INPUTS_JSON PRODUCT_DIR WORKFLOW_DIR RUNNER_TEMP)
for name in "${required[@]}"; do
  if [[ -z "${!name:-}" ]]; then
    echo "missing required environment variable: ${name}" >&2
    exit 2
  fi
done

readonly EXPECTED_PRODUCT_SHA="def481c8fef0ca460b997238f1edbf39e7f33ed9"
readonly EXPECTED_WORKFLOW_PATH=".github/workflows/sedna-branch-build.yml"
readonly EVIDENCE_DIR="${RUNNER_TEMP}/codex-packaged-smoke/${PLATFORM}"
readonly WORK_DIR="${RUNNER_TEMP}/codex-packaged-work/${PLATFORM}"
umask 077
mkdir -p "${EVIDENCE_DIR}"
mkdir -m 700 "${WORK_DIR}"
printf '%s\n' "${PACKAGE_INPUTS_JSON}" > "${EVIDENCE_DIR}/package-inputs.json"

if [[ "${PRODUCT_SHA}" != "${EXPECTED_PRODUCT_SHA}" ]]; then
  echo "unexpected product SHA: ${PRODUCT_SHA}" >&2
  exit 2
fi
if [[ "$(git -C "${WORKFLOW_DIR}" rev-parse HEAD)" != "${WORKFLOW_SHA}" ]]; then
  echo "workflow checkout does not match the workflow-host SHA" >&2
  exit 2
fi
if [[ "$(git -C "${PRODUCT_DIR}" rev-parse HEAD)" != "${PRODUCT_SHA}" ]]; then
  echo "product checkout does not match the requested product SHA" >&2
  exit 2
fi

case "${PLATFORM}:${TARGET}:$(uname -m)" in
  linux-x86_64:x86_64-unknown-linux-gnu:x86_64) ;;
  linux-aarch64:aarch64-unknown-linux-gnu:aarch64) ;;
  *)
    echo "runner platform, target, and native architecture disagree" >&2
    exit 2
    ;;
esac

jq -e --arg sha "${PRODUCT_SHA}" --arg target "${TARGET}" --arg platform "${PLATFORM}" \
  '.schema_version == 1 and .product_sha == $sha and .artifacts[$platform].target == $target' \
  "${EVIDENCE_DIR}/package-inputs.json" >/dev/null

api_root="${GITHUB_API_URL:-https://api.github.com}"
artifact_root="${WORK_DIR}/artifact-zips"
mkdir -m 700 "${artifact_root}"

fetch_component() {
  local component="$1"
  local record run_id artifact_id expected_name expected_digest expected_host_sha expected_host_branch
  local run_json artifacts_json artifact_json zip_path zip_digest extract_dir
  local archive_name metadata_name archive_sha_name binary_sha_name

  record="$(jq -ce --arg platform "${PLATFORM}" --arg component "${component}" '.artifacts[$platform][$component]' "${EVIDENCE_DIR}/package-inputs.json")"
  run_id="$(jq -er '.run_id | tostring' <<<"${record}")"
  artifact_id="$(jq -er '.artifact_id | tostring' <<<"${record}")"
  expected_name="$(jq -er '.name' <<<"${record}")"
  expected_digest="$(jq -er '.digest' <<<"${record}")"
  expected_host_sha="$(jq -er '.workflow_head_sha' <<<"${record}")"
  expected_host_branch="$(jq -er '.workflow_head_branch' <<<"${record}")"
  archive_name="$(jq -er '.archive_name' <<<"${record}")"
  metadata_name="$(jq -er '.metadata_name' <<<"${record}")"

  run_json="${artifact_root}/${component}-run.json"
  artifacts_json="${artifact_root}/${component}-artifacts.json"
  artifact_json="${artifact_root}/${component}-artifact.json"
  zip_path="${artifact_root}/${component}.zip"
  extract_dir="${WORK_DIR}/${component}"
  mkdir -m 700 "${extract_dir}"

  curl --fail --silent --show-error --location --retry 2 \
    -H "Authorization: Bearer ${GH_TOKEN}" \
    -H "Accept: application/vnd.github+json" \
    -H "X-GitHub-Api-Version: 2022-11-28" \
    "${api_root}/repos/${GITHUB_REPOSITORY}/actions/runs/${run_id}" \
    -o "${run_json}"
  jq -e --argjson id "${run_id}" --arg sha "${expected_host_sha}" --arg branch "${expected_host_branch}" \
    --arg repo "${GITHUB_REPOSITORY}" --arg path "${EXPECTED_WORKFLOW_PATH}" \
    '.id == $id and .head_sha == $sha and .head_branch == $branch and .head_repository.full_name == $repo and .path == $path and .event == "workflow_dispatch" and .status == "completed" and .conclusion == "success" and .run_attempt == 1' \
    "${run_json}" >/dev/null

  curl --fail --silent --show-error --location --retry 2 \
    -H "Authorization: Bearer ${GH_TOKEN}" \
    -H "Accept: application/vnd.github+json" \
    -H "X-GitHub-Api-Version: 2022-11-28" \
    "${api_root}/repos/${GITHUB_REPOSITORY}/actions/runs/${run_id}/artifacts" \
    -o "${artifacts_json}"
  jq -e --argjson id "${artifact_id}" --arg name "${expected_name}" --arg digest "${expected_digest}" --argjson run "${run_id}" \
    '[.artifacts[] | select(.id == $id)] as $matches | ($matches | length) == 1 and $matches[0].name == $name and $matches[0].digest == $digest and $matches[0].expired == false and $matches[0].workflow_run.id == $run' \
    "${artifacts_json}" >/dev/null
  jq -ce --argjson id "${artifact_id}" '.artifacts[] | select(.id == $id)' "${artifacts_json}" > "${artifact_json}"

  curl --fail --silent --show-error --location --retry 2 \
    -H "Authorization: Bearer ${GH_TOKEN}" \
    -H "Accept: application/vnd.github+json" \
    -H "X-GitHub-Api-Version: 2022-11-28" \
    "${api_root}/repos/${GITHUB_REPOSITORY}/actions/artifacts/${artifact_id}/zip" \
    -o "${zip_path}"
  zip_digest="sha256:$(sha256sum "${zip_path}" | cut -d ' ' -f 1)"
  if [[ "${zip_digest}" != "${expected_digest}" ]]; then
    echo "artifact ZIP digest mismatch for ${component}: ${zip_digest}" >&2
    exit 1
  fi

  if [[ "${component}" == "host" ]]; then
    archive_sha_name="$(jq -er '.archive_sha_name' <<<"${record}")"
    binary_sha_name="$(jq -er '.binary_sha_name' <<<"${record}")"
    python3 - "${zip_path}" "${extract_dir}" "${archive_name}" "${metadata_name}" "${archive_sha_name}" "${binary_sha_name}" <<'PY'
import stat
import sys
import zipfile
from pathlib import PurePosixPath

archive, destination, *expected = sys.argv[1:]
with zipfile.ZipFile(archive) as bundle:
    members = bundle.infolist()
    names = [member.filename for member in members]
    if sorted(names) != sorted(expected) or len(names) != len(set(names)):
        raise SystemExit(f"unexpected artifact ZIP members: {names!r}")
    for member in members:
        path = PurePosixPath(member.filename)
        mode = member.external_attr >> 16
        if path.is_absolute() or len(path.parts) != 1 or ".." in path.parts:
            raise SystemExit(f"unsafe artifact ZIP path: {member.filename!r}")
        if stat.S_ISLNK(mode) or member.is_dir():
            raise SystemExit(f"non-regular artifact ZIP member: {member.filename!r}")
        with bundle.open(member) as source, open(f"{destination}/{member.filename}", "xb") as target:
            target.write(source.read())
PY
  else
    python3 - "${zip_path}" "${extract_dir}" "${archive_name}" "${metadata_name}" <<'PY'
import stat
import sys
import zipfile
from pathlib import PurePosixPath

archive, destination, *expected = sys.argv[1:]
with zipfile.ZipFile(archive) as bundle:
    members = bundle.infolist()
    names = [member.filename for member in members]
    if sorted(names) != sorted(expected) or len(names) != len(set(names)):
        raise SystemExit(f"unexpected artifact ZIP members: {names!r}")
    for member in members:
        path = PurePosixPath(member.filename)
        mode = member.external_attr >> 16
        if path.is_absolute() or len(path.parts) != 1 or ".." in path.parts:
            raise SystemExit(f"unsafe artifact ZIP path: {member.filename!r}")
        if stat.S_ISLNK(mode) or member.is_dir():
            raise SystemExit(f"non-regular artifact ZIP member: {member.filename!r}")
        with bundle.open(member) as source, open(f"{destination}/{member.filename}", "xb") as target:
            target.write(source.read())
PY
  fi

  jq -e --arg sha "${PRODUCT_SHA}" --arg target "${TARGET}" --arg repo "${GITHUB_REPOSITORY}" \
    --arg run "${run_id}" '.repository == $repo and .commit == $sha and .ref == $sha and .target == $target and (.workflow | endswith($run))' \
    "${extract_dir}/${metadata_name}" >/dev/null
  if [[ "${component}" == "host" ]]; then
    jq -e --arg host_sha "${expected_host_sha}" '.workflowCommit == $host_sha and .artifact == "codex-code-mode-host"' \
      "${extract_dir}/${metadata_name}" >/dev/null
    (cd "${extract_dir}" && sha256sum -c "${binary_sha_name}")
    (cd "${extract_dir}" && sha256sum -c "${archive_sha_name}")
  fi

  mkdir -m 700 "${extract_dir}/unpacked"
  python3 - "${extract_dir}/${archive_name}" "${extract_dir}/unpacked" "${component}" <<'PY'
import os
import sys
import tarfile
from pathlib import PurePosixPath

archive, destination, component = sys.argv[1:]
expected = {"codex", "codex-responses-api-proxy"} if component == "core" else {"codex-code-mode-host"}
with tarfile.open(archive, "r:gz") as package:
    members = package.getmembers()
    files = {}
    for member in members:
        name = member.name.removeprefix("./")
        path = PurePosixPath(name)
        if member.isdir() and name in ("", "."):
            continue
        if path.is_absolute() or len(path.parts) != 1 or ".." in path.parts:
            raise SystemExit(f"unsafe package path: {member.name!r}")
        if not member.isfile() or name not in expected or name in files:
            raise SystemExit(f"unexpected package member: {member.name!r}")
        files[name] = member
    if set(files) != expected:
        raise SystemExit(f"package members differ: got {sorted(files)} expected {sorted(expected)}")
    for name, member in files.items():
        source = package.extractfile(member)
        if source is None:
            raise SystemExit(f"cannot read packaged member: {name}")
        output = os.path.join(destination, name)
        with source, open(output, "xb") as target:
            while chunk := source.read(1024 * 1024):
                target.write(chunk)
        os.chmod(output, 0o755)
PY

  jq -n --arg component "${component}" --arg run_id "${run_id}" --arg artifact_id "${artifact_id}" \
    --arg name "${expected_name}" --arg digest "${expected_digest}" --arg archive "${archive_name}" \
    --arg metadata "${metadata_name}" --arg host_sha "${expected_host_sha}" --arg host_branch "${expected_host_branch}" \
    --arg zip_sha256 "${zip_digest}" \
    '{component:$component,run_id:$run_id,artifact_id:$artifact_id,name:$name,digest:$digest,archive:$archive,metadata:$metadata,workflow_head_sha:$host_sha,workflow_head_branch:$host_branch,verified_zip_sha256:$zip_sha256}' \
    > "${EVIDENCE_DIR}/${component}-identity.json"
}

fetch_component core
fetch_component host

package_dir="${WORK_DIR}/package"
mkdir -m 700 "${package_dir}"
install -m 755 "${WORK_DIR}/core/unpacked/codex" "${package_dir}/codex"
install -m 755 "${WORK_DIR}/core/unpacked/codex-responses-api-proxy" "${package_dir}/codex-responses-api-proxy"
install -m 755 "${WORK_DIR}/host/unpacked/codex-code-mode-host" "${package_dir}/codex-code-mode-host"
install -m 600 "${WORK_DIR}/core/$(jq -er '.archive_name' <<<"$(jq -ce --arg p "${PLATFORM}" '.artifacts[$p].core' "${EVIDENCE_DIR}/package-inputs.json")")" "${package_dir}/core-archive.tar.gz"
install -m 600 "${WORK_DIR}/host/$(jq -er '.archive_name' <<<"$(jq -ce --arg p "${PLATFORM}" '.artifacts[$p].host' "${EVIDENCE_DIR}/package-inputs.json")")" "${package_dir}/host-archive.tar.gz"

for binary in codex codex-responses-api-proxy codex-code-mode-host; do
  readelf -h "${package_dir}/${binary}" > "${EVIDENCE_DIR}/${binary}-elf.txt"
  grep -q 'Machine:.*' "${EVIDENCE_DIR}/${binary}-elf.txt"
  case "${TARGET}" in
    x86_64-unknown-linux-gnu) grep -q 'Advanced Micro Devices X86-64' "${EVIDENCE_DIR}/${binary}-elf.txt" ;;
    aarch64-unknown-linux-gnu) grep -q 'AArch64' "${EVIDENCE_DIR}/${binary}-elf.txt" ;;
  esac
done
test -x "${package_dir}/codex" -a -x "${package_dir}/codex-code-mode-host" -a -x "${package_dir}/codex-responses-api-proxy"

python3 - "${package_dir}/codex-code-mode-host" "${EVIDENCE_DIR}/code-mode-host.stderr.log" "${EVIDENCE_DIR}/code-mode-protocol.json" <<'PY'
import json
import os
import select
import struct
import subprocess
import sys

host, stderr_path, receipt_path = sys.argv[1:]
process = subprocess.Popen(
    [host, "--listen", "stdio"],
    stdin=subprocess.PIPE,
    stdout=subprocess.PIPE,
    stderr=open(stderr_path, "wb"),
    bufsize=0,
)

def read_exact(size: int, timeout: int = 30) -> bytes:
    chunks = bytearray()
    while len(chunks) < size:
        ready, _, _ = select.select([process.stdout], [], [], timeout)
        if not ready:
            raise TimeoutError("timed out waiting for packaged code-mode host protocol")
        chunk = os.read(process.stdout.fileno(), size - len(chunks))
        if not chunk:
            raise EOFError("packaged code-mode host closed its protocol stream")
        chunks.extend(chunk)
    return bytes(chunks)

def send(value: dict) -> None:
    payload = json.dumps(value, separators=(",", ":")).encode()
    process.stdin.write(struct.pack("<I", len(payload)) + payload)
    process.stdin.flush()

def receive() -> dict:
    size = struct.unpack("<I", read_exact(4))[0]
    if size > 64 * 1024 * 1024:
        raise ValueError("packaged code-mode host frame exceeds the protocol limit")
    return json.loads(read_exact(size))

def receive_until(predicate) -> dict:
    for _ in range(8):
        message = receive()
        if predicate(message):
            return message
    raise AssertionError("expected packaged code-mode host response was not received")

try:
    send({"type":"connection/hello","supportedVersions":[1],"requiredCapabilities":[],"optionalCapabilities":[]})
    hello = receive()
    assert hello == {"type":"connection/ready","selectedVersion":1,"capabilities":[]}, hello
    session_id = "packaged-smoke-session"
    send({"type":"operation/request","id":1,"request":{"method":"session/open","sessionId":session_id}})
    opened = receive_until(lambda item: item.get("type") == "operation/response" and item.get("id") == 1)
    assert opened.get("result", {}).get("status") == "ok", opened
    assert opened["result"]["value"] == {"type":"session/ready","sessionId":session_id}, opened
    send({
        "type":"operation/request",
        "id":2,
        "request":{
            "method":"session/execute",
            "sessionId":session_id,
            "request":{
                "tool_call_id":"packaged-smoke-call",
                "enabled_tools":[],
                "source":"text('packaged-code-mode-host-ok');",
                "yield_time_ms":10000,
                "max_output_tokens":128,
            },
        },
    })
    started = receive_until(lambda item: item.get("type") == "operation/response" and item.get("id") == 2)
    assert started.get("result", {}).get("status") == "ok", started
    assert started["result"]["value"].get("type") == "execution/started", started
    executed = receive_until(lambda item: item.get("type") == "execute/initialResponse" and item.get("id") == 2)
    runtime = executed.get("result", {}).get("value", {}).get("Result", {})
    assert executed.get("result", {}).get("status") == "ok", executed
    assert runtime.get("error_text") is None, executed
    assert any(item.get("text") == "packaged-code-mode-host-ok" for item in runtime.get("content_items", [])), executed
    send({"type":"operation/request","id":3,"request":{"method":"session/shutdown","sessionId":session_id}})
    closed = receive_until(lambda item: item.get("type") == "operation/response" and item.get("id") == 3)
    assert closed.get("result", {}).get("status") == "ok", closed
    assert closed["result"]["value"] == {"type":"session/closed","sessionId":session_id}, closed
    process.stdin.close()
    exit_code = process.wait(timeout=15)
    if exit_code != 0:
        raise RuntimeError(f"packaged code-mode host exited with status {exit_code}")
    with open(receipt_path, "w", encoding="utf-8") as receipt:
        json.dump({"protocol_version":1,"session_open":True,"code_execution":True,"session_shutdown":True,"output":"packaged-code-mode-host-ok"}, receipt, sort_keys=True, indent=2)
except BaseException:
    if process.poll() is None:
        process.terminate()
        try:
            process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait(timeout=5)
    raise
PY

fixture_home="${WORK_DIR}/codex-home"
mkdir -m 700 "${fixture_home}"
python3 "${WORKFLOW_DIR}/.github/scripts/sedna/create-legacy-state-fixture.py" \
  --migrations-dir "${PRODUCT_DIR}/codex-rs/state/migrations" \
  --database-path "${fixture_home}/state_5.sqlite" \
  --receipt-path "${EVIDENCE_DIR}/legacy-fixture.json"

python3 - "${package_dir}/codex" "${fixture_home}" "${PRODUCT_DIR}/codex-rs/state/migrations" "${EVIDENCE_DIR}" <<'PY'
import hashlib
import json
import os
import re
import select
import sqlite3
import subprocess
import sys
from pathlib import Path

codex, codex_home, migrations_dir, evidence_dir = sys.argv[1:]
database_path = Path(codex_home) / "state_5.sqlite"
migrations_dir = Path(migrations_dir)
evidence_dir = Path(evidence_dir)
migrations = {}
for path in migrations_dir.glob("*.sql"):
    match = re.fullmatch(r"(\d+)_([a-z0-9_]+)\.sql", path.name)
    if not match:
        raise AssertionError(f"unexpected state migration filename: {path.name}")
    migrations[int(match.group(1))] = (match.group(2).replace("_", " "), hashlib.sha256(path.read_bytes()).digest())

def request(process, request_id, method, params):
    process.stdin.write((json.dumps({"jsonrpc":"2.0","id":request_id,"method":method,"params":params}, separators=(",", ":")) + "\n").encode())
    process.stdin.flush()
    for _ in range(100):
        ready, _, _ = select.select([process.stdout], [], [], 30)
        if not ready:
            raise TimeoutError(f"app-server timed out waiting for {method}")
        line = process.stdout.readline()
        if not line:
            raise EOFError(f"app-server closed stdout before {method}")
        message = json.loads(line)
        if message.get("id") == request_id:
            if "error" in message:
                raise RuntimeError(f"app-server {method} failed: {message['error']}")
            return message.get("result")
    raise AssertionError(f"app-server did not answer {method}")

def start_and_query(round_number):
    stderr_path = evidence_dir / f"app-server-round-{round_number}.stderr.log"
    env = os.environ.copy()
    env["CODEX_HOME"] = codex_home
    env.pop("CODEX_CODE_MODE_HOST_PATH", None)
    env["PATH"] = str(Path(codex).parent) + os.pathsep + env.get("PATH", "")
    with open(stderr_path, "wb") as stderr:
        process = subprocess.Popen(
            [codex, "app-server", "--listen", "stdio://"],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=stderr,
            env=env,
            bufsize=0,
        )
        try:
            initialized = request(process, 1, "initialize", {
                "clientInfo":{"name":"codex-packaged-smoke","title":"Codex package smoke","version":"1"},
                "capabilities":{"experimentalApi":True,"requestAttestation":False},
            })
            if not isinstance(initialized, dict):
                raise AssertionError(f"unexpected initialize response: {initialized!r}")
            process.stdin.write((json.dumps({"jsonrpc":"2.0","method":"initialized"}, separators=(",", ":")) + "\n").encode())
            process.stdin.flush()
            models = request(process, 2, "model/list", {"includeHidden":True,"limit":100})
            if not isinstance(models, dict) or not isinstance(models.get("data"), list):
                raise AssertionError(f"unexpected model/list response: {models!r}")
            model_ids = sorted({model.get("id") or model.get("model") for model in models["data"] if isinstance(model, dict)})
            model_ids = [model_id for model_id in model_ids if isinstance(model_id, str)]
            if "gpt-6-luna" not in model_ids:
                raise AssertionError("model/list did not expose the expected gpt-6-luna catalog entry")
            process.stdin.close()
            exit_code = process.wait(timeout=30)
            if exit_code != 0:
                raise RuntimeError(f"packaged app-server exited with status {exit_code}")
            return {"initialize":True,"model_count":len(model_ids),"expected_catalog_entry":"gpt-6-luna","catalog_entry_present":True}
        except BaseException:
            if process.poll() is None:
                process.terminate()
                try:
                    process.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait(timeout=5)
            raise

def inspect_migration_state():
    with sqlite3.connect(database_path) as connection:
        rows = connection.execute(
            "SELECT version, description, success, checksum FROM _sqlx_migrations WHERE version IN (24,25,26,27,28,29,58,9000,9001,9002) ORDER BY version"
        ).fetchall()
        expected_versions = {24,25,26,27,28,29,58,9000,9001,9002}
        actual_versions = {row[0] for row in rows}
        if actual_versions != expected_versions or len(rows) != 10:
            raise AssertionError(f"canonical migration set differs: {sorted(actual_versions)}")
        for version, description, success, checksum in rows:
            if version not in migrations:
                raise AssertionError(f"migration {version} is not present in the checked-out product source")
            expected_description, expected_checksum = migrations[version]
            if not success or description != expected_description or checksum != expected_checksum:
                raise AssertionError(f"migration {version} metadata does not match the product source")
        thread = connection.execute(
            "SELECT title, first_user_message, created_at_ms, updated_at_ms, preview, thread_source FROM threads WHERE id = ?",
            ("thread-preserved",),
        ).fetchone()
        expected_thread = ("legacy title", "legacy first message", 1_700_000_000_000, 1_700_000_001_000, "legacy first message", None)
        if thread != expected_thread:
            raise AssertionError(f"legacy thread changed unexpectedly: {thread!r}")
        dynamic_tool = connection.execute(
            "SELECT namespace, namespace_description, persist_on_resume, capability_json FROM thread_dynamic_tools WHERE thread_id = ?",
            ("thread-preserved",),
        ).fetchone()
        if dynamic_tool != (None, None, 1, None):
            raise AssertionError(f"legacy dynamic tool changed unexpectedly: {dynamic_tool!r}")
        return {"canonical_migration_rows":len(rows),"versions":sorted(actual_versions),"preserved_thread":True,"preserved_dynamic_tool":True}

first_round = start_and_query(1)
first_migrations = inspect_migration_state()
second_round = start_and_query(2)
second_migrations = inspect_migration_state()
if first_migrations != second_migrations:
    raise AssertionError("migration repair/reopen state changed across the second packaged startup")
if first_round != second_round:
    raise AssertionError("app-server catalog response changed across the second packaged startup")
with open(evidence_dir / "packaged-app-server.json", "w", encoding="utf-8") as receipt:
    json.dump({"startup_rounds":[first_round,second_round],"migration_rounds":[first_migrations,second_migrations],"isolated_codex_home":True}, receipt, sort_keys=True, indent=2)
PY

jq -n --arg product_sha "${PRODUCT_SHA}" --arg workflow_sha "${WORKFLOW_SHA}" --arg platform "${PLATFORM}" --arg target "${TARGET}" \
  --slurpfile core "${EVIDENCE_DIR}/core-identity.json" \
  --slurpfile host "${EVIDENCE_DIR}/host-identity.json" \
  --slurpfile fixture "${EVIDENCE_DIR}/legacy-fixture.json" \
  --slurpfile host_protocol "${EVIDENCE_DIR}/code-mode-protocol.json" \
  --slurpfile app_server "${EVIDENCE_DIR}/packaged-app-server.json" \
  '{product_sha:$product_sha,workflow_host_sha:$workflow_sha,platform:$platform,target:$target,core_artifact:$core[0],host_artifact:$host[0],fixture:$fixture[0],code_mode_host_protocol:$host_protocol[0],app_server:$app_server[0]}' \
  > "${EVIDENCE_DIR}/package-verification.json"
echo "packaged smoke passed for ${PLATFORM} at product ${PRODUCT_SHA} (workflow host ${WORKFLOW_SHA})"
