#!/usr/bin/env python3
"""Tries each request in evals/requests.txt on its own copy of a world, with the real
agent, and keeps what happened: the agent's reply, its test_game runs, errors, and the
world's log. `evals/run.py BASE_SAVES OUT [ids...]` (BASE_SAVES: a `saves` folder
holding the world; CUBEWISH_EVAL_WORLD names it, default valley; CUBEWISH_EVAL_JOBS
how many run at once, default 4; CUBEWISH_EVAL_REQUESTS another requests file)."""
import concurrent.futures, os, re, shutil, subprocess, sys, time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
BIN = os.path.join(ROOT, "target", "debug", "cubewish")
WORLD = os.environ.get("CUBEWISH_EVAL_WORLD", "valley")
LIMIT = 1500  # seconds for one request
ANSI = re.compile(r"\x1b\[[0-9;]*m")


def requests():
    for line in open(os.environ.get("CUBEWISH_EVAL_REQUESTS", os.path.join(ROOT, "evals", "requests.txt"))):
        if line.strip() and not line.startswith("#"):
            name, text = (s.strip() for s in line.split("|", 1))
            yield name, text


def run(base, out, name, text):
    place = os.path.join(out, name)
    shutil.rmtree(place, ignore_errors=True)
    os.makedirs(place)
    shutil.copytree(base, os.path.join(place, "saves"))
    env = dict(os.environ, BEVY_ASSET_ROOT=ROOT, CUBEWISH_WORLD=WORLD, CUBEWISH_PROMPT=text)
    log_path = os.path.join(place, "run.log")
    start = time.time()
    with open(log_path, "w") as log:
        game = subprocess.Popen([BIN], cwd=place, env=env, stdout=log, stderr=subprocess.STDOUT)
        replied = None
        while game.poll() is None and time.time() - start < LIMIT:
            time.sleep(5)
            if replied is None and "Claude Code replied" in open(log_path, errors="replace").read():
                replied = time.time()
            # Let its last changes save, then stop.
            if replied and time.time() - replied > 10:
                break
        game.terminate()
        try:
            game.wait(20)
        except subprocess.TimeoutExpired:
            game.kill()
    lines = [ANSI.sub("", l) for l in open(log_path, errors="replace")]
    keep = [l.split(" cubewish", 1)[-1].strip() for l in lines
            if " cubewish" in l and re.search(r"replied|test_game|agent tool|error|refus|no longer applies|panick|is live|folder", l, re.I)]
    keep += [l.strip() for l in lines if "panicked" in l]
    with open(os.path.join(place, "summary.txt"), "w") as f:
        f.write(f"{name}: {text}\n{int(time.time() - start)} s\n\n" + "\n".join(keep) + "\n")
    return name, int(time.time() - start), any("replied" in k for k in keep)


def main():
    base, out, only = sys.argv[1], sys.argv[2], set(sys.argv[3:])
    todo = [(n, t) for n, t in requests() if not only or n in only]
    jobs = int(os.environ.get("CUBEWISH_EVAL_JOBS", "4"))
    with concurrent.futures.ThreadPoolExecutor(jobs) as pool:
        for name, secs, replied in pool.map(lambda r: run(base, out, *r), todo):
            print(f"{name}: {'replied' if replied else 'NO REPLY'} in {secs} s", flush=True)


if __name__ == "__main__":
    main()
