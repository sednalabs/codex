"""Explicit, prefix-verifiable publication plans; no implicit retry or rollback."""
from __future__ import annotations

from dataclasses import dataclass
import hashlib
import json
import os
from pathlib import Path
import tempfile


BATCH_SIZE = 100
PLAN_SCHEMA = "history-rewrite-staged-plan-v1"
INTENT_SCHEMA = "history-rewrite-staged-intent-v1"


class PlanError(ValueError):
    """A fixed-text rejection of an inconsistent staged contract."""


def digest(value: object) -> str:
    raw = json.dumps(value, sort_keys=True, separators=(",", ":"), ensure_ascii=False).encode()
    return hashlib.sha256(raw).hexdigest()


def build_plan(selected: dict, output: dict, *, canary_ref: str, final_refs: list[str],
               required_final_refs: set[str]) -> dict:
    if set(selected) != set(output) or not selected:
        raise PlanError("staged plan requires one nonempty unchanged reference domain")
    if (not isinstance(final_refs, list) or not all(isinstance(ref, str) for ref in final_refs)
            or final_refs != sorted(set(final_refs)) or not set(final_refs) <= set(selected)
            or not required_final_refs <= set(final_refs) or len(final_refs) > BATCH_SIZE):
        raise PlanError("staged final coupled reference set is invalid")
    changed = {ref for ref in selected if selected[ref] != output[ref]}
    if (not isinstance(canary_ref, str) or canary_ref not in changed
            or not canary_ref.startswith("refs/heads/") or canary_ref in final_refs):
        raise PlanError("staged canary must be one changed noncritical branch")
    final = sorted(changed.intersection(final_refs))
    if not final:
        raise PlanError("staged final coupled batch must contain a real update")
    middle = sorted(changed - set(final) - {canary_ref})
    groups = [[canary_ref], *(middle[index:index + BATCH_SIZE] for index in range(0, len(middle), BATCH_SIZE)), final]
    current = dict(selected)
    batches = []
    seen = {digest(current)}
    for index, refs in enumerate(groups):
        before = digest(current)
        current.update({ref: output[ref] for ref in refs})
        after = digest(current)
        if after in seen:
            raise PlanError("staged plan has an ambiguous repeated prefix")
        seen.add(after)
        batches.append({"index": index, "refs": refs, "before_refs_sha256": before, "after_refs_sha256": after})
    if current != output:
        raise PlanError("staged plan does not terminate at the entire approved output")
    return {"schema": PLAN_SCHEMA, "mode": "staged", "batch_size": BATCH_SIZE,
            "canary_ref": canary_ref, "final_refs": final_refs,
            "selected_refs_sha256": digest(selected), "output_refs_sha256": digest(output), "batches": batches}


def validated_plan(manifest: dict, *, required_final_refs: set[str]) -> dict | None:
    if "publication_plan" not in manifest and "publication_plan_sha256" not in manifest:
        return None
    value = manifest.get("publication_plan")
    if not isinstance(value, dict):
        raise PlanError("explicit staged publication plan must be an object")
    expected = build_plan(manifest["selected_refs"], manifest["output_refs"], canary_ref=value.get("canary_ref"),
                          final_refs=value.get("final_refs"), required_final_refs=required_final_refs)
    if value != expected or digest(value) != digest(expected) or manifest.get("publication_plan_sha256") != digest(expected):
        raise PlanError("explicit staged plan or digest differs from deterministic approved transitions")
    return expected


def prefix_maps(plan: dict, selected: dict, output: dict) -> list[dict]:
    values = [dict(selected)]
    for batch in plan["batches"]:
        current = dict(values[-1])
        current.update({ref: output[ref] for ref in batch["refs"]})
        values.append(current)
    return values


def locate_prefix(maps: list[dict], actual: dict) -> int:
    matches = [index for index, expected in enumerate(maps) if actual == expected]
    if len(matches) != 1:
        raise PlanError("remote namespace is not exactly one approved staged prefix")
    return matches[0]


def mutation_intent(manifest: dict, binding: dict, *, preflight_sha256: str,
                    control_plan_sha256: str, restoration_intent_sha256: str) -> dict:
    plan = manifest["publication_plan"]
    selected, output = manifest["selected_refs"], manifest["output_refs"]
    return {"schema": INTENT_SCHEMA, "repository": manifest["repository"], "binding": binding,
            "manifest_sha256": digest(manifest), "publication_plan_sha256": digest(plan),
            "preflight_sha256": preflight_sha256, "control_plan_sha256": control_plan_sha256,
            "restoration_intent_sha256": restoration_intent_sha256,
            "permitted_prefixes": list(range(len(plan["batches"]) + 1)),
            "batches": [{**batch, "leased_updates": [
                {"ref": ref, "expected_old_oid": selected[ref], "new_oid": output[ref]}
                for ref in batch["refs"]]} for batch in plan["batches"]]}


