"""The managed certificate: a real name for a private address.

`--public-url none` plus `--managed-cert auto` advertises this device as
`<address-with-dashes>.clientctrl.cc`, whose public DNS points straight back at
the private address, and serves the matching public wildcard certificate. The
payoff is that guests get no warning page at all, so the check that matters here
is a TLS handshake that validates against the *system* trust store -- not one we
tell Python to ignore.

The cases that need the network say so and skip when it is absent: this file is
run by hand on a laptop, and a device with no route should not look like a
regression.
"""
import os, socket, ssl, sys, time, urllib.request
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from test_cast import Server, http, check, failures, SP, TLS

MANAGED_DOMAIN = "clientctrl.cc"
API = "api.clientcontrol.cc"


def sender_url():
    return http("GET", "/api/cast/info")[1]["sender_url"]


def private_ipv4():
    """The address the controller would pick, by the same UDP-connect trick."""
    try:
        s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        s.connect(("10.254.254.254", 1))
        addr = s.getsockname()[0]
        s.close()
    except OSError:
        return None
    a, b = (int(x) for x in addr.split(".")[:2])
    private = a == 10 or (a == 172 and 16 <= b <= 31) or (a == 192 and b == 168)
    return addr if private else None


def api_reachable():
    try:
        with socket.create_connection((API, 443), timeout=5):
            return True
    except OSError:
        return False


def main():
    print("\n[38] the managed name is opt-out, and only for --public-url none")

    with Server(managed_cert="off"):
        url = sender_url()
        check("off keeps the bare address", MANAGED_DOMAIN not in url, url)

    with Server(managed_cert="auto", public_url="mdns"):
        url = sender_url()
        check("a chosen name wins over the managed one (mdns)",
              MANAGED_DOMAIN not in url and ".local" in url, url)

    with Server(managed_cert="auto", public_url="signage.example.com"):
        url = sender_url()
        check("a chosen name wins over the managed one (bare host)",
              url == f"https://signage.example.com:{TLS}/", url)

    addr = private_ipv4()
    if addr is None:
        print("  SKIP  no private IPv4 here, so there is nothing to name")
        return
    if not api_reachable():
        print(f"  SKIP  {API} is unreachable, so the certificate cannot be fetched")
        return

    expected_host = f"{addr.replace('.', '-')}.{MANAGED_DOMAIN}"
    cache = f"{SP}/cert.pem.managed.json"
    for leftover in (cache,):
        try:
            os.remove(leftover)
        except FileNotFoundError:
            pass

    print("\n[39] a real certificate for a private address")
    with Server(managed_cert="auto"):
        url = sender_url()
        check("the device is advertised under the dashed name",
              url == f"https://{expected_host}:{TLS}/", url)

        # The point of the whole feature. Default context, system trust store,
        # no exceptions made -- exactly what a guest's browser does.
        try:
            with socket.create_connection((expected_host, TLS), timeout=10) as raw:
                with ssl.create_default_context().wrap_socket(
                    raw, server_hostname=expected_host
                ) as tls:
                    peer = tls.getpeercert()
            names = {v for k, v in peer.get("subjectAltName", ()) if k == "DNS"}
            check("the handshake validates against the system trust store", True)
            check(f"and the certificate covers *.{MANAGED_DOMAIN}",
                  f"*.{MANAGED_DOMAIN}" in names, names)
        except (ssl.SSLError, ssl.CertificateError) as e:
            check("the handshake validates against the system trust store", False, e)
        except OSError as e:
            # The name resolves publicly, so this usually means a resolver on
            # this machine refuses a public name that points into the LAN.
            print(f"  SKIP  cannot reach {expected_host}: {e}")
            return

        check("the certificate is cached", os.path.exists(cache))
        check("and the cache is not world-readable",
              oct(os.stat(cache).st_mode & 0o777) == "0o600",
              oct(os.stat(cache).st_mode & 0o777))

    print("\n[40] a second start uses the cache instead of fetching again")
    before = os.stat(cache).st_mtime
    time.sleep(1.1)
    with Server(managed_cert="auto", fresh=False):
        check("still advertised under the same name",
              sender_url() == f"https://{expected_host}:{TLS}/", sender_url())
    check("the cache was not rewritten", os.stat(cache).st_mtime == before)


main()
print("\n" + ("ALL PASSED" if not failures else f"{len(failures)} FAILED: {failures}"))
sys.exit(1 if failures else 0)
