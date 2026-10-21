#!/usr/bin/env python3
"""Compare the corpus answer sets (Opus, gpt-6-luna, mimo-v2.6-pro, zen/deepseek-v4.1-flash,
jev-1.13-free, and the five gateway routes gw-*), with and without user_instructions.
A set with no files is skipped; a missing answer for one case is reported, not fatal.

Usage: python examples/corpus/analyse.py [corpus_dir] [--sets opus,luna,...] [--json out.json]
  corpus_dir defaults to this script's directory; --sets limits the comparison to those sets;
  --json also writes the summary numbers (used by the charts) to out.json.
"""
import json, glob, os, re, statistics as st, sys
from collections import defaultdict

ARGS = sys.argv[1:]
ONLY = None
if "--sets" in ARGS:
    i = ARGS.index("--sets"); ONLY = ARGS[i + 1].split(","); del ARGS[i:i + 2]
if "--json" in ARGS:
    i = ARGS.index("--json"); del ARGS[i:i + 2]
C =ARGS[0] if ARGS else os.path.dirname(os.path.abspath(__file__))
SRC = {"opus": "", "luna": ".gpt-6-luna", "mimo": ".mimo-v2.6-pro", "deepseek": ".zen-deepseek-v4.1-flash", "jev": ".jev-1.13-free",
       "gw-deep": ".gw-jev-deep", "gw-latest": ".gw-jev-latest", "gw-mimo": ".gw-jev-mimo",
       "gw-openai": ".gw-jev-openai", "gw-openai-deep": ".gw-jev-openai-deep"}
SETS = [s for s in SRC if (ONLY is None or s in ONLY)
        and (s == "opus" or glob.glob(os.path.join(C, "*", f"L*.expected{SRC[s]}.json")))]
SRC = {s: SRC[s] for s in SETS}

def load(p):
    return json.load(open(p, encoding="utf-8"))

def norm(d):
    vals = {k: (v if isinstance(v, (int, float)) and v > 0 else 0.0) for k, v in d.items()}
    s = sum(vals.values())
    return {k: (v / s if s else 1 / len(vals)) for k, v in vals.items()}

def choice_conf(p):
    n = len(p); m = max(p)
    return 1.0 if n <= 1 else max(0.0, (m - 1 / n) / (1 - 1 / n))

def score_stats(p):
    n = len(p); score = sum(i * x for i, x in enumerate(p))
    if n <= 1: return score, 1.0
    m = max(range(n), key=lambda i: (p[i], -i))
    mad = sum(x * abs(i - m) for i, x in enumerate(p))
    mu = (n - 1) / 2; madu = sum(abs(i - mu) for i in range(n)) / n
    return score, max(0.0, 1 - mad / madu)

issues = defaultdict(list)
data = {}   # (case, variant, set) -> {qname: answer}
reasons = {}
cases = []
for d in sorted(os.listdir(C)):
    if not os.path.isdir(os.path.join(C, d)) or d == "runs": continue
    for n in range(1, 6):
        cid = f"{d}-L{n}"; base = os.path.join(C, d, f"L{n}")
        if not os.path.exists(base + ".json"): continue
        req = load(base + ".json"); meta = load(base + ".meta.json"); cases.append((cid, req, meta))
        for s, suf in SRC.items():
            if s == "opus":
                e = load(base + ".expected.json")
                data[(cid, "ui", s)] = e["with_user_instructions"]["answers"]
                data[(cid, "noui", s)] = e["without_user_instructions"]["answers"]
                reasons[(cid, "ui", s)] = e["with_user_instructions"].get("reasons", {})
                continue
            for v, f, key in (("ui", base + f".expected{suf}.json", "with_user_instructions"),
                              ("noui", base + f".expected{suf}.no_user_instructions.json", None)):
                if not os.path.exists(f): issues[s].append(f"{cid} {v}: missing"); continue
                e = load(f)
                k = key if key and key in e else next((x for x in e if x not in ("case", "source")), None)
                data[(cid, v, s)] = e[k]["answers"]
                reasons[(cid, v, s)] = e[k].get("reasons", {})

