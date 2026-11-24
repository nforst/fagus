#!/usr/bin/env bash
# Renders the corpus plain, encrypted, as PDF/A (1b, 2a, 2b, 2u, 3a, 3b, 3u, 4, 4e, 4f), as PDF/UA-1
# (also encrypted, once without and once with a user password) and as Factur-X invoices, the invoices also signed (with the test keys of tests/signing), the pages with raster images
# also with their images as JPEGs (--image-quality 80: plain, as PDF/A-1b, 2b and 3u and as
# PDF/UA-1), and checks
# the files: every one with `qpdf --check` (https://qpdf.sourceforge.io), the signatures with
# `openssl cms -verify` (tests/signing/verify.py),
# the PDF/A and PDF/UA files with veraPDF (https://verapdf.org) against the profile of the level
# they were written for, and the invoices with Mustang (https://www.mustangproject.org). veraPDF
# reads encrypted files only without a user password: the PDF/UA files with one are checked
# decrypted by qpdf, after qpdf has shown that they allow the extraction for accessibility.
#
# Usage: tools/validate.sh [--subset] OUT_DIR
#
#   --subset   only tests/corpus/*.html, the invoices and the Quittio documents (about 20 pages
#              instead of 240), for a quick check
#
# The tools are downloaded at the versions below into FAGUS_TOOLS (default target/validate-tools)
# and checked against their SHA-256, unless VERAPDF (the verapdf script), MUSTANG (the Mustang CLI
# jar) or QPDF (the qpdf binary) name others. veraPDF and Mustang need Java 11 or later.
# FAGUS names the binary (default target/release/fagus), JOBS the number of renders at once.
# FAGUS_OPTIONS holds further options for every render, separated by spaces, such as
# "--page-mode attachments --zoom fit-width" to check the files with an initial view.
#
# Pages that Fagus refuses for a level (images without alternative text, headings that do not
# start at h1, characters that the fonts in tests/fonts lack) are left out; the message says why.
# Any other failure to write a file fails the check. OUT_DIR/pdf is emptied first.
set -euo pipefail

verapdf_version=1.30.4
verapdf_sha256=04415ef3f149526a3048b9889723e81320ddf013bd891bcc7f0f297f3eadcc23
mustang_version=2.26.0
mustang_sha256=42d7868cb68264874a7b8cab4c3587b03b23ccc7cd72373da917f66758bb9736
qpdf_version=12.4.2

root=$(cd "$(dirname "$0")/.." && pwd)
subset=0
if [[ ${1:-} == --subset ]]; then
  subset=1
  shift
fi
out=${1:?usage: tools/validate.sh [--subset] OUT_DIR}
fagus=${FAGUS:-$root/target/release/fagus}
tools=${FAGUS_TOOLS:-$root/target/validate-tools}
jobs=${JOBS:-$(getconf _NPROCESSORS_ONLN 2>/dev/null || echo 2)}
fonts=$root/tests/fonts
xml=$root/tests/corpus/invoice/factur-x.xml
mkdir -p "$out" "$tools"
out=$(cd "$out" && pwd)
tools=$(cd "$tools" && pwd)

sha256() {
  if command -v sha256sum > /dev/null; then sha256sum "$1"; else shasum -a 256 "$1"; fi | cut -d' ' -f1
}

# Downloads URL to FILE unless it is there, and fails unless its SHA-256 is SUM.
fetch() { # url sum file
  if [[ ! -f $3 ]]; then
    echo "downloading $1"
    curl --fail --silent --show-error --location --retry 3 -o "$3.part" "$1"
    mv "$3.part" "$3"
  fi
  local actual
  actual=$(sha256 "$3")
  if [[ $actual != "$2" ]]; then
    echo "$3: SHA-256 $actual, expected $2" >&2
    rm -f "$3"
    exit 1
  fi
}

if [[ -z ${VERAPDF:-} ]]; then
  VERAPDF=$tools/verapdf-$verapdf_version/verapdf
  if [[ ! -x $VERAPDF ]]; then
    zip=$tools/verapdf-greenfield-$verapdf_version-installer.zip
    fetch "https://software.verapdf.org/releases/${verapdf_version%.*}/verapdf-greenfield-$verapdf_version-installer.zip" \
      "$verapdf_sha256" "$zip"
    rm -rf "$tools/verapdf-installer"
    unzip -q "$zip" -d "$tools/verapdf-installer"
    # The installer's answers for an installation without questions.
    cat > "$tools/verapdf-installer/auto-install.xml" <<EOF
