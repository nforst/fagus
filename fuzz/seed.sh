#!/usr/bin/env bash
# Fills fuzz/corpus/<target> with seeds: the small ones of fuzz/seeds and the documents, style
# sheets, fonts and images of the tests. Files already there stay; run it again at any time.
set -euo pipefail

fuzz="$(cd "$(dirname "$0")" && pwd)"
root="$(dirname "$fuzz")"

# seed <target> <largest size in bytes> <find arguments>...: copies the files found under the
# repository (outside fuzz/ and target/) into the corpus of the target.
seed() {
    local target="$1" size="$2"
    shift 2
    mkdir -p "$fuzz/corpus/$target"
    if [ -d "$fuzz/seeds/$target" ]; then
        cp "$fuzz/seeds/$target"/* "$fuzz/corpus/$target/"
    fi
    find "$root/tests" "$root/crates" -type f -size "-${size}c" \( "$@" \) -print0 |
        while IFS= read -r -d '' file; do
            # Named by the path, so files of the same name in different directories both stay.
            cp "$file" "$fuzz/corpus/$target/$(echo "${file#"$root"/}" | tr / _)"
        done
}

seed render 65536 -name '*.html' -o -name '*.xhtml'
# The same input as render, and so its seeds.
seed invariants 65536 -name '*.html' -o -name '*.xhtml'
cp "$fuzz/seeds/render"/* "$fuzz/corpus/invariants/"
seed css 262144 -name '*.css'
seed font 1048576 -name '*.ttf' -o -name '*.otf' -o -name '*.ttc' -o -name '*.woff' -o -name '*.woff2'
seed image 262144 -name '*.png' -o -name '*.jpg' -o -name '*.jpeg' -o -name '*.gif' -o -name '*.webp' \
    -o -name '*.bmp' -o -name '*.tif' -o -name '*.tiff' -o -name '*.avif'
seed avif 65536 -name '*.avif'
seed pdf 65536 -name '*.pdf'
seed svg 262144 -name '*.svg'
seed worker 1 -name '-'

for target in "$fuzz"/corpus/*; do
    echo "$(basename "$target"): $(find "$target" -type f | wc -l | tr -d ' ') seeds"
done
