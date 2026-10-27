#!/usr/bin/env bash
# Tracks Node.js security releases against the Node pins in Dockerfile.unified
# and the Node inside the published ghcr.io/openabdev/openab images. Keeps one
# open issue labeled node-security while anything is behind and closes it once
# everything is current. Exits non-zero only when the check itself breaks, so
# stale data never turns the workflow red. See #1567.
#
# Contract with the Dockerfile: each Node stage starts FROM node:${NODE<major>_VERSION}
# or from another Node stage, and each non-base-* stage is published as <channel>-<stage>.
#
# Env: DOCKERFILE (default Dockerfile.unified), DRY_RUN=1 prints the decision
# and issue body without calling gh, GH_TOKEN for gh.
set -euo pipefail
shopt -s inherit_errexit

DOCKERFILE=${DOCKERFILE:-Dockerfile.unified}
# Reserved for this script's tracker. close-unlinked-stale-issues.yml and issue-check.yml
# exempt it by label alone; rename it in all three files (this script and both workflows).
LABEL='node-security'
MARKER='<!-- node-security-check -->'
REPO=openabdev/openab
CHANNELS=(stable beta)
MANIFEST_ACCEPT=application/vnd.oci.image.index.v1+json,application/vnd.docker.distribution.manifest.list.v2+json,application/vnd.oci.image.manifest.v1+json,application/vnd.docker.distribution.manifest.v2+json

MAJORS=()
IMAGE_ROWS=()
declare -A PINNED=() AGENTS=() LATEST=() PIN_STATUS=() STALE_MAJOR=()

die() { echo "::error::$*" >&2; exit 1; }

version_lt() { [[ $1 != "$2" && $(printf '%s\n%s\n' "$1" "$2" | sort -V | head -n1) == "$1" ]]; }

status_of() {
  if [[ -z $1 ]]; then echo "not published"
  elif [[ ${1%%.*} != "$2" ]]; then echo "major mismatch"
  elif [[ -n ${LATEST[$2]} ]] && version_lt "$1" "${LATEST[$2]}"; then echo stale
  else echo current
  fi
}

is_behind() { [[ $1 == stale || $1 == "major mismatch" ]]; }

