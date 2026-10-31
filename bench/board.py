"""Show our live-board runs and each track's top five.

    python bench/board.py [team-substring]

Reads the public feed the board page uses. The feed is replaced every few seconds,
so a read can catch it mid-upload; retry until it parses.
"""
import json
import sys
import time
import urllib.request

FEED = "https://s3.amazonaws.com/raptors.dev/speeddemon.dev/2026/board/board.json"


def fetch():
    for attempt in range(5):
        try:
            with urllib.request.urlopen(FEED, timeout=20) as resp:
                return json.load(resp)
        except (ValueError, OSError):
            time.sleep(2 + attempt)
    sys.exit("could not read the board feed")


def row(run):
    if not run:
        return "-"
    if not run.get("ok"):
        return f"{run['run_id']} FAILED at {run.get('stage')}: {run.get('failure')}"
    return (f"{run['run_id']} K={run['K']:.2f} pts={run['points']:.1f} T={run['T_s'] * 1000:.1f}ms "
            f"M={run['M_bytes'] / 1e6:.1f}MB S={run['S']} D={run['D']} commit={run['commit'][:7]}")


def main():
    me = (sys.argv[1] if len(sys.argv) > 1 else "aflatoon").lower()
    board = fetch()
    print("feed generated", board["generated_at"], "-", len(board["teams"]), "teams")
    for track in "abcd":
        ranked = sorted(
            ((t["tracks"][track]["best"]["K"], t["team"]) for t in board["teams"]
             if (t["tracks"].get(track) or {}).get("best", {}) and t["tracks"][track]["best"].get("ok")),
            reverse=True)
        mine = [t for t in board["teams"] if me in t["team"].lower()]
        rank = next((i + 1 for i, (_, name) in enumerate(ranked) if me in name.lower()), None)
        print(f"\nTrack {track.upper()}: {len(ranked)} scored, our rank {rank}")
        for k, name in ranked[:5]:
            print(f"   K={k:6.2f} {name}")
        for t in mine:
            tr = t["tracks"].get(track) or {}
            print(f"   ours best:   {row(tr.get('best'))}")
            print(f"   ours latest: {row(tr.get('latest'))}")
    for t in board["teams"]:
        if me in t["team"].lower():
            print("\nwatch:", t["watch"], "queue:", t["queue"])


if __name__ == "__main__":
    main()
