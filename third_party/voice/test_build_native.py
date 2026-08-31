"""Check native build preconditions and subprocess failure propagation."""

import json
import os
from pathlib import Path
import platform
import shlex
import shutil
import subprocess
import sys
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import patch

from build_native import NativeBuild, validate_target


class NativeBuildTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        machine = platform.machine().lower()
        architecture = {"arm64": "aarch64", "amd64": "x86_64"}.get(machine, machine)
        suffix = {
            "Darwin": "apple-darwin",
            "Linux": "unknown-linux-gnu",
            "Windows": "pc-windows-msvc",
        }[platform.system()]
        self.args = SimpleNamespace(
            target=f"{architecture}-{suffix}",
            deployment_target="11.0",
            output=self.root / "build output",
            archives=self.root / "archives",
            cc=Path(sys.executable),
            cxx=Path(sys.executable),
            cmake=Path(sys.executable),
            make=Path(sys.executable),
            pkg_config=Path(sys.executable),
            shell=Path(sys.executable),
            bootstrap_make=None,
            jobs=2,
        )
        self.environment = {
            **os.environ,
            "INCLUDE": "fixture include",
            "LIB": "fixture lib",
        }

    def test_cli_entrypoint_imports_its_sibling_with_safe_path_enabled(self):
        result = subprocess.run(
            [
                sys.executable,
                "-P",
                str(Path(__file__).with_name("build_native.py")),
                "--help",
            ],
            cwd=self.root,
            capture_output=True,
            text=True,
            check=False,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("--archives", result.stdout)

    def test_rejects_cross_host_and_musl_builds(self):
        for target, system, machine, libc in [
            ("x86_64-pc-windows-msvc", "Darwin", "x86_64", ""),
            ("aarch64-apple-darwin", "Darwin", "x86_64", ""),
            ("x86_64-unknown-linux-gnu", "Linux", "x86_64", "musl"),
            ("x86_64-unknown-linux-musl", "Linux", "x86_64", "musl"),
        ]:
            with self.subTest(target=target, system=system):
                with self.assertRaises(ValueError):
                    validate_target(target, system, machine, libc, "11.0")

    def test_requires_explicit_macos_deployment_target(self):
        with self.assertRaisesRegex(ValueError, "Declare the macOS deployment target"):
            validate_target("aarch64-apple-darwin", "Darwin", "arm64", "", None)

    def test_missing_tool_does_not_create_output(self):
        self.args.cc = self.root / "missing compiler"
        with self.assertRaisesRegex(ValueError, "Missing build tool"):
            NativeBuild(self.args, self.environment)
        self.assertFalse(self.args.output.exists())

    @unittest.skipIf(os.name == "nt", "Exercises the Unix clang++ driver symlink")
    def test_bootstrap_links_cpp_runtime_through_compiler_symlink(self):
        tools = {name: shutil.which(name) for name in ("clang", "cmake", "make")}
        if not all(tools.values()):
            self.skipTest("Requires clang, CMake and make")
        compiler = self.root / "clang++"
        compiler.symlink_to(Path(tools["clang"]).resolve())
        self.args.cc = Path(tools["clang"])
        self.args.cxx = compiler
        self.args.cmake = Path(tools["cmake"])
        self.args.make = Path(tools["make"])
        source = self.root / "cpp-source"
        source.mkdir()
        (source / "CMakeLists.txt").write_text(
            "cmake_minimum_required(VERSION 3.15)\n"
            "project(cpp_driver LANGUAGES CXX)\n"
            "add_executable(cpp_driver main.cpp)\n"
            "install(TARGETS cpp_driver DESTINATION bin)\n"
        )
        (source / "main.cpp").write_text(
            '#include <iostream>\nint main() { std::cout << "linked"; }\n'
        )
        build = NativeBuild(self.args, self.environment)
        build.output.mkdir()
        build.sources = {"cpp-driver": source}
        build.cmake("cpp-driver", [], bootstrap=True)
        result = subprocess.run(
            [build.tools / "bin" / "cpp_driver"],
            check=True,
            capture_output=True,
            text=True,
        )
        self.assertEqual(result.stdout, "linked")

    def test_build_refuses_existing_output_before_reading_sources(self):
        self.args.output.mkdir()
        (self.args.output / "keep").write_text("untouched")
        with self.assertRaises(FileExistsError):
            NativeBuild(self.args, self.environment).build()
        self.assertEqual(
            {p.name: p.read_text() for p in self.args.output.iterdir()},
            {"keep": "untouched"},
        )

    def test_failure_retains_log_and_state_without_completion_marker(self):
        build = NativeBuild(self.args, self.environment)
        build.output.mkdir()
        command = [
            sys.executable,
            "-c",
            "print('synthetic failure'); raise SystemExit(23)",
        ]
        with self.assertRaises(subprocess.CalledProcessError) as error:
            build.run("failure", command)
        self.assertEqual(error.exception.returncode, 23)
        self.assertEqual(
            (build.output / "failure.log").read_text().strip(), "synthetic failure"
        )
        self.assertEqual(
            json.loads((build.output / "build-state.json").read_text()),
            {
                "target": self.args.target,
                "deployment_target": "11.0",
                "steps": [{"name": "failure", "command": command, "exit_code": 23}],
            },
        )
        self.assertFalse((build.output / "built.json").exists())

    def test_ambient_native_discovery_variables_are_not_inherited(self):
        inherited = {
            **self.environment,
            "PKG_CONFIG_PATH": "/ambient",
            "CMAKE_PREFIX_PATH": "/ambient",
            "CPATH": "/ambient",
            "CFLAGS": "-I/ambient",
            "LDFLAGS": "-L/ambient",
        }
        environment = NativeBuild(self.args, inherited).environment
        self.assertFalse(any("/ambient" in value for value in environment.values()))
        self.assertEqual(environment["PKG_CONFIG_PATH"], "")

    def test_meson_receives_private_link_inputs_with_spaces(self):
        build = NativeBuild(self.args, self.environment)
        build.sources = {"meson": self.root / "meson", "glib": self.root / "glib"}
        with patch.object(build, "run"):
            build.meson("glib", [])
        if build.windows:
            self.assertEqual(
                build.environment["LDFLAGS"], f'"/LIBPATH:{build.prefix / "lib"}"'
            )
        else:
            self.assertEqual(
                shlex.split(build.environment["LDFLAGS"]),
                [f"-L{build.prefix / 'lib'}", f"-Wl,-rpath,{build.prefix / 'lib'}"],
            )
