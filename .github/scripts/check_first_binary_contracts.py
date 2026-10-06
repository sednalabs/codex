#!/usr/bin/env python3
"""Check static selected-source contracts, never replace consumer execution."""
import argparse
import ast
import os
import re
import shlex
import subprocess
import sys
from pathlib import Path


WORKFLOW = ".github/workflows/sedna-branch-build.yml"
STEP_BUILD = "Run packaged first-binary consumers and preserve pytest status"
STEP_EXISTING = "Run existing-package consumers with explicit producer provenance"
TEST_ROOT = "scripts/codex_package/smoke_tests/first_binary"
CONSUMER_JOBS = {
    "consume-linux-x86_64": "Consume native Linux x86_64 package",
    "consume-linux-aarch64": "Consume native Linux ARM64 package",
}


class Diagnostics:
    def __init__(self):
        self.errors = []
        self.warnings = []

    def error(self, mode, job, path, message):
        self.errors.append("ERROR mode={} job={} path={}: {}".format(mode, job, path, message))

    def warning(self, mode, job, path, message):
        self.warnings.append("WARNING mode={} job={} path={}: {}".format(mode, job, path, message))


def _literal_string(node):
    if isinstance(node, ast.Constant) and isinstance(node.value, str):
        return node.value
    raise ValueError("expected literal string")


def _workflow_steps(path):
    text = path.read_text(encoding="utf-8")
    lines = text.splitlines()
    wanted = {STEP_BUILD, STEP_EXISTING}
    found = {name: [] for name in wanted}
    in_jobs = False
    current_job = None
    current_job_name = None
    i = 0
    while i < len(lines):
        line = lines[i]
        if re.match(r"^jobs:\s*$", line):
            in_jobs = True
        elif in_jobs:
            job_match = re.match(r"^  ([A-Za-z0-9_-]+):\s*$", line)
            if job_match:
                current_job, current_job_name = job_match.group(1), None
            else:
                job_name_match = re.match(r"^    name:\s*(.+?)\s*$", line)
                if job_name_match:
                    current_job_name = job_name_match.group(1).strip("\"'")
        match = re.match(r"^(\s*)- name: (.+?)\s*$", line)
        if not match or match.group(2) not in wanted:
            i += 1
            continue
        indent, step_name = len(match.group(1)), match.group(2)
        block = []
        i += 1
        while i < len(lines):
            if lines[i].strip() and len(lines[i]) - len(lines[i].lstrip()) <= indent:
                break
            block.append(lines[i])
            i += 1
        mode = "build" if step_name == STEP_BUILD else "consume-existing"
        conditions = [line.strip() for line in block if re.match(r"^\s{8}if:", line)]
        expected_condition = "if: ${{ inputs.mode == '" + mode + "' }}"
        if conditions != [expected_condition]:
            raise ValueError("{} has unsupported mode condition".format(step_name))
        if CONSUMER_JOBS.get(current_job) != current_job_name:
            raise ValueError("{} is not in its declared native consumer job".format(step_name))
        run = None
        j = 0
        while j < len(block):
            m = re.match(r"^\s+run:\s*\|\s*$", block[j])
            if m:
                run_indent = len(block[j]) - len(block[j].lstrip()) + 2
                body = []
                j += 1
                while j < len(block) and (not block[j].strip() or len(block[j]) - len(block[j].lstrip()) >= run_indent):
                    body.append(block[j][run_indent:] if block[j].strip() else "")
                    j += 1
                run = "\n".join(body)
                break
            j += 1
        if run is None:
            raise ValueError("{} step has no literal run block".format(step_name))
        found[match.group(2)].append((current_job_name or current_job or "unknown-job", run))
    for key in wanted:
        labels = [label for label, _ in found[key]]
        if len(labels) != 2 or set(labels) != set(CONSUMER_JOBS.values()):
            raise ValueError("{} must occur once in each native consumer architecture".format(key))
    return found


