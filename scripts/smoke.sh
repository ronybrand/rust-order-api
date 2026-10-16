#!/usr/bin/env bash
# End-to-end smoke test of the compose stack: builds the images, starts PostgreSQL, RabbitMQ, Mailpit, the API and
# the worker, drives the API with real tokens, and checks that confirming an order sends an e-mail.
#
# Usage: bash scripts/smoke.sh          (needs docker, openssl, curl and jq)
set -euo pipefail

cd "$(dirname "$0")/.."
API=http://127.0.0.1:8080
MAILPIT=http://127.0.0.1:8025
AUDIENCE=order-api
ISSUER=https://idp.example

cleanup() {
  status=$?
  if [ "$status" -ne 0 ]; then
    echo "::group::compose logs"
    docker compose logs --no-color --tail=200 || true
    echo "::endgroup::"
  fi
  docker compose down -v --remove-orphans >/dev/null 2>&1 || true
  rm -rf keys
  exit "$status"
}
trap cleanup EXIT

step() { echo "==> $*"; }
fail() { echo "FAILED: $*" >&2; exit 1; }

b64url() { openssl base64 -A | tr '+/' '-_' | tr -d '='; }

# The service only verifies tokens, so the test plays the issuer: it makes a key pair and signs RS256 tokens.
step "key pair"
rm -rf keys && mkdir keys
openssl genrsa -out keys/jwt_private.pem 2048 2>/dev/null
openssl rsa -in keys/jwt_private.pem -pubout -out keys/jwt_public.pem 2>/dev/null

token() { # token <subject> <roles json array>
  local header payload signature unsigned
  header=$(printf '{"alg":"RS256","typ":"JWT"}' | b64url)
  payload=$(printf '{"sub":"%s","aud":"%s","iss":"%s","exp":%s,"roles":%s}' "$1" "$AUDIENCE" "$ISSUER" "$(($(date +%s) + 3600))" "$2" | b64url)
  unsigned="$header.$payload"
  signature=$(printf '%s' "$unsigned" | openssl dgst -sha256 -sign keys/jwt_private.pem | b64url)
  printf '%s.%s' "$unsigned" "$signature"
}
ADMIN=$(token admin-1 '["user","admin"]')
USER_TOKEN=$(token user-1 '["user"]')

step "build and start the stack"
docker compose up -d --build --wait --wait-timeout 420

# call <expected status> <method> <path> <token or -> [json body]; prints the response body.
call() {
  local expected=$1 method=$2 path=$3 bearer=$4 body=${5:-}
  local args=(-sS -o /tmp/smoke-body -w '%{http_code}' -X "$method" "$API$path")
  [ "$bearer" != "-" ] && args+=(-H "Authorization: Bearer $bearer")
  [ -n "$body" ] && args+=(-H 'Content-Type: application/json' -d "$body")
  local status
  status=$(curl "${args[@]}")
  [ "$status" = "$expected" ] || fail "$method $path answered $status, expected $expected: $(cat /tmp/smoke-body)"
  cat /tmp/smoke-body
}

step "health is public"
call 200 GET /health - | jq -e '.status == "ok"' >/dev/null

step "no token is 401, a plain user cannot create a customer (403)"
call 401 GET /orders/search - | jq -e '.code == "AUTH-01"' >/dev/null
call 403 POST /customers "$USER_TOKEN" '{"name":"Ada","tax_id":"SMOKE-0001","email":"ada@example.com"}' | jq -e '.code == "AUTH-02"' >/dev/null

step "an admin creates a customer, the response carries no personal data"
customer=$(call 201 POST /customers "$ADMIN" '{"name":"Ada Lovelace","tax_id":"SMOKE-0001","email":"ada@example.com"}')
echo "$customer" | jq -e 'keys == ["id","name"]' >/dev/null
customer_id=$(echo "$customer" | jq -r .id)

step "a duplicate tax id is a 409"
call 409 POST /customers "$ADMIN" '{"name":"Other","tax_id":"smoke0001","email":"other@example.com"}' | jq -e '.code == "CONFLICT-01"' >/dev/null

step "a user opens an order and the total is exact"
order=$(call 201 POST /orders "$USER_TOKEN" "{\"customer_id\":\"$customer_id\",\"items\":[{\"description\":\"Pen\",\"unit_price\":0.10,\"quantity\":3},{\"description\":\"Book\",\"unit_price\":19.99,\"quantity\":2}]}")
order_id=$(echo "$order" | jq -r .id)
echo "$order" | jq -e '.total == 40.28 and .status == "OPEN"' >/dev/null

step "confirming the order changes its status"
call 200 POST "/orders/$order_id/confirm" "$USER_TOKEN" | jq -e '.status == "CONFIRMED"' >/dev/null

step "the worker sends the e-mail (the order status change went through RabbitMQ to Mailpit)"
found=""
for _ in $(seq 1 40); do
  found=$(curl -sS "$MAILPIT/api/v1/messages" | jq -r '[.messages[]? | select(.Subject == "Order CONFIRMED")] | length')
  [ "${found:-0}" -ge 1 ] && break
  sleep 1
done
[ "${found:-0}" -ge 1 ] || fail "no 'Order CONFIRMED' e-mail arrived in Mailpit within 40 s"
curl -sS "$MAILPIT/api/v1/messages" | jq -e '.messages[] | select(.Subject == "Order CONFIRMED") | .To[0].Address == "ada@example.com"' >/dev/null

step "a customer with an order cannot be deleted, then can once the order is gone"
call 409 DELETE "/customers/$customer_id" "$ADMIN" | jq -e '.code == "CONFLICT-03"' >/dev/null
call 204 DELETE "/orders/$order_id" "$USER_TOKEN" >/dev/null
call 204 DELETE "/customers/$customer_id" "$ADMIN" >/dev/null

echo "smoke test passed"
