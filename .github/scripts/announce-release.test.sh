#!/usr/bin/env bash
# Tests for announce-release.sh. `curl` and `gh` are replaced by stubs on PATH,
# so nothing here touches the network. Run: .github/scripts/announce-release.test.sh
set -euo pipefail

SCRIPT="$(cd "$(dirname "$0")" && pwd)/announce-release.sh"
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

WEBHOOK="https://discord.test/api/webhooks/1/s3cr3t-token"
failures=0

mkdir "$WORK/bin"
# curl stub: records its arguments, the --data body and stdin, answers with $STUB_HTTP_CODE.
cat > "$WORK/bin/curl" <<'EOF'
#!/usr/bin/env bash
printf '%s\n' "$@" > "$STUB_DIR/curl.args"
while [ "$#" -gt 0 ]; do
  [ "$1" != "--data" ] || printf '%s' "$2" > "$STUB_DIR/curl.data"
  shift
done
cat > "$STUB_DIR/curl.stdin"
echo "curl: (22) error talking to $(cat "$STUB_DIR/curl.stdin")" >&2
printf '%s' "${STUB_HTTP_CODE:-204}"
exit "${STUB_CURL_EXIT:-0}"
EOF
# gh stub: records the call, then answers `gh api <path> --jq <filter>` the way
# gh does: the jobs of that attempt ($STUB_GH_ATTEMPT_<n>, default none) run
# through the real filter the script passed.
cat > "$WORK/bin/gh" <<'EOF'
#!/usr/bin/env bash
printf '%s\n' "$*" >> "$STUB_DIR/gh.calls"
[ "${STUB_GH_EXIT:-0}" = "0" ] || exit "$STUB_GH_EXIT"
filter=.
attempt=
while [ "$#" -gt 0 ]; do
  case "$1" in
    --jq) filter=$2; shift ;;
    */attempts/*/jobs) attempt=${1%/jobs}; attempt=${attempt##*/} ;;
  esac
  shift
done
jobs_var="STUB_GH_ATTEMPT_${attempt}"
jq -r "$filter" <<<"${!jobs_var:-{\"jobs\":[]\}}"
EOF
chmod +x "$WORK/bin/curl" "$WORK/bin/gh"

# run NAME [VAR=value ...] — runs the script with a clean stub directory.
# Sets: status, out (stdout+stderr), STUB_DIR.
run() {
  STUB_DIR="$WORK/$1"
  shift
  mkdir -p "$STUB_DIR"
  status=0
  out=$(env -i PATH="$WORK/bin:$PATH" STUB_DIR="$STUB_DIR" \
    RELEASE_TAG=v1.2.3 \
    RELEASE_URL=https://github.com/shaharia-lab/ferrox/releases/tag/v1.2.3 \
    GITHUB_REPOSITORY=shaharia-lab/ferrox GITHUB_RUN_ID=77 GITHUB_RUN_ATTEMPT=1 \
    GITHUB_OUTPUT="$STUB_DIR/output" \
    "$@" "$SCRIPT" 2>&1) || status=$?
}

check() { # check DESCRIPTION CONDITION...
  local what=$1
  shift
  if "$@"; then
    echo "ok   - $what"
  else
    echo "FAIL - $what"
    failures=$((failures + 1))
  fi
}
contains() { case "$1" in *"$2"*) return 0 ;; *) return 1 ;; esac; }
lacks() { ! contains "$1" "$2"; }
posted() { [ -f "$STUB_DIR/curl.args" ]; }
not_posted() { ! posted; }
valid_json() { jq -e . >/dev/null 2>&1 <<<"$1"; }

# ── payload (DRY_RUN) ────────────────────────────────────────────────────────
# shellcheck disable=SC2016  # literal backticks, nothing to expand
run dry DRY_RUN=1 RELEASE_BODY='Fixes "quotes", `ticks` and @everyone @here'
check "dry run exits 0" [ "$status" -eq 0 ]
check "dry run posts nothing" not_posted
check "dry run prints valid JSON" valid_json "$out"
check "title carries the tag" [ "$(jq -r '.embeds[0].title' <<<"$out")" = "Ferrox v1.2.3" ]
check "embed links to the release" \
  [ "$(jq -r '.embeds[0].url' <<<"$out")" = "https://github.com/shaharia-lab/ferrox/releases/tag/v1.2.3" ]