# validity
for (cid, v, s), ans in data.items():
    req = next(r for c, r, _ in cases if c == cid)
    for q, spec in req["questions"].items():
        a = ans.get(q)
        if a is None: issues[s].append(f"{cid} {v} {q}: missing answer"); continue
        if spec["type"] == "noul":
            x = a.get("noul", a.get("probability"))
            if not isinstance(x, (int, float)) or not 0 <= x <= 1: issues[s].append(f"{cid} {v} {q}: bad noul {x}")
        else:
            keys = list(spec["criteria"]) if spec["type"] == "choice" else [str(i) for i in range(len(spec["criteria"]))]
            p = a.get("probabilities", {})
            if set(p) != set(keys): issues[s].append(f"{cid} {v} {q}: keys {sorted(set(keys) ^ set(p))}")
            tot = sum(x for x in p.values() if isinstance(x, (int, float)))
            # Each value is rounded to 2 dp on its own (gateway and official API), so the
            # sum may drift by up to 0.005 per option.
            if abs(tot - 1) > max(0.011, 0.005 * len(keys)): issues[s].append(f"{cid} {v} {q}: sum {tot:.2f}")

def derive(cid, v, s, q, spec):
    a = data.get((cid, v, s), {}).get(q)
    if a is None: return None
    if spec["type"] == "noul":
        return {"t": "noul", "p": a.get("noul", a.get("probability"))}
    keys = list(spec["criteria"]) if spec["type"] == "choice" else [str(i) for i in range(len(spec["criteria"]))]
    p = norm({k: a.get("probabilities", {}).get(k, 0) for k in keys}); vec = [p[k] for k in keys]
    if spec["type"] == "choice":
        top = max(keys, key=lambda k: (p[k], [-ord(c) for c in k]))
        return {"t": "choice", "vec": vec, "top": top, "conf": choice_conf(vec)}
    sc, cf = score_stats(vec)
    return {"t": "score", "vec": vec, "score": sc, "conf": cf}

def tv(a, b): return 0.5 * sum(abs(x - y) for x, y in zip(a, b))

# pairwise agreement per variant
out = []
for v in ("ui", "noui"):
    out.append(f"\n## Pairwise agreement ({'with' if v=='ui' else 'without'} user_instructions)")
    out.append("pair | choice top match | choice TV dist | score |diff| | noul |diff|")
    for i, a in enumerate(SETS):
        for b in SETS[i + 1:]:
            ct = []; ctv = []; sd = []; nd = []
            for cid, req, meta in cases:
                for q, spec in req["questions"].items():
                    x = derive(cid, v, a, q, spec); y = derive(cid, v, b, q, spec)
                    if not x or not y: continue
                    if spec["type"] == "choice": ct.append(x["top"] == y["top"]); ctv.append(tv(x["vec"], y["vec"]))
                    elif spec["type"] == "score": sd.append(abs(x["score"] - y["score"]))
                    else: nd.append(abs(x["p"] - y["p"]))
            out.append(f"{a}-{b} | {sum(ct)}/{len(ct)} | {st.mean(ctv):.2f} | {st.mean(sd):.2f} | {st.mean(nd):.2f}")

# consensus & per-model deviation from leave-one-out consensus
out.append("\n## Deviation from the other sets (with user_instructions)")
out.append("model | choice top = majority of others | mean score |diff| vs others | mean noul |diff| vs others | mean choice conf | mean score conf")
for s in SETS:
    agree = []; sdiff = []; ndiff = []; cc = []; scf = []
    for cid, req, meta in cases:
        for q, spec in req["questions"].items():
            x = derive(cid, "ui", s, q, spec)
            others = [derive(cid, "ui", o, q, spec) for o in SETS if o != s]
            others = [o for o in others if o]
            if not x or not others: continue
            if spec["type"] == "choice":
                tops = [o["top"] for o in others]; maj = max(set(tops), key=tops.count)
                agree.append(x["top"] == maj and tops.count(maj) >= 2); cc.append(x["conf"])
            elif spec["type"] == "score":
                sdiff.append(abs(x["score"] - st.mean(o["score"] for o in others))); scf.append(x["conf"])
            else:
                ndiff.append(abs(x["p"] - st.mean(o["p"] for o in others)))
    out.append(f"{s} | {sum(agree)}/{len(agree)} | {st.mean(sdiff):.2f} | {st.mean(ndiff):.2f} | {st.mean(cc):.2f} | {st.mean(scf):.2f}")

