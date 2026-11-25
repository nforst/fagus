#!/usr/bin/env bash
# Fuzzes one target for a while: ./run.sh <target> [seconds] [libFuzzer options...]
#
# Seeds the corpus (seed.sh), then runs `cargo +nightly fuzz run` with the target's limits. New
# inputs go to corpus/<target>, crashes to artifacts/<target>. The limits leave room for
# documents that are heavy but legitimate: the engine stops at 10,000 pages and 200 megapixels
# per image, which takes seconds, so only a minute without an answer counts as a hang.
#
# Targets are built without a sanitizer but with overflow checks and debug assertions: the
# engine has no unsafe code, and AddressSanitizer makes rendering (drawing filters, decoding
# images) five to ten times slower. FUZZ_SANITIZER=address turns it on, for the unsafe code in
# dependencies.
set -euo pipefail

cd "$(dirname "$0")"
target="${1:?usage: run.sh <render|invariants|css|font|image|avif|pdf|svg|worker> [seconds] [options...]}"
seconds="${2:-600}"
shift $(($# < 2 ? $# : 2))

case "$target" in
    render) options=(-max_len=16384 -dict=dicts/html.dict) ;;
    invariants) options=(-max_len=16384 -dict=dicts/html.dict) ;;
    css) options=(-max_len=4096 -dict=dicts/css.dict) ;;
    font) options=(-max_len=524288) ;;
    image) options=(-max_len=262144) ;;
    avif) options=(-max_len=65536 -dict=dicts/avif.dict) ;;
    pdf) options=(-max_len=65536 -dict=dicts/pdf.dict) ;;
    svg) options=(-max_len=32768 -dict=dicts/html.dict) ;;
    worker) options=(-max_len=16384) ;;
    *)
        echo "unknown target: $target" >&2
        exit 2
        ;;
esac

./seed.sh > /dev/null
# cargo-fuzz builds for the platform it was itself built for unless told otherwise; prebuilt
# binaries of cargo-fuzz (as in CI) are built for musl, whose standard library nightly lacks here.
host=$(rustc +nightly -vV | sed -n 's/^host: //p')
exec cargo +nightly fuzz run --target "$host" --sanitizer "${FUZZ_SANITIZER:-none}" "$target" "corpus/$target" -- \
    -max_total_time="$seconds" -timeout=60 -rss_limit_mb=4096 -print_final_stats=1 \
    "${options[@]}" "$@"
