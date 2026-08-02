use anyhow::Context;
use anyhow::Result;
use codex_app_server_protocol::generate_json_with_experimental;
use codex_app_server_protocol::generate_typescript_schema_fixture_subtree_for_tests;
use codex_app_server_protocol::read_schema_fixture_subtree;
use serde_json::Value;
use similar::TextDiff;
use std::collections::BTreeMap;
use std::path::Path;
use std::path::PathBuf;

#[test]
fn typescript_schema_fixtures_match_generated() -> Result<()> {
    let schema_root = schema_root()?;
    let fixture_tree = read_tree(&schema_root, "typescript")?;
    let generated_tree = generate_typescript_schema_fixture_subtree_for_tests()
        .context("generate in-memory typescript schema fixtures")?;

    assert_schema_trees_match("typescript", &fixture_tree, &generated_tree)?;
    let config_requirements = generated_tree
        .get(Path::new("v2/ConfigRequirements.ts"))
        .context("generated ConfigRequirements.ts should exist")?;
    anyhow::ensure!(
        !String::from_utf8_lossy(config_requirements).contains("../PathUri")
            || generated_tree.contains_key(Path::new("PathUri.ts")),
        "stable ConfigRequirements.ts imports PathUri but PathUri.ts was not generated"
    );
    for response_path in [
        "v2/ThreadListResponse.ts",
        "v2/ThreadLoadedListResponse.ts",
    ] {
        let response = generated_tree
            .get(Path::new(response_path))
            .with_context(|| format!("generated {response_path} should exist"))?;
        anyhow::ensure!(
            String::from_utf8_lossy(response).contains("ancestorFilterApplied?: boolean"),
            "{response_path} must keep the older-server ancestor acknowledgement optional"
        );
    }

    Ok(())
}

#[test]
fn json_schema_fixtures_match_generated() -> Result<()> {
    assert_schema_fixtures_match_generated("json", |output_dir| {
        generate_json_with_experimental(output_dir, /*experimental_api*/ false)
    })
}

/// Locks the additive subagent terminal-state contract across every checked-in JSON schema that
/// exposes a subagent activity. The whole-tree fixture test above proves the files are generated
/// from Rust; this focused check gives a compatibility failure a direct, legible assertion.
#[test]
fn json_schema_fixtures_keep_subagent_activity_kind_legacy_and_terminal_detail_additive(
) -> Result<()> {
    let schema_root = schema_root()?;
    let fixture_tree = read_tree(&schema_root, "json")?;
    let expected_kind = serde_json::json!(["started", "interacted", "interrupted"]);
    let expected_terminal_state = serde_json::json!(["errored"]);

    for (path, bytes) in fixture_tree.iter().filter(|(path, bytes)| {
        path.extension().is_some_and(|extension| extension == "json")
            && String::from_utf8_lossy(bytes).contains("\"SubAgentActivityKind\"")
    }) {
        anyhow::ensure!(
            bytes.last() == Some(&b'}'),
            "{} must use serde_json::to_vec_pretty bytes without a trailing newline",
            path.display()
        );
        let schema: Value = serde_json::from_slice(bytes)
            .with_context(|| format!("parse JSON schema fixture {}", path.display()))?;
        let mut activity_kinds = Vec::new();
        collect_named_schema_values(&schema, "SubAgentActivityKind", &mut activity_kinds);
        assert_eq!(
            activity_kinds.len(),
            1,
            "{} must define SubAgentActivityKind exactly once",
            path.display()
        );
        assert_eq!(
            activity_kinds[0].get("enum"),
            Some(&expected_kind),
            "{} must retain the three legacy activity kinds",
            path.display()
        );

        let mut terminal_states = Vec::new();
        collect_named_schema_values(
            &schema,
            "SubAgentActivityTerminalState",
            &mut terminal_states,
        );
        assert_eq!(
            terminal_states.len(),
            1,
            "{} must define the additive terminal-state enum exactly once",
            path.display()
        );
        assert_eq!(
            terminal_states[0].get("enum"),
            Some(&expected_terminal_state),
            "{} must expose the errored terminal detail",
            path.display()
        );

        let mut terminal_properties = Vec::new();
        collect_named_schema_values(&schema, "terminalState", &mut terminal_properties);
        assert_eq!(
            terminal_properties.len(),
            1,
            "{} must expose terminalState on subAgentActivity",
            path.display()
        );
        let terminal_property = terminal_properties[0];
        let any_of = terminal_property
            .get("anyOf")
            .and_then(Value::as_array)
            .with_context(|| format!("{} terminalState must be nullable", path.display()))?;
        assert!(
            any_of.iter().any(|schema| {
                schema
                    .get("$ref")
                    .and_then(Value::as_str)
                    .is_some_and(|reference| reference.ends_with("SubAgentActivityTerminalState"))
            }),
            "{} terminalState must refer to SubAgentActivityTerminalState",
            path.display()
        );
        assert!(
            any_of
                .iter()
                .any(|schema| schema.get("type") == Some(&Value::String("null".to_string()))),
            "{} terminalState must remain optional and nullable",
            path.display()
        );
    }

    Ok(())
}

