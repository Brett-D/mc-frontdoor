"""End-to-end test of mc-frontdoor against a fake Minecraft server, Geyser and portal (all local, no internet).

Usage: python tests/e2e.py path/to/mc-frontdoor   (needs `pip install "httpx[http2]"` and openssl on PATH,
or OPENSSL=path/to/openssl). Exits non-zero if any check fails.
"""
import asyncio, base64, json, os, struct, subprocess, sys, tempfile, threading, time, uuid
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

FD = sys.argv[1]
FD_PORT, BACKEND_PORT, MAP_PORT, PORTAL_PORT = 25598, 25599, 18100, 18080
STATE = {"up": False, "start_calls": 0}


def varint(n):
    n &= 0xFFFFFFFF
    out = bytearray()
    while True:
        b = n & 0x7F; n >>= 7
        out.append(b | (0x80 if n else 0))
        if not n: return bytes(out)


def mcs(s):
    b = s.encode(); return varint(len(b)) + b


def frame(pid, payload=b""):
    body = varint(pid) + payload; return varint(len(body)) + body


async def rvarint(r):
    n = s = 0
    while True:
        b = (await r.readexactly(1))[0]; n |= (b & 0x7F) << s; s += 7
        if not b & 0x80: return n


async def rpacket(r):
    ln = await rvarint(r); body = await r.readexactly(ln)
    i = 0; pid = 0; s = 0
    while True:
        b = body[i]; i += 1; pid |= (b & 0x7F) << s; s += 7
        if not b & 0x80: break
    return pid, body[i:]


def rstr(data, i=0):
    n = s = 0
    while True:
        b = data[i]; i += 1; n |= (b & 0x7F) << s; s += 7
        if not b & 0x80: break
    return data[i:i + n].decode(), i + n


def parse_nbt(data):
    """Parses network NBT (nameless root) into Python values."""
    def name(i):
        n = struct.unpack(">H", data[i:i + 2])[0]; return data[i + 2:i + 2 + n].decode("utf-8"), i + 2 + n
    def payload(t, i):
        if t == 1: return bool(data[i]), i + 1
        if t == 3: return struct.unpack(">i", data[i:i + 4])[0], i + 4
        if t == 8: return name(i)
        if t == 9:
            et = data[i]; n = struct.unpack(">i", data[i + 1:i + 5])[0]; i += 5; out = []
            for _ in range(n): v, i = payload(et, i); out.append(v)
            return out, i
        if t == 10:
            out = {}
            while data[i] != 0:
                et = data[i]; k, i = name(i + 1); v, i = payload(et, i); out[k] = v
            return out, i + 1
        raise ValueError(t)
    v, i = payload(data[0], 1)
    assert i == len(data), "trailing NBT bytes"
    return v


# ---- fake real server: answers status; answers logins with a disconnect so relays can be checked ----
async def backend_conn(r, w):
    try:
        pid, body = await rpacket(r)
        proto = 0; i = 0; s = 0
        while True:
            b = body[i]; i += 1; proto |= (b & 0x7F) << s; s += 7
            if not b & 0x80: break
        intent = body[-1]
        if intent == 1:
            await rpacket(r)
            w.write(frame(0, mcs(json.dumps({"version": {"name": "Paper 26.3", "protocol": 777},
                                              "players": {"max": 20, "online": 0},
                                              "description": {"text": "REAL SERVER"}}))))
        else:
            await rpacket(r)
            w.write(frame(0, mcs(json.dumps({"text": f"backend saw protocol {proto}"}))))
        await w.drain()
    except Exception:
        pass
    w.close()


async def map_conn(r, w):
    await r.read(1000); w.write(b"HTTP/1.1 200 OK\r\nContent-Length: 8\r\n\r\nREAL MAP"); await w.drain(); w.close()


class Portal(BaseHTTPRequestHandler):
    def log_message(self, *a): pass
    def _send(self, obj):
        b = json.dumps(obj).encode(); self.send_response(200); self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(b))); self.end_headers(); self.wfile.write(b)
    def do_POST(self):
        self.rfile.read(int(self.headers.get("content-length", 0)))
        assert self.headers.get("x-portal-key") == "testkey"
        STATE["start_calls"] += 1
        threading.Timer(8, lambda: STATE.update(up=True)).start()
        self._send({"vm": "starting", "games": {"minecraft": {"state": "starting"}}})
    def do_GET(self):
        self._send({"vm": "running", "games": {"minecraft": {"state": "running" if STATE["up"] else "starting"}}})


