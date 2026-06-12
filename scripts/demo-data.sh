#!/bin/sh
# Build the synthetic demo archive for assets/demo.tape (never live data).
# Curated story sessions + bulk filler corpus (same generator style as bench.sh).
set -eu

DEMO="${DEMO_DIR:-/tmp/subrosa-demo}"
export SUBROSA_DIR="$DEMO/data"
export SUBROSA_PROJECTS_DIR="$DEMO/projects"

rm -rf "$DEMO"
D="$SUBROSA_PROJECTS_DIR/-work-acme-api"
mkdir -p "$D"

cat > "$D/aaaa1111-0000-4000-8000-000000000001.jsonl" <<'EOF'
{"type":"user","timestamp":"2026-05-02T09:14:00Z","uuid":"u1","cwd":"/work/acme/api","message":{"role":"user","content":"prod incident: aurora failover took 4 minutes and the api errored the whole time. why so slow?"}}
{"type":"assistant","timestamp":"2026-05-02T09:16:00Z","uuid":"a1","cwd":"/work/acme/api","message":{"role":"assistant","content":[{"type":"text","text":"Root cause: pgbouncer kept pinning the old writer after the aurora failover — server_check_delay was 30s and the DNS TTL cached the dead endpoint. Fix: set server_check_delay=2, dns_max_ttl=1, and enable fast_failover in the cluster parameter group."}]}}
{"type":"user","timestamp":"2026-05-02T09:40:00Z","uuid":"u2","cwd":"/work/acme/api","message":{"role":"user","content":"applied. failover drill now completes in 40 seconds, zero 5xx. documenting this in the runbook"}}
EOF

cat > "$D/bbbb2222-0000-4000-8000-000000000002.jsonl" <<'EOF'
{"type":"user","timestamp":"2026-05-20T14:02:00Z","uuid":"u1","cwd":"/work/acme/api","message":{"role":"user","content":"we need rate limiting on api-gateway before the launch. redis sliding window or token bucket?"}}
{"type":"assistant","timestamp":"2026-05-20T14:05:00Z","uuid":"a1","cwd":"/work/acme/api","message":{"role":"assistant","content":[{"type":"text","text":"Decided: redis sliding window on api-gateway, limit 50 req/s per key with burst=200. Token bucket rejected — burst smoothing matters more than strict average here. Implemented in rate_limiter.rs with a lua script for atomicity."}]}}
EOF

cat > "$D/cccc3333-0000-4000-8000-000000000003.jsonl" <<'EOF'
{"type":"user","timestamp":"2026-06-01T10:30:00Z","uuid":"u1","cwd":"/work/acme/api","message":{"role":"user","content":"bump the terraform aws provider and the vpc module, then apply in staging"}}
{"type":"assistant","timestamp":"2026-06-01T10:42:00Z","uuid":"a1","cwd":"/work/acme/api","message":{"role":"assistant","content":[{"type":"text","text":"Done: aws provider 5.9 to 6.3, vpc module 5.21 to 6.0.1. tofu apply clean in staging, 14 resources updated in place, no replacements. Prod apply scheduled for thursday."}]}}
EOF

# Filler corpus so the dashboard looks like a real archive (deterministic LCG awk,
# same approach as scripts/bench.sh).
awk -v root="$SUBROSA_PROJECTS_DIR" -v sessions=96 -v turns=250 'BEGIN {
  seed = 42
  np = split("-work-acme-web -work-acme-infra -work-acme-billing -home-jp-oss-zola-site -home-jp-scratch -work-acme-data -work-acme-mobile -home-jp-dotfiles", projs, " ")
  nw = split("deploy rollout cluster ingress gateway terraform module bucket lambda queue " \
             "retry timeout migration schema index vacuum replica failover snapshot backup " \
             "latency throughput budget alert dashboard pipeline runner artifact release " \
             "candidate incident postmortem rollback canary traffic shard partition broker", words, " ")
  ni = split("svc-cache-prod svc-auth-prod svc-billing-prod svc-search-prod svc-ingest-prod " \
             "cache-gateway-prod us-east-1", idents, " ")
  for (n = 1000; n < 1040; n++) idents[++ni] = "TICKET-" n
  for (s = 0; s < sessions; s++) {
    proj = projs[(s % np) + 1]
    cwd = "/work/demo" (s % np)
    system("mkdir -p \"" root "/" proj "\"")
    f = sprintf("%s/%s/demo-%04d-0000-4000-8000-%012d.jsonl", root, proj, s, s)
    for (t = 0; t < turns; t++) {
      ts = sprintf("2026-%02d-%02dT%02d:%02d:00Z", 3 + int(s / 31), (s % 28) + 1, int(t / 60) % 24, t % 60)
      if (t % 2 == 0)
        printf "{\"type\":\"user\",\"timestamp\":\"%s\",\"uuid\":\"u%d-%d\",\"cwd\":\"%s\",\"message\":{\"role\":\"user\",\"content\":\"%s\"}}\n", ts, s, t, cwd, sentence() > f
      else
        printf "{\"type\":\"assistant\",\"timestamp\":\"%s\",\"uuid\":\"a%d-%d\",\"cwd\":\"%s\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"%s %s\"}]}}\n", ts, s, t, cwd, sentence(), sentence() > f
    }
    close(f)
  }
}
function rnd(n) { seed = (seed * 16807) % 2147483647; return seed % n }
function pick(arr, n) { return arr[rnd(n) + 1] }
function sentence(   k, i, out) {
  k = 8 + rnd(13); out = ""
  for (i = 0; i < k; i++) out = out (i ? " " : "") words[rnd(nw) + 1]
  if (rnd(10) < 4) out = out " " pick(idents, ni)
  return out
}'

subrosa ingest --sweep --quiet
subrosa init
echo "demo archive ready: SUBROSA_DIR=$SUBROSA_DIR SUBROSA_PROJECTS_DIR=$SUBROSA_PROJECTS_DIR"
