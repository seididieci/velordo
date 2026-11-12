#!/usr/bin/env bash
# Test TLS 1.3 (N6.2, ADR-0072): **fuori dal gate** (dipendenza host: `passt`
# per la rete e Python `ssl` come server TLS).
#
# Vittoria: un handshake TLS 1.3 **reale** dal guest (`testland/tlsprobe`)
# verso un server sull'host, con catena e hostname verificati (CA di test in
# `sys:ca.der`, non `NoVerify`) e un record applicativo in chiaro dopo
# l'handshake (GET HTTP). Prova cio' che serve a N6.3 (DoT): trasporto TCP
# dell'endpoint `net` + `libs/tls`.
#
# La sandbox non ha il binario `openssl`: i certificati li genera il tool host
# `tools/tls-test-certs` (puro Rust, RSA-2048) e il server usa Python `ssl`
# (OpenSSL 3.x, TLS 1.3, ALPN `dot`).
#
# Uso: ./scripts/run-tls-tests.sh
set -euo pipefail
cd "$(dirname "$0")/.."
ROOT="$PWD"

LOG="${TLS_LOG:-/tmp/tls-tests.log}"
# La suite completa gira prima del probe (testfs..dns): su host lenti il
# probe TLS parte dopo ~100 s. Il default copre build + suite + probe.
TIMEOUT="${TLS_TIMEOUT:-300}"
PORT="${TLS_TEST_PORT:-8443}"
SNI="${TLS_TEST_SNI:-tls.test}"

WORK="$(mktemp -d "${TMPDIR:-/tmp}/velordo-tls-XXXXXX")"
TOOL="$ROOT/build-meta/tls-test-certs"
SRV_PY="$WORK/tls-server.py"
SRV_LOG="$WORK/server.log"
SRV_PID=""

cleanup() {
    if [ -n "$SRV_PID" ]; then
        kill "$SRV_PID" 2>/dev/null || true
        wait "$SRV_PID" 2>/dev/null || true
    fi
    rm -rf "$WORK"
}
trap cleanup EXIT

# ── Certificati di test (CA + leaf RSA-2048, SAN DNS:$SNI) ──────────────
# Il tool e' host (std) ma la config del repo forza il target freestanding:
# si compila da una directory neutra con `--manifest-path` (come arca-tool).
mkdir -p "$ROOT/build-meta"
( cd "${TMPDIR:-/tmp}" && cargo build --release --manifest-path "$ROOT/tools/tls-test-certs/Cargo.toml" )
cp "$ROOT/tools/tls-test-certs/target/release/tls-test-certs" "$TOOL"
"$TOOL" "$WORK" "$SNI"

# ── Server TLS dell'host (TLS 1.3, ALPN dot, risposta HTTP) ─────────────
cat > "$SRV_PY" <<'PY'
import socket, ssl, sys

cert, key, port = sys.argv[1], sys.argv[2], int(sys.argv[3])
ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
ctx.minimum_version = ssl.TLSVersion.TLSv1_3
ctx.maximum_version = ssl.TLSVersion.TLSv1_3
ctx.set_alpn_protocols(["dot"])
ctx.load_cert_chain(cert, key)

srv = socket.socket(socket.AF_INET6, socket.SOCK_STREAM)
srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
srv.bind(("::", port))
srv.listen(4)
print("[tls-host] in ascolto su [::]:%d" % port, flush=True)

while True:
    c, peer = srv.accept()
    try:
        s = ctx.wrap_socket(c, server_side=True)
        print("[tls-host] handshake da %r alpn=%r" % (peer, s.selected_alpn_protocol()), flush=True)
        data = s.recv(4096)
        print("[tls-host] richiesta: %r" % (data[:32],), flush=True)
        body = b"hello from tls.test\n"
        s.sendall(
            b"HTTP/1.0 200 OK\r\nContent-Type: text/plain\r\nContent-Length: %d\r\n\r\n"
            % len(body)
            + body
        )
        s.close()
    except Exception as e:  # un client che sbaglia non deve fermare il server
        print("[tls-host] errore: %r" % (e,), flush=True)
PY

python3 "$SRV_PY" "$WORK/server.pem" "$WORK/server.key" "$PORT" >"$SRV_LOG" 2>&1 &
SRV_PID=$!
sleep 0.5
if ! kill -0 "$SRV_PID" 2>/dev/null; then
    echo "[tls-tests] FAIL: server TLS non partito su [::]:$PORT"
    cat "$SRV_LOG"
    exit 1
fi
echo "[tls-tests] server TLS su [::]:$PORT (sni $SNI, CA $WORK/ca.der)"

# Il seed di `sys:tls.conf` + `sys:ca.der` e' gated su TLS_TEST_CA (il gate
# resta identico: senza, `tlsprobe` si salta da solo).
export TLS_TEST_CA="$WORK/ca.der"
export TLS_TEST_SNI="$SNI"
export TLS_TEST_PORT="$PORT"

echo "[tls-tests] boot con virtio-net + passt (timeout ${TIMEOUT}s)"
TLS=1 RUN_TESTS=1 RUN_DISPLAY=none timeout "$TIMEOUT" ./run.sh >"$LOG" 2>&1 || true

fail=0
rg -q '\[tlsprobe\] handshake: PASS' "$LOG" || { echo "[tls-tests] manca: handshake TLS 1.3"; fail=1; }
rg -q '\[tlsprobe\] appdata: PASS' "$LOG" || { echo "[tls-tests] manca: record applicativo (HTTP)"; fail=1; }
rg -q '\[tlsprobe\] risultato: PASS' "$LOG" || { echo "[tls-tests] manca: risultato del probe"; fail=1; }
# Un probe saltato "passa" per convenzione: qui non e' una vittoria.
if rg -q '\[tlsprobe\] SKIP' "$LOG"; then
    echo "[tls-tests] FAIL: probe saltato (config non seedata?)"
    fail=1
fi
# Prova lato server: il guest ha davvero parlato TLS 1.3 con ALPN dot.
rg -q "alpn='dot'" "$SRV_LOG" || { echo "[tls-tests] manca: handshake lato server con ALPN dot"; fail=1; }

LOGRE='\[tlsprobe\]|\[tls-host\]|\[net\]|\[dns\]'
if [ "$fail" = "0" ]; then
    echo "[tls-tests] PASS: handshake TLS 1.3 reale + catena/hostname verificati + record applicativo"
    rg "$LOGRE" "$LOG" | tail -20
    rg '\[tls-host\]' "$SRV_LOG" | tail -5
    exit 0
fi

echo "[tls-tests] FAIL"
rg "$LOGRE" "$LOG" | tail -40 || true
echo "── log server ──"
tail -20 "$SRV_LOG" || true
exit 1
