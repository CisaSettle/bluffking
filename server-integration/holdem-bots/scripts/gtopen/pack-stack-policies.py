#!/usr/bin/env python3
"""Pack or reproduce the exact-stack preflop policies exported by the pinned GTOpen solver."""
import argparse
import concurrent.futures
import io
import json
import os
from pathlib import Path
import struct
import subprocess


MAGIC = b"SPF1"


def body(exported, seats):
    result = bytearray(struct.pack("<II", exported["root"], len(exported["nodes"])))
    for node in exported["nodes"]:
        edges = node["edges"]
        assert len(node["posted"]) == seats and len(node["reach"]) == 169
        assert len(node["strategy"]) == 169 * len(edges)
        result.extend(struct.pack(f"<II{seats}I", node["actor"], len(edges), *node["posted"]))
        for edge in edges:
            move = edge["play"]
            tag = {"Fold": 0, "Check": 1, "Call": 2}[move] if isinstance(move, str) else 3 + move["Raise"]
            child = 0xffffffff if edge["next"] is None else edge["next"]
            result.extend(struct.pack("<II", tag, child))
        for field in ["reach", "strategy"]:
            weights = node[field]
            result.extend(struct.pack(f"<{len(weights)}f", *weights))
    return result


def pack(profiles):
    entries = {}
    for profile in profiles:
        exported = json.loads(profile.read_bytes())
        config = exported["config"]
        n = len(config["positions"])
        assert 2 <= n <= 9 and config["rake_pct"] == 0 and config["ante"] == 0
        assert config["posts"] == [0] * (n - 2) + [0.5, 1.0]
        assert not any(config.get(k) for k in ("bb_ante", "utg_straddle", "icm", "call_only_seats"))
        assert config["realization"] == "leafnet" and config["leaf_net"] == "v3"
        assert n == 2 or exported["model"] == "coupled_deck_v1"
        assert len(exported["model_br_gap_bb"]) == n
        assert all(0 <= gap <= 0.01 for gap in exported["model_br_gap_bb"])
        stacks = config.get("stacks", [config["stack"]] * n)
        assert len(stacks) == n and all((2 * depth).is_integer() for depth in map(float, stacks))
        key = tuple(int(2 * depth) for depth in stacks)
        assert key not in entries and sorted(key)[-1] == sorted(key)[-2]
        info = {field: value for field, value in exported.items() if field not in ("root", "nodes")}
        metadata = json.dumps(info, sort_keys=True, separators=(",", ":")).encode()
        metadata += b" " * (-len(metadata) % 4)
        data = body(exported, n)
        entries[key] = struct.pack(f"<III{n}I", n, len(metadata), len(data), *key) + metadata + data
    return MAGIC + b"".join(value for _, value in sorted(entries.items()))


def catalog_profiles(path):
    with io.BytesIO(bytes.fromhex(path.read_text(encoding="ascii"))) as file:
        assert file.read(4) == MAGIC
        while header := file.read(12):
            n, metadata, size = struct.unpack("<III", header)
            assert 2 <= n <= 9 and metadata % 4 == size % 4 == 0
            key = list(struct.unpack(f"<{n}I", file.read(n * 4)))
            info = json.loads(file.read(metadata))
            projection = info.get("projection")
            assert projection in (None, "equal_100bb_public_actions_v1")
            file.seek(size, 1)
            yield key, projection


def reproduce(args):
    args.cache.mkdir(parents=True, exist_ok=True)
    equity_dir = args.upstream.resolve()
    exporter = str(args.exporter.resolve())
    trainer = str(args.ring_trainer.resolve())
    env = dict(os.environ, RAYON_NUM_THREADS="1")

    specs = list(catalog_profiles(args.catalog))
    primary = {len(key) for key, projection in specs if projection}

    def standard(n):
        path = args.cache.resolve() / f"standard-{n}.preflop"
        train = [trainer] if n >= 5 else [exporter, "train-ring"]
        with path.with_suffix(".log").open("w") as log:
            subprocess.run(train + [str(n), str(path)], cwd=equity_dir, env=env, stdout=log, stderr=log, check=True)
        return n, str(path)

    def run(spec):
        key, projection = spec
        n = len(key)
        stem = args.cache.resolve() / ("stack-" + "-".join(map(str, key)))
        profile = stem.with_suffix(".json")
        snapshot = str(stem.with_suffix(".preflop"))
        if n == 2:
            assert key[0] == key[1]
            calls = [[exporter, str(key[0] / 2), str(profile)]]
        elif projection:
            calls = [[trainer, "project", seeds[n], snapshot, *[str(d / 2) for d in key]],
                     [exporter, "export-projection", snapshot, str(profile)]]
        else:
            short = min(key)
            assert key.count(200) == n - 1 and short < 200
            arguments = [str(n), snapshot, str(key.index(short)), str(short / 2)]
            train = [trainer] if n >= 6 else [exporter, "train-ring"]
            calls = [train + arguments, [exporter, "export-ring", snapshot, str(profile)]]
        with stem.with_suffix(".log").open("w") as log:
            for call in calls:
                subprocess.run(call, cwd=equity_dir, env=env, stdout=log, stderr=log, check=True)
        return profile

    with concurrent.futures.ThreadPoolExecutor(max_workers=args.jobs) as pool:
        seeds = dict(pool.map(standard, sorted(primary)))
        return pack(pool.map(run, specs))


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    packing = commands.add_parser("pack", help="check and pack JSON policy exports")
    packing.add_argument("output", type=Path)
    packing.add_argument("profiles", nargs="+", type=Path)
    regen = commands.add_parser("reproduce", help="solve every starting-stack vector in a catalog")
    for name in ["catalog", "output", "cache", "upstream", "exporter", "ring_trainer"]:
        regen.add_argument(name, type=Path)
    regen.add_argument("--jobs", default=2, type=int)
    args = parser.parse_args()
    data = pack(args.profiles) if args.command == "pack" else reproduce(args)
    with args.output.open("w", encoding="ascii", newline="\n") as output:
        for offset in range(0, len(data), 128):
            output.write(data[offset:offset + 128].hex() + "\n")
