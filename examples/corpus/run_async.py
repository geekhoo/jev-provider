#!/usr/bin/env python3
"""Send the corpus to the gateway on several routes concurrently.

Each case is sent twice per model: "ui" (as written, with user_instructions) and
"noui" (user_instructions removed). Calls run concurrently. On the first failed
response (429, 5xx, timeout or connection error) every worker pauses for a backoff
that doubles with each further failure, and concurrency is halved; successes slowly
restore it. A failed call is retried up to --retries times, then recorded as failed,
and the run carries on until every call has a result. Standard library only.

Usage:
  JEV_TOKEN=... python examples/corpus/run_async.py --base-url http://127.0.0.1:8080 \
      --models jev-deep,jev-latest,jev-mimo,jev-openai,jev-openai-deep --concurrency 16
"""
import argparse, asyncio, copy, json, os, random, sys, time, urllib.error, urllib.request
from pathlib import Path

HERE = Path(__file__).resolve().parent
RETRYABLE = {408, 429, 500, 502, 503, 504, 529}


def cases():
    for d in sorted(p for p in HERE.iterdir() if p.is_dir() and p.name != "runs"):
        for n in range(1, 6):
            f = d / f"L{n}.json"
            if f.exists():
                yield f"{d.name}-L{n}", json.loads(f.read_text(encoding="utf-8"))


def post(url, token, body, timeout):
    headers = {"content-type": "application/json"}
    if token:
        headers["authorization"] = f"Bearer {token}"
    req = urllib.request.Request(url, data=json.dumps(body, ensure_ascii=False).encode("utf-8"),
                                 headers=headers, method="POST")
    t0 = time.monotonic()
    try:
        with urllib.request.urlopen(req, timeout=timeout) as r:
            return r.status, r.read(), time.monotonic() - t0
    except urllib.error.HTTPError as e:
        return e.code, e.read(), time.monotonic() - t0
    except Exception as e:
        return 0, str(e).encode(), time.monotonic() - t0


class Throttle:
    """Shared concurrency limit plus a global pause after failures."""

    def __init__(self, limit, base_backoff, max_backoff):
        self.max = self.limit = limit
        self.active = 0
        self.cond = asyncio.Condition()
        self.resume_at = 0.0
        self.base, self.cap, self.failures = base_backoff, max_backoff, 0
        self.wins = 0
        self.total_backoffs = 0

    async def acquire(self):
        async with self.cond:
            while True:
                wait = self.resume_at - time.monotonic()
                if wait > 0:
                    self.cond.release()
                    try:
                        await asyncio.sleep(wait)
                    finally:
                        await self.cond.acquire()
                    continue
                if self.active < self.limit:
                    self.active += 1
                    return
                await self.cond.wait()

    async def release(self, ok):
        async with self.cond:
            self.active -= 1
            if ok:
                self.failures = 0          # backoff grows with consecutive failures only
                self.wins += 1
                if self.wins >= 10 and self.limit < self.max:
                    self.limit += 1
                    self.wins = 0
            else:
                self.failures += 1
                self.total_backoffs += 1
                self.wins = 0
                delay = min(self.cap, self.base * 2 ** (self.failures - 1)) * (0.75 + random.random() / 2)
                self.resume_at = max(self.resume_at, time.monotonic() + delay)
                self.limit = max(1, self.limit // 2)
                print(f"  backoff {delay:.1f}s, concurrency -> {self.limit}", flush=True)
            self.cond.notify_all()


async def run_one(th, url, token, job, out, a, results):
    cid, mode, model, body = job
    attempt = 0
    while True:
        attempt += 1
        await th.acquire()
        status, raw, dt = await asyncio.to_thread(post, url, token, body, a.timeout)
        retry = status == 0 or status in RETRYABLE
        await th.release(not retry)
        if retry and attempt <= a.retries:
            continue
        break
    try:
        resp = json.loads(raw)
    except ValueError:
        resp = raw.decode(errors="replace")[:2000]
    (out / f"{model}" ).mkdir(exist_ok=True)
    (out / model / f"{cid}.{mode}.json").write_text(json.dumps(
        {"case": cid, "variant": mode, "model": model, "status": status, "attempts": attempt,
         "latency_s": round(dt, 2), "response": resp}, ensure_ascii=False, indent=1), encoding="utf-8")
    results.append((cid, mode, model, status, attempt, dt))
    print(f"{len(results):3}/{a.total} {model:16} {cid:15} {mode:4} {status} x{attempt} {dt:5.1f}s", flush=True)


async def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--base-url", default="http://127.0.0.1:8080")
    ap.add_argument("--models", required=True)
    ap.add_argument("--token-env", default="JEV_TOKEN")
    ap.add_argument("--concurrency", type=int, default=16)
    ap.add_argument("--retries", type=int, default=3)
    ap.add_argument("--backoff", type=float, default=2.0)
    ap.add_argument("--max-backoff", type=float, default=60.0)
    ap.add_argument("--timeout", type=float, default=180)
    ap.add_argument("--out", default=str(HERE / "runs" / f"gateway-{time.strftime('%Y%m%dT%H%M%S')}"))
    ap.add_argument("--only-failed", action="store_true", help="resend only calls whose saved result in --out is not 200")
    a = ap.parse_args()
    token = os.environ.get(a.token_env, "")
    models = [m.strip() for m in a.models.split(",") if m.strip()]
    url = a.base_url.rstrip("/") + "/v1/systemone"
    out = Path(a.out); out.mkdir(parents=True, exist_ok=True)
    jobs = []
    for cid, body in cases():
        for mode in ("ui", "noui"):
            for model in models:
                prev = out / model / f"{cid}.{mode}.json"
                if a.only_failed and prev.exists() and json.loads(prev.read_text(encoding="utf-8")).get("status") == 200:
                    continue
                b = copy.deepcopy(body)
                if mode == "noui":
                    b.pop("user_instructions", None)
                b["model"] = model
                jobs.append((cid, mode, model, b))
    random.Random(7).shuffle(jobs)   # spread load across routes
    a.total = len(jobs)
    from concurrent.futures import ThreadPoolExecutor
    asyncio.get_running_loop().set_default_executor(ThreadPoolExecutor(max_workers=a.concurrency))
    th = Throttle(a.concurrency, a.backoff, a.max_backoff)
    results = []
    t0 = time.monotonic()
    await asyncio.gather(*(run_one(th, url, token, j, out, a, results) for j in jobs))
    wall = time.monotonic() - t0
    print(f"\nwall {wall:.0f}s  total {len(results)}  backoffs {th.total_backoffs}")
    print("model            ok  fail  retried  p50_s  max_s  statuses")
    for m in models:
        rs = [r for r in results if r[2] == m]
        ok = [r for r in rs if r[3] == 200]
        lat = sorted(r[5] for r in ok) or [0]
        sts = {}
        for r in rs:
            sts[r[3]] = sts.get(r[3], 0) + 1
        print(f"{m:16} {len(ok):3} {len(rs) - len(ok):5} {sum(1 for r in rs if r[4] > 1):8} "
              f"{lat[len(lat) // 2]:6.1f} {lat[-1]:6.1f}  {sts}")
    ok = sum(1 for r in results if r[3] == 200)
    print(f"overall ok {ok}/{len(results)}  failure rate {100 * (len(results) - ok) / max(1, len(results)):.1f}%  out {out}")


if __name__ == "__main__":
    asyncio.run(main())