# user_instructions effect on the dependent question
out.append("\n## Effect of removing user_instructions on the dependent question")
out.append("model | top choice changed | mean TV shift (choice) | mean |shift| (score/noul)")
for s in SETS:
    flips = 0; nchoice = 0; tvs = []; other = []
    for cid, req, meta in cases:
        q = meta.get("ui_dependent_question"); spec = req["questions"][q]
        x = derive(cid, "ui", s, q, spec); y = derive(cid, "noui", s, q, spec)
        if not x or not y: continue
        if spec["type"] == "choice":
            nchoice += 1; flips += x["top"] != y["top"]; tvs.append(tv(x["vec"], y["vec"]))
        elif spec["type"] == "score": other.append(abs(x["score"] - y["score"]))
        else: other.append(abs(x["p"] - y["p"]))
    out.append(f"{s} | {flips}/{nchoice} | {st.mean(tvs):.2f} | {st.mean(other) if other else float('nan'):.2f}")

# per-level and per-domain disagreement on the dependent choice (with UI)
out.append(f"\n## Dependent-question top choice across the {len(SETS)} sets (with user_instructions)")
out.append("case | " + " | ".join(SETS) + " | agreement")
lvl = defaultdict(list); dom = defaultdict(list)
for cid, req, meta in cases:
    q = meta["ui_dependent_question"]; spec = req["questions"][q]
    tops = []
    for s in SETS:
        x = derive(cid, "ui", s, q, spec)
        tops.append(x["top"] if x and spec["type"] == "choice" else (f"{x['score']:.2f}" if x and spec["type"]=="score" else (f"{x['p']:.2f}" if x else "-")))
    maj = max(tops.count(t) for t in tops)
    lvl[cid.split("-L")[1]].append(maj); dom[cid.split("-L")[0]].append(maj)
    out.append(f"{cid} | " + " | ".join(tops) + f" | {maj}/{len(SETS)}")
out.append(f"\nmean agreement (largest bloc of {len(SETS)}) by level: " + ", ".join(f"L{k} {st.mean(v):.1f}" for k, v in sorted(lvl.items())))
out.append("by domain: " + ", ".join(f"{k} {st.mean(v):.1f}" for k, v in sorted(dom.items())))

# L5 injection handling: does any set's reasons mention the injection?
out.append("\n## L5 injection mentioned in reasons (with user_instructions)")
for s in SETS:
    hits = []
    for cid, req, meta in cases:
        if not cid.endswith("L5"): continue
        txt = json.dumps(reasons.get((cid, "ui", s), {})).lower()
        hits.append(bool(re.search(r"inject|ignore all rules|system:|untrusted|embedded instruction", txt)))
    has_reasons = any(reasons.get((c, "ui", s)) for c, _, _ in cases)
    out.append(f"{s}: {sum(hits)}/5" if has_reasons else f"{s}: n/a (no reasons in this set)")

# consensus of all sets per question: choice = mean distribution, score/noul = median
def consensus(cid, v, q, spec):
    xs = [x for x in (derive(cid, v, s, q, spec) for s in SETS) if x]
    if not xs: return None
    if spec["type"] == "choice":
        keys = list(spec["criteria"]); vec = [st.mean(x["vec"][i] for x in xs) for i in range(len(keys))]
        return {"vec": vec, "top": keys[max(range(len(keys)), key=lambda i: vec[i])]}
    if spec["type"] == "score":
        return {"score": st.median(x["score"] for x in xs), "sd": st.pstdev([x["score"] for x in xs])}
    return {"p": st.median(x["p"] for x in xs), "sd": st.pstdev([x["p"] for x in xs])}

