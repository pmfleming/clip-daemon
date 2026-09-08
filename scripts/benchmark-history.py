#!/usr/bin/env python3
"""Compare the cached projection against fc1ac57 using identical release tooling.

Only disposable source archives and synthetic fixtures are used. Results include
allocation counts/bytes, latency samples, and matching semantic fingerprints.
"""
import json
from pathlib import Path
import shutil
import statistics
import subprocess
import tempfile

PROJECT = Path(__file__).resolve().parents[1]
BASELINE = "fc1ac57"
ADAPTER = '''    let projection = cached.clone();
    QueryAccumulator {
        needle: &query.query.trim().to_lowercase(),
        current_id: projection.current_id,
        offset: query.offset,
        limit: query.limit.clamp(1, MAX_QUERY_LIMIT),
        collapse_echoes: query.collapse_self_echoes,
        complete: projection.complete,
        candidates: projection.candidates,
    }.finish()'''


def measure(directory, label, output):
    command = ["cargo", "test", "--release", "--locked", "--features", "benchmarks",
               "--lib", "ringboard::benchmarks::history_projection", "--", "--ignored", "--nocapture", "--test-threads=1"]
    result = subprocess.run(command, cwd=directory, text=True, capture_output=True, timeout=600)
    (output / f"{label}.log").write_text(result.stdout + result.stderr)
    result.check_returncode()
    records = [json.loads(line.split("QUERY_BENCH ", 1)[1]) for line in result.stdout.splitlines() if "QUERY_BENCH " in line]
    assert len(records) == 12, f"incomplete benchmark: {label}"
    return records


def main():
    output = PROJECT / "target/history-benchmark"
    output.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="clip-benchmark-") as temporary:
        root = Path(temporary)
        baseline = root / "baseline"
        baseline.mkdir()
        archive = subprocess.check_output(["git", "archive", BASELINE], cwd=PROJECT)
        subprocess.run(["tar", "-x", "-C", str(baseline)], input=archive, check=True)
        (root / "daemon-framework").symlink_to(PROJECT.parent / "daemon-framework", target_is_directory=True)
        for name in ["Cargo.toml", "Cargo.lock"]:
            shutil.copy2(PROJECT / name, baseline / name)
        source = baseline / "src/ringboard.rs"
        source.write_text(source.read_text().replace("mod artifacts;", 'mod artifacts;\n#[cfg(all(test, feature = "benchmarks"))]\nmod benchmarks;', 1))
        benchmark = (PROJECT / "src/ringboard/benchmarks.rs").read_text()
        assert "    cached.project(query)" in benchmark
        (baseline / "src/ringboard/benchmarks.rs").write_text(benchmark.replace("    cached.project(query)", ADAPTER, 1))
        # Shared build artifacts avoid rebuilding unchanged dependencies twice.
        import os
        os.environ["CARGO_TARGET_DIR"] = str(PROJECT / "target")
        runs = {"before": [], "after": []}
        for round_number in range(3):
            order = [("before", baseline), ("after", PROJECT)]
            if round_number % 2:
                order.reverse()
            for label, directory in order:
                runs[label].append(measure(directory, f"{label}-{round_number}", output))
        before, after = [] , []
        for label, records in [("before", before), ("after", after)]:
            for index in range(12):
                row = dict(runs[label][0][index])
                samples = sorted(sample for run in runs[label] for sample in run[index]["samples_ns"])
                assert all(run[index]["fingerprint"] == row["fingerprint"] for run in runs[label])
                row.update(samples_ns=samples, iterations=len(samples), median_ns=statistics.median(samples), p95_ns=samples[len(samples) * 95 // 100])
                records.append(row)
    comparisons = []
    for old, new in zip(before, after, strict=True):
        for key in ["size", "scenario", "fingerprint"]:
            assert old[key] == new[key], f"semantic mismatch: {key}"
        comparisons.append({
            "size": old["size"], "scenario": old["scenario"], "before": old, "after": new,
            "median_speedup": round(old["median_ns"] / new["median_ns"], 2),
            "allocated_bytes_reduction_percent": round(100 * (1 - new["allocated_bytes"] / old["allocated_bytes"]), 2),
        })
    report = {"baseline": BASELINE, "current": subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=PROJECT, text=True).strip(),
              "rustc": subprocess.check_output(["rustc", "--version"], text=True).strip(),
              "scope": "synthetic warm projection only; excludes DB, hashing, D-Bus and thumbnail cleanup", "comparisons": comparisons}
    (output / "results.json").write_text(json.dumps(report, indent=2) + "\n")
    for result in comparisons:
        print(f"{result['size']:>6} {result['scenario']:<12} {result['median_speedup']:>5}x  allocated bytes -{result['allocated_bytes_reduction_percent']}%")


if __name__ == "__main__":
    main()