def _pytest_argv(run):
    if len(re.findall(r"^\s*(?:python(?:3)?\s+-m\s+)?pytest\b", run, re.M)) != 1:
        raise ValueError("expected exactly one literal pytest command")
    lines = run.splitlines()
    command = []
    in_pytest = False
    for line in lines:
        stripped = line.strip()
        if not in_pytest and re.search(r"\bpytest\s+-q\s*\\?$", stripped):
            in_pytest = True
            command.append(stripped[:-1].rstrip() if stripped.endswith("\\") else stripped)
            continue
        if in_pytest:
            if not stripped:
                break
            command.append(stripped[:-1].rstrip() if stripped.endswith("\\") else stripped)
            if not stripped.endswith("\\"):
                break
    if not command:
        raise ValueError("literal pytest -q command not found")
    try:
        argv = shlex.split(" ".join(command))
        if any(any(char in token for char in ";|&<>`") or "$(" in token for token in argv):
            raise ValueError("unsupported shell control, substitution or redirection in pytest command")
        return argv
    except ValueError as exc:
        raise ValueError("unsupported pytest quoting: {}".format(exc))


def _validate_selected_paths(run, argv):
    command = re.search(r"^\s*(?:python(?:3)?\s+-m\s+)?pytest\b", run, re.M)
    case = re.search(r"^\s*case\b", run, re.M)
    binding_limit = min(command.start(), case.start()) if case else command.start()
    for variable in ("TEST_ROOT", "test_root"):
        if any("${" + variable + "}" in token for token in argv):
            bindings = list(re.finditer(r"^\s*" + variable + r"=(.*)$", run, re.M))
            if (len(bindings) != 1 or bindings[0].group(1) != '"${FIXTURE_ROOT}/' + TEST_ROOT + '"'
                    or bindings[0].start() >= binding_limit):
                raise ValueError("selected test root {} must bind the literal fixture directory".format(variable))
    if "--confcutdir" in argv:
        raise ValueError("pytest confcutdir must use exactly one supported --confcutdir= form")
    cutdirs = [token.split("=", 1)[1] for token in argv if token.startswith("--confcutdir=")]
    normalized = [value.replace("${PRODUCT_ROOT}/", "").replace("${TEST_ROOT}", TEST_ROOT).replace("${test_root}", TEST_ROOT) for value in cutdirs]
    if normalized != [TEST_ROOT]:
        raise ValueError("pytest confcutdir must bind the selected first_binary parser directory")


def _selected_pytest_argv(run, mode, profile, step_name):
    argv = _pytest_argv(run)
    arrays = re.findall(r"\$\{([A-Za-z_]+)\[@\]\}", " ".join(argv))
    if not arrays:
        if mode != "build" or any("${" in arg and "[@]" in arg for arg in argv):
            raise ValueError("selected consumer argument array missing")
        _validate_selected_paths(run, argv)
        return argv  # Native ARM build uses direct literal arguments.
    expected = {"TEST_ARGS", "PRODUCER_ARGS", "FIXTURE_ARGS"} if mode == "build" else {"test_args"}
    if set(arrays) != expected or len(arrays) != len(expected):
        raise ValueError("unsupported or duplicate selected consumer argument arrays")
    selector = "${MODE}:${CONSUMER_PROFILE}" if mode == "build" else "${CONSUMER_PROFILE}"
    cases = re.findall(r'case\s+"' + re.escape(selector) + r'"\s+in\s*\n(.*?)\nesac', run, re.S)
    if len(cases) != 1:
        raise ValueError("selected argv case statement missing or duplicated")
    label = (
        "build:browser-diagnostic"
        if mode == "build" and profile == "browser-diagnostic"
        else "build:*" if mode == "build" else profile
    )
    arms = re.findall(r"^\s*" + re.escape(label) + r"\)\s*(.*?)\s*;;", cases[0], re.M | re.S)
    if len(arms) != 1:
        raise ValueError("selected case arm {!r} missing or duplicated".format(label))
    expanded = {}
    for variable in arrays:
        values = re.findall(r"\b" + variable + r"=\(([^()]*)\)", arms[0], re.S)
        if len(values) != 1 or "`" in values[0]:
            raise ValueError("selected argv array {} missing or unsupported".format(variable))
        expanded["${" + variable + "[@]}"] = shlex.split(values[0])
    selected = [value for arg in argv for value in expanded.get(arg, [arg])]
    _validate_selected_paths(run, selected)
    return selected


