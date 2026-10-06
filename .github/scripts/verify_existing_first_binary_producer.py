#!/usr/bin/env python3
"""Verify admitted first-binary producer records and current consumer identity.

Cross-run consumption is selected only by an exact eligible row in the
workflow-host manifest; diagnostic rows can never qualify. The build-mode path
remains same-run and resolves only the just-built native artifact. API material
is passed through typed environment fields; secrets are never printed or
written to the evidence records.
"""

from __future__ import annotations

import hashlib
import json
import os
import platform
import re
import subprocess
import sys
from pathlib import Path
import xml.etree.ElementTree as ET
from typing import Any, Mapping
from urllib.error import HTTPError, URLError
from urllib.parse import urlsplit
from urllib.request import Request, urlopen

REPOSITORY = "sednalabs/codex"
REPOSITORY_ID = 1152496647
WORKFLOW_ID = 250252262
WORKFLOW_PATH = ".github/workflows/sedna-branch-build.yml"
ACCEPTED_INPUTS_PATH = Path(__file__).resolve().parents[1] / "first-binary-accepted-inputs.json"
TRUSTED_CONSUMER_BRANCH = "reconstruct/first-binary-package-workflow-20261001"
PERMANENTLY_DIAGNOSTIC_PRODUCER_RUN_IDS = frozenset({36800811941})
SHA = re.compile(r"[0-9a-f]{40}\Z")
DIGEST = re.compile(r"sha256:[0-9a-f]{64}\Z")
RUN_ID = re.compile(r"[1-9][0-9]*\Z")
ARCHES = {
    "x86_64": {
        "target": "x86_64-unknown-linux-gnu",
        "runner": "ubuntu-24.04",
        "run_arch": "X64",
        "package_job": "Package native Linux x86_64",
        "consumer_job": "Consume native Linux x86_64 package",
    },
    "aarch64": {
        "target": "aarch64-unknown-linux-gnu",
        "runner": "ubuntu-24.04-arm",
        "run_arch": "ARM64",
        "package_job": "Package native Linux ARM64",
        "consumer_job": "Consume native Linux ARM64 package",
    },
}
PRODUCER_JOB_NAMES = {
    "Verify standard runner graph and exact identities",
    "Package native Linux x86_64",
    "Package native Linux ARM64",
    "Prepare exact Cargo lock and app-server schema diff",
    "Consume native Linux ARM64 package",
    "Consume native Linux x86_64 package",
}
H2_PRODUCER_WORKFLOW_HOST_SHA = "76a4538cd791f681a94db103da5940cb02d34165"
H2_PRODUCER_JOB_NAMES = PRODUCER_JOB_NAMES | {
    "Verify exact SDK runtime-version parser selectors",
}
H9E58_PRODUCER_WORKFLOW_HOST_SHA = "9e58d011bde9609eb28b3e047bdc0bce4784cd0a"
H9E58_PRODUCER_RUN_ID = 37182140778
H9E58_PRODUCER_JOB_NAMES = PRODUCER_JOB_NAMES | {
    "Verify exact SDK runtime-version parser selectors",
}
Q2_FIXTURE_SHA = "c6b1888354315575e1b22abbe240b482e65b4f46"
Q3_FIXTURE_SHA = "b5d2ffd3dcdf4486297d8380cbe562a0b30bcc30"
Q4_FIXTURE_SHA = "c61cc2b4943079d924a3c645512eb2b6600bdd57"
Q5_FIXTURE_SHA = "2c0304235001132f2d06ea42129719c2a9e98bb7"
Q11_FIXTURE_SHA = "05ac52f72f5fa259673fe8cc9a8e61daa52f1714"
Q13_FIXTURE_SHA = "aa5b3496a26f9f164d870aec85ac942e87af3eb0"
Q14_FIXTURE_SHA = "8c0daf2bfce6c8d8ab4b963755cfed77751c574a"
Q15_FIXTURE_SHA = "53cf4664d00dd689f7a80754d045b1779b7efa77"
Q16_FIXTURE_SHA = "7a1fd5f93fca4e6ef7d54d4fc2d29f6b3526cdd5"
Q17_FIXTURE_SHA = "9de6963decf2b68869ab22e6207da413fad672bb"
Q18_FIXTURE_SHA = "a478628dd92b84b54211acbda6d319478ed0bbfb"
Q20_FIXTURE_SHA = "18d14e4a6eec3ead887d5f59c5db5f9fdbb8a8f8"
Q22_FIXTURE_SHA = "c8b1242685baba9e9e60c47305604003f2cfea12"
Q24_FIXTURE_SHA = "856502535018d9c03ec95bc7586984526a24683d"
Q25_FIXTURE_SHA = "90f44d691ef7db3a6e1e441d60908ddc1c0639b1"
Q26_FIXTURE_SHA = "ab22fc2f866fee2312537575ccbc00cf8f98449e"
Q57_FIXTURE_SHA = "57142efe44bfbcda87349b806923d7662026bf62"
Q59_FIXTURE_SHA = "7d021a283d1378559c6c55370a99f8bab1094537"
Q60_FIXTURE_SHA = "5ae0963d9719bfdce7ab90dadaee0ed5ee910306"
Q61_FIXTURE_SHA = "df91a30e31834a59f9fbb6023f654ed87fbfa5d3"
Q62_FIXTURE_SHA = "8b7d7fc710a5f7d79ebaa02f0d47f1a782f7bd0f"
Q63_FIXTURE_SHA = "14e689f721d7a8bc3f1131281cdaba1f9921648e"
Q64_FIXTURE_SHA = "cbbb99e99b40c533fa93d2851c02f4f87fbcb0ca"
Q66_FIXTURE_SHA = "9fd64260e295861cab58d0a9b8fe6e852a3a2222"
Q67_FIXTURE_SHA = "b9d83117a8eef946c00f89368e838eeb55dd9c45"
Q68_FIXTURE_SHA = "e4970c51879dcda90a7bad93e1148f023d8b7225"
Q69_FIXTURE_SHA = "258588aabeaf92a02a3d73a8d278b9d2a7ffeb5a"
S0_SDK_SHA = "dc802999023f8ed8b8021b415ea15f77afc41248"
S1_SDK_SHA = "f7151a5ce6b228b64e9421d9ec2f9567435c7a88"
S2_SDK_SHA = "7b99a7683e96fc1824aec519f0c814f9562efc77"
S3_SDK_SHA = "b0b13d9d4b02500f27e31e60eab06b2de36bb0d6"
S4_SDK_SHA = "e8bec7e6dc09de16d81c6b105e0cbed9f24bfa1a"
BROWSER_DIAGNOSTIC_PROFILE = "browser-diagnostic"
BROWSER_DIAGNOSTIC_PRODUCT_SHA = "940dcc0a6d839216d2b1604658a78a9abf00317e"
BROWSER_DIAGNOSTIC_BASE_REF = "validation/interrupt-guardian-fixture-8389-20261004"
BROWSER_DIAGNOSTIC_BASE_SHA = "8389b61d82cb6fb936e4e500b977f31682441ffe"
BROWSER_DIAGNOSTIC_FIXTURE_SHA = "614ebadcd49997c10efc8a5b986ce81747b9b487"
BROWSER_DIAGNOSTIC_SDK_SHA = S4_SDK_SHA
BROWSER_DIAGNOSTIC_TEST_NAME = "test_packaged_tui_browser_output_keeps_images_and_manifest_metadata_separate"
BROWSER_DIAGNOSTIC_PROPERTY = "browser_output_diagnostic_json"
BROWSER_DIAGNOSTIC_CALL_ID = "browser-visual-fixture-call"
BROWSER_DIAGNOSTIC_TOOL_NAMES = frozenset({"browser_observe", "browser_step"})
BROWSER_DIAGNOSTIC_FIELDS = frozenset(
    {
        "fixture_function_call_emitted",
        "responses_request_count",
        "second_request_observed",
        "second_request_input_available",
        "function_call_output_count",
        "function_call_output_call_ids",
        "function_call_output_tool_names",
        "synthetic_browser_provider_invoked",
        "synthetic_browser_provider_tool_name",
    }
)
EXPECTED_STATE_POSITIVE = frozenset(
    {
        "fresh", "u23", "u55", "u56", "u57", "u58", "f56", "f57",
        "f58", "f_full", "f_alias_pair", "shift24", "shift29", "shift38",
        "shift45", "shift50",
    }
)
EXPECTED_STATE_NEGATIVE = frozenset(
    {
        "bad_checksum", "mixed_ids", "missing_middle", "failed_row", "incomplete_f58",
        "partial_alias", "partial_upstream_schema", "unknown_id",
    }
)
FULL_PLAIN_TESTS = frozenset(
    {
        "test_exact_single_artifact_and_native_layout",
        "test_wrong_source_manifest_is_rejected_before_unpack",
        "test_wrong_target_and_missing_helper_are_rejected",
        "test_banner_native_client_and_same_source_proxy_execution",
        "test_persistent_code_mode_and_reopen_from_real_package",
        "test_missing_host_cannot_qualify_code_mode",
        "test_model_receives_and_executes_root_only_agent_list",
        "test_model_spawn_hidden_metadata_has_no_private_fields",
        "test_visible_model_spawn_and_resumed_list_keep_stable_identity",
        "test_actual_tui_agents_entry_has_initial_empty_search",
        "test_actual_tui_nested_filter_clear_live_rename_and_replay",
        "test_real_response_usage_joins_standard_rate_scenario_after_resume",
    }
)
Q3_ADDITIONAL_PLAIN_TESTS = frozenset(
    {
        "test_packaged_model_wait_agent_times_out_without_activity",
        "test_packaged_model_nested_spawn_recovery_and_list_after_resume",
        "test_packaged_model_queue_only_message_does_not_wake_until_followup_and_exact_join",
        "test_packaged_model_goal_continuation_and_terminal_transition",
        "test_packaged_tui_agents_details_render_configured_identity_and_unknown_effective_identity",
    }
)
FOCUSED_REPAIR_PLAIN_TESTS = frozenset(
    {
        "test_packaged_model_nested_spawn_recovery_and_list_after_resume",
        "test_packaged_model_queue_only_message_does_not_wake_until_followup_and_exact_join",
        "test_packaged_model_goal_continuation_and_terminal_transition",
        "test_packaged_tui_agents_details_render_configured_identity_and_unknown_effective_identity",
        "test_actual_tui_agents_entry_has_initial_empty_search",
        "test_actual_tui_nested_filter_clear_live_rename_and_replay",
    }
)
CONSUME_EXISTING_TEST_PLANS = {
    (Q2_FIXTURE_SHA, S0_SDK_SHA): {
        "profiles": frozenset({"pair", "full"}),
        "full_plain": FULL_PLAIN_TESTS,
    },
    (Q3_FIXTURE_SHA, S1_SDK_SHA): {
        "profiles": frozenset({"full"}),
        "full_plain": FULL_PLAIN_TESTS | Q3_ADDITIONAL_PLAIN_TESTS,
    },
    (Q4_FIXTURE_SHA, S2_SDK_SHA): {
        "profiles": frozenset({"pair", "full"}),
        "full_plain": FULL_PLAIN_TESTS | Q3_ADDITIONAL_PLAIN_TESTS,
    },
    (Q5_FIXTURE_SHA, S2_SDK_SHA): {
        "profiles": frozenset({"focused", "full"}),
        "focused_plain": FOCUSED_REPAIR_PLAIN_TESTS,
        "full_plain": FULL_PLAIN_TESTS | Q3_ADDITIONAL_PLAIN_TESTS,
    },
    (Q11_FIXTURE_SHA, S3_SDK_SHA): {
        "profiles": frozenset({"focused"}),
        "focused_plain": FOCUSED_REPAIR_PLAIN_TESTS,
    },
    (Q13_FIXTURE_SHA, S4_SDK_SHA): {
        "profiles": frozenset({"focused"}),
        "focused_plain": FOCUSED_REPAIR_PLAIN_TESTS,
    },
    (Q14_FIXTURE_SHA, S4_SDK_SHA): {
        "profiles": frozenset({"focused"}),
        "focused_plain": FOCUSED_REPAIR_PLAIN_TESTS,
    },
    (Q15_FIXTURE_SHA, S4_SDK_SHA): {
        "profiles": frozenset({"focused"}),
        "focused_plain": FOCUSED_REPAIR_PLAIN_TESTS,
    },
    (Q17_FIXTURE_SHA, S4_SDK_SHA): {
        "profiles": frozenset({"focused"}),
        "focused_plain": FOCUSED_REPAIR_PLAIN_TESTS,
    },
    (Q18_FIXTURE_SHA, S4_SDK_SHA): {
        "profiles": frozenset({"focused"}),
        "focused_plain": FOCUSED_REPAIR_PLAIN_TESTS,
    },
    (Q20_FIXTURE_SHA, S4_SDK_SHA): {
        "profiles": frozenset({"focused"}),
        "focused_plain": FOCUSED_REPAIR_PLAIN_TESTS,
    },
    (Q22_FIXTURE_SHA, S4_SDK_SHA): {
        "profiles": frozenset({"focused", "full"}),
        "focused_plain": FOCUSED_REPAIR_PLAIN_TESTS,
        "full_plain": FULL_PLAIN_TESTS | Q3_ADDITIONAL_PLAIN_TESTS,
    },
    (Q24_FIXTURE_SHA, S4_SDK_SHA): {
        "profiles": frozenset({"focused", "full"}),
        "focused_plain": FOCUSED_REPAIR_PLAIN_TESTS,
        "full_plain": FULL_PLAIN_TESTS | Q3_ADDITIONAL_PLAIN_TESTS,
    },
    (Q25_FIXTURE_SHA, S4_SDK_SHA): {
        "profiles": frozenset({"focused", "full"}),
        "focused_plain": FOCUSED_REPAIR_PLAIN_TESTS,
        "full_plain": FULL_PLAIN_TESTS | Q3_ADDITIONAL_PLAIN_TESTS,
    },
    (Q26_FIXTURE_SHA, S4_SDK_SHA): {
        "profiles": frozenset({"focused", "full"}),
        "focused_plain": FOCUSED_REPAIR_PLAIN_TESTS,
        "full_plain": FULL_PLAIN_TESTS | Q3_ADDITIONAL_PLAIN_TESTS,
    },
    (Q57_FIXTURE_SHA, S4_SDK_SHA): {
        "profiles": frozenset({"focused", "full"}),
        "focused_plain": FOCUSED_REPAIR_PLAIN_TESTS,
        "full_plain": FULL_PLAIN_TESTS | Q3_ADDITIONAL_PLAIN_TESTS,
    },
    (Q59_FIXTURE_SHA, S4_SDK_SHA): {
        "profiles": frozenset({"focused", "full"}),
        "focused_plain": FOCUSED_REPAIR_PLAIN_TESTS,
        "full_plain": FULL_PLAIN_TESTS | Q3_ADDITIONAL_PLAIN_TESTS,
    },
    (Q60_FIXTURE_SHA, S4_SDK_SHA): {
        "profiles": frozenset({"focused", "full"}),
        "focused_plain": FOCUSED_REPAIR_PLAIN_TESTS,
        "full_plain": FULL_PLAIN_TESTS | Q3_ADDITIONAL_PLAIN_TESTS,
    },
    (Q61_FIXTURE_SHA, S4_SDK_SHA): {
        "profiles": frozenset({"focused", "full"}),
        "focused_plain": FOCUSED_REPAIR_PLAIN_TESTS,
        "full_plain": FULL_PLAIN_TESTS | Q3_ADDITIONAL_PLAIN_TESTS,
    },
    (Q62_FIXTURE_SHA, S4_SDK_SHA): {
        "profiles": frozenset({"focused", "full"}),
        "focused_plain": FOCUSED_REPAIR_PLAIN_TESTS,
        "full_plain": FULL_PLAIN_TESTS | Q3_ADDITIONAL_PLAIN_TESTS,
    },
    (Q63_FIXTURE_SHA, S4_SDK_SHA): {
        "profiles": frozenset({"focused", "full"}),
        "focused_plain": FOCUSED_REPAIR_PLAIN_TESTS,
        "full_plain": FULL_PLAIN_TESTS | Q3_ADDITIONAL_PLAIN_TESTS,
    },
    (Q64_FIXTURE_SHA, S4_SDK_SHA): {
        "profiles": frozenset({"focused", "full"}),
        "focused_plain": FOCUSED_REPAIR_PLAIN_TESTS,
        "full_plain": FULL_PLAIN_TESTS | Q3_ADDITIONAL_PLAIN_TESTS,
    },
    (Q66_FIXTURE_SHA, S4_SDK_SHA): {
        "profiles": frozenset({"focused", "full"}),
        "focused_plain": FOCUSED_REPAIR_PLAIN_TESTS,
        "full_plain": FULL_PLAIN_TESTS | Q3_ADDITIONAL_PLAIN_TESTS,
    },
    (Q67_FIXTURE_SHA, S4_SDK_SHA): {
        "profiles": frozenset({"focused", "full"}),
        "focused_plain": FOCUSED_REPAIR_PLAIN_TESTS,
        "full_plain": FULL_PLAIN_TESTS | Q3_ADDITIONAL_PLAIN_TESTS,
    },
    (Q68_FIXTURE_SHA, S4_SDK_SHA): {
        "profiles": frozenset({"focused", "full"}),
        "focused_plain": FOCUSED_REPAIR_PLAIN_TESTS,
        "full_plain": FULL_PLAIN_TESTS | Q3_ADDITIONAL_PLAIN_TESTS,
    },
    (Q69_FIXTURE_SHA, S4_SDK_SHA): {
        "profiles": frozenset({"focused", "full"}),
        "focused_plain": FOCUSED_REPAIR_PLAIN_TESTS,
        "full_plain": FULL_PLAIN_TESTS | Q3_ADDITIONAL_PLAIN_TESTS,
    },
}
SDK_TEST_PLAN_BY_SHA = {
    S0_SDK_SHA: {
        "selectors": (
            "sdk/python/tests/test_client_rpc_methods.py::test_thread_fork_accepts_cargo_sedna_dev_runtime_version",
            "sdk/python/tests/test_client_rpc_methods.py::test_new_options_reject_unsupported_runtime_before_sending",
        ),
        "expected_test_cases": {
            "test_thread_fork_accepts_cargo_sedna_dev_runtime_version": 1,
            "test_new_options_reject_unsupported_runtime_before_sending": 45,
        },
    },
    S1_SDK_SHA: {
        "selectors": (
            "sdk/python/tests/test_client_rpc_methods.py::test_thread_fork_accepts_cargo_sedna_dev_runtime_version",
            "sdk/python/tests/test_client_rpc_methods.py::test_new_options_reject_unsupported_runtime_before_sending",
            "sdk/python/tests/test_app_server_harness_request_routing.py::test_fifo_response_selection_remains_the_default",
            "sdk/python/tests/test_app_server_harness_request_routing.py::test_request_routes_match_exact_requests_and_wait_outside_selector_lock",
            "sdk/python/tests/test_app_server_harness_request_routing.py::test_bad_request_route_sets_fail_and_are_reported_on_teardown",
            "sdk/python/tests/test_app_server_harness_request_routing.py::test_unused_one_shot_routes_fail_at_teardown",
            "sdk/python/tests/test_app_server_harness_request_routing.py::test_one_shot_route_cannot_be_reused",
            "sdk/python/tests/test_app_server_harness_request_routing.py::test_fifo_and_request_matched_modes_cannot_be_mixed",
            "sdk/python/tests/test_app_server_harness_request_routing.py::test_server_teardown_releases_a_gated_request",
        ),
        "expected_test_cases": {
            "test_thread_fork_accepts_cargo_sedna_dev_runtime_version": 1,
            "test_new_options_reject_unsupported_runtime_before_sending": 45,
            "test_fifo_response_selection_remains_the_default": 1,
            "test_request_routes_match_exact_requests_and_wait_outside_selector_lock": 1,
            "test_bad_request_route_sets_fail_and_are_reported_on_teardown": 2,
            "test_unused_one_shot_routes_fail_at_teardown": 1,
            "test_one_shot_route_cannot_be_reused": 1,
            "test_fifo_and_request_matched_modes_cannot_be_mixed": 1,
            "test_server_teardown_releases_a_gated_request": 1,
        },
    },
    S2_SDK_SHA: {
        "selectors": (
            "sdk/python/tests/test_client_rpc_methods.py::test_thread_fork_accepts_cargo_sedna_dev_runtime_version",
            "sdk/python/tests/test_client_rpc_methods.py::test_new_options_reject_unsupported_runtime_before_sending",
            "sdk/python/tests/test_app_server_harness_request_routing.py::test_fifo_response_selection_remains_the_default",
            "sdk/python/tests/test_app_server_harness_request_routing.py::test_request_routes_match_exact_requests_and_wait_outside_selector_lock",
            "sdk/python/tests/test_app_server_harness_request_routing.py::test_bad_request_route_sets_fail_and_are_reported_on_teardown",
            "sdk/python/tests/test_app_server_harness_request_routing.py::test_unused_one_shot_routes_fail_at_teardown",
            "sdk/python/tests/test_app_server_harness_request_routing.py::test_one_shot_route_cannot_be_reused",
            "sdk/python/tests/test_app_server_harness_request_routing.py::test_fifo_and_request_matched_modes_cannot_be_mixed",
            "sdk/python/tests/test_app_server_harness_request_routing.py::test_server_teardown_releases_a_gated_request",
            "sdk/python/tests/test_app_server_harness_request_routing.py::test_used_route_does_not_make_one_new_matching_route_ambiguous",
        ),
        "expected_test_cases": {
            "test_thread_fork_accepts_cargo_sedna_dev_runtime_version": 1,
            "test_new_options_reject_unsupported_runtime_before_sending": 45,
            "test_fifo_response_selection_remains_the_default": 1,
            "test_request_routes_match_exact_requests_and_wait_outside_selector_lock": 1,
            "test_bad_request_route_sets_fail_and_are_reported_on_teardown": 2,
            "test_unused_one_shot_routes_fail_at_teardown": 1,
            "test_one_shot_route_cannot_be_reused": 1,
            "test_fifo_and_request_matched_modes_cannot_be_mixed": 1,
            "test_server_teardown_releases_a_gated_request": 1,
            "test_used_route_does_not_make_one_new_matching_route_ambiguous": 1,
        },
    },
}
SDK_TEST_PLAN_BY_SHA[S3_SDK_SHA] = SDK_TEST_PLAN_BY_SHA[S2_SDK_SHA]
SDK_TEST_PLAN_BY_SHA[S4_SDK_SHA] = SDK_TEST_PLAN_BY_SHA[S3_SDK_SHA]


