#!/usr/bin/env python3

"""Select a deterministic shard of the complete GNU Bazel target set."""

import argparse
import sys


GNU_TARGET_QUERY = (
    '//... except attr("tags", "[\\\\[ ]manual[,\\\\]]", //...) '
    "except //third_party/v8:all "
    "except //codex-rs/v8-poc:v8-poc-unit-tests"
)
GNU_SUITE_QUERY = f'kind("test_suite rule", ({GNU_TARGET_QUERY}))'
SHARD_COUNT = 2


def assign_targets(targets: list[str], shard_count: int) -> list[list[str]]:
    if shard_count < 1:
        raise ValueError("shard count must be positive")
    if not targets:
        raise ValueError("no GNU Bazel targets were provided")
    if any(not target.strip() for target in targets):
        raise ValueError("Bazel target labels must not be empty")
    if any(not target.startswith("//") for target in targets):
        raise ValueError("expected absolute main-repository Bazel labels")
    if len(targets) != len(set(targets)):
        raise ValueError("duplicate Bazel target labels were provided")
    if shard_count > len(targets):
        raise ValueError("shard count exceeds target count; empty shards are forbidden")

    assignments: list[list[str]] = [[] for _ in range(shard_count)]
    for index, target in enumerate(sorted(targets)):
        assignments[index % shard_count].append(target)
    return assignments


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    mode = parser.add_mutually_exclusive_group()
    mode.add_argument(
        "--query", action="store_true", help="print the exact GNU target query"
    )
    mode.add_argument(
        "--suite-query",
        action="store_true",
        help="print the nonmanual suite guard query",
    )
    mode.add_argument("--shard", type=int, help="1-based selected shard")
    parser.add_argument("--shard-count", type=int, help="fixed total shard count")
    args = parser.parse_args()

    if args.query:
        if args.shard_count is not None:
            parser.error("--shard-count is only valid with --shard")
        print(GNU_TARGET_QUERY)
        return
    if args.suite_query:
        if args.shard_count is not None:
            parser.error("--shard-count is only valid with --shard")
        print(GNU_SUITE_QUERY)
        return
    if args.shard is None or args.shard_count is None:
        parser.error("--shard and --shard-count are required unless --query is used")
    if args.shard_count != SHARD_COUNT:
        parser.error(f"GNU target sharding requires exactly {SHARD_COUNT} shards")
    if not 1 <= args.shard <= args.shard_count:
        parser.error("shard must be between 1 and shard count")

    try:
        targets = sys.stdin.read().splitlines()
        assignments = assign_targets(targets, args.shard_count)
    except ValueError as error:
        parser.error(str(error))

    selected = assignments[args.shard - 1]
    if not selected:
        parser.error(
            f"no GNU Bazel targets selected for shard {args.shard}/{args.shard_count}"
        )
    print(
        f"GNU Bazel target shards: counts={[len(shard) for shard in assignments]}; "
        f"selected={args.shard}/{args.shard_count}.",
        file=sys.stderr,
    )
    sys.stdout.buffer.write(("\n".join(selected) + "\n").encode("utf-8"))


if __name__ == "__main__":
    main()
