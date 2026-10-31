"""Overall standings from the live feed: geometric mean of each team's best three
track points (machine score only; the judges' 35% is not on the board).

    python bench/overall.py [team-substring]
"""
import math
import sys

from board import fetch


def main():
    me = (sys.argv[1] if len(sys.argv) > 1 else "aflatoon").lower()
    rows = []
    for team in fetch()["teams"]:
        points = sorted((run["points"] for run in (
            (team["tracks"].get(t) or {}).get("best") for t in "abcd") if run and run.get("ok")), reverse=True)[:3]
        if len(points) == 3 and min(points) > 0:
            rows.append((math.exp(sum(map(math.log, points)) / 3), team["team"], [round(p, 1) for p in points]))
    rows.sort(reverse=True)
    for rank, (overall, name, points) in enumerate(rows, 1):
        if rank <= 8 or me in name.lower():
            print(f"{rank:3} {overall:6.2f} {name:28} {points}")
    print(len(rows), "teams with three scored tracks")


if __name__ == "__main__":
    main()
