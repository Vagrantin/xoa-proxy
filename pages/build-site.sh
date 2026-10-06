#!/usr/bin/env bash
# Builds the whole Pages site (xcp-hl#154, jenkins-infra docs/promotion-gate.md v3): stable from stable.json,
# testing from the newest candidates. Every RPM's SHA-256 is checked; any missing part fails the run.
# Env: REPO (owner/name), STABLE_JSON, PROMOTIONS_COMMIT, PKG_GLOB_RE, GH_TOKEN (optional),
#      XCPNG_SERIES, REPO_ARCH, KEEP_CANDIDATES. Output: _site/ (metadata unsigned; the workflow signs it).
set -euo pipefail
: "${REPO:?}" "${STABLE_JSON:?}" "${PKG_GLOB_RE:?}"
SERIES="${XCPNG_SERIES:-8.3}" ARCH="${REPO_ARCH:-x86_64}" KEEP="${KEEP_CANDIDATES:-5}"
API="https://api.github.com/repos/${REPO}"
STABLE_DIR="_site/${SERIES}/${ARCH}"
TESTING_DIR="_site/testing/${SERIES}/${ARCH}"
NOW=$(date -u +%Y-%m-%dT%H:%M:%SZ)
gh_api() { curl -fsS --retry 5 --retry-all-errors ${GH_TOKEN:+-H "Authorization: Bearer ${GH_TOKEN}"} -H 'Accept: application/vnd.github+json' "$@"; }

jq -e '.generation and (.entries | type == "array")' "$STABLE_JSON" >/dev/null || { echo "stable.json is not a promotion record" >&2; exit 1; }
rm -rf _site && mkdir -p "$STABLE_DIR" "$TESTING_DIR"
releases=$(gh_api "${API}/releases?per_page=100")

# fetch DIR TAG NAME SHA256: one release asset, kept only if its bytes are the recorded ones.
fetch() {
  local dir="$1" tag="$2" name="$3" want="$4" url got
  url=$(jq -r --arg t "$tag" --arg n "$name" '.[] | select(.tag_name == $t) | .assets[] | select(.name == $n) | .browser_download_url' <<<"$releases")
  [[ -n "$url" ]] || { echo "missing: ${tag} has no asset ${name}" >&2; return 1; }
  curl -fsSL --retry 5 --retry-all-errors -o "${dir}/${name}" "$url"
  got=$(sha256sum "${dir}/${name}" | awk '{print $1}')
  [[ "$got" == "$want" ]] || { echo "digest mismatch: ${name} is ${got}, expected ${want}" >&2; return 1; }
  echo "  ${tag}: ${name}"
}

# Stable: exactly the promotion record. Withdrawn never; retired only until its retention date.
echo "stable, generation $(jq -r .generation "$STABLE_JSON") (promotions ${PROMOTIONS_COMMIT:-unknown}):"
jq -r --arg now "$NOW" '.entries[] | select(.status == "stable" or (.status == "retired" and (.retain_until // "") > $now))
  | .tag as $t | .assets[] | "\($t) \(.name) \(.sha256)"' "$STABLE_JSON" > stable.list
[[ -s stable.list ]] || { echo "stable.json lists nothing to serve" >&2; exit 1; }
while read -r tag name sha; do fetch "$STABLE_DIR" "$tag" "$name" "$sha"; done < stable.list

# Superseded (fix-forward): the file stays downloadable until retain_until, so cached metadata never 404s,
# but it is left out of the metadata, so nothing new can select or downgrade to it.
jq -r --arg now "$NOW" '.entries[] | select(.status == "superseded" and (.retain_until // "") > $now)
  | .tag as $t | .assets[] | "\($t) \(.name) \(.sha256)"' "$STABLE_JSON" > superseded.list
if [[ -s superseded.list ]]; then
  echo "superseded, kept unlisted:"
  while read -r tag name sha; do fetch "$STABLE_DIR" "$tag" "$name" "$sha"; done < superseded.list
fi

# Testing: the newest candidates (pre-releases), never a withdrawn one. Separate window from stable.
echo "testing (newest ${KEEP} candidates):"
jq -r --argjson w "$(jq -c '[.entries[] | select(.status == "withdrawn") | .tag]' "$STABLE_JSON")" --arg re "$PKG_GLOB_RE" --argjson n "$KEEP" '
  [.[] | select(.prerelease and (.draft | not) and (.tag_name as $t | $w | index($t) | not))]
  | sort_by(.published_at) | reverse | .[:$n][] | .tag_name as $t
  | .assets[] | select(.name | test($re)) | "\($t) \(.name) \((.digest // "") | sub("^sha256:"; ""))"' <<<"$releases" > testing.list
while read -r tag name sha; do
  [[ -n "$sha" ]] || { echo "missing: ${tag} ${name} has no digest" >&2; exit 1; }
  fetch "$TESTING_DIR" "$tag" "$name" "$sha"
done < testing.list
echo "  ($(wc -l < testing.list) candidate RPM(s))"

# XCP-ng 8.3 dom0 is CentOS 7 (yum 3.4.3): gz metadata, sqlite databases, no zchunk.
ZCK=""; createrepo_c --help 2>&1 | grep -q -- '--no-zck' && ZCK="--no-zck"
# Newer createrepo_c defaults XML metadata to zstd, which yum 3.4 cannot read: ask for gzip where supported.
createrepo_c --help 2>&1 | grep -q -- '--general-compress-type' && ZCK="$ZCK --general-compress-type=gz"
EXCLUDES=(); while read -r _ name _; do EXCLUDES+=(--excludes "$name"); done < superseded.list
createrepo_c --database --compress-type=gz --checksum=sha256 --retain-old-md=0 ${ZCK} "${EXCLUDES[@]}" "$STABLE_DIR" >/dev/null
createrepo_c --database --compress-type=gz --checksum=sha256 --retain-old-md=0 ${ZCK} "$TESTING_DIR" >/dev/null

# Advisories (fix-forward): updateinfo.xml from the stable entries that carry one; yum updateinfo reads it.
jq -r -f pages/updateinfo.jq "$STABLE_JSON" > updateinfo.xml
if grep -q '<update ' updateinfo.xml; then
  modifyrepo_c --mdtype=updateinfo --compress-type=gz updateinfo.xml "$STABLE_DIR/repodata" >/dev/null
  echo "updateinfo: $(grep -c '<update ' updateinfo.xml) advisory(ies)"
fi
jq -n --slurpfile s "$STABLE_JSON" --arg c "${PROMOTIONS_COMMIT:-unknown}" --arg at "$NOW" \
  '{generation: $s[0].generation, latest: $s[0].latest, promotions_commit: $c, built_at: $at}' > _site/stable-generation.json
echo "site built: stable $(wc -l < stable.list) RPM(s), $(wc -l < superseded.list) superseded kept unlisted, testing $(wc -l < testing.list) RPM(s)"