parse_dockerfile() {
  [[ -r $DOCKERFILE ]] || die "cannot read $DOCKERFILE"
  local line stage m
  local -a stages=() unpinned=()
  local -A stage_major=()
  while IFS= read -r line; do
    if [[ $line =~ ^ARG\ NODE([0-9]+)_VERSION=([0-9]+\.[0-9]+\.[0-9]+)$ ]]; then
      PINNED[${BASH_REMATCH[1]}]=${BASH_REMATCH[2]}
    elif [[ $line =~ ^FROM\ node:\$\{NODE([0-9]+)_VERSION\}[^\ ]*\ AS\ ([a-z0-9_-]+) ]]; then
      stage_major[${BASH_REMATCH[2]}]=${BASH_REMATCH[1]}
      stages+=("${BASH_REMATCH[2]}")
    elif [[ $line =~ ^FROM\ node: ]]; then
      unpinned+=("$line")
    elif [[ $line =~ ^FROM\ ([a-z0-9_-]+)\ AS\ ([a-z0-9_-]+) && -n ${stage_major[${BASH_REMATCH[1]}]:-} ]]; then
      stage_major[${BASH_REMATCH[2]}]=${stage_major[${BASH_REMATCH[1]}]}
      stages+=("${BASH_REMATCH[2]}")
    fi
  done <"$DOCKERFILE"

  ((${#PINNED[@]})) || die "no 'ARG NODE<major>_VERSION=<x.y.z>' found in $DOCKERFILE"
  ((${#unpinned[@]} == 0)) || die "Node stage not pinned through NODE<major>_VERSION in $DOCKERFILE: ${unpinned[*]}"
  for stage in "${stages[@]}"; do
    m=${stage_major[$stage]}
    [[ -n ${PINNED[$m]:-} ]] || die "stage $stage uses NODE${m}_VERSION but $DOCKERFILE declares no ARG for it"
    # base-* stages are shared parents, not published image targets.
    [[ $stage == base-* ]] || AGENTS[$m]+="$stage "
  done
  mapfile -t MAJORS < <(printf '%s\n' "${!PINNED[@]}" | sort -n)
}

load_latest_security() {
  local index m
  index=$(curl -fsSL --retry 3 --connect-timeout 10 --max-time 30 https://nodejs.org/dist/index.json) || die "cannot fetch https://nodejs.org/dist/index.json"
  for m in "${MAJORS[@]}"; do
    LATEST[$m]=$(jq -r --arg p "v$m." '.[] | select(.security and (.version | startswith($p))) | .version | ltrimstr("v")' <<<"$index" | sort -V | tail -n1)
  done
}

# Prints the response body, or nothing when the registry answers 404.
registry_get() {
  local code
  code=$(curl -sSL --retry 3 --connect-timeout 10 --max-time 30 -o "$TMP/registry" -w '%{http_code}' \
    -H "Authorization: Bearer $REGISTRY_TOKEN" -H "Accept: $2" \
    "https://ghcr.io/v2/$REPO/$1") || die "GET ghcr.io/v2/$REPO/$1 failed"
  case $code in
    200) cat "$TMP/registry" ;;
    404) ;;
    *) die "GET ghcr.io/v2/$REPO/$1 returned HTTP $code" ;;
  esac
}

# Prints NODE_VERSION from the linux/amd64 image config, or nothing when the tag is not published.
image_node_version() {
  local manifest digest config_digest config
  manifest=$(registry_get "manifests/$1" "$MANIFEST_ACCEPT")
  [[ -n $manifest ]] || return 0
  digest=$(jq -r '.manifests[]? | select(.platform.os == "linux" and .platform.architecture == "amd64") | .digest' <<<"$manifest" | head -n1)
  if [[ -n $digest ]]; then
    manifest=$(registry_get "manifests/$digest" "$MANIFEST_ACCEPT")
  fi
  config_digest=$(jq -er '.config.digest' <<<"$manifest") || die "$1: no linux/amd64 config in manifest"
  config=$(registry_get "blobs/$config_digest" '*/*')
  jq -er '.config.Env[] | select(startswith("NODE_VERSION=")) | ltrimstr("NODE_VERSION=")' <<<"$config" ||
    die "$1: NODE_VERSION not set in image config"
}

assess() {
  local m agent channel node status
  REGISTRY_TOKEN=$(curl -fsS --retry 3 --connect-timeout 10 --max-time 30 "https://ghcr.io/token?scope=repository:$REPO:pull" | jq -er .token) ||
    die "cannot get an anonymous ghcr.io pull token"
  for m in "${MAJORS[@]}"; do
    PIN_STATUS[$m]=$(status_of "${PINNED[$m]}" "$m")
    if is_behind "${PIN_STATUS[$m]}"; then STALE_MAJOR[$m]=1; fi
    for agent in ${AGENTS[$m]:-}; do
      for channel in "${CHANNELS[@]}"; do
        node=$(image_node_version "$channel-$agent")
        status=$(status_of "$node" "$m")
        IMAGE_ROWS+=("| \`$channel-$agent\` | ${node:--} | $status |")
        if is_behind "$status"; then STALE_MAJOR[$m]=1; fi
      done
    done
  done
}

release_link() {
  if [[ -n $1 ]]; then echo "[$1](https://nodejs.org/en/blog/release/v$1)"; else echo none; fi
}

render_tables() {
  local m agents
  echo "### Pinned base images (\`$DOCKERFILE\`)"
  echo
  echo "| Node line | Pinned | Latest security release | Status | Agents |"
  echo "|---|---|---|---|---|"
  for m in "${MAJORS[@]}"; do
    agents=${AGENTS[$m]:-}
    agents=${agents% }
    echo "| $m | ${PINNED[$m]} | $(release_link "${LATEST[$m]}") | ${PIN_STATUS[$m]} | ${agents// /, } |"
  done
  echo
  echo "### Published images (\`ghcr.io/$REPO\`, linux/amd64)"
  echo
  echo "| Tag | Node | Status |"
  echo "|---|---|---|"
  printf '%s\n' "${IMAGE_ROWS[@]}"
}

render_issue_body() {
  cat <<EOF
$MARKER
A pinned base image or a published openab image is behind the latest Node.js security release for its line, or is on the wrong Node major. The \`node-security-check\` workflow keeps this issue up to date daily and closes it once every pin and published image is current.

$(render_tables)

### Next steps

1. For each stale pin, bump \`ARG NODE<major>_VERSION\` to the latest security release in every Dockerfile that declares it (\`git grep -lE 'ARG NODE[0-9]+_VERSION' -- 'Dockerfile*'\`). \`$DOCKERFILE\` declares every line and each other Dockerfile declares only its own. The smoke tests read the expected versions from the Dockerfiles, so a bump needs no workflow edit.
2. A \`major mismatch\` row means the pin or image is on a different Node major than its line. Fix the \`ARG NODE<major>_VERSION\` value, or rebuild the image from the correct line, instead of bumping the patch version.
3. Follow the "Base image security release" runbook in [RELEASING.md](https://github.com/$REPO/blob/main/RELEASING.md#base-image-security-release) to cut a beta, verify \`node --version\` in each \`beta-<agent>\` image, and promote to stable.
4. Background and policy: #1567.

Last checked: $(date -u '+%Y-%m-%d %H:%M UTC')
EOF
}

issue_title() {
  local m versions=()
  for m in "${MAJORS[@]}"; do
    # A line can be behind on major mismatch alone, with no security release to name.
    if [[ -n ${STALE_MAJOR[$m]:-} ]]; then versions+=("${LATEST[$m]:-Node $m major mismatch}"); fi
  done
  local joined
  joined=$(printf '%s / ' "${versions[@]}")
  echo "Node.js security release pending: ${joined% / }"
}

# (anything stale, marked issue open) -> action.
decide() {
  case "$1$2" in
    10) echo create ;;
    11) echo update ;;
    01) echo close ;;
    00) echo none ;;
  esac
}

ensure_label() {
  local exists
  exists=$(gh label list --search "$LABEL" --json name --jq "any(.[]; .name == \"$LABEL\")")
  [[ $exists == true ]] || gh label create "$LABEL" --color B60205 --description "Node.js security release pending for openab images"
}

# Args: stale (0|1), title, body file.
reconcile() {
  local issue action has_issue=0
  # Only issues carrying MARKER are ours; a hand-labeled issue is left alone.
  # The label is reserved for this script, so at most one open issue is expected.
  issue=$(gh issue list --label "$LABEL" --state open --limit 100 --json number,body \
    --jq "[.[] | select((.body // \"\") | contains(\"$MARKER\"))][0].number // empty")
  if [[ -n $issue ]]; then has_issue=1; fi
  action=$(decide "$1" "$has_issue")
  echo "decision: $action${issue:+ (#$issue)}"
  case $action in
    create)
      ensure_label
      gh issue create --title "$2" --label "$LABEL" --body-file "$3"
      ;;
    update) gh issue edit "$issue" --title "$2" --body-file "$3" ;;
    close) gh issue close "$issue" --comment "Every Node pin and published image is on the latest Node.js security release for its line. Closing.

$(render_tables)" ;;
    none) ;;
  esac
}

main() {
  TMP=$(mktemp -d)
  trap 'rm -rf "$TMP"' EXIT
  parse_dockerfile
  load_latest_security
  assess

  local stale=0 title
  if ((${#STALE_MAJOR[@]})); then stale=1; fi
  title=$(issue_title)
  render_issue_body >"$TMP/body.md"
  if [[ -n ${GITHUB_STEP_SUMMARY:-} ]]; then render_tables >>"$GITHUB_STEP_SUMMARY"; fi

  if [[ ${DRY_RUN:-} == 1 ]]; then
    if ((stale)); then echo "decision: create or update issue"; else echo "decision: close the open issue if any, else none"; fi
    echo "title: $title"
    echo
    cat "$TMP/body.md"
  else
    reconcile "$stale" "$title" "$TMP/body.md"
  fi
}

if [[ ${BASH_SOURCE[0]} == "$0" ]]; then main "$@"; fi
