#!/bin/bash
# certbot --manual-auth-hook: records the TXT certbot needs, then polls public
# DNS until that exact value is live, then returns 0 so certbot validates.
# Lets a human add the TXT in Namecheap while certbot waits (no tmux, no keypress).
echo "_acme-challenge.${CERTBOT_DOMAIN} = ${CERTBOT_VALIDATION}" >> /tmp/sigil-acme-needed.txt
for i in $(seq 1 150); do
  if dig +short TXT "_acme-challenge.${CERTBOT_DOMAIN}" @1.1.1.1 2>/dev/null | tr -d '"' | grep -qF "${CERTBOT_VALIDATION}"; then
    exit 0
  fi
  sleep 10
done
exit 1
