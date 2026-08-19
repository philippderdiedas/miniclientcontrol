"""Port clashes: explicit means crash, default means take the next one free."""
import json, os, socket, subprocess, sys, time, urllib.request
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from test_cast import BIN, SP, check, failures

HTTP = 3041
DEFAULT_TLS = 3443

def hold(port):
    """Occupy a port the way a stray earlier instance would."""
    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    sock.bind(("0.0.0.0", port))
    sock.listen(1)
    return sock

def launch(extra):
    return subprocess.Popen(
        [BIN, "--port", str(HTTP), "--database-path", f"{SP}/port.db",
         "--assets-dir", f"{SP}/assets", "--cast-cert-path", f"{SP}/cert.pem",
         "--no-launch-browser"] + extra,
        stdout=subprocess.PIPE, stderr=subprocess.PIPE)

def state():
    for _ in range(40):
        try:
            with urllib.request.urlopen(f"http://127.0.0.1:{HTTP}/api/cast/state", timeout=2) as res:
                return json.load(res)
        except Exception:
            time.sleep(0.2)
    return None

def main():
    print("\n[26] an explicitly configured port that is taken is fatal")
    busy = hold(3499)
    proc = launch(["--cast-tls-port", "3499"])
    out, err = proc.communicate(timeout=30)
    combined = (out + err).decode()
    check("the process exits non-zero", proc.returncode != 0, proc.returncode)
    check("and says which port and why", "3499" in combined, combined[-300:])
    check("with a hint at the usual cause",
          "miniclientcontrol" in combined or "--cast-tls-port" in combined, combined[-300:])
    busy.close()

    print("\n[27] without an explicit port it moves to the next free one")
    busy = hold(DEFAULT_TLS)
    proc = launch([])
    st = state()
    check("the controller came up anyway", st is not None)
    if st:
        check(f"and moved off {DEFAULT_TLS}", st["tls_port"] == DEFAULT_TLS + 1, st["tls_port"])
        check("the advertised URL uses the port it really bound",
              st["sender_url"].endswith(f":{DEFAULT_TLS + 1}/"), st["sender_url"])
    proc.terminate(); proc.wait(timeout=10)
    busy.close()

    print("\n[28] with the default port free it uses it")
    proc = launch([])
    st = state()
    if st:
        check(f"bound {DEFAULT_TLS}", st["tls_port"] == DEFAULT_TLS, st["tls_port"])
    proc.terminate(); proc.wait(timeout=10)

main()
print("\n" + ("ALL PASSED" if not failures else f"{len(failures)} FAILED: {failures}"))
sys.exit(1 if failures else 0)