check "mentions are disabled" [ "$(jq -c '.allowed_mentions' <<<"$out")" = '{"parse":[]}' ]
desc=$(jq -r '.embeds[0].description' <<<"$out")
# shellcheck disable=SC2016
check "notes survive quoting" contains "$desc" 'Fixes "quotes", `ticks` and @everyone @here'
check "image hint drops the v prefix" contains "$desc" 'ghcr.io/shaharia-lab/ferrox:1.2.3`'
check "brew hint is present" contains "$desc" 'brew install shaharia-lab/tap/ferrox'
check "short notes are not truncated" lacks "$desc" 'read the full notes'

long=$(printf 'x%.0s' $(seq 1 5000))
run long DRY_RUN=1 RELEASE_BODY="$long"
desc=$(jq -r '.embeds[0].description' <<<"$out")
check "long notes fit Discord's 4096 limit" [ "${#desc}" -le 4096 ]
check "long notes keep 3500 characters" contains "$desc" "$(printf 'x%.0s' $(seq 1 3500))"
check "long notes are cut at 3500" lacks "$desc" "$(printf 'x%.0s' $(seq 1 3501))"
check "truncated notes link to the release" \
  contains "$desc" '… [read the full notes](https://github.com/shaharia-lab/ferrox/releases/tag/v1.2.3)'

run fence DRY_RUN=1 RELEASE_BODY="$(printf 'intro\n```sh\n%s' "$long")"
desc=$(jq -r '.embeds[0].description' <<<"$out")
check "a code fence cut by truncation is closed" \
  [ $(($(grep -o '```' <<<"$desc" | wc -l) % 2)) -eq 0 ]
check "fenced long notes fit the limit" [ "${#desc}" -le 4096 ]

run empty DRY_RUN=1
desc=$(jq -r '.embeds[0].description' <<<"$out")
check "empty notes still give install hints" contains "$desc" '**Install**'

# ── posting ──────────────────────────────────────────────────────────────────
run post DISCORD_WEBHOOK="$WEBHOOK" RELEASE_BODY="notes"
check "success exits 0" [ "$status" -eq 0 ]
check "success posts once" posted
check "url goes to curl on stdin, with wait=true" \
  [ "$(cat "$STUB_DIR/curl.stdin")" = "url = \"${WEBHOOK}?wait=true\"" ]
check "webhook is not on curl's command line" lacks "$(cat "$STUB_DIR/curl.args")" "s3cr3t-token"
check "webhook is not in the output" lacks "$out" "s3cr3t-token"
check "posted body is the embed" [ "$(jq -r '.embeds[0].title' "$STUB_DIR/curl.data")" = "Ferrox v1.2.3" ]
check "first attempt makes no API call" [ ! -f "$STUB_DIR/gh.calls" ]
check "success sets the announced output" [ "$(cat "$STUB_DIR/output")" = "announced=true" ]

run post-query DISCORD_WEBHOOK="${WEBHOOK}?thread_id=9"
check "a webhook with a query gets &wait=true" \
  [ "$(cat "$STUB_DIR/curl.stdin")" = "url = \"${WEBHOOK}?thread_id=9&wait=true\"" ]

run rejected DISCORD_WEBHOOK="$WEBHOOK" STUB_HTTP_CODE=404
check "non-2xx fails the job" [ "$status" -eq 1 ]
check "non-2xx warns with the status" \
  contains "$out" "::warning::Discord announcement failed (HTTP 404, curl exit 0)"
check "failure does not leak the webhook" lacks "$out" "s3cr3t-token"
check "failure does not set the announced output" [ ! -s "$STUB_DIR/output" ]
check "the POST is never retried by curl" lacks "$(cat "$STUB_DIR/curl.args")" "--retry"

run neterr DISCORD_WEBHOOK="$WEBHOOK" STUB_HTTP_CODE=000 STUB_CURL_EXIT=6
check "network error fails the job" [ "$status" -eq 1 ]
check "network error warns" \
  contains "$out" "::warning::Discord announcement failed (HTTP 000, curl exit 6)"
