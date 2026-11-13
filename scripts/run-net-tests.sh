#!/usr/bin/env bash
# Test di rete (ADR-0069): **fuori dal gate** (dipende da `passt`, che e' una
# dipendenza host). Boot con virtio-net + passt (IPv6 reale, unprivileged),
# SLAAC/NDP e ping6 verso il router. Vittoria N1: `[net] echo reply`.
#
# N6.0: un resolver DNS di prova gira sull'host e passt lo espone al guest
# sulla porta dell'upstream (`--map-host-loopback` = indirizzo del gateway).
# Il probe verifica che il servizio `dns` inoltri un nome fuori tabella.
#
# N6.3 (DoT) NON e' qui: il resolver in `mode = dot` ha bisogno di un server
# TLS sull'host e i certificati di test, quindi vive in
# `scripts/run-tls-tests.sh` (un solo boot con TLS 1.3 e DoT).
#
# N7.0 (ADR-0074): i client di questi probe (netudp/net3/net4/dns) usano
# `civis::net` / `net-ep` (apertura, slot, cursori, backpressure): la suite
# prova che il client del meccanismo parla lo stesso protocollo del servizio.
#
# Uso: ./scripts/run-net-tests.sh
set -euo pipefail
cd "$(dirname "$0")/.."

LOG="${NET_LOG:-/tmp/net-tests.log}"
# N5: la sequenza netudp→net3→net4→dns arriva oltre i 90 s su host lenti
# (il probe DNS parte dopo ~98 s); il default copre l'intera suite.
TIMEOUT="${NET_TIMEOUT:-180}"
# Porta dell'upstream DNS di prova (unprivileged, nessun root richiesto).
UPSTREAM_PORT="${NET_DNS_UPSTREAM_PORT:-5353}"

# Porta dell'echo server TCP di rete reale (N6.1d).
TCP_ECHO_PORT="${NET_TCP_ECHO_PORT:-5354}"

DNS_SRV_PID=""
DNS_SRV_PY=""
cleanup() {
    if [ -n "$DNS_SRV_PID" ]; then
        kill "$DNS_SRV_PID" 2>/dev/null || true
        wait "$DNS_SRV_PID" 2>/dev/null || true
    fi
    if [ -n "$DNS_SRV_PY" ]; then
        rm -f "$DNS_SRV_PY"
    fi
}
trap cleanup EXIT

# ── Upstream DNS di prova (host, loopback) ──────────────────────────────
DNS_SRV_PY="$(mktemp /tmp/velordo-dns-XXXXXX.py)"
cat > "$DNS_SRV_PY" <<'PY'
import socket, struct, sys

port = int(sys.argv[1])
addr = bytes([198, 51, 100, 7])  # TEST-NET-2, lo stesso atteso dal probe

s = socket.socket(socket.AF_INET6, socket.SOCK_DGRAM)
s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
s.bind(("::", port))
print("[dns-host] in ascolto su [::]:%d" % port, flush=True)

while True:
    data, peer = s.recvfrom(2048)
    if len(data) < 12:
        continue
    qd = struct.unpack("!H", data[4:6])[0]
    if qd != 1:
        continue
    off = 12
    while off < len(data) and data[off] != 0:
        if data[off] & 0xC0 == 0xC0:
            off += 1
            break
        off += 1 + data[off]
    off += 1 + 4  # zero finale + qtype + qclass
    if off > len(data):
        continue
    question = data[12:off]
    # header: QR=1, RD copiato, RA=1, rcode 0, 1 domanda, 1 risposta A
    resp = data[0:2] + struct.pack("!HHHHH", 0x8180, 1, 1, 0, 0) + question
    resp += b"\xc0\x0c" + struct.pack("!HHIH", 1, 1, 60, 4) + addr
    s.sendto(resp, peer)
PY

python3 "$DNS_SRV_PY" "$UPSTREAM_PORT" &
DNS_SRV_PID=$!
sleep 0.3
if ! kill -0 "$DNS_SRV_PID" 2>/dev/null; then
    echo "[net-tests] FAIL: upstream DNS di prova non partito su [::]:$UPSTREAM_PORT"
    exit 1
fi
export NET_DNS_UPSTREAM_PORT="$UPSTREAM_PORT"

# ── Peer TCP di rete reale (N6.1d, opzionale) ───────────────────────────
# Il TCP verso l'esterno dipende dall'ambientazione: passt **non** inoltra il
# TCP al loopback dell'host (l'UDP del DNS sì) e in questa sandbox anche un
# peer pubblico IPv6 risponde RST. Perciò il probe si esegue solo se chi
# chiama dichiara un peer raggiungibile, e solo allora diventa obbligatorio:
#   NET_TCP_PEER=<ipv6> [NET_TCP_ECHO_PORT=53] ./scripts/run-net-tests.sh
if [ -n "${NET_TCP_PEER:-}" ]; then
    export NET_TCP_ECHO_ADDR="$NET_TCP_PEER"
    export NET_TCP_ECHO_PORT="${NET_TCP_ECHO_PORT:-53}"
    PEER_DESC="peer TCP ${NET_TCP_PEER}:${NET_TCP_ECHO_PORT}"
else
    PEER_DESC="peer TCP esterno non configurato (probe saltato)"
fi

echo "[net-tests] boot con virtio-net + passt (timeout ${TIMEOUT}s, upstream DNS host:${UPSTREAM_PORT}, ${PEER_DESC})"
NET=1 RUN_TESTS=1 RUN_DISPLAY=none timeout "$TIMEOUT" ./run.sh >"$LOG" 2>&1 || true

fail=0
rg -q '\[net\] echo reply' "$LOG" || { echo "[net-tests] manca: echo reply (N1)"; fail=1; }
rg -q '\[netudp\] risultato: PASS' "$LOG" || { echo "[net-tests] manca: UDP echo (N2)"; fail=1; }
rg -q '\[net3\] risultato: PASS' "$LOG" || { echo "[net-tests] manca: TCP loopback (N3/N6.1)"; fail=1; }
rg -q '\[net4\] risultato: PASS' "$LOG" || { echo "[net-tests] manca: SIIT (N4)"; fail=1; }
rg -q '\[dns\] risultato: PASS' "$LOG" || { echo "[net-tests] manca: DNS (N5)"; fail=1; }
rg -q '\[dns\] upstream: PASS' "$LOG" || { echo "[net-tests] manca: upstream DNS (N6.0)"; fail=1; }
if [ -n "${NET_TCP_PEER:-}" ]; then
    rg -q '\[nettcp\] risultato: PASS' "$LOG" || { echo "[net-tests] manca: TCP rete reale (N6.1d)"; fail=1; }
fi

LOGRE='\[net\]|\[netdev\]|\[netecho\]|\[netudp\]|\[net3\]|\[net4\]|\[nettcp\]|\[siit\]|\[dns\]|\[dns-host\]'
if [ "$fail" = "0" ]; then
    echo "[net-tests] PASS: echo reply + UDP echo + TCP (handshake e echo reali) + IPv4 SIIT + DNS probe + upstream DNS${NET_TCP_PEER:+ + TCP rete reale}"
    rg "$LOGRE" "$LOG" | tail -45
    exit 0
fi

echo "[net-tests] FAIL"
rg "$LOGRE|\[init\] net" "$LOG" | tail -50 || true
exit 1
