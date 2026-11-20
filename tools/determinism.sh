#!/usr/bin/env bash
# Renders every page of the corpus twice with the same creation date, plain, as PDF/UA-1 and as
# PDF/A-3u (the invoices also as PDF/A-3a, PDF/UA-1 and Factur-X), once in a new engine and once
# in a warm one, and fails unless both renders give the same bytes (or Fagus refuses both). Writes the SHA-256 of each file to OUT_DIR/SHA256SUMS, so that the files of different
# machines can be compared:
#
#   tools/determinism.sh OUT_DIR
#   diff linux-x64/SHA256SUMS linux-arm64/SHA256SUMS
#
# FAGUS names the binary (default target/release/fagus), JOBS the number of renders at once.
# OUT_DIR/pdf is emptied first.
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
out=${1:?usage: tools/determinism.sh OUT_DIR}
fagus=${FAGUS:-$root/target/release/fagus}
jobs=${JOBS:-$(getconf _NPROCESSORS_ONLN 2>/dev/null || echo 2)}
fonts=$root/tests/fonts
xml=$root/tests/corpus/invoice/factur-x.xml
created=2026-01-01T12:00:00Z
mkdir -p "$out"
out=$(cd "$out" && pwd)
pdfs=$out/pdf
rm -rf "$pdfs"
mkdir -p "$pdfs"

# In the format of sha256sum ("<sum>  <file>"), also with FreeBSD's sha256.
sha256() {
  if command -v sha256sum > /dev/null; then
    sha256sum "$@"
  elif command -v shasum > /dev/null; then
    shasum -a 256 "$@"
  else
    sha256 -r "$@" | sed 's/ /  /'
  fi
}

for page in "$root"/tests/corpus/*.html "$root"/tests/corpus/*/*.html; do
  name=$(basename "$(dirname "$page")")-$(basename "$page" .html)
  echo "plain $name $page"
  echo "ua $name $page"
  echo "3u $name $page"
done > "$out/renders.txt"
for page in "$root"/tests/corpus/invoice/*.html; do
  echo "factur-x-ua $(basename "$page" .html) $page"
done >> "$out/renders.txt"

# shellcheck disable=SC2329 # run by xargs
render() { # variant name page
  local options=()
  case $1 in
    plain) ;;
    ua) options=(--accessible) ;;
    factur-x-ua) options=(--pdfa 3a --accessible --factur-x "$xml") ;;
    *) options=(--pdfa "$1") ;;
  esac
  # The second run renders the page three times in one engine and writes the last, so that the
  # caches of a warm engine (as in a worker) must not change the file either.
  for run in 1 2; do
    "$fagus" render "$3" -o "$pdfs/$2.$1.$run.pdf" --font-dir "$fonts" --root "$root/tests" \
      --created "$created" --repeat $((run == 1 ? 1 : 3)) ${options[@]+"${options[@]}"} 2> "$pdfs/$2.$1.$run.log" || true
  done
}
export -f render
export fagus fonts root xml pdfs created
xargs -P "$jobs" -L 1 bash -c 'render "$@"' _ < "$out/renders.txt"

failed=0
count=0
refused=0
while read -r variant name _; do
  first=$pdfs/$name.$variant.1.pdf
  second=$pdfs/$name.$variant.2.pdf
  # Pages that Fagus refuses as not conforming (characters the fonts lack, images without
  # alternative text) have no file, the same on every platform; any other failure is an error.
  if [[ ! -f $first && ! -f $second ]] && grep -q '^Error: the document does not meet ' "$pdfs/$name.$variant.1.log"; then
    refused=$((refused + 1))
    continue
  fi
  if [[ ! -f $first || ! -f $second ]]; then
    echo "not rendered: $name.$variant"
    cat "$pdfs/$name.$variant.1.log" "$pdfs/$name.$variant.2.log"
    failed=1
    continue
  fi
  count=$((count + 1))
  if ! cmp -s "$first" "$second"; then
    echo "different bytes in two renders: $name.$variant"
    failed=1
  fi
  mv "$first" "$pdfs/$name.$variant.pdf"
  rm -f "$second"
done < "$out/renders.txt"
rm -f "$pdfs"/*.log
(cd "$pdfs" && sha256 ./*.pdf | sed 's| \./| |') | sort -k2 > "$out/SHA256SUMS"
echo "$count files the same in both renders, $refused refused as not conforming; sums in $out/SHA256SUMS"
exit $failed