# ---- client helpers ---------------------------------------------------------------------------------
async def status():
    r, w = await asyncio.open_connection("127.0.0.1", FD_PORT)
    w.write(frame(0, varint(777) + mcs("localhost") + struct.pack(">H", FD_PORT) + varint(1)) + frame(0))
    await w.drain()
    pid, body = await rpacket(r)
    w.write(frame(1, struct.pack(">q", 42))); await w.drain()
    pid2, pong = await rpacket(r)
    assert pid2 == 1 and struct.unpack(">q", pong)[0] == 42
    w.close()
    return json.loads(rstr(body)[0])


async def login(name, proto=777, respond_keepalive=True, max_wait=60):
    r, w = await asyncio.open_connection("127.0.0.1", FD_PORT)
    u = uuid.uuid4().bytes
    w.write(frame(0, varint(proto) + mcs("localhost") + struct.pack(">H", FD_PORT) + varint(2)) + frame(0, mcs(name) + u))
    await w.drain()
    pid, body = await asyncio.wait_for(rpacket(r), 10)
    if pid == 0:
        w.close(); return {"kicked": json.loads(rstr(body)[0])["text"]}
    assert pid == 2, f"expected login success, got {pid}"
    assert body[:16] == u, "uuid not echoed"
    nm, i = rstr(body, 16); assert nm == name
    assert body[i] == 0, "properties should be empty"; i += 1
    assert len(body) - i == 16, f"session id should be 16 bytes, got {len(body) - i}"
    w.write(frame(3)); w.write(frame(0, mcs("en_us") + b"\x0a")); await w.drain()  # ack + (junk) client info
    seen = {"dialogs": [], "keepalives": 0}
    t0 = time.time()
    while time.time() - t0 < max_wait:
        pid, body = await asyncio.wait_for(rpacket(r), 25)
        if pid == 0x04:
            seen["keepalives"] += 1
            if respond_keepalive: w.write(frame(0x04, body)); await w.drain()
        elif pid == 0x13:
            seen["dialogs"].append(parse_nbt(body))
        elif pid == 0x0C:
            host, i = rstr(body); port = body[i]  # small varint
            seen["transfer"] = (host, port if port < 128 else None); seen["after"] = round(time.time() - t0, 1)
            w.close(); return seen
        elif pid == 0x02:
            seen["config_kick"] = parse_nbt(body); w.close(); return seen
        else:
            seen.setdefault("other", []).append(pid)
    w.close(); seen["timeout"] = True; return seen


# ---- Bedrock: a test signing key, tokens, and NetherNet requests ---------------------------------------
BEDROCK_PORT = 19198
ISSUER = "https://auth.test.example/"


def _probable_prime(bits):
    import secrets
    small = [p for p in range(3, 2000, 2) if all(p % q for q in range(3, int(p ** 0.5) + 1, 2))]
    while True:
        n = secrets.randbits(bits) | (1 << (bits - 1)) | 1
        if any(n % p == 0 for p in small): continue
        d, s = n - 1, 0
        while d % 2 == 0: d //= 2; s += 1
        for _ in range(20):
            x = pow(secrets.randbelow(n - 3) + 2, d, n)
            if x in (1, n - 1): continue
            for _ in range(s - 1):
                x = pow(x, 2, n)
                if x == n - 1: break
            else: break
        else:
            return n


def rsa_key():
    e = 65537
    while True:
        p, q = _probable_prime(1024), _probable_prime(1024)
        phi = (p - 1) * (q - 1)
        if p != q and phi % e: return p * q, e, pow(e, -1, phi)


def b64u(b): return base64.urlsafe_b64encode(b).rstrip(b"=").decode()


def sign_token(key, claims, kid="test-key"):
    import hashlib
    n, e, d = key
    h = b64u(json.dumps({"alg": "RS256", "kid": kid, "typ": "JWT"}).encode()); p = b64u(json.dumps(claims).encode())
    size = (n.bit_length() + 7) // 8
    digest = bytes.fromhex("3031300d060960864801650304020105000420") + hashlib.sha256(f"{h}.{p}".encode()).digest()
    em = b"\x00\x01" + b"\xff" * (size - len(digest) - 3) + b"\x00" + digest
    return f"{h}.{p}.{b64u(pow(int.from_bytes(em, 'big'), d, n).to_bytes(size, 'big'))}"