def reverse_lease_plan(plan: dict, selected: dict, output: dict, actual: dict) -> list[dict]:
    """Describe exact recovery; this function neither authorizes nor executes it."""
    maps = prefix_maps(plan, selected, output)
    position = locate_prefix(maps, actual)
    return [{"from_prefix": index + 1, "to_prefix": index,
             "before_refs_sha256": digest(maps[index + 1]), "after_refs_sha256": digest(maps[index]),
             "leased_updates": [{"ref": ref, "expected_old_oid": output[ref], "new_oid": selected[ref]}
                                for ref in plan["batches"][index]["refs"]]}
            for index in range(position - 1, -1, -1)]


def write_checkpoint(path: Path, value: dict) -> None:
    """Atomically persist local diagnostic progress, not cross-run authority."""
    path.parent.mkdir(parents=True, exist_ok=True)
    descriptor, temporary = tempfile.mkstemp(prefix=".staged-checkpoint-", dir=path.parent)
    try:
        with os.fdopen(descriptor, "w") as stream:
            json.dump(value, stream, sort_keys=True, separators=(",", ":"))
            stream.write("\n")
            stream.flush()
            os.fsync(stream.fileno())
        os.replace(temporary, path)
        directory = os.open(path.parent, os.O_RDONLY | os.O_DIRECTORY)
        try:
            os.fsync(directory)
        finally:
            os.close(directory)
    finally:
        if os.path.exists(temporary):
            os.unlink(temporary)


@dataclass(frozen=True)
class BatchResult:
    outcome: str
    reason: str
    after_refs: dict
    progress: dict
    diagnostics: tuple


def run_batches(plan: dict, selected: dict, output: dict, *, read_refs, push, check_gate,
                capture_complete, checkpoint, error_type) -> BatchResult:
    """Advance exact prefixes, resolving each attempted push with one readback."""
    maps = prefix_maps(plan, selected, output)
    actual = read_refs()
    position = locate_prefix(maps, actual)
    start = position
    diagnostics = ()
    checkpoint_ok = True

    def progress(stage: str, *, attempted_batch: int | None = None) -> dict:
        return {"schema": "history-rewrite-staged-progress-v1", "publication_plan_sha256": digest(plan),
                "start_prefix": start, "completed_prefix": position, "batch_count": len(plan["batches"]),
                "actual_refs_sha256": digest(actual), "stage": stage, "attempted_batch": attempted_batch,
                "checkpoint_status": "available" if checkpoint_ok else "unavailable",
                "prior_run_capture_custody": "requires-independent-reconciliation" if start else "not-applicable"}

    def record(stage: str, attempted: int | None = None) -> None:
        nonlocal checkpoint_ok
        try:
            checkpoint(progress(stage, attempted_batch=attempted))
        except OSError:
            checkpoint_ok = False

    def stop(reason: str, stage: str = "checkpointed") -> BatchResult:
        record(stage)
        return BatchResult("checkpointed", reason, actual, progress(stage), diagnostics)

    record("observed-prefix")
    while position < len(plan["batches"]):
        if not checkpoint_ok:
            return stop("local checkpoint unavailable; no further batch attempted")
        if not capture_complete():
            return stop("encrypted capture incomplete; no further batch attempted")
        # Administrative observation may take time; read the complete namespace
        # again afterward and recheck freshness immediately before mutation.
        try:
            check_gate(observe=True)
            latest = read_refs()
            if latest != maps[position]:
                raise PlanError("remote namespace changed before the next staged batch")
            actual = latest
            record("before-batch", position)
            if not checkpoint_ok or not capture_complete():
                return stop("pre-batch checkpoint or capture unavailable")
            check_gate(observe=False)
        except error_type:
            actual = read_refs()
            if actual != maps[position]:
                raise PlanError("remote namespace changed while reconciling the batch gate") from None
            return stop("mutation gate requires fresh reconciliation; no further batch attempted")
        batch = plan["batches"][position]
        push_error = None
        try:
            push(batch, maps[position], maps[position + 1])
        except error_type as error:
            push_error = error
            diagnostics += tuple(getattr(error, "diagnostics", ()))
        # A lost response is not a failed transaction, and a zero exit is not
        # acceptance. Never repeat the push to discover what happened.
        try:
            actual = read_refs()
        except error_type as error:
            if push_error is not None:
                raise error_type("staged push response and authoritative readback unavailable",
                                 diagnostics=diagnostics + tuple(getattr(error, "diagnostics", ()))) from None
            raise
        if actual == maps[position + 1]:
            position += 1
            record("batch-applied", batch["index"])
        elif actual == maps[position]:
            return stop("batch did not advance; exact prefix preserved and no retry attempted")
        else:
            raise PlanError("staged push left a foreign or non-prefix namespace")
        if not capture_complete():
            return stop("batch applied but encrypted capture is incomplete")
    record("output-observed")
    if not checkpoint_ok:
        return stop("approved output observed but local checkpoint is unavailable")
    return BatchResult("success", "all approved staged batches observed", actual, progress("output-observed"), diagnostics)
