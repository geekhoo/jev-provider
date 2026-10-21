#!/usr/bin/env bash
# Send the corpus cases to POST /v1/systemone and save the responses.
# Usage: run.sh [-u BASE_URL] [-m MODEL] [-o OUT_DIR] [-f FILTER]
#   -u  base URL            (default http://127.0.0.1:8080)
#   -m  model alias         (default jev-latest)
#   -o  output directory    (default ./runs/<model>-<timestamp>)
#   -f  filter: domain, Ln, or domain/Ln  (e.g. payments, L3, moderation/L5)
# The token is read from JEV_TOKEN and sent as a Bearer token. It is never printed.
set -u

BASE="http://127.0.0.1:8080"
MODEL="jev-latest"
OUT=""
FILTER=""
while getopts "u:m:o:f:h" opt; do
  case "$opt" in
    u) BASE="$OPTARG" ;;
    m) MODEL="$OPTARG" ;;
    o) OUT="$OPTARG" ;;
    f) FILTER="$OPTARG" ;;
    *) sed -n '2,9p' "$0"; exit 2 ;;
  esac
done

PY=""
for c in python3 python; do
  if command -v "$c" >/dev/null 2>&1 && "$c" -c 'import sys' >/dev/null 2>&1; then PY="$c"; break; fi
done
[ -n "$PY" ] || { echo "python3 or python is required" >&2; exit 2; }
command -v curl >/dev/null 2>&1 || { echo "curl is required" >&2; exit 2; }

DIR="$(cd "$(dirname "$0")" && pwd)"
[ -n "$OUT" ] || OUT="./runs/${MODEL}-$(date +%Y%m%d-%H%M%S)"
mkdir -p "$OUT"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

AUTH_ARGS=()
if [ -n "${JEV_TOKEN:-}" ]; then
  printf 'Authorization: Bearer %s\n' "$JEV_TOKEN" > "$TMP/auth"
  AUTH_ARGS=(-H "@$TMP/auth")
fi

FDOM=""; FLEV=""
if [ -n "$FILTER" ]; then
  case "$FILTER" in
    */*) FDOM="${FILTER%%/*}"; FLEV="${FILTER##*/}" ;;
    L[0-9]) FLEV="$FILTER" ;;
    *) FDOM="$FILTER" ;;
  esac
fi

n_ok=0; n_4xx=0; n_5xx=0; n_other=0; n_conn=0; total=0
echo "Base URL: $BASE   Model: $MODEL   Output: $OUT   Token: $([ -n "${JEV_TOKEN:-}" ] && echo set || echo not set)"

for f in "$DIR"/*/L[0-9].json; do
  [ -e "$f" ] || continue
  lev="$(basename "$f" .json)"
  dom="$(basename "$(dirname "$f")")"
  [ -z "$FDOM" ] || [ "$FDOM" = "$dom" ] || continue
  [ -z "$FLEV" ] || [ "$FLEV" = "$lev" ] || continue
  case_id="$dom-$lev"
  total=$((total + 1))

  "$PY" -c 'import json,sys
d=json.load(open(sys.argv[1],encoding="utf-8"))
d["model"]=sys.argv[2]
sys.stdout.buffer.write(json.dumps(d,ensure_ascii=False).encode("utf-8"))' "$f" "$MODEL" > "$TMP/body.json"

  code_time="$(curl -sS -o "$TMP/resp.body" -D "$TMP/resp.hdr" --max-time 300 \
      -w '%{http_code} %{time_total}' -X POST "${BASE%/}/v1/systemone" \
      -H 'Content-Type: application/json' "${AUTH_ARGS[@]+"${AUTH_ARGS[@]}"}" \
      --data-binary "@$TMP/body.json" 2>"$TMP/curl.err")"
  rc=$?
  if [ $rc -ne 0 ] || [ "${code_time%% *}" = "000" ]; then
    n_conn=$((n_conn + 1))
    printf '%-14s %-8s %s\n' "$case_id" "FAILED" "connect error: $(head -c 200 "$TMP/curl.err" | tr '\n' ' ')"
    continue
  fi
  status="${code_time%% *}"
  secs="${code_time##* }"
  ms="$(awk -v s="$secs" 'BEGIN{printf "%d", s*1000+0.5}')"

  "$PY" -c 'import json,sys
case,status,ms,hdr,body,out=sys.argv[1:7]
h={}
for line in open(hdr,encoding="latin-1"):
    if line.startswith("HTTP/"): h={}
    elif ":" in line:
        k,v=line.split(":",1); h[k.strip().lower()]=v.strip()
raw=open(body,"rb").read().decode("utf-8","replace")
try: b=json.loads(raw)
except Exception: b=raw
r={"case":case,"http_status":int(status),"request_id":h.get("x-request-id"),
   "response_mode":h.get("x-system-one-response-mode"),"latency_ms":int(ms),"body":b}
open(out,"w",encoding="utf-8").write(json.dumps(r,ensure_ascii=False,indent=2)+"\n")' \
    "$case_id" "$status" "$ms" "$TMP/resp.hdr" "$TMP/resp.body" "$OUT/$case_id.response.json"

  case "$status" in
    2??) n_ok=$((n_ok + 1)) ;;
    4??) n_4xx=$((n_4xx + 1)) ;;
    5??) n_5xx=$((n_5xx + 1)) ;;
    *) n_other=$((n_other + 1)) ;;
  esac
  printf '%-14s %-8s %s ms\n' "$case_id" "$status" "$ms"
done

failed=$((n_4xx + n_5xx + n_other + n_conn))
rate="$(awk -v f="$failed" -v t="$total" 'BEGIN{ if (t==0) print "0.0"; else printf "%.1f", 100*f/t }')"
echo
printf '%-12s %s\n' "Total" "$total"
printf '%-12s %s\n' "ok (2xx)" "$n_ok"
printf '%-12s %s\n' "4xx" "$n_4xx"
printf '%-12s %s\n' "5xx" "$n_5xx"
printf '%-12s %s\n' "no connect" "$n_conn"
printf '%-12s %s%%\n' "failure rate" "$rate"
echo "Responses saved in: $OUT"
[ "$n_conn" -eq 0 ] || exit 1
exit 0