def consume_existing_test_plan(fixture_sha: str, sdk_sha: str, profile: str) -> dict[str, Any]:
    plan = CONSUME_EXISTING_TEST_PLANS.get((fixture_sha, sdk_sha))
    _require(plan is not None, "fixture/SDK source pair has no exact consume-existing test inventory")
    _require(profile in plan["profiles"], "fixture/SDK source pair does not admit this consumer profile")
    return {
        "state": (
            frozenset({"fresh", "bad_checksum"})
            if profile == "pair"
            else frozenset(EXPECTED_STATE_POSITIVE | EXPECTED_STATE_NEGATIVE)
            if profile == "full"
            else frozenset()
        ),
        "plain": (
            frozenset()
            if profile == "pair"
            else plan["focused_plain"]
            if profile == "focused"
            else plan["full_plain"]
        ),
    }


def browser_diagnostic_test_plan(
    *,
    mode: str,
    profile: str,
    product_sha: str,
    base_ref: str,
    base_sha: str,
    fixture_sha: str,
    sdk_sha: str,
) -> dict[str, Any]:
    """Return the one closed same-run Browser diagnostic selection."""
    _require(mode == "build", "Browser diagnostic profile is available only in same-run build mode")
    _require(profile == BROWSER_DIAGNOSTIC_PROFILE, "Browser diagnostic profile name mismatch")
    _require(product_sha == BROWSER_DIAGNOSTIC_PRODUCT_SHA, "Browser diagnostic product SHA is not admitted")
    _require(base_ref == BROWSER_DIAGNOSTIC_BASE_REF, "Browser diagnostic comparison ref is not admitted")
    _require(base_sha == BROWSER_DIAGNOSTIC_BASE_SHA, "Browser diagnostic comparison SHA is not admitted")
    _require(fixture_sha == BROWSER_DIAGNOSTIC_FIXTURE_SHA, "Browser diagnostic fixture SHA is not admitted")
    _require(sdk_sha == BROWSER_DIAGNOSTIC_SDK_SHA, "Browser diagnostic SDK SHA is not admitted")
    return {"state": frozenset(), "plain": frozenset({BROWSER_DIAGNOSTIC_TEST_NAME})}


