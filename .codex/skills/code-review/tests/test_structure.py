"""Keep the repository-scoped review skill consolidated and public-safe."""

from pathlib import Path


ROOT = Path(__file__).parents[1]
SKILL = ROOT / "SKILL.md"
REFERENCES = ROOT / "references"


def test_code_review_skill_loads_all_reference_guidance() -> None:
    text = SKILL.read_text()
    expected = {
        "breaking-changes": "references/breaking-changes.md",
        "change-size": "references/change-size.md",
        "context": "references/context.md",
        "testing": "references/testing.md",
    }

    assert all(path in text for path in expected.values())
    assert all((REFERENCES / f"{name}.md").is_file() for name in expected)
    assert not any(
        (ROOT.parent / f"code-review-{name}" / "SKILL.md").exists()
        for name in expected
    )


def test_code_review_skill_preserves_review_contract_without_static_model_policy() -> None:
    text = SKILL.read_text().lower()

    assert "code-reviewed" in text
    assert "do not leave github comments" in text
    assert "subagent" not in text
    assert "reasoning" not in text
