"""Just enough Chrome DevTools Protocol to drive a page, over wsclient."""
import asyncio, json, os, sys, urllib.request
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import wsclient

def targets(port):
    with urllib.request.urlopen(f"http://127.0.0.1:{port}/json", timeout=5) as res:
        return json.load(res)

def page_ws(port, predicate=lambda t: True):
    for t in targets(port):
        if t.get("type") == "page" and t.get("webSocketDebuggerUrl") and predicate(t):
            return t["webSocketDebuggerUrl"], t
    return None, None

class Session:
    def __init__(self, ws_url):
        self.ws_url = ws_url
        self.next_id = 0
    async def __aenter__(self):
        rest = self.ws_url.split("://", 1)[1]
        host_port, path = rest.split("/", 1)
        host, port = host_port.split(":")
        self.reader, self.writer = await wsclient.connect(host, int(port), "/" + path, secure=False)
        return self
    async def __aexit__(self, *a):
        self.writer.close()
    async def call(self, method, params=None, timeout=20):
        self.next_id += 1
        want = self.next_id
        await wsclient.send_json(self.writer, {"id": want, "method": method, "params": params or {}})
        while True:
            frame = await asyncio.wait_for(wsclient.recv_json(self.reader), timeout)
            if frame is None:
                raise RuntimeError("cdp socket closed")
            if frame.get("id") == want:
                if "error" in frame:
                    raise RuntimeError(f"{method}: {frame['error']}")
                return frame.get("result", {})
    async def eval(self, expression, timeout=20):
        result = await self.call("Runtime.evaluate",
                                 {"expression": expression, "returnByValue": True,
                                  "awaitPromise": True}, timeout)
        return result.get("result", {}).get("value")
