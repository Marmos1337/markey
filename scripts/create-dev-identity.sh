#!/usr/bin/env bash
# Create a stable self-signed code-signing identity, "MarKey Dev", in the
# login keychain of the build Mac.
#
# Why: macOS ties the Accessibility grant to the app's code signature. An
# ad-hoc signature ("-") changes with every build, so after each rebuild the
# switch in System Settings stays on but MarKey silently loses access. A
# certificate that stays the same across builds keeps the grant.
#
# This is not a Developer ID: Gatekeeper still warns on other Macs and the
# app is not notarized. It only gives the build Mac a constant identity.
#
# Run once, by hand, on the build Mac:
#   ./scripts/create-dev-identity.sh
# then build with it:
#   APPLE_SIGNING_IDENTITY="MarKey Dev" cargo tauri build
#
# The private key is generated locally, imported into the login keychain
# (usable by codesign only) and the temporary files are deleted. macOS asks
# for the login password once to trust the certificate for code signing.
set -euo pipefail

NAME="${1:-MarKey Dev}"
KEYCHAIN="${KEYCHAIN:-$HOME/Library/Keychains/login.keychain-db}"
DAYS=3650

if security find-identity -v -p codesigning "$KEYCHAIN" 2>/dev/null | grep -q "\"$NAME\""; then
  echo "\"$NAME\" already exists in $KEYCHAIN; nothing to do."
  security find-identity -v -p codesigning "$KEYCHAIN" | grep "\"$NAME\""
  exit 0
fi

WORK="$(mktemp -d)"
cleanup() { rm -rf "$WORK"; }
trap cleanup EXIT
chmod 700 "$WORK"

cat > "$WORK/cert.cnf" <<CNF
[ req ]
distinguished_name = dn
prompt = no
x509_extensions = ext
[ dn ]
CN = $NAME
[ ext ]
basicConstraints = critical, CA:false
keyUsage = critical, digitalSignature
extendedKeyUsage = critical, codeSigning
subjectKeyIdentifier = hash
CNF

openssl req -x509 -newkey rsa:3072 -sha256 -nodes -days "$DAYS" \
  -keyout "$WORK/key.pem" -out "$WORK/cert.pem" -config "$WORK/cert.cnf" 2>/dev/null

# A random one-time password only protects the .p12 on its way into the
# keychain; the file is deleted right after.
P12_PASS="$(openssl rand -hex 24)"
openssl pkcs12 -export -legacy -inkey "$WORK/key.pem" -in "$WORK/cert.pem" \
  -name "$NAME" -out "$WORK/identity.p12" -passout "pass:$P12_PASS" 2>/dev/null \
  || openssl pkcs12 -export -inkey "$WORK/key.pem" -in "$WORK/cert.pem" \
       -name "$NAME" -out "$WORK/identity.p12" -passout "pass:$P12_PASS"

security import "$WORK/identity.p12" -k "$KEYCHAIN" -P "$P12_PASS" -T /usr/bin/codesign
security add-trusted-cert -r trustRoot -p codeSign -k "$KEYCHAIN" "$WORK/cert.pem"

echo
security find-identity -v -p codesigning "$KEYCHAIN" | grep "\"$NAME\"" \
  || { echo "The identity was imported but is not valid for code signing." >&2; exit 1; }
echo
echo "Done. Build with:  APPLE_SIGNING_IDENTITY=\"$NAME\" cargo tauri build"
echo "After the first signed build, grant Accessibility once more; later builds keep it."