def sdp_with(token):
    assertion = json.dumps({"token": token, "fingerprints": "eyJhbGciOiJFUzM4NCJ9..c2ln"})
    ident = base64.b64encode(json.dumps({"idp": {"domain": "auth.test.example", "protocol": "default"},
                                         "assertion": assertion}).encode()).decode()
    return ("v=0\r\no=- 1 1 IN IP4 0.0.0.0\r\ns=-\r\nt=0 0\r\na=fingerprint:sha-256 4A:AD\r\n"
            f"a=identity:{ident}\r\nm=application 9 UDP/DTLS/SCTP webrtc-datachannel\r\n")


async def http(method, path, body=b"", port=BEDROCK_PORT, tls=False):
    if tls:
        import ssl
        ctx = ssl.create_default_context(); ctx.check_hostname = False; ctx.verify_mode = ssl.CERT_NONE
        ctx.set_alpn_protocols(["h2", "http/1.1"])
        r, w = await asyncio.open_connection("127.0.0.1", port, ssl=ctx, server_hostname="mc.test.example")
    else:
        r, w = await asyncio.open_connection("127.0.0.1", port)
    w.write(f"{method} {path} HTTP/1.1\r\nHost: test\r\nContent-Type: application/sdp\r\n"
            f"Content-Length: {len(body)}\r\n\r\n".encode() + body)
    await w.drain()
    # Read the head, then exactly Content-Length bytes (servers may keep the connection open).
    head = await asyncio.wait_for(r.readuntil(b"\r\n\r\n"), 10)
    length = 0
    for line in head.split(b"\r\n"):
        if line.lower().startswith(b"content-length:"):
            length = int(line.split(b":", 1)[1])
    payload = await asyncio.wait_for(r.readexactly(length), 10) if length else b""
    w.close()
    return int(head.split(b" ")[1]), payload.decode()


async def https(method, path, body=b""):
    return await http(method, path, body, tls=True)


async def h2(path):
    """GET over HTTPS offering HTTP/2 (needs `pip install httpx[http2]`); returns [status, version, body]."""
    import httpx
    async with httpx.AsyncClient(http2=True, verify=False, timeout=10) as c:
        r = await c.get(f"https://127.0.0.1:{BEDROCK_PORT}{path}")
        return [r.status_code, r.http_version, r.text]


async def geyser_conn(r, w):
    head = await r.readuntil(b"\r\n\r\n")
    STATE.setdefault("geyser_requests", []).append(head.decode("latin-1"))
    body = json.dumps({"dataVersion": 7, "name": "Geyser", "protocol": 924, "version": "1.26.60", "level": "world",
                       "players": 1, "maxPlayers": 20, "gameType": 0, "onlineAuth": True, "nonce": "abc",
                       "transportLayer": 2, "connection": 4}).encode()
    w.write(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: " + str(len(body)).encode()
            + b"\r\nConnection: close\r\n\r\n" + body)
    await w.drain(); w.close()


