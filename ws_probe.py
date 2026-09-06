"""抓取 kimi web WS 事件类型，运行 60 秒。"""
import asyncio, json, pathlib, time
import urllib.request

import websockets

BASE = "http://127.0.0.1:58627"
TOKEN = pathlib.Path.home().joinpath(".kimi-code/server.token").read_text().strip()


async def main():
    data = json.load(urllib.request.urlopen(
        urllib.request.Request(f"{BASE}/api/v1/sessions",
                               headers={"Authorization": f"Bearer {TOKEN}"})))
    if isinstance(data, dict) and "data" in data:
        data = data["data"]
    items = data["items"] if isinstance(data, dict) else data
    sid_list = [s["id"] for s in items]
    print(f"共 {len(sid_list)} 个会话，busy 的: {[s['id'][:24] for s in items if s.get('busy')]}")

    seen = {}
    async with websockets.connect(
            "ws://127.0.0.1:58627/api/v1/ws",
            subprotocols=[f"kimi-code.bearer.{TOKEN}"]) as ws:
        await ws.send(json.dumps({
            "type": "subscribe", "id": "sub-0",
            "payload": {"session_ids": sid_list}}))
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
            if seen[et] <= 2:
                print(f"[{et}] {json.dumps(ev, ensure_ascii=False)[:220]}")
    print("\n==== 事件类型统计 ====")
    for k, v in sorted(seen.items(), key=lambda x: -x[1]):
        print(f"{v:6d}  {k}")

asyncio.run(main())