def _custom_options(argv):
    options = set()
    for arg in argv:
        if arg.startswith("--"):
            name = arg[2:].split("=", 1)[0]
            if name not in {"confcutdir", "junitxml"}:
                options.add(name)
    return options


def _selected_nodeids(argv):
    nodeids = []
    for arg in argv:
        normalized = arg.replace("${PRODUCT_ROOT}/", "").replace("${TEST_ROOT}", TEST_ROOT).replace("${test_root}", TEST_ROOT)
        if normalized == TEST_ROOT or normalized.startswith(TEST_ROOT + "/"):
            nodeids.append(normalized)
        elif "::" in normalized:
            nodeids.append(normalized)
    return nodeids


def _module_test_map(root, modules):
    owners = {}
    for module in modules:
        path = root / module
        if not path.is_file():
            raise ValueError("selected source module missing: {}".format(module))
        tree = ast.parse(path.read_text(encoding="utf-8"))
        for node in tree.body:
            if isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef)) and node.name.startswith("test_"):
                owners.setdefault(node.name, []).append(module)
    return owners


def _nodeid_parts(nodeid):
    if not nodeid.startswith(TEST_ROOT + "/") or "::" not in nodeid:
        return None, None
    module, name = nodeid[len(TEST_ROOT) + 1:].rsplit("::", 1)
    return module, name


def _registered_options(source):
    tree = ast.parse(source)
    functions = [node for node in ast.walk(tree) if isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef)) and node.name == "pytest_addoption"]
    if len(functions) != 1 or functions[0] not in tree.body or not isinstance(functions[0], ast.FunctionDef):
        raise ValueError("expected one synchronous pytest_addoption function")
    function = functions[0]
    if not function.args.args or not isinstance(function.args.args[0], ast.arg):
        raise ValueError("pytest_addoption parser parameter is unsupported")
    group_name = None
    group_assigned = False
    for stmt in function.body:
        if isinstance(stmt, ast.Assign) and len(stmt.targets) == 1 and isinstance(stmt.targets[0], ast.Name):
            if group_assigned:
                raise ValueError("pytest_addoption must use one option group")
            value = stmt.value
            if not (isinstance(value, ast.Call) and isinstance(value.func, ast.Attribute) and value.func.attr == "getgroup" and isinstance(value.func.value, ast.Name) and value.func.value.id == function.args.args[0].arg and len(value.args) == 1 and not value.keywords):
                raise ValueError("unsupported pytest_addoption assignment")
            _literal_string(value.args[0])
            group_name = stmt.targets[0].id
            group_assigned = True
        elif isinstance(stmt, ast.Expr) and isinstance(stmt.value, ast.Call):
            _validate_addoption(stmt.value, group_name)
        elif isinstance(stmt, ast.For):
            if stmt.orelse or group_name is None or not isinstance(stmt.target, ast.Name) or not isinstance(stmt.iter, (ast.Tuple, ast.List)):
                raise ValueError("unsupported pytest_addoption option loop")
            values = [_literal_string(item) for item in stmt.iter.elts]
            if not values or len(values) != len(set(values)) or any(value.startswith("--") for value in values):
                raise ValueError("option loop must contain unique literal option stems")
            if len(stmt.body) != 1 or not isinstance(stmt.body[0], ast.Expr) or not isinstance(stmt.body[0].value, ast.Call):
                raise ValueError("unsupported pytest_addoption loop body")
            call = stmt.body[0].value
            _validate_addoption(call, group_name, loop_var=stmt.target.id)
        else:
            raise ValueError("unsupported statement in pytest_addoption")
    options = set()
    required = set()
    for stmt in function.body:
        calls = [stmt.value] if isinstance(stmt, ast.Expr) and isinstance(stmt.value, ast.Call) else [stmt.body[0].value] if isinstance(stmt, ast.For) else []
        for call in calls:
            names = [_literal_string(call.args[0])[2:]] if isinstance(call.args[0], ast.Constant) else []
            if not isinstance(call.args[0], ast.Constant) and isinstance(stmt, ast.For):
                names = [_literal_string(item) for item in stmt.iter.elts]
            if options.intersection(names):
                raise ValueError("duplicate option registration")
            options.update(names)
            if any(k.arg == "required" and isinstance(k.value, ast.Constant) and k.value.value is True for k in call.keywords):
                required.update(names)
    return options, required


