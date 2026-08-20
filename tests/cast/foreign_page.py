"""A page served from an origin that is *not* the controller.

The overlay's DOM lives in the displayed page's document, so anything it fetches
with a relative URL would be fetched from that page's host. One port of one plain
page is enough to make that mistake fail a test instead of a display.

`HTTPServer` and not `TCPServer`: it sets `allow_reuse_address`, without which a
re-run inside the TIME_WAIT window fails to bind and looks like a real failure.
"""
import http.server
import sys

PORT = int(sys.argv[1]) if len(sys.argv) > 1 else 3061


class Handler(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        body = b"<!doctype html><title>foreign</title><p>foreign page"
        self.send_response(200)
        self.send_header("Content-Type", "text/html")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *args):
        pass


http.server.HTTPServer(("127.0.0.1", PORT), Handler).serve_forever()