def _browser_diagnostic_from_junit(cases: list[ET.Element], issues: list[str]) -> dict[str, Any] | None:
    properties = [
        prop
        for case in cases
        for prop in case.findall("./properties/property")
        if prop.attrib.get("name") == BROWSER_DIAGNOSTIC_PROPERTY
    ]
    if len(properties) != 1:
        issues.append("JUnit does not contain exactly one Browser diagnostic property")
        return None
    raw = properties[0].attrib.get("value")
    if not isinstance(raw, str):
        issues.append("JUnit Browser diagnostic property has no value")
        return None
    try:
        diagnostic = _object(json.loads(raw), "Browser diagnostic property is malformed")
    except (json.JSONDecodeError, ValueError):
        issues.append("JUnit Browser diagnostic property is malformed")
        return None
    if frozenset(diagnostic) != BROWSER_DIAGNOSTIC_FIELDS:
        issues.append("JUnit Browser diagnostic property has an unexpected field inventory")
        return None
    boolean_keys = (
        "fixture_function_call_emitted",
        "second_request_observed",
        "second_request_input_available",
        "synthetic_browser_provider_invoked",
    )
    if any(type(diagnostic.get(key)) is not bool for key in boolean_keys):
        issues.append("JUnit Browser diagnostic booleans are malformed")
        return None
    count_keys = ("responses_request_count", "function_call_output_count")
    if any(type(diagnostic.get(key)) is not int or diagnostic[key] < 0 for key in count_keys):
        issues.append("JUnit Browser diagnostic counts are malformed")
        return None
    call_ids = diagnostic.get("function_call_output_call_ids")
    tool_names = diagnostic.get("function_call_output_tool_names")
    allowed_call_ids = {BROWSER_DIAGNOSTIC_CALL_ID, "<other>", "<missing>"}
    allowed_tool_names = BROWSER_DIAGNOSTIC_TOOL_NAMES | {"<other>", "<absent>"}
    if (
        not isinstance(call_ids, list)
        or len(call_ids) > 16
        or any(not isinstance(value, str) or value not in allowed_call_ids for value in call_ids)
        or not isinstance(tool_names, list)
        or len(tool_names) > 16
        or any(not isinstance(value, str) or value not in allowed_tool_names for value in tool_names)
        or len(call_ids) != diagnostic["function_call_output_count"]
        or len(tool_names) != diagnostic["function_call_output_count"]
    ):
        issues.append("JUnit Browser diagnostic call identifiers or tool names are malformed")
        return None
    provider_tool = diagnostic.get("synthetic_browser_provider_tool_name")
    if provider_tool is not None and (
        not isinstance(provider_tool, str)
        or provider_tool not in (BROWSER_DIAGNOSTIC_TOOL_NAMES | {"<other>"})
    ):
        issues.append("JUnit Browser diagnostic provider tool name is malformed")
        return None
    if diagnostic["synthetic_browser_provider_invoked"] != (provider_tool is not None):
        issues.append("JUnit Browser diagnostic provider flag disagrees with its sanitized tool name")
        return None
    if diagnostic["second_request_observed"] != (diagnostic["responses_request_count"] >= 2):
        issues.append("JUnit Browser diagnostic second-request flag disagrees with request count")
        return None
    if diagnostic["function_call_output_count"] and not diagnostic["second_request_input_available"]:
        issues.append("JUnit Browser diagnostic call outputs lack a second-request input witness")
        return None
    return diagnostic


def sdk_test_plan(sdk_sha: str) -> dict[str, Any]:
    plan = SDK_TEST_PLAN_BY_SHA.get(sdk_sha)
    _require(plan is not None, "SDK source SHA has no exact hosted selector inventory")
    return {
        "selectors": list(plan["selectors"]),
        "expected_test_cases": dict(plan["expected_test_cases"]),
    }


def _require(condition: bool, message: str) -> None:
    if not condition:
        raise ValueError(message)


def _is_int(value: object, *, minimum: int = 1) -> bool:
    return type(value) is int and value >= minimum


def _object(value: object, message: str) -> Mapping[str, Any]:
    _require(isinstance(value, dict), message)
    return value


def _validate_hosted_job(job: Mapping[str, Any], label: str, *, ran: bool) -> None:
    _require(job.get("labels") == [label], "Actions job runner label is not an exact standard label")
    if ran:
        _require(job.get("runner_group_id") == 0, "Actions job is not in the GitHub-hosted runner group")
        _require(job.get("runner_group_name") == "GitHub Actions", "Actions job runner group is not GitHub-hosted")
        runner_name = job.get("runner_name")
        _require(
            isinstance(runner_name, str) and runner_name.startswith("GitHub Actions "),
            "Actions job does not identify a GitHub-hosted runner",
        )
    else:
        _require(job.get("runner_group_id") is None, "skipped Actions job unexpectedly acquired a runner")
        _require(job.get("runner_group_name") is None, "skipped Actions job unexpectedly acquired a runner group")
        _require(job.get("runner_name") is None, "skipped Actions job unexpectedly acquired a runner")


def _complete_rows(payload: Mapping[str, Any], key: str, label: str) -> list[Mapping[str, Any]]:
    rows = payload.get(key)
    total = payload.get("total_count")
    _require(isinstance(rows, list), f"{label} API response omitted its row list")
    _require(type(total) is int and total == len(rows), f"{label} API response is incomplete")
    return [_object(row, f"{label} API row is malformed") for row in rows]


