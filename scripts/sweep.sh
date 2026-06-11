#!/bin/sh
# Public-repo guard: block secrets, database files, and naming that doesn't
# belong here. Used by .githooks/pre-commit and CI. Exits 1 on any hit.
cd "$(git rev-parse --show-toplevel)" || exit 1
status=0

# Tracked files plus anything staged, minus this script (it holds the patterns).
files=$({ git ls-files; git diff --cached --name-only --diff-filter=ACM; } \
  | sort -u | grep -v '^scripts/sweep.sh$')

# Secret shapes. The AWS doc example key used in the redaction tests is allowed.
SECRETS='ghp_[A-Za-z0-9]{20,}|gho_[A-Za-z0-9]{20,}|github_pat_|AKIA[0-9A-Z]{16}|ASIA[0-9A-Z]{16}|-----BEGIN [A-Z ]*PRIVATE KEY|sk-ant-|xox[baprs]-'
# Allowed fixtures: the AWS doc example key and the fake PEM in the redaction tests.
hits=$(printf '%s\n' "$files" | tr '\n' '\0' | xargs -0 grep -nIE "$SECRETS" 2>/dev/null \
  | grep -v 'AKIAIOSFODNN7EXAMPLE' | grep -v 'redact.rs:[0-9]*: *let pem = ')
if [ -n "$hits" ]; then
  echo "sweep: possible secrets found:"
  echo "$hits"
  status=1
fi

# Legacy naming that must not resurface — this is a standalone project.
legacy=$(printf '%s\n' "$files" | tr '\n' '\0' | xargs -0 grep -nIiE 'python|pylike' 2>/dev/null)
legacy_mem=$(printf '%s\n' "$files" | tr '\n' '\0' | xargs -0 grep -nIwE 'mem' 2>/dev/null)
if [ -n "$legacy$legacy_mem" ]; then
  echo "sweep: legacy naming found:"
  printf '%s\n%s\n' "$legacy" "$legacy_mem" | grep .
  status=1
fi

# Never commit database files.
if git diff --cached --name-only | grep -E '\.(db|db-wal|db-shm)$' >/dev/null 2>&1; then
  echo "sweep: refusing to commit database files"
  status=1
fi

[ $status -eq 0 ] && echo "sweep: clean"
exit $status
