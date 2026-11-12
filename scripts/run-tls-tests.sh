#!/usr/bin/env bash
# Test TLS 1.3 + DoT (N6.2/N6.3, ADR-0072): **fuori dal gate** (dipendenze
# host: `passt` per la rete e Python `ssl` come server TLS).
#
# Un solo boot prova due cose, perché i server stanno sull'host e il guest li
# raggiunge entrambi dal gateway di passt:
#
# 1. `testland/tlsprobe` — handshake TLS 1.3 con catena **e** hostname
#    verificati (CA di test in `sys:ca.der`, non `NoVerify`) e un record
#    applicativo HTTP in chiaro dopo l'handshake: prova l'adattatore.
# 2. `testland/dnsprobe` — il resolver `dns` in `mode = dot` (una connessione
#    per query, framing RFC 7858): due query buone (la seconda prova il
#    **reconnect**) e una terza in cui il server presenta un certificato
#    fuori dalla trust anchor → **SERVFAIL** (fail-closed: mai risposta in
#    chiaro, mai NXDOMAIN). Nessun boot aggiuntivo: il responder sceglie il
#    certificato per numero di connessione.
#
# La sandbox non ha il binario `openssl`: i certificati li genera il tool host
# `tools/tls-test-certs` (puro Rust, RSA-2048, SAN multipli + un leaf
# self-signed "cattivo") e i server usano Python `ssl` (OpenSSL 3.x, TLS 1.3,
# ALPN `dot`).
#
# Uso: ./scripts/run-tls-tests.sh
set -euo pipefail
cd "$(dirname "$0")/.."
ROOT="$PWD"

LOG="${TLS_LOG:-/tmp/tls-tests.log}"
# La suite completa gira prima dei probe: su host lenti il probe TLS parte
# dopo ~180 s. Il default copre build + suite + probe + DoT.
TIMEOUT="${TLS_TIMEOUT:-360}"
PORT="${TLS_TEST_PORT:-8443}"
SNI="${TLS_TEST_SNI:-tls.test}"
DOT_PORT="${NET_DNS_DOT_PORT:-8853}"
DOT_SNI="${NET_DNS_DOT_SNI:-dns.test}"

WORK="$(mktemp -d "${TMPDIR:-/tmp}/velordo-tls-XXXXXX")"
TOOL="$ROOT/build-meta/tls-test-certs"
SRV_PY="$WORK/tls-server.py"
DOT_PY="$WORK/dot-server.py"
SRV_LOG="$WORK/server.log"
DOT_LOG="$WORK/dot.log"
SRV_PID=""
DOT_PID=""

cleanup() {
    for p in "$SRV_PID" "$DOT_PID"; do
        if [ -n "$p" ]; then
            kill "$p" 2>/dev/null || true
            wait "$p" 2>/dev/null || true
        fi
    done
    rm -rf "$WORK"
}
trap cleanup EXIT

# ── Certificati di test ────────────────────────────────────────────────
# Un solo leaf con due SAN serve HTTP (tls.test) e DoT (dns.test); `bad.*`
# e' un leaf self-signed per il test fail-closed.
# Il tool e' host (std) ma la config del repo forza il target freestanding:
# si compila da una directory neutra con `--manifest-path` (come arca-tool).
mkdir -p "$ROOT/build-meta"
( cd "${TMPDIR:-/tmp}" && cargo build --release --manifest-path "$ROOT/tools/tls-test-certs/Cargo.toml" )
cp "$ROOT/tools/tls-test-certs/target/release/tls-test-certs" "$TOOL"
"$TOOL" "$WORK" "$SNI,$DOT_SNI"

# ── Server HTTP su TLS (tlsprobe) ──────────────────────────────────────
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

# ── Server DoT (dns in mode=dot) ───────────────────────────────────────
# DNS over TCP/TLS con framing a prefisso di 2 byte (RFC 7858 §3.3).
# Le prime tre connessioni presentano il certificato buono (N6.0
# `upstream.test`, DoT query 1 e DoT query 2 = reconnect); dalla quarta un
# leaf self-signed: la verifica della catena deve fallire e il resolver
# rispondere SERVFAIL. Conteggio deterministico: un handshake riuscito = una
# connessione (le ritrasmissioni SYN non ne creano altre).
cat > "$DOT_PY" <<'PY'
import socket, ssl, struct, sys

