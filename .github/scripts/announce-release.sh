#!/usr/bin/env bash
# Posts one release announcement to Discord. Run by the `announce` job of
# .github/workflows/release.yml; never run it by hand against the real webhook.
#
# Inputs (environment, set by the workflow):
#   DISCORD_WEBHOOK   webhook URL (secret). Empty → nothing is posted, exit 0.
#   RELEASE_TAG       e.g. v1.2.3
#   RELEASE_URL       html_url of the GitHub Release
#   RELEASE_BODY      release notes (markdown), may be empty
#   GH_TOKEN, GITHUB_REPOSITORY, GITHUB_RUN_ID, GITHUB_RUN_ATTEMPT
#                     used to look up earlier attempts of this run (dedup)
#   DRY_RUN=1         print the payload and exit: no webhook, no API call
#
# The webhook URL is a credential: it is never echoed, never passed on a command
# line (curl reads it from stdin) and curl's own error output is discarded.
set -euo pipefail

# Display name of the workflow job that runs this script. The dedup below finds
# earlier successful announcements by this name, so it must stay identical to
# `jobs.announce.name` in .github/workflows/release.yml.
ANNOUNCE_JOB_NAME="Announce on Discord"

# Discord caps an embed description at 4096 characters. The notes get 3500 and
# the rest is headroom for the "read the full notes" link and the install hints.
MAX_NOTES_CHARS=3500

: "${RELEASE_TAG:?RELEASE_TAG is required}"
: "${RELEASE_URL:?RELEASE_URL is required}"
RELEASE_BODY="${RELEASE_BODY:-}"
DRY_RUN="${DRY_RUN:-0}"

if [ "$DRY_RUN" != "1" ]; then
  if [ -z "${DISCORD_WEBHOOK:-}" ]; then
    echo "::notice::DISCORD_WEBHOOK not set, skipping the Discord announcement"
    exit 0
  fi

  # "Re-run all jobs" starts a new attempt of the same run. If an earlier
  # attempt already announced this release, do not announce it again.
  attempt="${GITHUB_RUN_ATTEMPT:-1}"
  n=1
  while [ "$n" -lt "$attempt" ]; do
    if ! announced=$(gh api --paginate \
      "repos/${GITHUB_REPOSITORY}/actions/runs/${GITHUB_RUN_ID}/attempts/${n}/jobs" \
      --jq ".jobs[] | select(.name == \"${ANNOUNCE_JOB_NAME}\" and .conclusion == \"success\") | .id"); then
      # Posting without knowing would risk a duplicate; failing leaves the job
      # re-runnable.
      echo "::warning::Discord announcement not sent: could not read attempt ${n} of this run to rule out a duplicate"
      exit 1
    fi
    if [ -n "$announced" ]; then
      echo "::notice::${RELEASE_TAG} was already announced in attempt ${n} of this run, skipping"
      exit 0
    fi
    n=$((n + 1))
  done
fi

payload=$(jq -n \
  --arg tag "$RELEASE_TAG" \
  --arg url "$RELEASE_URL" \
  --arg body "$RELEASE_BODY" \
  --argjson max "$MAX_NOTES_CHARS" '
  ($tag | ltrimstr("v")) as $version
  | ($body | gsub("\r"; "") | sub("\\s+$"; "")) as $notes
  | (if ($notes | length) > $max then
       ($notes[0:$max] | sub("\\s+$"; "")) as $cut
       # Truncation counts characters, so it can land inside a code block:
       # close the fence, or the rest of the embed renders as code.
       | (if ([$cut | scan("```")] | length) % 2 == 1 then $cut + "\n```" else $cut end)
         + "\n\n… [read the full notes](" + $url + ")"
     else $notes end) as $shown
  | {
      username: "Ferrox Releases",
      allowed_mentions: {parse: []},
      embeds: [{
        title: ("Ferrox " + $tag),
        url: $url,
        description: (
          (if $shown == "" then "" else $shown + "\n\n" end)
          + "**Install**\n"
          + "`docker pull ghcr.io/shaharia-lab/ferrox:" + $version + "`\n"
          + "`brew install shaharia-lab/tap/ferrox`"
        )
      }]
    }')

if [ "$DRY_RUN" = "1" ]; then
  printf '%s\n' "$payload"
  exit 0
fi

case "$DISCORD_WEBHOOK" in
  *\?*) endpoint="${DISCORD_WEBHOOK}&wait=true" ;;
  *) endpoint="${DISCORD_WEBHOOK}?wait=true" ;;
esac

rc=0
code=$(printf 'url = "%s"\n' "$endpoint" | curl --config - \
  --silent --output /dev/null --write-out '%{http_code}' \
  --max-time 20 --retry 2 \
  --header 'Content-Type: application/json' \
  --data "$payload" 2>/dev/null) || rc=$?

case "$code" in
  2??)
    if [ "$rc" -eq 0 ]; then
      echo "Announced ${RELEASE_TAG} on Discord (HTTP ${code})"
      exit 0
    fi
    ;;
esac

echo "::warning::Discord announcement failed (HTTP ${code:-000}, curl exit ${rc})"
exit 1