def _validate_manifest_record(value: object) -> Mapping[str, Any]:
    record = _object(value, "accepted-input record is malformed")
    required = {"record_id", "disposition", "w14780_eligible", "identity", "producer", "jobs", "artifacts"}
    _require(set(record) == required, "accepted-input record has unexpected or missing fields")
    _require(
        isinstance(record.get("record_id"), str)
        and re.fullmatch(r"[a-z0-9][a-z0-9-]{0,79}", record["record_id"]) is not None,
        "accepted-input record ID is invalid",
    )
    disposition = record.get("disposition")
    eligible = record.get("w14780_eligible")
    _require(isinstance(disposition, str) and disposition in {"accepted", "diagnostic"}, "accepted-input disposition is invalid")
    _require(type(eligible) is bool, "accepted-input eligibility flag is invalid")
    _require((disposition == "accepted") == eligible, "diagnostic records cannot be acceptance eligible")

    identity = _object(record.get("identity"), "accepted-input identity is malformed")
    identity_fields = {"product_sha", "comparison_base_ref", "comparison_base_sha", "fixture_sha", "sdk_sha", "profile"}
    _require(set(identity) == identity_fields, "accepted-input identity has unexpected or missing fields")
    for field in ("product_sha", "comparison_base_sha", "fixture_sha", "sdk_sha"):
        _require(isinstance(identity.get(field), str) and SHA.fullmatch(identity[field]) is not None, f"accepted-input {field} is invalid")
    _require(identity.get("comparison_base_ref") == "main", "accepted-input comparison base ref is unsupported")
    _require(
        isinstance(identity.get("profile"), str)
        and identity["profile"] in {"pair", "focused", "full"},
        "accepted-input consumer profile is unsupported",
    )

    producer = _object(record.get("producer"), "accepted-input producer is malformed")
    producer_fields = {
        "repository", "repository_id", "workflow_id", "workflow_path", "run_id", "run_attempt", "event",
        "workflow_host_sha", "branch", "status", "conclusion",
    }
    _require(set(producer) == producer_fields, "accepted-input producer has unexpected or missing fields")
    _require(producer.get("repository") == REPOSITORY and producer.get("repository_id") == REPOSITORY_ID, "accepted-input producer repository is unsupported")
    _require(producer.get("workflow_id") == WORKFLOW_ID and producer.get("workflow_path") == WORKFLOW_PATH, "accepted-input producer workflow is unsupported")
    _require(_is_int(producer.get("run_id")) and _is_int(producer.get("run_attempt")), "accepted-input producer run identity is invalid")
    if producer["run_id"] in PERMANENTLY_DIAGNOSTIC_PRODUCER_RUN_IDS:
        _require(disposition == "diagnostic" and eligible is False, "the superseded diagnostic producer run is permanently ineligible")
    _require(producer.get("event") == "workflow_dispatch", "accepted-input producer event is unsupported")
    _require(isinstance(producer.get("workflow_host_sha"), str) and SHA.fullmatch(producer["workflow_host_sha"]) is not None, "accepted-input producer host SHA is invalid")
    _require(producer.get("branch") == TRUSTED_CONSUMER_BRANCH, "accepted-input producer branch is unsupported")
    _require(
        producer.get("status") == "completed"
        and isinstance(producer.get("conclusion"), str)
        and producer["conclusion"] in {"success", "failure"},
        "accepted-input producer terminal state is invalid",
    )

    expected_job_names = PRODUCER_JOB_NAMES
    if producer["workflow_host_sha"] == H2_PRODUCER_WORKFLOW_HOST_SHA:
        expected_job_names = H2_PRODUCER_JOB_NAMES
    elif producer["workflow_host_sha"] == H9E58_PRODUCER_WORKFLOW_HOST_SHA:
        expected_job_names = H9E58_PRODUCER_JOB_NAMES
        _require(producer["run_id"] == H9E58_PRODUCER_RUN_ID, "H9e58 producer run ID differs from the exact admitted run")
    jobs_value = record.get("jobs")
    _require(isinstance(jobs_value, list) and len(jobs_value) == len(expected_job_names), "accepted-input producer job contract is incomplete")
    jobs: dict[str, Mapping[str, Any]] = {}
    for job_value in jobs_value:
        job = _object(job_value, "accepted-input producer job contract is malformed")
        _require(set(job) == {"name", "status", "conclusion", "runner", "ran"}, "accepted-input producer job has unexpected fields")
        name = job.get("name")
        _require(isinstance(name, str) and name in expected_job_names and name not in jobs, "accepted-input producer job name is unknown or duplicated")
        _require(job.get("status") == "completed", "accepted-input producer job is not terminal")
        _require(isinstance(job.get("conclusion"), str) and job["conclusion"] in {"success", "failure", "skipped"}, "accepted-input producer job conclusion is invalid")
        expected_runner = "ubuntu-24.04-arm" if name in {"Package native Linux ARM64", "Consume native Linux ARM64 package"} else "ubuntu-24.04"
        _require(job.get("runner") == expected_runner, "accepted-input producer job runner is not a literal standard runner")
        _require(type(job.get("ran")) is bool and job["ran"] == (job["conclusion"] != "skipped"), "accepted-input producer job execution flag is inconsistent")
        jobs[name] = job
    _require(set(jobs) == expected_job_names, "accepted-input producer job inventory is not closed")
    _require(jobs["Verify standard runner graph and exact identities"]["conclusion"] == "success", "producer runner-policy job must succeed")
    _require(jobs["Package native Linux x86_64"]["conclusion"] == "success", "producer x86_64 package job must succeed")
    _require(jobs["Package native Linux ARM64"]["conclusion"] == "success", "producer ARM64 package job must succeed")
    _require(jobs["Prepare exact Cargo lock and app-server schema diff"]["conclusion"] == "skipped", "producer preparation job must be skipped")
    if producer["workflow_host_sha"] == H2_PRODUCER_WORKFLOW_HOST_SHA:
        _require(
            jobs["Verify exact SDK runtime-version parser selectors"]["conclusion"] == "skipped",
            "H2 producer SDK parser job must be skipped",
        )
        _require(
            jobs["Consume native Linux ARM64 package"]["conclusion"] == "failure"
            and jobs["Consume native Linux x86_64 package"]["conclusion"] == "failure",
            "H2 producer bundled consumer jobs differ from the exact admitted run",
        )
    elif producer["workflow_host_sha"] == H9E58_PRODUCER_WORKFLOW_HOST_SHA:
        _require(
            jobs["Verify exact SDK runtime-version parser selectors"]["conclusion"] == "skipped",
            "H9e58 producer SDK parser job must be skipped",
        )
        _require(
            jobs["Consume native Linux ARM64 package"]["conclusion"] == "failure"
            and jobs["Consume native Linux x86_64 package"]["conclusion"] == "failure",
            "H9e58 producer bundled consumer jobs differ from the exact admitted run",
        )
    expected_run_conclusion = "failure" if any(job["conclusion"] == "failure" for job in jobs.values()) else "success"
    _require(producer["conclusion"] == expected_run_conclusion, "producer run conclusion does not match its closed job inventory")

    artifacts_value = _object(record.get("artifacts"), "accepted-input artifacts are malformed")
    _require(set(artifacts_value) == set(ARCHES), "accepted-input artifact architecture inventory is not closed")
    artifact_ids: set[int] = set()
    for architecture, artifact_value in artifacts_value.items():
        artifact = _object(artifact_value, "accepted-input artifact record is malformed")
        _require(set(artifact) == {"id", "name", "digest", "size_in_bytes"}, "accepted-input artifact has unexpected or missing fields")
        _require(_is_int(artifact.get("id")), "accepted-input artifact ID is invalid")
        _require(artifact["id"] not in artifact_ids, "accepted-input architecture artifacts share an ID")
        artifact_ids.add(artifact["id"])
        _require(artifact.get("name") == f"sedna-first-binary-{identity['product_sha']}-{architecture}-{producer['run_id']}", "accepted-input artifact name is not canonical")
        _require(isinstance(artifact.get("digest"), str) and DIGEST.fullmatch(artifact["digest"]) is not None, "accepted-input artifact digest is invalid")
        _require(_is_int(artifact.get("size_in_bytes")), "accepted-input artifact size is invalid")
    return record


