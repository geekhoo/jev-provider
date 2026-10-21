#!/usr/bin/env python3
"""Send the corpus to an official-style Jev endpoint (e.g. LiteLLM /v1/systemone).

The official API ignores `user_instructions`, so each case is sent twice:
  ui   - user_instructions rendered as a plain-text "Domain context" block and
         prepended to every question's `instructions` (the field the official
         API uses to define the decision); the user_instructions field is dropped.
  noui - user_instructions removed, nothing added.

Models are tried in order; a 429 moves to the next model and the rest of the run
stays on it. The API key comes from an environment variable and is never printed.

Usage:
  JEV_KEY=... python examples/corpus/run_official.py \
      --base-url https://litellm.example.com --models jev-1.13-free,jev-1.13 --out runs/official
"""
import argparse, copy, json, os, sys, time, urllib.error, urllib.request
from pathlib import Path

HERE = Path(__file__).resolve().parent


def render_context(ui):
    lines = [f"Domain context ({ui.get('domain', 'unspecified')}): objective is {ui.get('objective', 'unspecified')}"]
    for k in ("locale", "risk_posture", "uncertainty_policy"):
        if ui.get(k):
            lines.append(f"{k.replace('_', ' ')}: {ui[k]}")
    for t in ui.get("terminology", []):
        lines.append(f"term '{t['term']}': {t['definition']}")
    for c in ui.get("constraints", []):
        lines.append(f"{c['kind'].replace('_', ' ')}: {c['statement']}")
    for f in ui.get("reference_facts", []):
        lines.append(f"fact {f['name']} ({f['authority']}): {f['value']}")
    for r in sorted(ui.get("decision_rules", []), key=lambda r: -r["priority"]):
        lines.append(f"rule (priority {r['priority']}): when {r['when']} then {r['then']}")
    return "\n".join(lines)


PAIRS = [("true", "false"), ("yes", "no"), ("agree", "disagree"), ("accept", "reject"),
         ("approve", "deny"), ("allow", "block"), ("pass", "fail"), ("positive", "negative")]


def official_criteria(crit):
    """The official API only accepts noul criteria keyed "true"/"false"; map the
    gateway's positive/negative pair extension onto them (as the gateway does)."""
    if not isinstance(crit, dict) or len(crit) != 2:
        return crit
    low = {k.lower(): v for k, v in crit.items()}
    for pos, neg in PAIRS:
        if set(low) == {pos, neg}:
            return {"true": low[pos], "false": low[neg]}
    return crit


def variant(body, mode):
    b = copy.deepcopy(body)
    for q in b["questions"].values():
        if q.get("type") == "noul" and "criteria" in q:
            q["criteria"] = official_criteria(q["criteria"])
    ui = b.pop("user_instructions", None)
    if mode == "ui" and ui:
        ctx = render_context(ui)
        for q in b["questions"].values():
            ins = q.get("instructions")
            q["instructions"] = ctx if ins in (None, "") else f"{ctx}\n\nQuestion: {ins if isinstance(ins, str) else json.dumps(ins, ensure_ascii=False)}"
    return b


def post(url, key, body, timeout):
    req = urllib.request.Request(url, data=json.dumps(body, ensure_ascii=False).encode("utf-8"), method="POST",
                                 headers={"content-type": "application/json", "authorization": f"Bearer {key}"})
    t0 = time.monotonic()
    try:
        with urllib.request.urlopen(req, timeout=timeout) as r:
            return r.status, r.read(), time.monotonic() - t0
    except urllib.error.HTTPError as e:
        return e.code, e.read(), time.monotonic() - t0
    except Exception as e:  # connection or timeout
        return 0, str(e).encode(), time.monotonic() - t0


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--base-url", required=True)
    ap.add_argument("--models", default="jev-1.13-free,jev-1.13")
    ap.add_argument("--key-env", default="JEV_KEY")
    ap.add_argument("--out", default=str(HERE / "runs" / f"official-{time.strftime('%Y%m%dT%H%M%S')}"))
    ap.add_argument("--timeout", type=float, default=180)
    ap.add_argument("--only-failed", action="store_true", help="re-send only cases whose saved result in --out is not 200")
    a = ap.parse_args()
    key = os.environ.get(a.key_env) or sys.exit(f"set {a.key_env}")
    models = [m.strip() for m in a.models.split(",") if m.strip()]
    url = a.base_url.rstrip("/") + "/v1/systemone"
    out = Path(a.out); out.mkdir(parents=True, exist_ok=True)
    mi, rows = 0, []
    for d in sorted(p for p in HERE.iterdir() if p.is_dir() and p.name != "runs"):
        for n in range(1, 6):
            f = d / f"L{n}.json"
            if not f.exists():
                continue
            body = json.loads(f.read_text(encoding="utf-8"))
            for mode in ("ui", "noui"):
                prev = out / f"{d.name}-L{n}.{mode}.json"
                if a.only_failed and prev.exists() and json.loads(prev.read_text(encoding="utf-8")).get("status") == 200:
                    continue
                b = variant(body, mode)
                while True:
                    b["model"] = models[mi]
                    status, raw, dt = post(url, key, b, a.timeout)
                    if status == 429 and mi + 1 < len(models):
                        print(f"429 on {models[mi]}, falling back to {models[mi + 1]}", file=sys.stderr)
                        mi += 1
                        continue
                    break
                try:
                    resp = json.loads(raw)
                except ValueError:
                    resp = raw.decode(errors="replace")[:2000]
                cid = f"{d.name}-L{n}"
                (out / f"{cid}.{mode}.json").write_text(json.dumps(
                    {"case": cid, "variant": mode, "model": models[mi], "status": status,
                     "latency_s": round(dt, 2), "request": b, "response": resp}, ensure_ascii=False, indent=1),
                    encoding="utf-8")
                rows.append((cid, mode, models[mi], status, dt))
                print(f"{cid:15} {mode:4} {models[mi]:14} {status} {dt:6.1f}s", flush=True)
    ok = sum(1 for r in rows if r[3] == 200)
    print(f"\ntotal {len(rows)}  ok {ok}  4xx {sum(1 for r in rows if 400 <= r[3] < 500)}  "
          f"5xx {sum(1 for r in rows if r[3] >= 500)}  conn {sum(1 for r in rows if r[3] == 0)}  "
          f"failure rate {100 * (len(rows) - ok) / max(1, len(rows)):.1f}%  out {out}")


if __name__ == "__main__":
    main()
