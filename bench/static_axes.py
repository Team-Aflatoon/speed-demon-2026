"""Run the organisers' harness/static_axes.py (S and D, exactly as scored) on Windows.

    python bench/static_axes.py track-a

Expects the official repo cloned next to this one as ../sd-official and its tools
image built (docker build -t speeddemon/static-tools:2026-10-28 harness/tools).
Two Windows fixes, neither of which changes what is counted:
  * the harness calls os.getuid/getgid, which Windows lacks; any uid works for the
    throwaway formatting container;
  * text files it writes for the container (the formatter's file list) must use LF,
    or every path ends in a CR, rustfmt cannot open the file and the Rust code is
    counted unformatted.
"""
import builtins
import os
import runpy
import sys

_open = builtins.open


def lf_open(file, mode="r", *args, **kwargs):
    if ("w" in mode or "a" in mode) and "b" not in mode and "newline" not in kwargs and len(args) < 3:
        kwargs["newline"] = "\n"
    return _open(file, mode, *args, **kwargs)


builtins.open = lf_open
os.getuid = getattr(os, "getuid", lambda: 1000)
os.getgid = getattr(os, "getgid", lambda: 1000)
script = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "..", "sd-official", "harness", "static_axes.py")
sys.argv = [script, "--track-dir", sys.argv[1]] + sys.argv[2:]
runpy.run_path(script, run_name="__main__")