def _json_object_without_duplicate_keys(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
    result: dict[str, Any] = {}
    for key, value in pairs:
        _require(key not in result, "accepted-input manifest contains a duplicate JSON key")
        result[key] = value
    return result


def _read_manifest(path: Path = ACCEPTED_INPUTS_PATH) -> Mapping[str, Any]:
    try:
        with path.open(encoding="utf-8") as stream:
            value = json.load(stream, object_pairs_hook=_json_object_without_duplicate_keys)
    except (OSError, json.JSONDecodeError):
        raise ValueError("accepted-input manifest is missing or malformed") from None
    manifest = _object(value, "accepted-input manifest must be an object")
    _require(set(manifest) == {"schema_version", "records"}, "accepted-input manifest has unexpected fields")
    _require(manifest.get("schema_version") == "sedna-first-binary-accepted-inputs-v1", "accepted-input manifest schema version is unsupported")
    rows = manifest.get("records")
    _require(isinstance(rows, list) and rows, "accepted-input manifest has no records")
    seen: set[str] = set()
    for index, row_value in enumerate(rows):
        try:
            row = _validate_manifest_record(row_value)
        except ValueError as exc:
            record_id = row_value.get("record_id") if isinstance(row_value, dict) else None
            label = record_id if isinstance(record_id, str) and re.fullmatch(r"[a-z0-9][a-z0-9-]{0,79}", record_id) else f"index {index}"
            raise ValueError(f"accepted-input record {label} rejected: {exc}") from exc
        _require(row["record_id"] not in seen, "accepted-input manifest repeats a record ID")
        seen.add(row["record_id"])
    return manifest


def select_accepted_record(manifest: Mapping[str, Any], inputs: Mapping[str, Any]) -> Mapping[str, Any]:
    _require(manifest.get("schema_version") == "sedna-first-binary-accepted-inputs-v1", "accepted-input manifest schema version is unsupported")
    rows = manifest.get("records")
    _require(isinstance(rows, list), "accepted-input manifest has no record list")
    matches: list[Mapping[str, Any]] = []
    for row_value in rows:
        row = _validate_manifest_record(row_value)
        identity = row["identity"]
        producer = row["producer"]
        if (
            identity["product_sha"] == inputs.get("product_sha")
            and identity["comparison_base_ref"] == inputs.get("comparison_base_ref")
            and identity["comparison_base_sha"] == inputs.get("comparison_base_sha")
            and identity["fixture_sha"] == inputs.get("fixture_sha")
            and identity["sdk_sha"] == inputs.get("sdk_sha")
            and identity["profile"] == inputs.get("profile")
            and producer["run_id"] == inputs.get("producer_run_id")
            and producer["workflow_host_sha"] == inputs.get("producer_workflow_host_sha")
        ):
            matches.append(row)
    _require(len(matches) == 1, "no unique accepted-input record matches the exact T/B/Q/S/producer/profile tuple")
    selected = matches[0]
    _require(selected["disposition"] == "accepted" and selected["w14780_eligible"] is True, "matching producer record is diagnostic and cannot qualify for w14780")
    return selected


def _accepted_inputs_from_env(env: Mapping[str, str]) -> dict[str, Any]:
    producer_run_text = _required_env(env, "PRODUCER_RUN_ID")
    producer_host = _required_env(env, "PRODUCER_WORKFLOW_HOST_SHA")
    _require(RUN_ID.fullmatch(producer_run_text) is not None, "producer run ID is invalid")
    _require(SHA.fullmatch(producer_host) is not None, "producer workflow host SHA is invalid")
    return {
        "product_sha": _required_env(env, "TARGET_SHA"),
        "comparison_base_ref": _required_env(env, "BASE_REF"),
        "comparison_base_sha": _required_env(env, "BASE_SHA"),
        "fixture_sha": _required_env(env, "FIXTURE_SHA"),
        "sdk_sha": _required_env(env, "SDK_SHA"),
        "profile": _required_env(env, "CONSUMER_PROFILE"),
        "producer_run_id": int(producer_run_text),
        "producer_workflow_host_sha": producer_host,
    }


def _verify_trusted_consumer_ref(env: Mapping[str, str], *, checkout_sha: str | None = None) -> None:
    expected_ref = f"refs/heads/{TRUSTED_CONSUMER_BRANCH}"
    _require(env.get("GITHUB_EVENT_NAME") == "workflow_dispatch", "consume-existing requires workflow_dispatch")
    _require(env.get("GITHUB_REF") == expected_ref, "consume-existing workflow host ref is not the trusted branch")
    _require(env.get("GITHUB_REF_NAME") == TRUSTED_CONSUMER_BRANCH, "consume-existing workflow host branch is not trusted")
    _require(env.get("GITHUB_WORKFLOW_REF") == f"{REPOSITORY}/{WORKFLOW_PATH}@{expected_ref}", "consume-existing workflow_ref is not the trusted workflow host")
    host_sha = env.get("GITHUB_SHA", "")
    _require(SHA.fullmatch(host_sha) is not None, "consume-existing workflow host SHA is invalid")
    expected_h = env.get("EXPECTED_H", host_sha)
    _require(expected_h == host_sha, "workflow host SHA differs from GITHUB_SHA")
    if checkout_sha is not None:
        _require(checkout_sha == host_sha, "checked-out workflow source differs from GITHUB_SHA")


def validate_runner_policy_inputs(env: Mapping[str, str]) -> None:
    if env.get("MODE") == "build" and env.get("CONSUMER_PROFILE") == BROWSER_DIAGNOSTIC_PROFILE:
        _require(env.get("GITHUB_REPOSITORY") == REPOSITORY, "Browser diagnostic repository is not admitted")
        browser_diagnostic_test_plan(
            mode=env.get("MODE", ""),
            profile=env.get("CONSUMER_PROFILE", ""),
            product_sha=env.get("TARGET_SHA", ""),
            base_ref=env.get("BASE_REF", ""),
            base_sha=env.get("BASE_SHA", ""),
            fixture_sha=env.get("FIXTURE_SHA", ""),
            sdk_sha=env.get("SDK_SHA", ""),
        )
        _verify_trusted_consumer_ref(env)
        try:
            checkout_sha = subprocess.check_output(["git", "rev-parse", "HEAD"], text=True).strip()
        except (OSError, subprocess.CalledProcessError):
            raise ValueError("cannot read checked-out workflow host SHA") from None
        _verify_trusted_consumer_ref(env, checkout_sha=checkout_sha)
        print("exact same-run Browser diagnostic profile admitted")
        return
    if env.get("MODE") != "consume-existing":
        return
    _require(env.get("GITHUB_REPOSITORY") == REPOSITORY, "consume-existing repository is not admitted")
    _verify_trusted_consumer_ref(env)
    try:
        checkout_sha = subprocess.check_output(["git", "rev-parse", "HEAD"], text=True).strip()
    except (OSError, subprocess.CalledProcessError):
        raise ValueError("cannot read checked-out workflow host SHA") from None
    _verify_trusted_consumer_ref(env, checkout_sha=checkout_sha)
    selected = select_accepted_record(_read_manifest(), _accepted_inputs_from_env(env))
    print(f"accepted-input manifest record selected: {selected['record_id']}")


def _diagnostic_compatibility_constants() -> tuple[Mapping[str, Any], dict[str, Any], dict[str, Any], dict[str, Any]]:
    diagnostic_records = [row for row in _read_manifest()["records"] if row["disposition"] == "diagnostic"]
    _require(len(diagnostic_records) == 1, "manifest must retain exactly one diagnostic record for regression fixtures")
    record = diagnostic_records[0]
    identity = record["identity"]
    producer = record["producer"]
    producer_constants = {
        **producer,
        "product_sha": identity["product_sha"],
        "comparison_base_ref": identity["comparison_base_ref"],
        "comparison_base_sha": identity["comparison_base_sha"],
        "fixture_sha": identity["fixture_sha"],
        "sdk_sha": identity["sdk_sha"],
        "profile": identity["profile"],
    }
    artifact_constants = {key: dict(value) for key, value in record["artifacts"].items()}
    job_constants = {job["name"]: (job["conclusion"], job["runner"], job["ran"]) for job in record["jobs"]}
    return record, producer_constants, artifact_constants, job_constants


DIAGNOSTIC_RECORD, CROSS_RUN_PRODUCER, CROSS_RUN_ARTIFACTS, PRODUCER_JOB_CONTRACT = _diagnostic_compatibility_constants()


def _workflow_matches(workflow: Mapping[str, Any], *, workflow_id: int) -> None:
    _require(workflow.get("id") == workflow_id == WORKFLOW_ID, "workflow API ID mismatch")
    _require(workflow.get("path") == WORKFLOW_PATH, "workflow API canonical path mismatch")
    _require(workflow.get("state") == "active", "registered producer workflow is not active")


def _repository_matches(run: Mapping[str, Any]) -> None:
    repository = run.get("repository")
    head_repository = run.get("head_repository")
    _require(isinstance(repository, dict), "Actions run omitted repository identity")
    _require(
        repository.get("id") == REPOSITORY_ID and repository.get("full_name") == REPOSITORY,
        "Actions run repository identity mismatch",
    )
    _require(isinstance(head_repository, dict), "Actions run omitted head repository identity")
    _require(
        head_repository.get("id") == REPOSITORY_ID and head_repository.get("full_name") == REPOSITORY,
        "Actions run head repository identity mismatch",
    )


def _workflow_run_matches(
    run: Mapping[str, Any],
    *,
    run_id: int,
    workflow_host_sha: str,
    branch: str,
    event: str,
    workflow_id: int,
) -> None:
    _require(run.get("id") == run_id, "Actions run ID mismatch")
    _require(run.get("workflow_id") == workflow_id == WORKFLOW_ID, "Actions run workflow ID mismatch")
    _require(run.get("head_sha") == workflow_host_sha and SHA.fullmatch(workflow_host_sha), "Actions run host SHA mismatch")
    _require(run.get("head_branch") == branch and isinstance(branch, str), "Actions run branch mismatch")
    _require(run.get("event") == event == "workflow_dispatch", "Actions run event is not workflow_dispatch")
    _repository_matches(run)


def _job_map(payload: Mapping[str, Any], *, expected_names: set[str] | None = None) -> dict[str, Mapping[str, Any]]:
    rows = _complete_rows(payload, "jobs", "jobs")
    names = [row.get("name") for row in rows]
    _require(all(isinstance(name, str) for name in names), "Actions job name is malformed")
    _require(len(names) == len(set(names)), "Actions API returned duplicate job names")
    result = {str(row["name"]): row for row in rows}
    if expected_names is not None:
        _require(set(result) == expected_names, "producer run job inventory differs from the frozen contract")
    return result


def _verify_producer_job_set(payload: Mapping[str, Any], expected_jobs: list[Mapping[str, Any]]) -> None:
    contract = {str(item["name"]): item for item in expected_jobs}
    jobs = _job_map(payload, expected_names=set(contract))
    for name, expected in contract.items():
        job = jobs[name]
        _require(job.get("status") == expected["status"], f"producer job {name} status differs from the accepted-input record")
        _require(job.get("conclusion") == expected["conclusion"], f"producer job {name} conclusion differs from the accepted-input record")
        _validate_hosted_job(job, expected["runner"], ran=expected["ran"])


def _artifact_map(payload: Mapping[str, Any]) -> list[Mapping[str, Any]]:
    return _complete_rows(payload, "artifacts", "artifact")


def _verify_artifact_association(
    artifact: Mapping[str, Any],
    *,
    run_id: int,
    workflow_host_sha: str,
    branch: str,
) -> None:
    association = artifact.get("workflow_run")
    _require(isinstance(association, dict), "artifact API omitted workflow-run association")
    _require(
        association.get("id") == run_id
        and association.get("head_sha") == workflow_host_sha
        and association.get("head_branch") == branch
        and association.get("repository_id") == REPOSITORY_ID
        and association.get("head_repository_id") == REPOSITORY_ID,
        "artifact association differs from the exact producer run",
    )


def verify_existing_producer(
    run: Mapping[str, Any],
    workflow: Mapping[str, Any],
    jobs_payload: Mapping[str, Any],
    artifacts_payload: Mapping[str, Any],
    *,
    producer_run_id: int,
    producer_workflow_host_sha: str,
    product_sha: str,
    base_ref: str,
    base_sha: str,
    accepted_record: Mapping[str, Any],
) -> dict[str, Mapping[str, Any]]:
    record = _validate_manifest_record(accepted_record)
    _require(record["disposition"] == "accepted" and record["w14780_eligible"] is True, "producer record is not acceptance eligible")
    identity = record["identity"]
    expected = record["producer"]
    _require(producer_run_id == expected["run_id"], "producer run is outside the accepted exact run")
    _require(producer_workflow_host_sha == expected["workflow_host_sha"], "producer host is outside the accepted exact SHA")
    _require(product_sha == identity["product_sha"], "product target is outside the accepted exact SHA")
    _require(base_ref == identity["comparison_base_ref"], "comparison base ref differs from the accepted base")
    _require(base_sha == identity["comparison_base_sha"], "comparison base SHA differs from the accepted base")
    _workflow_matches(workflow, workflow_id=expected["workflow_id"])
    _workflow_run_matches(
        run,
        run_id=expected["run_id"],
        workflow_host_sha=expected["workflow_host_sha"],
        branch=expected["branch"],
        event=expected["event"],
        workflow_id=expected["workflow_id"],
    )
    _require(run.get("run_attempt") == expected["run_attempt"], "producer run attempt differs from the admitted attempt")
    _require(run.get("status") == expected["status"] and run.get("conclusion") == expected["conclusion"], "producer run status differs from the accepted record")
    _verify_producer_job_set(jobs_payload, record["jobs"])

    artifacts = _artifact_map(artifacts_payload)
    prefix = f"sedna-first-binary-{product_sha}-"
    package_artifacts = [item for item in artifacts if isinstance(item.get("name"), str) and item["name"].startswith(prefix)]
    _require(len(package_artifacts) == 2, "producer package artifact inventory is not exactly the native pair")
    by_name = {str(item.get("name")): item for item in package_artifacts}
    _require(len(by_name) == 2, "producer package artifacts contain duplicate names")
    selected: dict[str, Mapping[str, Any]] = {}
    for arch, expected_artifact in record["artifacts"].items():
        artifact = by_name.get(expected_artifact["name"])
        _require(isinstance(artifact, dict), f"producer {arch} artifact name mismatch")
        _require(artifact.get("id") == expected_artifact["id"], f"producer {arch} artifact ID mismatch")
        _require(artifact.get("digest") == expected_artifact["digest"], f"producer {arch} artifact digest mismatch")
        _require(artifact.get("size_in_bytes") == expected_artifact["size_in_bytes"], f"producer {arch} artifact size mismatch")
        _require(artifact.get("expired") is False, f"producer {arch} artifact is expired")
        _require(type(artifact.get("size_in_bytes")) is int and artifact["size_in_bytes"] > 0, f"producer {arch} artifact is empty")
        _verify_artifact_association(
            artifact,
            run_id=expected["run_id"],
            workflow_host_sha=expected["workflow_host_sha"],
            branch=expected["branch"],
        )
        selected[arch] = artifact
    return selected


def verify_build_artifact(
    run: Mapping[str, Any],
    workflow: Mapping[str, Any],
    jobs_payload: Mapping[str, Any],
    artifacts_payload: Mapping[str, Any],
    *,
    run_id: int,
    workflow_host_sha: str,
    branch: str,
    product_sha: str,
    base_sha: str,
    architecture: str,
) -> Mapping[str, Any]:
    arch = ARCHES[architecture]
    _workflow_matches(workflow, workflow_id=WORKFLOW_ID)
    _workflow_run_matches(
        run,
        run_id=run_id,
        workflow_host_sha=workflow_host_sha,
        branch=branch,
        event="workflow_dispatch",
        workflow_id=WORKFLOW_ID,
    )
    _require(run.get("status") == "in_progress", "same-run consumer is not in the active build workflow")
    _require(product_sha and SHA.fullmatch(product_sha), "product SHA is invalid")
    _require(base_sha and SHA.fullmatch(base_sha), "comparison base SHA is invalid")
    jobs = _job_map(jobs_payload)
    for name, expected_runner in (
        ("Verify standard runner graph and exact identities", "ubuntu-24.04"),
        (arch["package_job"], arch["runner"]),
        (arch["consumer_job"], arch["runner"]),
    ):
        job = jobs.get(name)
        _require(isinstance(job, dict), f"required same-run job is missing: {name}")
        if name == arch["consumer_job"]:
            _require(job.get("status") == "in_progress" and job.get("conclusion") is None, "same-run consumer job is not active")
        else:
            _require(job.get("status") == "completed" and job.get("conclusion") == "success", f"required same-run job did not succeed: {name}")
        _validate_hosted_job(job, expected_runner, ran=True)

    artifacts = _artifact_map(artifacts_payload)
    expected_name = f"sedna-first-binary-{product_sha}-{architecture}-{run_id}"
    matches = [item for item in artifacts if item.get("name") == expected_name]
    _require(len(matches) == 1, "same-run API did not return exactly one native package artifact")
    artifact = matches[0]
    _require(type(artifact.get("id")) is int and artifact["id"] > 0, "same-run artifact ID is invalid")
    _require(artifact.get("expired") is False, "same-run package artifact is expired")
    _require(type(artifact.get("size_in_bytes")) is int and artifact["size_in_bytes"] > 0, "same-run package artifact is empty")
    _require(isinstance(artifact.get("digest"), str) and DIGEST.fullmatch(artifact["digest"]), "same-run artifact digest is invalid")
    _verify_artifact_association(artifact, run_id=run_id, workflow_host_sha=workflow_host_sha, branch=branch)
    return artifact


def verify_current_consumer(
    run: Mapping[str, Any],
    workflow: Mapping[str, Any],
    jobs_payload: Mapping[str, Any],
    *,
    env: Mapping[str, str],
    architecture: str,
    product_sha: str,
    base_sha: str,
) -> dict[str, Any]:
    arch = ARCHES[architecture]
    repository = env.get("GITHUB_REPOSITORY", "")
    _require(repository == REPOSITORY, "current Actions repository identity mismatch")
    _require(env.get("GITHUB_ACTIONS") == "true", "hosted GitHub Actions execution is required")
    _require(env.get("GITHUB_EVENT_NAME") == "workflow_dispatch", "consumer route requires workflow_dispatch")
    _require(env.get("RUNNER_OS") == "Linux" and platform.system() == "Linux", "consumer runner is not Linux")
    _require(env.get("RUNNER_ARCH") == arch["run_arch"], "consumer RUNNER_ARCH differs from the native target")
    _require(platform.machine().lower() == architecture, "consumer process architecture differs from the native target")
    _require(env.get("EXPECTED_RUNNER_LABEL") == arch["runner"], "consumer job label is not the exact standard runner")
    _require(env.get("PRODUCT_ARCH") == architecture, "consumer architecture input mismatch")
    _require(env.get("RUST_TARGET") == arch["target"], "consumer target input mismatch")
    _require(product_sha and SHA.fullmatch(product_sha), "consumer product SHA is invalid")
    _require(base_sha and SHA.fullmatch(base_sha), "consumer base SHA is invalid")

    run_id_text = env.get("GITHUB_RUN_ID", "")
    attempt_text = env.get("GITHUB_RUN_ATTEMPT", "")
    _require(RUN_ID.fullmatch(run_id_text) is not None, "current run ID is invalid")
    _require(RUN_ID.fullmatch(attempt_text) is not None, "current run attempt is invalid")
    run_id = int(run_id_text)
    attempt = int(attempt_text)
    host_sha = env.get("GITHUB_SHA", "")
    ref = env.get("GITHUB_REF", "")
    branch = env.get("GITHUB_REF_NAME", "")
    workflow_ref = env.get("GITHUB_WORKFLOW_REF", "")
    _require(SHA.fullmatch(host_sha) is not None, "current workflow host SHA is invalid")
    _require(ref.startswith("refs/heads/") and bool(branch), "consumer run is not on a branch ref")
    _require(run.get("status") == "in_progress", "current consumer Actions run is not in progress")
    _workflow_run_matches(
        run,
        run_id=run_id,
        workflow_host_sha=host_sha,
        branch=branch,
        event="workflow_dispatch",
        workflow_id=WORKFLOW_ID,
    )
    _workflow_matches(workflow, workflow_id=WORKFLOW_ID)
    _require(run.get("run_attempt") == attempt, "current consumer run attempt mismatch")
    _require(ref == f"refs/heads/{branch}", "current consumer ref and branch disagree")
    _require(workflow_ref == f"{REPOSITORY}/{WORKFLOW_PATH}@{ref}", "current consumer workflow_ref mismatch")
    _require(run.get("conclusion") is None, "current consumer run has a terminal conclusion")
    if env.get("MODE") == "consume-existing":
        _verify_trusted_consumer_ref(env)

    job_name = arch["consumer_job"]
    jobs = _job_map(jobs_payload)
    matches = [job for job in jobs.values() if job.get("name") == job_name]
    _require(len(matches) == 1, "current consumer job is absent or duplicated in Actions API")
    job = matches[0]
    _require(job.get("status") == "in_progress" and job.get("conclusion") is None, "current consumer job is not active")
    _validate_hosted_job(job, arch["runner"], ran=True)

    return {
        "schema_version": "sedna-first-binary-consumer-api-v1",
        "repository": REPOSITORY,
        "workflow_path": WORKFLOW_PATH,
        "event": "workflow_dispatch",
        "workflow_host_sha": host_sha,
        "run_id": run_id,
        "run_attempt": attempt,
        "ref": ref,
        "branch": branch,
        "workflow_ref": workflow_ref,
        "product_sha": product_sha,
        "comparison_base_sha": base_sha,
        "target": arch["target"],
        "architecture": architecture,
        "runner_label": arch["runner"],
    }


def reconcile_consumer_results(
    *,
    junit_path: Path,
    consumer_context_path: Path,
    producer_evidence_path: Path,
    witness_dir: Path,
    result_path: Path,
    pytest_exit: int,
    mode: str,
    profile: str,
    fixture_sha: str,
    sdk_sha: str,
    runner_temp: Path,
) -> dict[str, Any]:
    """Write a separate, exact-consumer result receipt without mutating inputs."""
    issues: list[str] = []
    try:
        consumer = _object(
            json.loads(consumer_context_path.read_text(encoding="utf-8")),
            "consumer API context is malformed",
        )
    except (OSError, json.JSONDecodeError, ValueError):
        consumer = {}
        issues.append("current consumer API context is missing or malformed")
    try:
        producer = _object(
            json.loads(producer_evidence_path.read_text(encoding="utf-8")),
            "producer API evidence is malformed",
        )
    except (OSError, json.JSONDecodeError, ValueError):
        producer = {}
        issues.append("producer API evidence is missing or malformed")

    cases: list[ET.Element] = []
    try:
        cases = list(ET.parse(junit_path).getroot().iter("testcase"))
    except (OSError, ET.ParseError):
        issues.append("pytest did not produce a parseable JUnit report")
    failures = sum(len(case.findall("failure")) for case in cases)
    errors = sum(len(case.findall("error")) for case in cases)
    skipped = sum(len(case.findall("skipped")) for case in cases)
    if pytest_exit != 0:
        issues.append(f"pytest exited with status {pytest_exit}")
    if not cases:
        issues.append("JUnit report contains zero executed test cases")
    if failures or errors:
        issues.append(f"JUnit reports failures={failures} errors={errors}")
    if skipped:
        issues.append(f"JUnit reports {skipped} skipped cases")

    positive_prefix = "test_packaged_historical_upgrade_and_reopen"
    negative_prefix = "test_packaged_historical_rejection_preserves_preimage"
    expected_positive = EXPECTED_STATE_POSITIVE
    expected_negative = EXPECTED_STATE_NEGATIVE

    def labels(prefix: str) -> list[str]:
        result: list[str] = []
        for case in cases:
            name = case.attrib.get("name", "")
            if name.startswith(prefix):
                if not name.startswith(prefix + "[") or not name.endswith("]"):
                    issues.append("state-history JUnit name is not an exact parameterized case")
                    continue
                result.append(name[len(prefix) + 1 : -1])
        return result

    positive = labels(positive_prefix)
    negative = labels(negative_prefix)
    browser_diagnostic: dict[str, Any] | None = None
    if mode == "build" and profile == BROWSER_DIAGNOSTIC_PROFILE:
        try:
            selected_plan = browser_diagnostic_test_plan(
                mode=mode,
                profile=profile,
                product_sha=str(producer.get("product_sha", "")),
                base_ref=str(producer.get("comparison_base_ref", "")),
                base_sha=str(producer.get("comparison_base_sha", "")),
                fixture_sha=fixture_sha,
                sdk_sha=sdk_sha,
            )
        except ValueError as error:
            issues.append(str(error))
            selected_plan = {"state": frozenset(), "plain": frozenset()}
        expected_state = selected_plan["state"]
        expected_plain = selected_plan["plain"]
        expected_consumer = {
            "product_sha": BROWSER_DIAGNOSTIC_PRODUCT_SHA,
            "comparison_base_sha": BROWSER_DIAGNOSTIC_BASE_SHA,
            "ref": f"refs/heads/{TRUSTED_CONSUMER_BRANCH}",
            "branch": TRUSTED_CONSUMER_BRANCH,
            "workflow_ref": f"{REPOSITORY}/{WORKFLOW_PATH}@refs/heads/{TRUSTED_CONSUMER_BRANCH}",
        }
        if any(consumer.get(key) != value for key, value in expected_consumer.items()):
            issues.append("Browser diagnostic consumer identity differs from its exact route")
        expected_producer = {
            "product_sha": BROWSER_DIAGNOSTIC_PRODUCT_SHA,
            "comparison_base_ref": BROWSER_DIAGNOSTIC_BASE_REF,
            "comparison_base_sha": BROWSER_DIAGNOSTIC_BASE_SHA,
        }
        if any(producer.get(key) != value for key, value in expected_producer.items()):
            issues.append("Browser diagnostic producer identity differs from its exact route")
        if producer.get("run_id") != consumer.get("run_id") or producer.get("workflow_host_sha") != consumer.get("workflow_host_sha"):
            issues.append("Browser diagnostic package and consumer are not from the same exact run/host")
        browser_diagnostic = _browser_diagnostic_from_junit(cases, issues)
    elif mode == "build":
        profile = "full"
        expected_state = expected_positive | expected_negative
        expected_plain = FULL_PLAIN_TESTS
    elif mode == "consume-existing":
        try:
            selected_plan = consume_existing_test_plan(fixture_sha, sdk_sha, profile)
        except ValueError as error:
            issues.append(str(error))
            selected_plan = {"state": frozenset(), "plain": frozenset()}
        expected_state = selected_plan["state"]
        expected_plain = selected_plan["plain"]
    else:
        issues.append("consumer result has an unsupported mode/profile")
        expected_state = frozenset()
        expected_plain = frozenset()
    if profile not in {"pair", "focused", "full", BROWSER_DIAGNOSTIC_PROFILE}:
        issues.append("consumer result has an unsupported mode/profile")
    if mode == "consume-existing" and profile == "pair":
        if positive != ["fresh"] or negative != ["bad_checksum"]:
            issues.append("pair profile did not execute its exact positive and negative state cases")
    elif mode == "consume-existing" and profile == "focused":
        if positive or negative:
            issues.append("focused profile unexpectedly executed state-history cases")
    elif mode in {"build", "consume-existing"} and profile == "full":
        if len(positive) != 16 or set(positive) != expected_positive:
            issues.append("JUnit does not contain the exact 16 positive state-history cases")
        if len(negative) != 8 or set(negative) != expected_negative:
            issues.append("JUnit does not contain the exact 8 negative state-history cases")
    plain_names = {
        case.attrib.get("name", "") for case in cases
        if not case.attrib.get("name", "").startswith((positive_prefix, negative_prefix))
    }
    if plain_names != expected_plain:
        issues.append("JUnit plain-test inventory differs from the exact selected profile")
    if len(cases) != len(expected_state) + len(expected_plain):
        issues.append("JUnit executed-case count differs from the exact selected profile")

    try:
        resolved_temp = runner_temp.resolve(strict=True)
        if witness_dir.is_symlink() or witness_dir.resolve(strict=True).parent != resolved_temp:
            raise ValueError("state-history witnesses are not in the exact runner temp directory")
        if witness_dir.stat().st_mode & 0o077:
            raise ValueError("state-history witness directory is not private")
        witness_paths = sorted(witness_dir.glob("*.json"))
    except (OSError, ValueError):
        witness_paths = []
        issues.append("state-history witness directory is missing, unsafe, or outside RUNNER_TEMP")

    witness_hashes: dict[str, str] = {}
    witness_cases: set[str] = set()
    witness_versions: set[str] = set()
    witness_archive_digests: set[str] = set()
    expected_fixture_sha = fixture_sha or str(producer.get("product_sha", ""))
    for path in witness_paths:
        try:
            witness = _object(
                json.loads(path.read_text(encoding="utf-8")), "witness is malformed"
            )
        except (OSError, json.JSONDecodeError, ValueError):
            issues.append("state-history witness JSON is missing or malformed")
            continue
        case_name = witness.get("case")
        if not isinstance(case_name, str) or case_name not in expected_state:
            issues.append("state-history witness has an unexpected case identity")
            continue
        witness_cases.add(case_name)
        expected_fields = {
            "product_target_sha": producer.get("product_sha"),
            "comparison_base_sha": producer.get("comparison_base_sha"),
            "fixture_source_sha": expected_fixture_sha,
            "producer_workflow_host_sha": producer.get("workflow_host_sha"),
            "producer_run_id": producer.get("run_id"),
            "consumer_workflow_host_sha": consumer.get("workflow_host_sha"),
            "consumer_run_id": consumer.get("run_id"),
            "consumer_run_attempt": consumer.get("run_attempt"),
            "target": producer.get("target"),
            "artifact_id": producer.get("artifact_id"),
            "artifact_name": producer.get("artifact_name"),
        }
        if any(witness.get(key) != value for key, value in expected_fields.items()):
            issues.append(f"state-history witness {case_name} producer/consumer identity mismatch")
        archive_digest = witness.get("package_archive_sha256")
        if not isinstance(archive_digest, str) or DIGEST.fullmatch(f"sha256:{archive_digest}") is None:
            issues.append(f"state-history witness {case_name} package digest is malformed")
        else:
            witness_archive_digests.add(archive_digest)
        version = witness.get("package_version")
        if not isinstance(version, str) or not version:
            issues.append(f"state-history witness {case_name} package version is missing")
        else:
            witness_versions.add(version)
        witness_hashes[case_name] = hashlib.sha256(path.read_bytes()).hexdigest()
    if len(witness_paths) != len(expected_state) or witness_cases != expected_state:
        issues.append("state-history witness inventory differs from the exact selected profile")
    if len(witness_versions) > 1 or len(witness_archive_digests) > 1:
        issues.append("state-history witnesses report inconsistent package identity")

    result = {
        "schema_version": "sedna-first-binary-consumer-result-v1",
        "mode": mode,
        "profile": profile,
        "fixture_source_sha": expected_fixture_sha,
        "sdk_source_sha": sdk_sha or str(producer.get("product_sha", "")),
        "consumer": consumer,
        "producer": producer,
        "pytest_exit_code": pytest_exit,
        "consumer_context_sha256": (
            hashlib.sha256(consumer_context_path.read_bytes()).hexdigest()
            if consumer_context_path.is_file() else None
        ),
        "producer_evidence_sha256": (
            hashlib.sha256(producer_evidence_path.read_bytes()).hexdigest()
            if producer_evidence_path.is_file() else None
        ),
        "junit_sha256": (
            hashlib.sha256(junit_path.read_bytes()).hexdigest() if junit_path.is_file() else None
        ),
        "executed_cases": len(cases),
        "failures": failures,
        "errors": errors,
        "skipped": skipped,
        "state_history_positive_cases": len(positive),
        "state_history_negative_cases": len(negative),
        "state_history_witnesses": len(witness_hashes),
        "state_history_witness_sha256": dict(sorted(witness_hashes.items())),
        "browser_output_diagnostic": browser_diagnostic,
        "issues": issues,
    }
    result_path.write_text(json.dumps(result, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    return result


def _fetch_json(api_url: str, token: str, path: str) -> dict[str, Any]:
    request = Request(
        f"{api_url.rstrip('/')}{path}",
        headers={
            "Accept": "application/vnd.github+json",
            "Authorization": f"Bearer {token}",
            "X-GitHub-Api-Version": "2026-03-10",
        },
    )
    try:
        with urlopen(request, timeout=30) as response:
            final_url = urlsplit(response.geturl())
            expected_url = urlsplit(api_url)
            _require(
                final_url.scheme == "https" and final_url.netloc == expected_url.netloc,
                "GitHub API redirected outside its verified HTTPS origin",
            )
            value = json.loads(response.read())
    except HTTPError as error:
        raise ValueError(f"GitHub Actions API returned HTTP {error.code} for a required identity read") from None
    except URLError as error:
        raise ValueError(f"GitHub Actions API request failed: {error.reason}") from None
    except json.JSONDecodeError:
        raise ValueError("GitHub Actions API returned malformed JSON") from None
    return dict(_object(value, "GitHub Actions API response is not an object"))


def _write_private_json(path_text: str, runner_temp_text: str, value: Mapping[str, Any]) -> None:
    runner_temp = Path(runner_temp_text).resolve(strict=True)
    path = Path(path_text)
    _require(path.is_absolute(), "identity output path must be absolute")
    _require(path.parent.resolve(strict=True) == runner_temp, "identity output must be directly under RUNNER_TEMP")
    _require(not path.exists() and not path.is_symlink(), "identity output already exists")
    flags = os.O_WRONLY | os.O_CREAT | os.O_EXCL
    if hasattr(os, "O_NOFOLLOW"):
        flags |= os.O_NOFOLLOW
    descriptor = os.open(path, flags, 0o600)
    with os.fdopen(descriptor, "w", encoding="utf-8") as stream:
        json.dump(value, stream, indent=2, sort_keys=True)
        stream.write("\n")


def _required_env(env: Mapping[str, str], name: str) -> str:
    value = env.get(name)
    if value is None:
        raise ValueError(f"required workflow environment value is missing: {name}")
    return value


def main() -> int:
    env = os.environ
    mode = _required_env(env, "MODE")
    architecture = _required_env(env, "PRODUCT_ARCH")
    if architecture not in ARCHES:
        raise ValueError("unsupported native consumer architecture")
    product_sha = _required_env(env, "TARGET_SHA")
    base_ref = _required_env(env, "BASE_REF")
    base_sha = _required_env(env, "BASE_SHA")
    if not SHA.fullmatch(product_sha) or not SHA.fullmatch(base_sha):
        raise ValueError("workflow target/base must be full lowercase SHAs")
    profile = env.get("CONSUMER_PROFILE", "full")
    if mode == "build" and profile == BROWSER_DIAGNOSTIC_PROFILE:
        browser_diagnostic_test_plan(
            mode=mode,
            profile=profile,
            product_sha=product_sha,
            base_ref=base_ref,
            base_sha=base_sha,
            fixture_sha=env.get("FIXTURE_SHA", ""),
            sdk_sha=env.get("SDK_SHA", ""),
        )
        _verify_trusted_consumer_ref(env)
    elif base_ref != "main":
        raise ValueError("consumer route requires the pinned main comparison ref")

    api_url = _required_env(env, "API_URL")
    api = urlsplit(api_url)
    if api.scheme != "https" or api.netloc != "api.github.com":
        raise ValueError("consumer route requires the official HTTPS GitHub API")
    token = _required_env(env, "GITHUB_TOKEN")
    _require(bool(token), "GitHub Actions API token is empty")
    repository = _required_env(env, "GITHUB_REPOSITORY")
    _require(repository == REPOSITORY, "workflow repository is not the admitted public repository")
    if mode == "consume-existing" or (mode == "build" and profile == BROWSER_DIAGNOSTIC_PROFILE):
        _verify_trusted_consumer_ref(env)
        try:
            consumer_checkout_sha = subprocess.check_output(
                ["git", "-C", ".workflow-src", "rev-parse", "HEAD"], text=True
            ).strip()
        except (OSError, subprocess.CalledProcessError):
            raise ValueError("cannot read checked-out workflow host SHA in consumer job") from None
        _verify_trusted_consumer_ref(env, checkout_sha=consumer_checkout_sha)

    current_run_id = int(_required_env(env, "GITHUB_RUN_ID"))
    current_run = _fetch_json(api_url, token, f"/repos/{REPOSITORY}/actions/runs/{current_run_id}")
    current_workflow_id = current_run.get("workflow_id")
    _require(type(current_workflow_id) is int, "current run omitted a numeric workflow ID")
    current_workflow = _fetch_json(api_url, token, f"/repos/{REPOSITORY}/actions/workflows/{current_workflow_id}")
    current_jobs = _fetch_json(api_url, token, f"/repos/{REPOSITORY}/actions/runs/{current_run_id}/jobs?per_page=100")
    context = verify_current_consumer(
        current_run,
        current_workflow,
        current_jobs,
        env=env,
        architecture=architecture,
        product_sha=product_sha,
        base_sha=base_sha,
    )

    if mode == "consume-existing":
        accepted_record = select_accepted_record(_read_manifest(), _accepted_inputs_from_env(env))
        producer_run_text = _required_env(env, "PRODUCER_RUN_ID")
        producer_host = _required_env(env, "PRODUCER_WORKFLOW_HOST_SHA")
        _require(RUN_ID.fullmatch(producer_run_text) is not None, "producer run ID is invalid")
        _require(SHA.fullmatch(producer_host) is not None, "producer workflow host SHA is invalid")
        producer_run_id = int(producer_run_text)
        _require(context["run_id"] != producer_run_id, "cross-run consumer must use a distinct current run")
        producer_run = _fetch_json(api_url, token, f"/repos/{REPOSITORY}/actions/runs/{producer_run_id}")
        producer_workflow_id = producer_run.get("workflow_id")
        _require(type(producer_workflow_id) is int, "producer run omitted a numeric workflow ID")
        producer_workflow = _fetch_json(api_url, token, f"/repos/{REPOSITORY}/actions/workflows/{producer_workflow_id}")
        producer_jobs = _fetch_json(api_url, token, f"/repos/{REPOSITORY}/actions/runs/{producer_run_id}/jobs?per_page=100")
        producer_artifacts = _fetch_json(api_url, token, f"/repos/{REPOSITORY}/actions/runs/{producer_run_id}/artifacts?per_page=100")
        selected = verify_existing_producer(
            producer_run,
            producer_workflow,
            producer_jobs,
            producer_artifacts,
            producer_run_id=producer_run_id,
            producer_workflow_host_sha=producer_host,
            product_sha=product_sha,
            base_ref=base_ref,
            base_sha=base_sha,
            accepted_record=accepted_record,
        )
        artifact = selected[architecture]
    elif mode == "build":
        _require(not env.get("PRODUCER_RUN_ID") and not env.get("PRODUCER_WORKFLOW_HOST_SHA"), "build mode must not accept cross-run producer inputs")
        if profile == BROWSER_DIAGNOSTIC_PROFILE:
            browser_diagnostic_test_plan(
                mode=mode,
                profile=profile,
                product_sha=product_sha,
                base_ref=base_ref,
                base_sha=base_sha,
                fixture_sha=env.get("FIXTURE_SHA", ""),
                sdk_sha=env.get("SDK_SHA", ""),
            )
        else:
            _require(not env.get("FIXTURE_SHA") and not env.get("SDK_SHA"), "build mode must not accept separate fixture/SDK revisions")
        producer_run_id = context["run_id"]
        producer_host = context["workflow_host_sha"]
        jobs_payload = current_jobs
        artifacts_payload = _fetch_json(api_url, token, f"/repos/{REPOSITORY}/actions/runs/{producer_run_id}/artifacts?per_page=100")
        artifact = verify_build_artifact(
            current_run,
            current_workflow,
            jobs_payload,
            artifacts_payload,
            run_id=producer_run_id,
            workflow_host_sha=producer_host,
            branch=context["branch"],
            product_sha=product_sha,
            base_sha=base_sha,
            architecture=architecture,
        )
    else:
        raise ValueError("consumer jobs may run only for build or consume-existing modes")

    expected_name = f"sedna-first-binary-{product_sha}-{architecture}-{producer_run_id}"
    _require(artifact.get("name") == expected_name, "selected artifact name is not canonical")
    producer_record = {
        "schema_version": "sedna-first-binary-producer-api-v1",
        "repository": REPOSITORY,
        "workflow_path": WORKFLOW_PATH,
        "event": "workflow_dispatch",
        "workflow_host_sha": producer_host,
        "run_id": producer_run_id,
        "branch": (accepted_record["producer"]["branch"] if mode == "consume-existing" else context["branch"]),
        "product_sha": product_sha,
        "comparison_base_ref": base_ref,
        "comparison_base_sha": base_sha,
        "artifact_id": artifact["id"],
        "artifact_name": artifact["name"],
        "artifact_digest": artifact["digest"],
        "artifact_size_bytes": artifact["size_in_bytes"],
        "architecture": architecture,
        "target": ARCHES[architecture]["target"],
        "runner_label": ARCHES[architecture]["runner"],
    }
    _write_private_json(_required_env(env, "CONSUMER_CONTEXT_OUT"), _required_env(env, "RUNNER_TEMP"), context)
    _write_private_json(_required_env(env, "PRODUCER_EVIDENCE_OUT"), _required_env(env, "RUNNER_TEMP"), producer_record)

    output_path = Path(_required_env(env, "GITHUB_OUTPUT"))
    with output_path.open("a", encoding="utf-8") as output:
        output.write(f"artifact_id={artifact['id']}\n")
        output.write(f"artifact_name={artifact['name']}\n")
        output.write(f"artifact_digest={artifact['digest']}\n")
        output.write(f"producer_run_id={producer_run_id}\n")
        output.write(f"producer_workflow_host_sha={producer_host}\n")
    return 0


if __name__ == "__main__":
    try:
        if sys.argv[1:] == ["--validate-accepted-inputs"]:
            validate_runner_policy_inputs(os.environ)
            raise SystemExit(0)
        if sys.argv[1:] == ["--verify-consumer-results"]:
            environment = os.environ
            result = reconcile_consumer_results(
                junit_path=Path(_required_env(environment, "CONSUMER_JUNIT")),
                consumer_context_path=Path(_required_env(environment, "CONSUMER_CONTEXT")),
                producer_evidence_path=Path(_required_env(environment, "PRODUCER_EVIDENCE")),
                witness_dir=Path(_required_env(environment, "STATE_WITNESS_DIR")),
                result_path=Path(_required_env(environment, "CONSUMER_RESULT")),
                pytest_exit=int(_required_env(environment, "PYTEST_EXIT_CODE")),
                mode=_required_env(environment, "MODE"),
                profile=environment.get("CONSUMER_PROFILE", "full"),
                fixture_sha=environment.get("FIXTURE_SHA", ""),
                sdk_sha=environment.get("SDK_SHA", ""),
                runner_temp=Path(_required_env(environment, "RUNNER_TEMP")),
            )
            print(
                "consumer test gate: "
                f"exit={result['pytest_exit_code']} cases={result['executed_cases']} "
                f"failures={result['failures']} errors={result['errors']} "
                f"skipped={result['skipped']} "
                f"state_witnesses={result['state_history_witnesses']}"
            )
            for issue in result["issues"]:
                print(f"consumer test gate failed: {issue}", file=sys.stderr)
            raise SystemExit(1 if result["issues"] else 0)
        raise SystemExit(main())
    except (ValueError, KeyError, TypeError, OSError) as error:
        print(f"first-binary producer verification failed: {error}", file=sys.stderr)
        raise SystemExit(1)
