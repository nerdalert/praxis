#!/usr/bin/env python3
"""Summarize GuideLLM benchmark results from a matrix output directory.

Usage:
    python3 benchmarks/llm-d/summarize-guidellm-results.py [RESULTS_DIR]

Defaults to target/criterion/llmd-guidellm-matrix/ if no argument given.
Reads all benchmark-results.json files and prints a summary table.
"""

import json
import os
import sys


def main():
    base = sys.argv[1] if len(sys.argv) > 1 else "target/criterion/llmd-guidellm-matrix"

    if not os.path.isdir(base):
        print(f"error: {base} is not a directory", file=sys.stderr)
        sys.exit(1)

    rows = []
    for dirpath, _, filenames in os.walk(base):
        for fn in filenames:
            if fn != "benchmark-results.json":
                continue
            fpath = os.path.join(dirpath, fn)
            rel = os.path.relpath(dirpath, base)
            parts = rel.split(os.sep)
            if len(parts) < 2:
                continue
            profile, kind = parts[0], parts[1]
            stream = "yes" if "nostream" not in kind else "no"

            with open(fpath) as f:
                d = json.load(f)
            for b in d.get("benchmarks", []):
                m = b["metrics"]
                rps = m["requests_per_second"]["successful"]["mean"]
                ttft_val = m["time_to_first_token_ms"]["successful"]["median"]
                ttft = f"{ttft_val:.2f}ms" if stream == "yes" else "n/a"
                itl_val = m["inter_token_latency_ms"]["successful"]["median"]
                itl = f"{itl_val:.3f}ms" if stream == "yes" else "n/a"
                lat = m["request_latency"]["successful"]["median"] * 1000
                reqs = m["request_totals"]["successful"]
                cfg = b.get("config", {})
                strategy = cfg.get("strategy", {})
                conc = strategy.get("max_concurrency", strategy.get("rate", "?"))
                rows.append((profile, stream, conc, rps, ttft, itl, lat, reqs))

    order = {"praxis-simple": 0, "praxis-native": 1, "envoy-go-epp": 2}
    rows.sort(key=lambda r: (order.get(r[0], 9), r[1] == "no", r[2]))

    hdr = f"{'Profile':>16s}  {'Stream':>6s}  {'Conc':>4s}  {'RPS':>6s}  {'TTFT':>8s}  {'ITL':>8s}  {'E2E':>8s}  {'Reqs':>5s}"
    print(hdr)
    print("-" * len(hdr))
    for profile, stream, conc, rps, ttft, itl, lat, reqs in rows:
        print(
            f"{profile:>16s}  {stream:>6s}  {conc:>4}  {rps:6.0f}  {ttft:>8s}  {itl:>8s}  {lat:7.1f}ms  {reqs:5d}"
        )

    # Markdown table for docs.
    print("\n--- Markdown ---\n")
    print("| Profile | Stream | Conc | RPS | TTFT | ITL | E2E | Reqs |")
    print("|---------|--------|------|-----|------|-----|-----|------|")
    for profile, stream, conc, rps, ttft, itl, lat, reqs in rows:
        print(f"| `{profile}` | {stream} | {conc} | {rps:.0f} | {ttft} | {itl} | {lat:.1f}ms | {reqs} |")


if __name__ == "__main__":
    main()
