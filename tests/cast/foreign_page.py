"""A page served from an origin that is *not* the controller.

The overlay's DOM lives in the displayed page's document, and that document's
origin decides what it may fetch. Bound to 0.0.0.0 so the test can reach it by the
machine's LAN address rather than 127.0.0.1: Chromium's Local Network Access only
gates requests to loopback from origins that are *not* loopback, so a foreign page
served from 127.0.0.1 would quietly pass a test that a real display fails.

`HTTPServer` and not `TCPServer`: it sets `allow_reuse_address`, without which a
re-run inside the TIME_WAIT window fails to bind and looks like a real failure.

A second argument turns on a nonce-based `style-src` -- the shape a Next.js site
sends, and the one that silently stripped the overlay's stylesheet. The nonce is
deliberately not one the overlay could ever guess.
"""
import http.server
import sys

PORT = int(sys.argv[1]) if len(sys.argv) > 1 else 3061
CSP = ("default-src 'self'; script-src 'self' 'nonce-pagenonce' 'strict-dynamic'; "
       "style-src 'self' 'nonce-pagenonce'; img-src 'self' data:; object-src 'none'") \
    if len(sys.argv) > 2 and sys.argv[2] == "csp" else None


class Handler(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        body = b"<!doctype html><title>foreign</title><p>foreign page"
        self.send_response(200)
        self.send_header("Content-Type", "text/html")
        if CSP:
            self.send_header("Content-Security-Policy", CSP)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *args):
        pass


http.server.HTTPServer(("0.0.0.0", PORT), Handler).serve_forever()