def _validate_addoption(call, group_name, loop_var=None):
    if not isinstance(call.func, ast.Attribute) or call.func.attr != "addoption" or not isinstance(call.func.value, ast.Name) or call.func.value.id != group_name or len(call.args) != 1:
        raise ValueError("unsupported pytest_addoption call")
    value = call.args[0]
    if loop_var is None:
        if not isinstance(value, ast.Constant) or not isinstance(value.value, str) or not value.value.startswith("--") or value.value == "--":
            raise ValueError("direct option registration must be a literal long option")
    else:
        if not (isinstance(value, ast.JoinedStr) and len(value.values) == 2 and isinstance(value.values[0], ast.Constant) and value.values[0].value == "--" and isinstance(value.values[1], ast.FormattedValue) and value.values[1].conversion == -1 and value.values[1].format_spec is None and isinstance(value.values[1].value, ast.Name) and value.values[1].value.id == loop_var):
            raise ValueError("option loop must use f'--{option}' literally")
    for kw in call.keywords:
        if kw.arg == "required" and not (isinstance(kw.value, ast.Constant) and type(kw.value.value) is bool):
            raise ValueError("required must be a literal boolean")
        if not isinstance(kw.value, (ast.Constant, ast.Tuple, ast.List)) and not (isinstance(kw.value, ast.Name) and kw.value.id in {"int", "str", "Path"}):
            raise ValueError("addoption keyword values must be literal")


def _observer_warnings(path, mode, job, diagnostics):
    tree = ast.parse(path.read_text(encoding="utf-8"))
    for node in ast.walk(tree):
        if not isinstance(node, ast.With):
            continue
        for item in node.items:
            call = item.context_expr
            if not (isinstance(call, ast.Call) and isinstance(call.func, ast.Name) and call.func.id == "PackagedTui"):
                continue
            cols = next((kw.value.value for kw in call.keywords if kw.arg == "columns" and isinstance(kw.value, ast.Constant) and type(kw.value.value) is int), None)
            if cols is None or not isinstance(item.optional_vars, ast.Name):
                continue
            variable = item.optional_vars.id
            calls = sorted(
                (child for child in ast.walk(node) if isinstance(child, ast.Call)),
                key=lambda child: (child.lineno, child.col_offset),
            )
            sent = set()
            for call in calls:
                if not isinstance(call.func, ast.Attribute) or not isinstance(call.func.value, ast.Name) or call.func.value.id != variable:
                    continue
                if not call.args or not isinstance(call.args[0], ast.Constant) or not isinstance(call.args[0].value, str):
                    continue
                marker = call.args[0].value
                if call.func.attr == "send":
                    if marker == "\r":
                        break
                    sent.add(marker)
                elif call.func.attr == "until" and marker in sent and len(marker) > cols:
                    diagnostics.warning(mode, job, str(path), "literal sent and awaited exceeds PackagedTui columns; raw contiguous soft-wrap may still match")
                    break


def _git_head(root, expected, mode, label, diagnostics):
    try:
        actual = subprocess.check_output(["git", "-C", str(root), "rev-parse", "HEAD"], text=True, stderr=subprocess.STDOUT).strip()
    except (OSError, subprocess.CalledProcessError) as exc:
        diagnostics.error(mode, label, str(root), "cannot read git HEAD: {}".format(exc))
        return False
    if not re.fullmatch(r"[0-9a-f]{40}", expected or "") or actual != expected:
        diagnostics.error(mode, label, str(root), "HEAD {} does not equal expected full SHA {}".format(actual, expected))
        return False
    return True