cert, key, badcert, badkey, port = sys.argv[1:6]
port = int(port)

def ctx_for(c, k):
    ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    ctx.minimum_version = ssl.TLSVersion.TLSv1_3
    ctx.maximum_version = ssl.TLSVersion.TLSv1_3
    ctx.set_alpn_protocols(["dot"])
    ctx.load_cert_chain(c, k)
    return ctx

good = ctx_for(cert, key)
bad = ctx_for(badcert, badkey)

def recvn(s, n):
    buf = b""
    while len(buf) < n:
        d = s.recv(n - len(buf))
        if not d:
            raise EOFError("eof")
        buf += d
    return buf

def answer(query):
    # header + question copiati; QR=1, RA=1, 1 risposta A.
    # `upstream.test` imita il responder plain di run-net-tests.sh (l'N6.0
    # check del probe vale anche in un boot DoT); ogni altro nome -> 203.0.113.9.
    qd = struct.unpack("!H", query[4:6])[0]
    off = 12
    for _ in range(qd):
        while query[off] != 0:
            off += 1 + query[off]
        off += 1 + 4  # zero finale + qtype + qclass
    question = query[12:off]
    name = query[12:off - 4]
    addr = bytes([198, 51, 100, 7]) if name == b"\x08upstream\x04test\x00" else bytes([203, 0, 113, 9])
    resp = query[0:2] + struct.pack("!HHHHH", 0x8180, 1, 1, 0, 0) + question
    resp += b"\xc0\x0c" + struct.pack("!HHIH", 1, 1, 60, 4) + addr
    return resp

srv = socket.socket(socket.AF_INET6, socket.SOCK_STREAM)
srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
srv.bind(("::", port))
srv.listen(8)
print("[dot-host] in ascolto su [::]:%d" % port, flush=True)

n = 0
while True:
    c, peer = srv.accept()
    n += 1
    ctx = good if n <= 3 else bad
    try:
        s = ctx.wrap_socket(c, server_side=True)
        print("[dot-host] #%d handshake alpn=%r" % (n, s.selected_alpn_protocol()), flush=True)
        ln = struct.unpack("!H", recvn(s, 2))[0]
        q = recvn(s, ln)
        resp = answer(q)
        s.sendall(struct.pack("!H", len(resp)) + resp)
        s.close()
    except Exception as e:
        print("[dot-host] #%d rifiutato: %r" % (n, e), flush=True)
PY

python3 "$SRV_PY" "$WORK/server.pem" "$WORK/server.key" "$PORT" >"$SRV_LOG" 2>&1 &
SRV_PID=$!
python3 "$DOT_PY" "$WORK/server.pem" "$WORK/server.key" "$WORK/bad.pem" "$WORK/bad.key" "$DOT_PORT" >"$DOT_LOG" 2>&1 &
DOT_PID=$!
sleep 0.5
for pid in "$SRV_PID" "$DOT_PID"; do
    if ! kill -0 "$pid" 2>/dev/null; then
        echo "[tls-tests] FAIL: server TLS non partito"
        cat "$SRV_LOG" "$DOT_LOG" 2>/dev/null || true
        exit 1
    fi
done
echo "[tls-tests] HTTP su [::]:$PORT (sni $SNI) + DoT su [::]:$DOT_PORT (sni $DOT_SNI)"

# Seed: `sys:tls.conf` + `sys:ca.der` (gated su TLS_TEST_CA) e
# `sys:dns.conf` in mode=dot (gated su NET_DNS_DOT_PORT). Senza, il gate
# resta identico perché nessuna delle due config viene seedata.
export TLS_TEST_CA="$WORK/ca.der"
export TLS_TEST_SNI="$SNI"
export TLS_TEST_PORT="$PORT"
export NET_DNS_DOT_PORT="$DOT_PORT"
export NET_DNS_DOT_SNI="$DOT_SNI"