check "curl's own error output is discarded" lacks "$out" "s3cr3t-token"

# ── skipping ─────────────────────────────────────────────────────────────────
run nosecret DISCORD_WEBHOOK=
check "absent secret exits 0" [ "$status" -eq 0 ]
check "absent secret posts nothing" not_posted
check "absent secret logs a notice" contains "$out" "::notice::DISCORD_WEBHOOK not set"
check "absent secret does not set the announced output" [ ! -s "$STUB_DIR/output" ]

# ── names shared with the workflow ───────────────────────────────────────────
WORKFLOW="$(dirname "$SCRIPT")/../workflows/release.yml"
job_name=$(sed -n 's/^ANNOUNCE_JOB_NAME="\(.*\)"$/\1/p' "$SCRIPT")
step_name=$(sed -n 's/^ANNOUNCED_STEP_NAME="\(.*\)"$/\1/p' "$SCRIPT")
check "release.yml names the job as the script expects" grep -qx "    name: ${job_name}" "$WORKFLOW"
check "release.yml names the marker step as the script expects" \
  grep -qx "      - name: ${step_name}" "$WORKFLOW"

# jobs_json JOB_CONCLUSION [MARKER_STEP_CONCLUSION] — one attempt's jobs, as the
# API returns them. Without a marker conclusion the announce job has no marker
# step, which is what a clean skip looks like.
jobs_json() {
  jq -n --arg job "$1" --arg marker "${2:-}" '{jobs: [
    {name: "Docker image (ferrox)", conclusion: "success",
     steps: [{name: "Mark release announced", conclusion: "success"}]},
    {name: "Announce on Discord", conclusion: $job,
     steps: ([{name: "Post release announcement", conclusion: $job}]
       + (if $marker == "" then [] else [{name: "Mark release announced", conclusion: $marker}] end))}
  ]}'
}

run rerun-dup DISCORD_WEBHOOK="$WEBHOOK" GITHUB_RUN_ATTEMPT=3 \
  STUB_GH_ATTEMPT_1="$(jobs_json failure)" STUB_GH_ATTEMPT_2="$(jobs_json success success)"
check "re-run after a successful announce exits 0" [ "$status" -eq 0 ]
check "re-run after a successful announce posts nothing" not_posted
check "re-run logs a notice" contains "$out" "::notice::v1.2.3 was already announced in attempt 2"
check "earlier attempts are read from this run" \
  contains "$(cat "$STUB_DIR/gh.calls")" "repos/shaharia-lab/ferrox/actions/runs/77/attempts/1/jobs"

run rerun-first DISCORD_WEBHOOK="$WEBHOOK" GITHUB_RUN_ATTEMPT=2
check "re-run with no earlier announce posts" posted
check "re-run with no earlier announce exits 0" [ "$status" -eq 0 ]

run rerun-failed DISCORD_WEBHOOK="$WEBHOOK" GITHUB_RUN_ATTEMPT=2 STUB_GH_ATTEMPT_1="$(jobs_json failure)"
check "re-run after a failed announce posts" posted

run rerun-skipped DISCORD_WEBHOOK="$WEBHOOK" GITHUB_RUN_ATTEMPT=2 STUB_GH_ATTEMPT_1="$(jobs_json success)"
check "re-run after a clean skip (job succeeded, nothing posted) posts" posted

run rerun-marker-skipped DISCORD_WEBHOOK="$WEBHOOK" GITHUB_RUN_ATTEMPT=2 \
  STUB_GH_ATTEMPT_1="$(jobs_json success skipped)"
check "re-run after a skipped marker step posts" posted

run rerun-apierr DISCORD_WEBHOOK="$WEBHOOK" GITHUB_RUN_ATTEMPT=2 STUB_GH_EXIT=1
check "unreadable earlier attempt fails the job" [ "$status" -eq 1 ]
check "unreadable earlier attempt posts nothing" not_posted
check "unreadable earlier attempt warns" contains "$out" "::warning::Discord announcement not sent"

echo
if [ "$failures" -gt 0 ]; then
  echo "$failures check(s) failed"
  exit 1
fi
echo "all checks passed"
