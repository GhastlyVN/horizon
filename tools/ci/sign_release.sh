#!/usr/bin/env bash
# Signs a release (docs/release-flow.md, "Signed checksums"): writes SHA256SUMS for the files given, then
# SHA256SUMS.sig beside them, one base64 Ed25519 signature per line over "horizon release <tag>\n" + SHA256SUMS,
# for every key set in RELEASE_SIGNING_KEY and RELEASE_SIGNING_KEY_NEXT (base64 PKCS#8, the `release`
# environment's secrets). The job that holds the private key runs only the runner's sha256sum and OpenSSL.
# For a Nightly, NIGHTLY_VERSION additionally writes SHA256SUMS.v2.sig over the same checksums, bound to
# that exact `nightly-<date>-<commit>` version so an older build cannot be replayed as a newer one.
# EPHEMERAL=yes (dry runs) signs with a throwaway key instead and writes its public key to ephemeral.pub.
#   tools/ci/sign_release.sh v2026.10.19 dist/horizon-linux-x86_64.tar.gz ...
set -euo pipefail
tag=$1; shift
dir=$(dirname "$1")
openssl=${OPENSSL:-openssl}
if command -v sha256sum > /dev/null; then sha=(sha256sum); else sha=(shasum -a 256); fi
umask 077
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

names=()
for file in "$@"; do
  [[ "$(dirname "$file")" == "$dir" ]] || { echo "$file: every file must be in $dir" >&2; exit 1; }
  names+=("$(basename "$file")")
done
(cd "$dir" && "${sha[@]}" -- "${names[@]}") > "$dir/SHA256SUMS"

nightly_version=${NIGHTLY_VERSION:-}
if [[ -n "$nightly_version" ]]; then
  [[ "$tag" == nightly || "$tag" == elyby-nightly ]] || {
    echo "NIGHTLY_VERSION is only valid when signing a Nightly channel" >&2; exit 1;
  }
  [[ "$nightly_version" =~ ^nightly-[0-9]{4}-[0-9]{2}-[0-9]{2}-[0-9a-f]{7,40}$ ]] || {
    echo "NIGHTLY_VERSION must be nightly-<date>-<commit>" >&2; exit 1;
  }
fi

sign_for() { # tag and output file
  local signed_tag=$1 output=$2
  { printf 'horizon release %s\n' "$signed_tag"; cat "$dir/SHA256SUMS"; } > "$work/message"
  rm -f "$dir/$output"
  if [[ -n "${EPHEMERAL:-}" ]]; then
    if [[ ! -f "$work/ephemeral.key" ]]; then
      "$openssl" genpkey -algorithm ed25519 -outform DER -out "$work/ephemeral.key"
      "$openssl" pkey -inform DER -in "$work/ephemeral.key" -pubout -outform DER | tail -c 32 | base64 > "$dir/ephemeral.pub"
    fi
    "$openssl" pkeyutl -sign -rawin -inkey "$work/ephemeral.key" -keyform DER \
      -in "$work/message" -out "$work/signature"
    base64 < "$work/signature" | tr -d '\n' >> "$dir/$output"
    echo >> "$dir/$output"
    rm -f "$work/signature"
  else
    for name in RELEASE_SIGNING_KEY RELEASE_SIGNING_KEY_NEXT; do
      if [[ -n "${!name:-}" ]]; then
        # From the environment into a private temporary file: never an argument, never printed.
        printf '%s' "${!name}" | base64 --decode > "$work/key"
        "$openssl" pkeyutl -sign -rawin -inkey "$work/key" -keyform DER \
          -in "$work/message" -out "$work/signature"
        base64 < "$work/signature" | tr -d '\n' >> "$dir/$output"
        echo >> "$dir/$output"
        rm -f "$work/key" "$work/signature"
      fi
    done
  fi
}

sign_for "$tag" SHA256SUMS.sig
if [[ -n "$nightly_version" ]]; then
  sign_for "$nightly_version" SHA256SUMS.v2.sig
fi
echo "$tag: SHA256SUMS lists ${#names[@]} files; $( [[ -s "$dir/SHA256SUMS.sig" ]] && echo "signed with $(wc -l < "$dir/SHA256SUMS.sig" | tr -d ' ') key(s)" || echo "no signing key, so unsigned")"
if [[ -n "$nightly_version" ]]; then
  echo "$nightly_version: wrote SHA256SUMS.v2.sig"
fi