<?xml version="1.0" encoding="UTF-8" standalone="no"?>
<AutomatedInstallation langpack="eng">
  <com.izforge.izpack.panels.htmlhello.HTMLHelloPanel id="welcome"/>
  <com.izforge.izpack.panels.target.TargetPanel id="install_dir">
    <installpath>$tools/verapdf-$verapdf_version</installpath>
  </com.izforge.izpack.panels.target.TargetPanel>
  <com.izforge.izpack.panels.packs.PacksPanel id="sdk_pack_select">
    <pack index="0" name="veraPDF GUI" selected="true"/>
    <pack index="1" name="veraPDF Batch files" selected="true"/>
    <pack index="2" name="veraPDF Validation model" selected="false"/>
    <pack index="3" name="veraPDF Documentation" selected="false"/>
    <pack index="4" name="veraPDF Sample Plugins" selected="false"/>
  </com.izforge.izpack.panels.packs.PacksPanel>
  <com.izforge.izpack.panels.install.InstallPanel id="install"/>
  <com.izforge.izpack.panels.finish.FinishPanel id="finish"/>
</AutomatedInstallation>
EOF
    java -jar "$tools/verapdf-installer/verapdf-greenfield-$verapdf_version/verapdf-izpack-installer-$verapdf_version.jar" \
      "$tools/verapdf-installer/auto-install.xml" > /dev/null
    rm -rf "$tools/verapdf-installer" "$zip"
  fi
fi
if [[ -z ${MUSTANG:-} ]]; then
  MUSTANG=$tools/Mustang-CLI-$mustang_version.jar
  fetch "https://repo1.maven.org/maven2/org/mustangproject/Mustang-CLI/$mustang_version/Mustang-CLI-$mustang_version.jar" \
    "$mustang_sha256" "$MUSTANG"
fi
if [[ -z ${QPDF:-} ]]; then
  case "$(uname -s)-$(uname -m)" in
    Linux-x86_64) platform=linux-x86_64 sum=db367d897829f22c4198ce1094143c9d467bd6ee7dfabc44ba6f02056b24f8b1 ;;
    Linux-aarch64 | Linux-arm64) platform=linux-aarch64 sum=8fd9d009eb0838398180a603f2cf76531e5c68a4ecc8847598e0a1ccb1d3b600 ;;
    Darwin-arm64) platform=macos-arm64 sum=62e46987a30ea167cbc530ccb22690aec3d8c812ed09979a941bcee92e504b79 ;;
    Darwin-x86_64) platform=macos-x86_64 sum=dd3b01f4414d198529bb0f13bba59c2f016a328bb3506695087a96fb80fc1481 ;;
    *) echo "no qpdf $qpdf_version for $(uname -sm); set QPDF to a qpdf binary" >&2; exit 1 ;;
  esac
  QPDF=$tools/qpdf-$qpdf_version-$platform/bin/qpdf
  if [[ ! -x $QPDF ]]; then
    zip=$tools/qpdf-$qpdf_version-bin-$platform.zip
    fetch "https://github.com/qpdf/qpdf/releases/download/v$qpdf_version/qpdf-$qpdf_version-bin-$platform.zip" \
      "$sum" "$zip"
    unzip -q -o "$zip" -d "$tools/qpdf-$qpdf_version-$platform"
    rm "$zip"
  fi
fi
echo "$("$VERAPDF" --version | head -1), Mustang $(basename "$MUSTANG" .jar | sed 's/^Mustang-CLI-//'), $("$QPDF" --version | head -1)"

# The renders, one per line: the variant, the name of the file and the page.
pdfs=$out/pdf
rm -rf "$pdfs"
mkdir -p "$pdfs"
if [[ $subset == 1 ]]; then
  pages=("$root"/tests/corpus/*.html "$root"/tests/corpus/invoice/*.html "$root"/tests/corpus/quittio/*.html)
else
  pages=("$root"/tests/corpus/*.html "$root"/tests/corpus/*/*.html)
fi
for page in "${pages[@]}"; do
  name=$(basename "$(dirname "$page")")-$(basename "$page" .html)
  for variant in plain encrypted 1b 2a 2b 2u 3a 3b 3u 4 4e 4f ua ua-encrypted ua-password; do
    echo "$variant $name $page"
  done
  if grep -qiE '\.(png|jpe?g|gif|webp|avif|bmp|tiff?)([^a-z]|$)|data:image/' "$page"; then
    for variant in q80 1b-q80 2b-q80 3u-q80 ua-q80; do
      echo "$variant $name $page"
    done
  fi
