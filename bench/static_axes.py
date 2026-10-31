"""Run the organisers' harness/static_axes.py (S and D, exactly as scored) on Windows.

    python bench/static_axes.py track-a

Expects the official repo cloned next to this one as ../sd-official and its tools
image built (docker build -t speeddemon/static-tools:2026-10-28 harness/tools).
The harness calls os.getuid/getgid, which Windows lacks; any uid works for the
throwaway formatting container.
"""
import os
import runpy
import sys

os.getuid = getattr(os, "getuid", lambda: 1000)
os.getgid = getattr(os, "getgid", lambda: 1000)
script = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "..", "sd-official", "harness", "static_axes.py")
sys.argv = [script, "--track-dir", sys.argv[1]] + sys.argv[2:]
runpy.run_path(script, run_name="__main__")