async def main():
    tmp = tempfile.mkdtemp()
    bedrock_xuid = 0x0009012345ABCDEF
    allow = os.path.join(tmp, "allow.json")
    json.dump(["Alex_Builder", "steve42", {"name": ".BedrockBea", "uuid": "00000000-0000-0000-0009-012345abcdef"}], open(allow, "w"))
    key = rsa_key()
    crt, crtkey = os.path.join(tmp, "tls.crt"), os.path.join(tmp, "tls.key")
    subprocess.run([os.environ.get("OPENSSL", "openssl"), "req", "-x509", "-newkey", "ec", "-pkeyopt", "ec_paramgen_curve:P-256",
                    "-nodes", "-days", "1", "-subj", "/CN=mc.test.example", "-keyout", crtkey, "-out", crt],
                   check=True, capture_output=True)
    jwks = os.path.join(tmp, "jwks.json")
    json.dump({"keys": [{"kty": "RSA", "kid": "test-key", "n": b64u(key[0].to_bytes(256, "big")), "e": b64u(key[1].to_bytes(3, "big"))}]}, open(jwks, "w"))
    portal = ThreadingHTTPServer(("127.0.0.1", PORTAL_PORT), Portal)
    threading.Thread(target=portal.serve_forever, daemon=True).start()
    env = dict(os.environ, LISTEN_HOST="127.0.0.1", LISTEN_PORT=str(FD_PORT), BACKEND_HOST="127.0.0.1",
               BACKEND_PORT=str(BACKEND_PORT), TRANSFER_HOST="203.0.113.7", TRANSFER_PORT="25565",
               RELAY_PORTS=f"{MAP_PORT}:{MAP_PORT + 1}", MAP_PORT=str(MAP_PORT), PORTAL_URL=f"http://127.0.0.1:{PORTAL_PORT}",
               PORTAL_KEY="testkey", ALLOW_FILE=allow, STATE_DIRECTORY=os.path.join(tmp, "state"), WAKE_TIMEOUT="60",
               BEDROCK_PORTS=f"{BEDROCK_PORT}:{BEDROCK_PORT + 1}", AUTH_JWKS_FILE=jwks, AUTH_ISSUER=ISSUER,
               TLS_CERT_FILE=crt, TLS_KEY_FILE=crtkey)
    # The relay port 18100 must be the backend's map port too, so serve the fake map there once "up".
    cmd = [sys.executable, FD] if FD.endswith(".py") else [FD]
    proc = subprocess.Popen(cmd, env=env, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
    await asyncio.sleep(1.5)
    results = {}
    try:
        results["status_asleep"] = (await status())["description"]["text"]
        r, w = await asyncio.open_connection("127.0.0.1", MAP_PORT); w.write(b"GET / HTTP/1.1\r\nHost: x\r\n\r\n"); await w.drain()
        results["map_asleep"] = (await r.read(200)).split(b"\r\n")[0].decode(); w.close()
        results["not_whitelisted"] = await login("Stranger")
        results["wrong_version_asleep"] = await login("Alex_Builder", proto=776)

        now = int(time.time())
        good = {"iss": ISSUER, "aud": "api://auth-minecraft-services/multiplayer", "exp": now + 600, "xid": str(bedrock_xuid)}
        results["bedrock_list_asleep"] = await http("GET", "/v1/join")
        results["bedrock_not_whitelisted"] = await http("POST", "/v1/join/123", sdp_with(sign_token(key, {**good, "xid": "2535400000000001"})).encode())
        results["bedrock_expired"] = await http("POST", "/v1/join/123", sdp_with(sign_token(key, {**good, "exp": now - 600})).encode())
        results["bedrock_wrong_audience"] = await http("POST", "/v1/join/123", sdp_with(sign_token(key, {**good, "aud": "other"})).encode())
        forged = sign_token(key, good).rsplit(".", 1)[0] + "." + b64u(b"\x01" * 256)
        results["bedrock_forged"] = await http("POST", "/v1/join/123", sdp_with(forged).encode())
        results["bedrock_unknown_key"] = await http("POST", "/v1/join/123", sdp_with(sign_token(key, good, kid="other")).encode())
        results["bedrock_no_identity"] = await http("POST", "/v1/join/123", b"v=0\r\ns=-\r\n")
        results["bedrock_other_path"] = await http("GET", "/admin")
        # RakNet server-list ping over UDP, and an HTTPS attempt (TLS ClientHello) that must fail at once.
        loop = asyncio.get_running_loop(); got = loop.create_future()
        class P(asyncio.DatagramProtocol):
            def datagram_received(self, d, a): got.done() or got.set_result(d)
        tr, _ = await loop.create_datagram_endpoint(P, remote_addr=("127.0.0.1", BEDROCK_PORT))
        tr.sendto(b"\x01" + struct.pack(">q", 1234) + bytes.fromhex("00ffff00fefefefefdfdfdfd12345678") + struct.pack(">q", 99))
        pong = await asyncio.wait_for(got, 5); tr.close()
        results["raknet_pong"] = [hex(pong[0]), struct.unpack(">q", pong[1:9])[0], pong[35:].decode()]
        t = time.time()
        results["https_list_asleep"] = await https("GET", "/v1/join")
        results["https_join_not_whitelisted"] = await https("POST", "/v1/join/9", sdp_with(sign_token(key, {**good, "xid": "2535400000000001"})).encode())
        results["https_seconds"] = round(time.time() - t, 2)
        results["h2_list_asleep"] = await h2("/v1/join")
        assert STATE["start_calls"] == 0, "rejected joins must not wake the server"

        # Backend comes up 8 s after the portal's start; it only listens once "up".
        async def bring_up():
            while not STATE["up"]: await asyncio.sleep(0.2)
            await asyncio.start_server(backend_conn, "127.0.0.1", BACKEND_PORT)
            await asyncio.start_server(map_conn, "127.0.0.1", MAP_PORT + 1)
            await asyncio.start_server(geyser_conn, "127.0.0.1", BEDROCK_PORT + 1)
        asyncio.create_task(bring_up())
        results["bedrock_whitelisted"] = await http("POST", "/v1/join/123", sdp_with(sign_token(key, good)).encode())
        results["start_calls_after_bedrock"] = STATE["start_calls"]
        results["bedrock_list_starting"] = await http("GET", "/v1/join")
        hold = asyncio.create_task(login("alex_builder"))
        await asyncio.sleep(3)
        second = asyncio.create_task(login("steve42"))
        await asyncio.sleep(1)
        results["status_starting"] = (await status())["description"]["text"]
        results["hold_first"] = await hold
        results["hold_second"] = await second
        results["start_calls"] = STATE["start_calls"]
        results["status_up"] = (await status())["description"]["text"]
        results["join_when_up"] = await login("Alex_Builder", max_wait=5)
        results["other_version_when_up"] = await login("Alex_Builder", proto=999)
        r, w = await asyncio.open_connection("127.0.0.1", MAP_PORT); w.write(b"GET / HTTP/1.1\r\nHost: x\r\n\r\n"); await w.drain()
        results["map_up"] = (await r.read(200)).decode().split("\r\n")[-1]; w.close()
        results["bedrock_list_up"] = await http("GET", "/v1/join")
        results["https_list_up"] = await https("GET", "/v1/join")
        results["h2_list_up"] = await h2("/v1/join")
        await asyncio.sleep(1)
        results["bedrock_info_remembered"] = json.load(open(os.path.join(tmp, "state", "last-status.json"))).get("bedrock")
        results["geyser_saw_forwarded_for"] = any("x-forwarded-for: 127.0.0.1" in h.lower() for h in STATE.get("geyser_requests", []))
    finally:
        proc.terminate(); out = proc.communicate(timeout=5)[0]; portal.shutdown()
    for k, v in results.items():
        print(f"{k}: {json.dumps(v)[:600]}")
    print("--- front door log ---"); print(out)

    rust = not FD.endswith(".py")
    transferred = lambda v: v.get("transfer", [None])[0] == "203.0.113.7"
    expect = {
        "status_asleep": lambda v: "Sleeping" in v,
        "map_asleep": lambda v: v.startswith("HTTP/1.1 503"),
        "not_whitelisted": lambda v: "not white-listed" in v.get("kicked", ""),
        "wrong_version_asleep": lambda v: "Switch to that version" in v.get("kicked", ""),
        "bedrock_list_asleep": lambda v: v[0] == 200 and "Sleeping" in v[1],
        "bedrock_not_whitelisted": lambda v: v[0] == 403,
        "bedrock_expired": lambda v: v[0] == 403,
        "bedrock_wrong_audience": lambda v: v[0] == 403,
        "bedrock_forged": lambda v: v[0] == 403,
        "bedrock_unknown_key": lambda v: v[0] == 403,
        "bedrock_no_identity": lambda v: v[0] == 403,
        "bedrock_other_path": lambda v: v[0] == 404,
        "raknet_pong": lambda v: v[0] == "0x1c" and v[1] == 1234 and "Sleeping" in v[2],
        "https_list_asleep": lambda v: v[0] == 200 and "Sleeping" in v[1],
        "https_join_not_whitelisted": lambda v: v[0] == 403,
        "h2_list_asleep": lambda v: v[0] == 200 and "Sleeping" in v[2] and (v[1] == "HTTP/2" or not rust),
        "bedrock_whitelisted": lambda v: v[0] == 503,
        "bedrock_list_starting": lambda v: v[0] == 200 and "Starting up" in v[1],
        "status_starting": lambda v: "Starting up" in v,
        "hold_first": lambda v: transferred(v) and v["dialogs"] and v["keepalives"] >= 1,
        "hold_second": lambda v: transferred(v) and v["dialogs"],
        "start_calls": lambda v: v == 1,
        "status_up": lambda v: v == "REAL SERVER",
        "join_when_up": lambda v: transferred(v) and not v["dialogs"],
        "other_version_when_up": lambda v: v.get("kicked") == "backend saw protocol 999",
        "map_up": lambda v: v == "REAL MAP",
        "bedrock_list_up": lambda v: v[0] == 200 and "Geyser" in v[1],
        "https_list_up": lambda v: v[0] == 200 and "Geyser" in v[1],
        "h2_list_up": lambda v: v[0] == 200 and "Geyser" in v[2] and (v[1] == "HTTP/2" or not rust),
        "bedrock_info_remembered": lambda v: (v or {}).get("protocol") == 924 and "nonce" not in v,
        "geyser_saw_forwarded_for": lambda v: v or not rust,
    }
    failed = [k for k, check in expect.items() if k not in results or not check(results[k])]
    print(f"--- {len(expect) - len(failed)}/{len(expect)} checks passed" + (f"; FAILED: {', '.join(failed)}" if failed else ""))
    sys.exit(1 if failed else 0)


asyncio.run(main())