def run(args, env=None):
    env = os.environ if env is None else env
    mode = env.get("MODE", "")
    profile = env.get("CONSUMER_PROFILE", "")
    browser_diagnostic = mode == "build" and profile == "browser-diagnostic"
    diagnostics = Diagnostics()
    if mode not in {"build", "consume-existing"}:
        diagnostics.error(mode, "selection", "MODE", "unsupported or missing MODE; expected build or consume-existing")
        return diagnostics
    if mode == "build" and profile not in {"full", "browser-diagnostic"}:
        diagnostics.error(mode, "selection", "CONSUMER_PROFILE", "build profile is not admitted")
        return diagnostics
    roots = {"workflow": args.workflow_root, "source": args.source_root}
    if mode == "consume-existing":
        roots.update(sdk=args.sdk_root, producer=args.producer_root)
    elif browser_diagnostic:
        roots.update(sdk=args.sdk_root)
    expected = {
        "workflow": env.get("EXPECTED_H"),
        "source": env.get("FIXTURE_SHA") if mode == "consume-existing" or browser_diagnostic else env.get("TARGET_SHA"),
    }
    if mode == "consume-existing" or browser_diagnostic:
        expected["sdk"] = env.get("SDK_SHA")
    if mode == "consume-existing":
        expected["producer"] = env.get("PRODUCER_WORKFLOW_HOST_SHA")
    for key, root in roots.items():
        if root is None:
            diagnostics.error(mode, key, "<root>", "required input root missing")
        else:
            _git_head(root, expected.get(key), mode, key, diagnostics)
    if diagnostics.errors:
        return diagnostics
    verifier = None
    if mode == "consume-existing" or browser_diagnostic:
        try:
            verifier = _load_verifier(args.workflow_root)
        except Exception as exc:
            diagnostics.error(mode, "manifest", "selected workflow verifier", "manifest/verifier unavailable (all-record validation): {}".format(exc))
    try:
        workflow_path = args.workflow_root / WORKFLOW
        steps = _workflow_steps(workflow_path)
    except (OSError, ValueError) as exc:
        diagnostics.error(mode, "workflow", WORKFLOW, str(exc))
        steps = None
    if steps:
        for step_name, step_runs in steps.items():
            step_mode = "build" if step_name == STEP_BUILD else "consume-existing"
            if step_mode != mode:
                continue
            source_file = args.source_root / TEST_ROOT / "conftest.py"
            try:
                reg, req = _registered_options(source_file.read_text(encoding="utf-8"))
            except (OSError, SyntaxError, ValueError) as exc:
                diagnostics.error(mode, step_name, str(source_file), "option registration unsupported: {}".format(exc))
                reg = req = None
            arch_nodeids_by_arch = []
            for job, run_block in step_runs:
                try:
                    argv = _selected_pytest_argv(run_block, mode, env.get("CONSUMER_PROFILE", ""), step_name)
                    nodeids = _selected_nodeids(argv)
                    arch_nodeids_by_arch.append((job, nodeids))
                    if mode == "build" and profile == "browser-diagnostic":
                        expected_nodeid = (
                            TEST_ROOT
                            + "/test_tui_agents_acceptance.py::"
                            + "test_packaged_tui_browser_output_keeps_images_and_manifest_metadata_separate"
                        )
                        if nodeids != [expected_nodeid]:
                            diagnostics.error(mode, job, str(workflow_path), "Browser diagnostic must select exactly its admitted nodeid")
                    elif mode == "build" and nodeids != [TEST_ROOT]:
                        diagnostics.error(mode, job, str(workflow_path), "build must select exactly the first_binary directory")
                    if reg is not None:
                        opts = _custom_options(argv)
                        for opt in sorted(opts - reg):
                            diagnostics.error(mode, job, str(workflow_path), "unknown pytest option --{}".format(opt))
                        for opt in sorted(req - opts):
                            diagnostics.error(mode, job, str(workflow_path), "missing required pytest option --{}".format(opt))
                except ValueError as exc:
                    diagnostics.error(mode, job, str(workflow_path), str(exc))
                    arch_nodeids_by_arch.append((job, []))
            if browser_diagnostic and verifier is not None:
                try:
                    plan = verifier.browser_diagnostic_test_plan(
                        mode=mode,
                        profile=profile,
                        product_sha=env["TARGET_SHA"],
                        base_ref=env["BASE_REF"],
                        base_sha=env["BASE_SHA"],
                        fixture_sha=env["FIXTURE_SHA"],
                        sdk_sha=env["SDK_SHA"],
                    )
                    expected_nodeid = (
                        TEST_ROOT
                        + "/test_tui_agents_acceptance.py::"
                        + "test_packaged_tui_browser_output_keeps_images_and_manifest_metadata_separate"
                    )
                    source_dir = args.source_root / TEST_ROOT
                    for label, nodeids in arch_nodeids_by_arch:
                        if nodeids != [expected_nodeid]:
                            diagnostics.error(mode, label, str(workflow_path), "Browser diagnostic selector inventory differs from its exact plan")
                        owners = _module_test_map(source_dir, ["test_tui_agents_acceptance.py"])
                        for name in sorted(plan["plain"]):
                            if owners.get(name) != ["test_tui_agents_acceptance.py"]:
                                diagnostics.error(mode, label, str(source_dir), "Browser diagnostic test is missing or duplicated in its exact source module")
                        if plan["state"]:
                            diagnostics.error(mode, label, str(workflow_path), "Browser diagnostic plan unexpectedly contains state-history cases")
                except Exception as exc:
                    diagnostics.error(mode, step_name, "Browser diagnostic plan", "cannot establish exact selector/source join: {}".format(exc))
            elif mode == "consume-existing" and verifier is not None:
                try:
                    plan = verifier.consume_existing_test_plan(env["FIXTURE_SHA"], env["SDK_SHA"], profile)
                    expected_plain = set(plan["plain"])
                    source_dir = args.source_root / TEST_ROOT
                    owners = _module_test_map(source_dir, sorted(path.name for path in source_dir.glob("test_*.py") if path.is_file()))
                    for label, nodeids in arch_nodeids_by_arch:
                        if profile == "full":
                            if nodeids != [TEST_ROOT]:
                                diagnostics.error(mode, label, str(workflow_path), "full profile must select exactly the first_binary directory")
                            actual = set(owners)
                            for name, modules in owners.items():
                                if len(modules) != 1:
                                    diagnostics.error(mode, label, str(source_dir), "plain test declaration is duplicated across modules: {}".format(name))
                            for name in sorted(expected_plain - actual):
                                diagnostics.error(mode, label, str(source_dir), "expected declared plain test missing: {}".format(name))
                            for name in sorted(actual - expected_plain):
                                diagnostics.error(mode, label, str(source_dir), "unexpected top-level plain test declaration: {}".format(name))
                        elif profile == "focused":
                            selected = nodeids
                            stems = []
                            for nodeid in selected:
                                module, test_name = _nodeid_parts(nodeid)
                                if module is None or "[" in test_name:
                                    diagnostics.error(mode, label, str(workflow_path), "focused selector has unsupported nodeid shape: {}".format(nodeid))
                                    continue
                                stems.append(test_name)
                                if owners.get(test_name) != [module]:
                                    diagnostics.error(mode, label, str(workflow_path), "focused selector module/function mismatch: {}".format(nodeid))
                            if len(stems) != len(set(stems)):
                                diagnostics.error(mode, label, str(workflow_path), "focused selectors contain duplicates")
                            for name in sorted(expected_plain - set(stems)):
                                diagnostics.error(mode, label, str(workflow_path), "selected nodeid missing expected test: {}".format(name))
                            for name in sorted(set(stems) - expected_plain):
                                diagnostics.error(mode, label, str(workflow_path), "selected nodeid is not in verifier plan: {}".format(name))
                        elif profile == "pair":
                            if plan["state"] != {"fresh", "bad_checksum"}:
                                diagnostics.error(mode, label, str(workflow_path), "pair plan state labels differ from exact workflow cases")
                            expected_nodes = {
                                TEST_ROOT + "/state_history/test_state_history.py::test_packaged_historical_upgrade_and_reopen[fresh]",
                                TEST_ROOT + "/state_history/test_state_history.py::test_packaged_historical_rejection_preserves_preimage[bad_checksum]",
                            }
                            if set(nodeids) != expected_nodes or len(nodeids) != len(expected_nodes):
                                diagnostics.error(mode, label, str(workflow_path), "pair selectors do not match exact selected state labels: {}".format(sorted(set(nodeids) ^ expected_nodes)))
                            state_owners = _module_test_map(source_dir, ["state_history/test_state_history.py"])
                            for nodeid in expected_nodes:
                                module, selected = _nodeid_parts(nodeid)
                                stem = selected.split("[", 1)[0]
                                if selected.split("[", 1)[1][:-1] not in plan["state"] or state_owners.get(stem) != [module]:
                                    diagnostics.error(mode, label, str(source_dir / module), "pair state selector or declaration mismatch: {}".format(nodeid))
                except Exception as exc:
                    diagnostics.error(mode, step_name, "selected source/plan", "cannot establish static plan join: {}".format(exc))
            if mode == "consume-existing":
                try:
                    selectors = verifier.sdk_test_plan(env["SDK_SHA"])["selectors"]
                    for selector in selectors:
                        module, name = selector.split("::", 1)
                        if not (args.sdk_root / module).is_file():
                            diagnostics.error(mode, step_name, str(args.sdk_root / module), "SDK selector module missing")
                            continue
                        found = _module_test_map(args.sdk_root, [module])
                        if found.get(name) != [module]:
                            diagnostics.error(mode, step_name, str(args.sdk_root / module), "SDK selector function missing: {}".format(name))
                except Exception as exc:
                    diagnostics.error(mode, step_name, "SDK selector plan", "cannot establish static SDK selector join: {}".format(exc))
                try:
                    accepted = verifier.select_accepted_record(verifier._read_manifest(), verifier._accepted_inputs_from_env(env))
                    producer_workflow = args.producer_root / WORKFLOW
                    names = _producer_job_names(producer_workflow)
                    wanted = {row["name"] for row in accepted["jobs"]}
                    if names != wanted:
                        diagnostics.error(mode, "historical-producer", str(producer_workflow), "static job-name set differs from selected manifest record {} (missing={}, extra={})".format(accepted["record_id"], sorted(wanted - names), sorted(names - wanted)))
                except Exception as exc:
                    diagnostics.error(mode, "manifest", "selected producer record", "cannot select/validate accepted record: {}".format(exc))
                observer = args.source_root / TEST_ROOT / "test_agent_control_tui_acceptance.py"
                try:
                    if observer.is_file():  # Older admitted fixture generations have no rich TUI observer.
                        _observer_warnings(observer, mode, step_name, diagnostics)
                except (OSError, SyntaxError) as exc:
                    diagnostics.error(mode, step_name, str(args.source_root / TEST_ROOT), "observer static source unavailable: {}".format(exc))
    return diagnostics


