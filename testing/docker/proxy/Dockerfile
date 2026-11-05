# Test proxies (ADR-027): SOCKS5 (Dante) on 1080 and HTTP CONNECT (tinyproxy) on 3128, both
# with a username and password. They reach the bastions and the databases by name, so tests
# check that the target name is resolved on the proxy's side.
FROM alpine:3.20

RUN apk add --no-cache dante-server tinyproxy \
 # Dante checks SOCKS usernames and passwords against the system's.
 && adduser -D -H -s /sbin/nologin proxyuser \
 && echo 'proxyuser:proxy-pass' | chpasswd

COPY sockd.conf /etc/sockd.conf
COPY tinyproxy.conf /etc/tinyproxy/tinyproxy.conf

EXPOSE 1080 3128
CMD ["/bin/sh", "-c", "tinyproxy -d -c /etc/tinyproxy/tinyproxy.conf & exec sockd -f /etc/sockd.conf"]
