import importlib.util
import sys
import textwrap
import unittest
from pathlib import Path


SCRIPT = Path(__file__).resolve().with_name("verify_arm64_gnullvm_aquery.py")
SPEC = importlib.util.spec_from_file_location("verify_arm64_gnullvm_aquery", SCRIPT)
if SPEC is None or SPEC.loader is None:
    raise RuntimeError(f"unable to load {SCRIPT}")
VERIFY_AQUERY = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = VERIFY_AQUERY
SPEC.loader.exec_module(VERIFY_AQUERY)

TARGET = "//codex-rs/otel:otel"


def action_block(
    *,
    target: str,
    execution_platform: str,
    inputs: str,
    mnemonic: str = "Rustc",
) -> str:
    return textwrap.dedent(
        f"""\
        action 'Compiling {target}'
          Mnemonic: {mnemonic}
          Target: {target}
          Execution platform: {execution_platform}
          Inputs: {inputs}
        """
    )


GNULLVM_PLATFORM = "@@rules_rs+//rs/platforms:aarch64-pc-windows-gnullvm"
GNULLVM_INPUTS = (
    "external/rustc_windows_aarch64_gnullvm_1_90_0/rustc "
    "external/cargo_windows_aarch64_gnullvm_1_90_0/cargo"
)


class VerifyArm64GnullvmAqueryTest(unittest.TestCase):
    def test_accepts_selected_rustc_action_with_gnullvm_platform_and_inputs(self) -> None:
        output = action_block(
            target=TARGET,
            execution_platform=GNULLVM_PLATFORM,
            inputs=GNULLVM_INPUTS,
        )

        VERIFY_AQUERY.verify_selected_rust_action(output, TARGET)

    def test_rejects_gnullvm_toolchain_text_in_an_unrelated_action(self) -> None:
        selected_action = action_block(
            target=TARGET,
            execution_platform=GNULLVM_PLATFORM,
            inputs=(
                "external/rustc_windows_aarch64_msvc_1_90_0/rustc "
                "external/cargo_windows_aarch64_msvc_1_90_0/cargo"
            ),
        )
        unrelated_action = action_block(
            target="//codex-rs/other:other",
            execution_platform=GNULLVM_PLATFORM,
            inputs=GNULLVM_INPUTS,
        )

        with self.assertRaisesRegex(
            VERIFY_AQUERY.AqueryValidationError,
            "missing ARM64 gnullvm toolchain inputs",
        ):
            VERIFY_AQUERY.verify_selected_rust_action(
                selected_action + "\n" + unrelated_action,
                TARGET,
            )

    def test_rejects_arm64_msvc_input_in_the_selected_action(self) -> None:
        output = action_block(
            target=TARGET,
            execution_platform=GNULLVM_PLATFORM,
            inputs=(
                GNULLVM_INPUTS
                + " external/rustc_windows_aarch64_msvc_1_90_0/rustc"
            ),
        )

        with self.assertRaisesRegex(
            VERIFY_AQUERY.AqueryValidationError,
            "must not use ARM64 MSVC toolchain inputs",
        ):
            VERIFY_AQUERY.verify_selected_rust_action(output, TARGET)

    def test_rejects_a_similarly_named_target(self) -> None:
        output = action_block(
            target="//codex-rs/otel:otel_extra",
            execution_platform=GNULLVM_PLATFORM,
            inputs=GNULLVM_INPUTS,
        )

        with self.assertRaisesRegex(
            VERIFY_AQUERY.AqueryValidationError,
            "expected exactly one Rustc action",
        ):
            VERIFY_AQUERY.verify_selected_rust_action(output, TARGET)


if __name__ == "__main__":
    unittest.main()