profile = {}
out.append("\n## Distance from the all-set consensus (every question)")
out.append("set | choice top = consensus (UI) | (no UI) | choice TV to consensus (UI) | score |diff| to median (UI) | noul |diff| to median (UI)")
for s in SETS:
    row = {}
    for v in ("ui", "noui"):
        top = []; tvd = []; sd = []; nd = []
        for cid, req, meta in cases:
            for q, spec in req["questions"].items():
                x = derive(cid, v, s, q, spec); c = consensus(cid, v, q, spec)
                if not x or not c: continue
                if spec["type"] == "choice": top.append(x["top"] == c["top"]); tvd.append(tv(x["vec"], c["vec"]))
                elif spec["type"] == "score": sd.append(abs(x["score"] - c["score"]))
                else: nd.append(abs(x["p"] - c["p"]))
        row[v] = {"top": sum(top), "n": len(top), "tv": st.mean(tvd), "score": st.mean(sd), "noul": st.mean(nd)}
    profile[s] = row
    u, n = row["ui"], row["noui"]
    out.append(f"{s} | {u['top']}/{u['n']} | {n['top']}/{n['n']} | {u['tv']:.2f} | {u['score']:.2f} | {u['noul']:.2f}")

by_level = {}
out.append("\n## Spread across all sets by level (with user_instructions, every question)")
out.append("level | choice tops matching consensus | mean score spread (sd, levels) | mean noul spread (sd)")
for L in "12345":
    tm = []; ss = []; ns = []
    for cid, req, meta in cases:
        if not cid.endswith("L" + L): continue
        for q, spec in req["questions"].items():
            c = consensus(cid, "ui", q, spec)
            if spec["type"] == "choice":
                tm += [derive(cid, "ui", s, q, spec)["top"] == c["top"] for s in SETS if derive(cid, "ui", s, q, spec)]
            elif spec["type"] == "score": ss.append(c["sd"])
            else: ns.append(c["sd"])
    by_level[L] = {"top_rate": sum(tm) / len(tm), "score_sd": st.mean(ss), "noul_sd": st.mean(ns)}
    out.append(f"L{L} | {sum(tm)}/{len(tm)} ({100 * sum(tm) / len(tm):.0f}%) | {st.mean(ss):.2f} | {st.mean(ns):.2f}")

out.append("\n## Validity issues")
for s in SETS:
    out.append(f"{s}: {len(issues[s])}" + ("" if not issues[s] else " -> " + "; ".join(issues[s][:8])))
print("\n".join(out))

if "--json" in sys.argv:
    # machine-readable summary for charts; same numbers as the text output
    pair = {v: {} for v in ("ui", "noui")}
    for v in ("ui", "noui"):
        for a in SETS:
            for b in SETS:
                m = n = 0
                for cid, req, meta in cases:
                    for q, spec in req["questions"].items():
                        if spec["type"] != "choice": continue
                        x = derive(cid, v, a, q, spec); y = derive(cid, v, b, q, spec)
                        if x and y: n += 1; m += x["top"] == y["top"]
                pair[v][f"{a}|{b}"] = [m, n]
    flips = {}; conf = {}
    for s in SETS:
        f = n = 0; cc = []; scf = []
        for cid, req, meta in cases:
            q = meta["ui_dependent_question"]; spec = req["questions"][q]
            x = derive(cid, "ui", s, q, spec); y = derive(cid, "noui", s, q, spec)
            if x and y: n += 1; f += x["top"] != y["top"]
            for qq, sp in req["questions"].items():
                z = derive(cid, "ui", s, qq, sp)
                if z and sp["type"] == "choice": cc.append(z["conf"])
                if z and sp["type"] == "score": scf.append(z["conf"])
        flips[s] = [f, n]; conf[s] = {"choice": st.mean(cc), "score": st.mean(scf)}
    dep = []
    for cid, req, meta in cases:
        q = meta["ui_dependent_question"]; spec = req["questions"][q]
        tops = {s: (derive(cid, "ui", s, q, spec) or {}).get("top") for s in SETS}
        vals = [t for t in tops.values() if t]
        dep.append({"case": cid, "question": q, "tops": tops, "majority": max(set(vals), key=vals.count)})
    json.dump({"sets": SETS, "pair": pair, "profile": profile, "by_level": by_level, "flips": flips,
               "confidence": conf, "dependent": dep, "validity": {s: len(issues[s]) for s in SETS}},
              open(sys.argv[sys.argv.index("--json") + 1], "w", encoding="utf-8"), indent=1)
