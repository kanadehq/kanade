#!/usr/bin/env python3
"""Disposable TLS byte tunnel for the real-broker WSS credential test.

TLS terminates here and the unchanged WebSocket HTTP upgrade and frames go to
NATS over loopback, like a TLS-terminating reverse proxy. No authentication
policy is implemented in the proxy. The client must trust the supplied CA.
"""

import os
import select
import socket
import socketserver
import ssl
import sys


class Tunnel(socketserver.BaseRequestHandler):
    def handle(self):
        try:
            with CONTEXT.wrap_socket(self.request, server_side=True) as downstream:
                with socket.create_connection(("127.0.0.1", TARGET), timeout=5) as upstream:
                    upstream.settimeout(None)
                    cookie = os.environ.get("KANADE_TEST_WS_COOKIE")
                    if cookie:
                        # Fixture-only cookie injection exercises the broker's
                        # real WebSocket cookie authorization, without a client
                        # library dependency. Never log the header or payload.
                        request = bytearray()
                        while b"\r\n\r\n" not in request:
                            chunk = downstream.recv(4096)
                            if not chunk or len(request) + len(chunk) > 16384:
                                return
                            request.extend(chunk)
                        headers, remainder = request.split(b"\r\n\r\n", 1)
                        upstream.sendall(headers + b"\r\nCookie: " + cookie.encode("ascii")
                                         + b"\r\n\r\n" + remainder)
                    while True:
                        ready, _, _ = select.select(
                            [downstream, upstream], [], [],
                            0 if downstream.pending() else 10,
                        )
                        # TLS may already have buffered another record.
                        if downstream.pending() and downstream not in ready:
                            ready.append(downstream)
                        for source in ready:
                            data = source.recv(65536)
                            if not data:
                                return
                            (upstream if source is downstream else downstream).sendall(data)
        except (OSError, ssl.SSLError):
            # Refused credentials and fixture teardown close sockets normally.
            # Never include broker payloads or connection addresses in diagnostics.
            return


if __name__ == "__main__":
    TARGET = int(sys.argv[1])
    CONTEXT = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    CONTEXT.load_cert_chain(sys.argv[2], sys.argv[3])
    with socketserver.ThreadingTCPServer(("127.0.0.1", 0), Tunnel) as listener:
        listener.daemon_threads = True
        print(listener.server_address[1], flush=True)
        listener.serve_forever()