done > "$out/renders.txt"
for page in "$root"/tests/corpus/invoice/*.html; do
  name=factur-x-$(basename "$page" .html)
  echo "factur-x $name $page"
  echo "factur-x-ua $name $page"
  echo "factur-x-signed $name $page"
  echo "factur-x-ua-signed $name $page"
  echo "1b-signed $name $page"
done >> "$out/renders.txt"

# shellcheck disable=SC2329 # run by xargs
render() { # variant name page
  local options=()
  case $1 in
    plain) ;;
    encrypted) options=(--user-password user --owner-password owner --permissions print,copy) ;;
    ua) options=(--accessible) ;;
    q80) options=(--image-quality 80) ;;
    ua-q80) options=(--accessible --image-quality 80) ;;
    *-q80) options=(--pdfa "${1%-q80}" --image-quality 80) ;;
    ua-encrypted) options=(--accessible --owner-password owner --permissions print) ;;
    ua-password) options=(--accessible --user-password user --owner-password owner --permissions print,copy) ;;
    factur-x) options=(--pdfa 3b --factur-x "$xml") ;;
    factur-x-ua) options=(--pdfa 3a --accessible --factur-x "$xml") ;;
    factur-x-signed) options=(--pdfa 3b --factur-x "$xml" --sign-p12 "$root/tests/signing/rsa.p12" --sign-reason Rechnung) ;;
    factur-x-ua-signed) options=(--pdfa 3a --accessible --factur-x "$xml" --sign-p12 "$root/tests/signing/ecdsa.p12") ;;
    1b-signed) options=(--pdfa 1b --sign-p12 "$root/tests/signing/rsa.p12") ;;
    *) options=(--pdfa "$1") ;;
  esac
  # The root is tests/, so that pages may use the fonts and images next to the corpus.
  # shellcheck disable=SC2086 # FAGUS_OPTIONS is split into its options
  FAGUS_SIGN_PASSWORD=fagus-test-only "$fagus" render "$3" -o "$pdfs/$2.$1.pdf" --font-dir "$fonts" \
    --root "$root/tests" ${options[@]+"${options[@]}"} ${FAGUS_OPTIONS:-} 2> "$pdfs/$2.$1.log" || true
}
export -f render
export fagus fonts root xml pdfs FAGUS_OPTIONS=${FAGUS_OPTIONS:-}
xargs -P "$jobs" -L 1 bash -c 'render "$@"' _ < "$out/renders.txt"

# The files for each veraPDF profile, and the refusals.
failed=0
summary=()
: > "$out/refused.txt"
rm -f "$out"/files.*
while read -r variant name _; do
  pdf=$pdfs/$name.$variant.pdf
  if [[ ! -f $pdf ]]; then
    log=$pdfs/$name.$variant.log
    if [[ $variant != plain && $variant != encrypted && $variant != q80 ]] && grep -q '^Error: the document does not meet ' "$log"; then
      echo "refused as $variant: $name: $(grep -m 1 '^Error: ' "$log" | sed 's/^Error: the document does not meet //')" >> "$out/refused.txt"
    else
      echo "not written as $variant: $name:"
      tail -3 "$log"
      failed=1
    fi
    continue
  fi
  echo "$pdf" >> "$out/files.all"
  case $variant in
    plain | encrypted | q80) ;;
    ua | ua-encrypted | ua-q80) echo "$pdf" >> "$out/files.ua1" ;;
    *-q80) echo "$pdf" >> "$out/files.${variant%-q80}" ;;
    ua-password)
      # veraPDF reads it decrypted, so qpdf checks the permission PDF/UA asks for (7.16).
      decrypted=${pdf%.pdf}.decrypted.pdf
      encryption=$("$QPDF" --password=user --show-encryption "$pdf" 2>&1 || true)
      if grep -q '^extract for accessibility: allowed$' <<< "$encryption" \
        && "$QPDF" --password=user --decrypt "$pdf" "$decrypted"; then
        echo "$decrypted" >> "$out/files.ua1"
      else
        echo "qpdf: $(basename "$pdf") does not allow the extraction for accessibility or cannot be decrypted"
        failed=1
      fi ;;
    factur-x) echo "$pdf" >> "$out/files.3b"; echo "$pdf" >> "$out/files.invoices" ;;
    factur-x-ua)
      echo "$pdf" >> "$out/files.3a"
      echo "$pdf" >> "$out/files.ua1"
      echo "$pdf" >> "$out/files.invoices" ;;
    factur-x-signed)
      echo "$pdf" >> "$out/files.3b"
      echo "$pdf" >> "$out/files.invoices"
      echo "$pdf" >> "$out/files.signed" ;;
    factur-x-ua-signed)
      echo "$pdf" >> "$out/files.3a"
      echo "$pdf" >> "$out/files.ua1"
      echo "$pdf" >> "$out/files.invoices"
      echo "$pdf" >> "$out/files.signed" ;;
    1b-signed) echo "$pdf" >> "$out/files.1b"; echo "$pdf" >> "$out/files.signed" ;;
    *) echo "$pdf" >> "$out/files.$variant" ;;
  esac
done < "$out/renders.txt"
refused=$(wc -l < "$out/refused.txt" | tr -d ' ')
echo "$refused renders refused by Fagus as not conforming, see $out/refused.txt"
summary+=("| Refused by Fagus as not conforming | $refused | | |")

# qpdf: the structure of every file (cross-reference table, streams, object streams), decrypted
# with the user password where it is encrypted (the owner password where there is no user
# password). Warnings (exit status 3) count as errors.
qpdf_failed=0
while read -r pdf; do
  password=user
  [[ $pdf == *.ua-encrypted.pdf ]] && password=owner
  if ! "$QPDF" --password="$password" --check "$pdf" > "${pdf%.pdf}.qpdf.txt" 2>&1; then
    echo "qpdf: $(basename "$pdf"):"
    grep -v '^checking\|^PDF Version\|^File is not encrypted\|^File is not linearized\|^R = \|^P = \|^User password = ' "${pdf%.pdf}.qpdf.txt" | head -5
    qpdf_failed=$((qpdf_failed + 1))
  fi
done < "$out/files.all"
total=$(wc -l < "$out/files.all" | tr -d ' ')
echo "qpdf --check: $((total - qpdf_failed)) of $total passed"
summary+=("| qpdf --check | $total | $((total - qpdf_failed)) | $qpdf_failed |")
[[ $qpdf_failed == 0 ]] || failed=1

# The signatures: the byte range covers the file, and OpenSSL accepts the CMS signature with the
# test CA.
signed_failed=0
count=0
while read -r pdf; do
  count=$((count + 1))
  if ! python3 "$root/tests/signing/verify.py" "$pdf" > "${pdf%.pdf}.signature.txt" 2>&1; then
    cat "${pdf%.pdf}.signature.txt"
    signed_failed=$((signed_failed + 1))
  fi
done < "$out/files.signed"
echo "openssl cms -verify: $((count - signed_failed)) of $count signatures passed"
summary+=("| openssl cms -verify (signatures) | $count | $((count - signed_failed)) | $signed_failed |")
[[ $signed_failed == 0 ]] || failed=1

# veraPDF, each set of files against the profile of its level, so that the identification in the
# metadata must match it too.
for flavour in 1b 2a 2b 2u 3a 3b 3u 4 4e 4f ua1; do
  label=$([[ $flavour == ua1 ]] && echo PDF/UA-1 || echo "PDF/A-$flavour")
  list=$out/files.$flavour
  if [[ ! -s $list ]]; then
    echo "veraPDF $label: no files"
    failed=1
    continue
  fi
  report=$out/verapdf-$flavour.txt
  files=()
  while read -r pdf; do files+=("$pdf"); done < "$list"
  "$VERAPDF" --format text -v -f "$flavour" --processes "$jobs" "${files[@]}" > "$report" 2> "$out/verapdf-$flavour.log" || true
  count=${#files[@]}
  passed=$(grep -c '^PASS ' "$report" || true)
  failures=$(grep -c '^FAIL ' "$report" || true)
  echo "veraPDF $label: $passed of $count passed"
  summary+=("| veraPDF $label | $count | $passed | $((count - passed)) |")
  if [[ $passed != "$count" ]]; then
    failed=1
    # The files that failed, each with the rules it broke.
    awk '/^FAIL /{show=1; print; next} /^[^ ]/{show=0} show' "$report"
    # A file that veraPDF could not read has neither PASS nor FAIL.
    if [[ $((passed + failures)) != "$count" ]]; then
      echo "veraPDF $label: $((count - passed - failures)) files without a result, see $out/verapdf-$flavour.log"
    fi
  fi
done

# Mustang: the invoice XML against the schema and the Schematron of its profile, and the PDF/A-3
# file around it.
mustang_failed=0
count=0
while read -r invoice; do
  count=$((count + 1))
  report=${invoice%.pdf}.mustang.xml
  java -jar "$MUSTANG" --disable-file-logging --action validate --source "$invoice" > "$report" 2> /dev/null || true
  if grep -q '<summary status="valid"/>' "$report" && ! grep -q '<summary status="invalid"/>' "$report"; then
    echo "Mustang: $(basename "$invoice") valid"
  else
    echo "Mustang: $(basename "$invoice") invalid, see $report"
    mustang_failed=$((mustang_failed + 1))
  fi
done < "$out/files.invoices"
summary+=("| Mustang (Factur-X) | $count | $((count - mustang_failed)) | $mustang_failed |")
[[ $mustang_failed == 0 ]] || failed=1

if [[ -n ${GITHUB_STEP_SUMMARY:-} ]]; then
  {
    echo "### PDF validation"
    echo
    echo "| Check | Files | Passed | Failed |"
    echo "|---|--:|--:|--:|"
    printf '%s\n' "${summary[@]}"
  } >> "$GITHUB_STEP_SUMMARY"
fi
exit $failed
