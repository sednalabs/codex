#!/usr/bin/env python3
from __future__ import annotations

import argparse
import json
import subprocess
from pathlib import Path


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument('--repo-root', required=True)
    parser.add_argument('--working-directory', required=True)
    parser.add_argument('--script-path', required=True)
    parser.add_argument('--script-args-json', default='[]')
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    repo_root = Path(args.repo_root).resolve()
    cwd = (repo_root / args.working_directory).resolve()
    script_path = (repo_root / args.script_path).resolve()
    script_args = json.loads(args.script_args_json or '[]')
    if not script_path.is_file():
        raise SystemExit(f'validation lane script not found: {script_path}')
    if not isinstance(script_args, list) or not all(isinstance(item, str) for item in script_args):
        raise SystemExit('script args must decode to a JSON array of strings')
    proc = subprocess.run(['bash', str(script_path), *script_args], cwd=cwd)
    return proc.returncode


if __name__ == '__main__':
    raise SystemExit(main())
