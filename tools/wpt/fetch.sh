#!/usr/bin/env bash
# Checks out web-platform-tests at the commit below, with only the directories of
# tools/wpt/dirs.txt and the support files their tests use (a sparse, shallow checkout: about
# 250 MB instead of several GB). Nothing of it goes into this repository.
#
#   tools/wpt/fetch.sh [CHECKOUT]     # default target/wpt/checkout
#
# Run it again after changing the commit or the directories; it fetches only what is missing.
# To move to a newer commit of web-platform-tests, change WPT_COMMIT, run
# `tools/wpt/run.sh --update --docs docs/compatibility.md` and commit the expectations and the
# table with it.
set -euo pipefail

WPT_COMMIT=ffa9ce07f3641e22cc8156d8006e2584cf036cf5
WPT_REPOSITORY=${WPT_REPOSITORY:-https://github.com/web-platform-tests/wpt.git}

root=$(cd "$(dirname "$0")/../.." && pwd)
checkout=${1:-$root/target/wpt/checkout}

# The directories with tests, and what their tests and references load: shared references and
# support files, fonts (Ahem) and images.
directories=()
while IFS= read -r line; do
  [[ -z $line || $line == \#* ]] || directories+=("$line")
done < "$root/tools/wpt/dirs.txt"
directories+=(common css/reference css/support css/CSS2/reference css/CSS2/support css/CSS2/positioning
  fonts images quirks/reference resources)

# A new, empty checkout.
fresh() {
  rm -rf "$checkout"
  mkdir -p "$checkout"
  git -C "$checkout" init -q
  git -C "$checkout" remote add origin "$WPT_REPOSITORY"
}

[[ -d $checkout/.git ]] || fresh
for attempt in 1 2 3; do
  git -C "$checkout" sparse-checkout set --cone "${directories[@]}"
  if [[ $(git -C "$checkout" rev-parse -q --verify HEAD 2> /dev/null) == "$WPT_COMMIT" ]]; then
    break
  fi
  # Only the commit, and the contents of files only as the sparse checkout needs them.
  if git -C "$checkout" fetch -q --depth 1 --filter=blob:none origin "$WPT_COMMIT" \
    && git -C "$checkout" -c advice.detachedHead=false checkout -q --detach "$WPT_COMMIT"; then
    break
  fi
  [[ $attempt != 3 ]] || exit 1
  # A checkout left from before (such as one a cache of target/ brought back) can make GitHub
  # refuse the fetch ("not our ref"); a new one fetches, and GitHub refuses one now and then.
  echo "fetching web-platform-tests failed; again from a new checkout in $((attempt * 20)) seconds" >&2
  sleep $((attempt * 20))
  fresh
done
echo "web-platform-tests $WPT_COMMIT in $checkout"