echo "[tls-tests] boot con virtio-net + passt (timeout ${TIMEOUT}s)"
TLS=1 RUN_TESTS=1 RUN_DISPLAY=none timeout "$TIMEOUT" ./run.sh >"$LOG" 2>&1 || true

fail=0
# ── N6.2: adattatore TLS ─────────────────────────────────────────────
rg -q '\[tlsprobe\] handshake: PASS' "$LOG" || { echo "[tls-tests] manca: handshake TLS 1.3"; fail=1; }
rg -q '\[tlsprobe\] appdata: PASS' "$LOG" || { echo "[tls-tests] manca: record applicativo (HTTP)"; fail=1; }
rg -q '\[tlsprobe\] risultato: PASS' "$LOG" || { echo "[tls-tests] manca: risultato del probe"; fail=1; }
# Un probe saltato "passa" per convenzione: qui non e' una vittoria.
if rg -q '\[tlsprobe\] SKIP' "$LOG"; then
    echo "[tls-tests] FAIL: probe saltato (config non seedata?)"
    fail=1
fi

# ── N6.3: DoT nel resolver ───────────────────────────────────────────
# Due query buone (la seconda sul reconnect) e una fail-closed.
dot_pass=$(rg -ac '\[dns\] dot: PASS' "$LOG" || true)
if [ "${dot_pass:-0}" -lt 2 ]; then
    echo "[tls-tests] manca: [dns] dot: PASS (trovate ${dot_pass:-0}, attese 2: query + reconnect)"
    fail=1
fi
rg -q '\[dns\] dotfail: PASS' "$LOG" || { echo "[tls-tests] manca: fail-closed su certificato non valido (SERVFAIL)"; fail=1; }
if rg -q '\[dns\] dot(fail)?: FAIL' "$LOG"; then
    echo "[tls-tests] FAIL: il resolver ha fallito un passo DoT"
    rg -a '\[dns\] dot' "$LOG" | tail -10
    fail=1
fi
# Lato server: il guest ha davvero parlato TLS 1.3 con ALPN dot su entrambi,
# e il terzo handshake e' stato rifiutato (certificato fuori trust).
rg -q "alpn='dot'" "$SRV_LOG" || { echo "[tls-tests] manca: handshake HTTP con ALPN dot"; fail=1; }
rg -q "alpn='dot'" "$DOT_LOG" || { echo "[tls-tests] manca: handshake DoT con ALPN dot"; fail=1; }
# La quarta connessione (query fail-closed) non deve completare l'handshake:
# il client rifiuta la catena e chiude. Almeno tre invece lo completano.
dot_hs=$(rg -c '\[dot-host\] #[0-9]+ handshake' "$DOT_LOG" || true)
if [ "${dot_hs:-0}" -lt 3 ]; then
    echo "[tls-tests] manca: handshake DoT completati (trovati ${dot_hs:-0}, attesi 3)"
    fail=1
fi
if rg -q '\[dot-host\] #4 handshake' "$DOT_LOG"; then
    echo "[tls-tests] FAIL: la query fail-closed ha completato l'handshake (certificato accettato?)"
    fail=1
fi

LOGRE='\[tlsprobe\]|\[dns\] (dot|risposta dot)|\[tls-host\]|\[dot-host\]'
if [ "$fail" = "0" ]; then
    echo "[tls-tests] PASS: handshake TLS 1.3 + catena/hostname + record applicativo + DoT (2 query, reconnect, fail-closed)"
    rg "$LOGRE" "$LOG" | tail -20
    rg '\[tls-host\]|\[dot-host\]' "$SRV_LOG" "$DOT_LOG" | tail -10
    exit 0
fi

echo "[tls-tests] FAIL"
rg "$LOGRE" "$LOG" | tail -40 || true
echo "── log server ──"
tail -20 "$SRV_LOG" || true
tail -20 "$DOT_LOG" || true
exit 1