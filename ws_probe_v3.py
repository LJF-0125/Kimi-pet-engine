"""求证脚本：对正在运行的 kimi web 服务验证 v1/v3 协议现状。

A) v3: 连 /api/v3/ws，等 hello，subscribe 当前会话，收集消息类型 20 秒
B) v1: 连 /api/v1/ws，收集 agent.status.updated 的 phase kind 集合 20 秒
"""
import asyncio, json, pathlib, time
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
print(f"端口: {PORT}")


def get_sessions():
    data = json.load(urllib.request.urlopen(urllib.request.Request(
        f"{BASE}/api/v1/sessions", headers={"Authorization": f"Bearer {TOKEN}"})))
    items = data["data"]["items"] if isinstance(data, dict) and "data" in data else data
    return items


async def probe_v3(sid):
    print("\n===== A) v3 探测 =====")
    try:
        async with websockets.connect(
                f"ws://127.0.0.1:{PORT}/api/v3/ws",
                subprotocols=[f"kimi-code.bearer.{TOKEN}"]) as ws:
            print("[v3] WS 升级成功")
            await ws.send(json.dumps({"type": "subscribe", "id": "s1", "session_id": sid}))
            seen = {}
            deadline = time.time() + 20
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
                seen[et] = seen.get(et, 0) + 1
                if seen[et] <= 2:
                    print(f"[v3][{et}] {json.dumps(ev, ensure_ascii=False)[:300]}")
            print("[v3] 类型统计:", dict(sorted(seen.items(), key=lambda x: -x[1])))
    except Exception as e:
        print(f"[v3] 失败: {type(e).__name__}: {e}")


async def probe_v1(sids):
    print("\n===== B) v1 phase kind 探测 =====")
    try:
        async with websockets.connect(
                f"ws://127.0.0.1:{PORT}/api/v1/ws",
                subprotocols=[f"kimi-code.bearer.{TOKEN}"]) as ws:
            await ws.send(json.dumps({
                "type": "subscribe", "id": "sub-0",
                "payload": {"session_ids": sids}}))
            kinds, seen = set(), {}
            deadline = time.time() + 20
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
                if et == "agent.status.updated":
                    p = ev.get("payload", {})
                    ph = p.get("phase") or p.get("status") or {}
                    kinds.add(json.dumps(ph, ensure_ascii=False)[:120])
                    if len(kinds) <= 8:
                        print(f"[v1][status] {json.dumps(p, ensure_ascii=False)[:300]}")
            print("[v1] phase 形态集合:", *sorted(kinds), sep="\n  - ")
            print("[v1] 类型统计:", dict(sorted(seen.items(), key=lambda x: -x[1])))
    except Exception as e:
        print(f"[v1] 失败: {type(e).__name__}: {e}")


async def main():
    items = get_sessions()
    busy = [s for s in items if s.get("busy")]
    print(f"会话 {len(items)} 个，busy: {[s['id'][:24] for s in busy]}")
    sids = [s["id"] for s in items]
    target = busy[0]["id"] if busy else sids[0]
    await probe_v3(target)
    await probe_v1(sids)

asyncio.run(main())
