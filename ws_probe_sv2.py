"""实测 subscribe_v2 transcript 档位：连 v1 ws，对 busy 会话订阅 delta 档，观察 transcript.ops。"""
import asyncio, json, pathlib, sys, time
import urllib.request

import websockets

HOME = pathlib.Path.home()
TOKEN = (HOME / ".kimi-code/server.token").read_text().strip()


def find_port():
    best, best_hb = None, 0
    for f in (HOME / ".kimi-code/server/instances").glob("*.json"):
        try:
            d = json.loads(f.read_text())
            if d.get("heartbeat_at", 0) > best_hb:
                best, best_hb = d["port"], d["heartbeat_at"]
        except Exception:
            pass
    return best


PORT = find_port()
BASE = f"http://127.0.0.1:{PORT}"


def get_sessions():
    data = json.load(urllib.request.urlopen(urllib.request.Request(
        f"{BASE}/api/v1/sessions", headers={"Authorization": f"Bearer {TOKEN}"})))
    return data["data"]["items"] if isinstance(data, dict) and "data" in data else data


async def main():
    items = get_sessions()
    if len(sys.argv) > 1:
        sid = sys.argv[1]
        print(f"订阅指定会话 {sid}")
    else:
        busy = [s for s in items if s.get("busy")]
        print(f"port={PORT} sessions={len(items)} busy={[s['id'][:20] for s in busy]}")
        if not busy:
            print("没有 busy 会话，看不到 delta，请在我工作时运行")
        sid = busy[0]["id"] if busy else items[0]["id"]

    seen = {}
    op_kinds = set()
    async with websockets.connect(
            f"ws://127.0.0.1:{PORT}/api/v1/ws",
            subprotocols=[f"kimi-code.bearer.{TOKEN}"]) as ws:
        await ws.send(json.dumps({
            "type": "subscribe_v2", "id": "sv2-1",
            "payload": {"session_id": sid, "transcript": {"*": "delta"}}}))
        deadline = time.time() + 60
        while time.time() < deadline:
            try:
                msg = await asyncio.wait_for(ws.recv(), timeout=max(deadline - time.time(), 1))
            except asyncio.TimeoutError:
                break
            try:
                ev = json.loads(msg)
            except Exception:
                continue
            et = ev.get("type", "?")
            if et == "ping":
                await ws.send(json.dumps({"type": "pong", "payload": ev.get("payload", {})}))
                continue
            seen[et] = seen.get(et, 0) + 1
            if et == "transcript.ops":
                p = ev.get("payload", {})
                for op in p.get("ops", []):
                    key = op.get("op", "?")
                    tgt = op.get("target", {})
                    frm = op.get("frame", {})
                    kind = frm.get("kind") or tgt.get("type") or ""
                    role = frm.get("role", "")
                    op_kinds.add(f"{key}/{kind}/{role}")
                if seen[et] <= 3:
                    print(f"[ops] {json.dumps(ev, ensure_ascii=False)[:400]}")
            elif seen[et] <= 3:
                print(f"[{et}] {json.dumps(ev, ensure_ascii=False)[:250]}")
    print("\nop 形态集合:", *sorted(op_kinds), sep="\n  - ")
    print("类型统计:", dict(sorted(seen.items(), key=lambda x: -x[1])))

asyncio.run(main())