fn collect_named_schema_values<'a>(
    schema: &'a Value,
    name: &str,
    output: &mut Vec<&'a Value>,
) {
    match schema {
        Value::Object(object) => {
            for (key, value) in object {
                if key == name {
                    output.push(value);
                }
                collect_named_schema_values(value, name, output);
            }
        }
        Value::Array(values) => {
            for value in values {
                collect_named_schema_values(value, name, output);
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
    }
}

fn assert_schema_fixtures_match_generated(
    label: &'static str,
    generate: impl FnOnce(&Path) -> Result<()>,
) -> Result<()> {
    let schema_root = schema_root()?;
    let fixture_tree = read_tree(&schema_root, label)?;

    let temp_dir = tempfile::tempdir().context("create temp dir")?;
    let generated_root = temp_dir.path().join(label);
    generate(&generated_root).with_context(|| {
        format!(
            "generate {label} schema fixtures into {}",
            generated_root.display()
        )
    })?;

    let generated_tree = read_tree(temp_dir.path(), label)?;

    assert_schema_trees_match(label, &fixture_tree, &generated_tree)?;

    Ok(())
}

fn assert_schema_trees_match(
    label: &str,
    fixture_tree: &BTreeMap<PathBuf, Vec<u8>>,
    generated_tree: &BTreeMap<PathBuf, Vec<u8>>,
) -> Result<()> {
    let fixture_paths = fixture_tree
        .keys()
        .map(|p| p.display().to_string())
        .collect::<Vec<_>>();
    let generated_paths = generated_tree
        .keys()
        .map(|p| p.display().to_string())
        .collect::<Vec<_>>();

    if fixture_paths != generated_paths {
        let expected = fixture_paths.join("\n");
        let actual = generated_paths.join("\n");
        let diff = TextDiff::from_lines(&expected, &actual)
            .unified_diff()
            .header("fixture", "generated")
            .to_string();

        panic!(
            "Vendored {label} app-server schema fixture file set doesn't match freshly generated output. \
Run `just write-app-server-schema` to overwrite with your changes.\n\n{diff}"
        );
    }

    // If the file sets match, diff contents for each file for a nicer error.
    for (path, expected) in fixture_tree {
        let actual = generated_tree
            .get(path)
            .ok_or_else(|| anyhow::anyhow!("missing generated file: {}", path.display()))?;

        if expected == actual {
            continue;
        }

        let expected_str = String::from_utf8_lossy(expected);
        let actual_str = String::from_utf8_lossy(actual);
        let diff = TextDiff::from_lines(&expected_str, &actual_str)
            .unified_diff()
            .header("fixture", "generated")
            .to_string();
        panic!(
            "Vendored {label} app-server schema fixture {} differs from generated output. \
Run `just write-app-server-schema` to overwrite with your changes.\n\n{diff}",
            path.display()
        );
    }

    Ok(())
}

fn schema_root() -> Result<PathBuf> {
    // In Bazel runfiles (especially manifest-only mode), resolving directories is not
    // reliable. Resolve a known file, then walk up to the schema root.
    let typescript_index = codex_utils_cargo_bin::find_resource!("schema/typescript/index.ts")
        .context("resolve TypeScript schema index.ts")?;
    let schema_root = typescript_index
        .parent()
        .and_then(|p| p.parent())
        .context("derive schema root from schema/typescript/index.ts")?
        .to_path_buf();

    // Sanity check that the JSON fixtures resolve to the same schema root.
    let json_bundle =
        codex_utils_cargo_bin::find_resource!("schema/json/codex_app_server_protocol.schemas.json")
            .context("resolve JSON schema bundle")?;
    let json_root = json_bundle
        .parent()
        .and_then(|p| p.parent())
        .context("derive schema root from schema/json/codex_app_server_protocol.schemas.json")?;
    anyhow::ensure!(
        schema_root == json_root,
        "schema roots disagree: typescript={} json={}",
        schema_root.display(),
        json_root.display()
    );

    Ok(schema_root)
}

fn read_tree(root: &Path, label: &str) -> Result<BTreeMap<PathBuf, Vec<u8>>> {
    read_schema_fixture_subtree(root, label).with_context(|| {
        format!(
            "read {label} schema fixture subtree from {}",
            root.display()
        )
    })
}