def _load_verifier(workflow_root):
    sys.path.insert(0, str(workflow_root / ".github/scripts"))
    import verify_existing_first_binary_producer as verifier
    return verifier


def _producer_job_names(path):
    names = []
    in_jobs = False
    current_has_name = False
    job_count = 0
    for line in path.read_text(encoding="utf-8").splitlines():
        if re.match(r"^jobs:\s*$", line):
            in_jobs = True
            continue
        if not in_jobs:
            continue
        if line.strip() and not line.startswith(" "):
            break
        job = re.match(r"^  ([A-Za-z0-9_-]+):\s*$", line)
        if job:
            if job_count and not current_has_name:
                raise ValueError("producer top-level job is missing a literal name")
            job_count += 1
            current_has_name = False
            continue
        match = re.match(r"^    name:\s*(.+?)\s*$", line)
        if match:
            value = match.group(1)
            if "${{" in value:
                raise ValueError("dynamic producer job name")
            if current_has_name:
                raise ValueError("producer top-level job has duplicate name fields")
            names.append(value.strip("\"'"))
            current_has_name = True
    if job_count and not current_has_name:
        raise ValueError("producer top-level job is missing a literal name")
    if not names or job_count != len(names) or len(names) != len(set(names)):
        raise ValueError("producer static job names are missing or duplicated")
    return set(names)


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--workflow-root", type=Path, required=True)
    parser.add_argument("--source-root", type=Path, required=True)
    parser.add_argument("--sdk-root", type=Path)
    parser.add_argument("--producer-root", type=Path)
    args = parser.parse_args(argv)
    result = run(args)
    for line in result.errors + result.warnings:
        print(line, file=sys.stderr if line.startswith("ERROR") else sys.stdout)
    if not result.errors:
        print("first-binary static contract checks passed (declaration evidence only)")
    return 1 if result.errors else 0


if __name__ == "__main__":
    raise SystemExit(main())
