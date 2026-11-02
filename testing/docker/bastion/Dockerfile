# Minimal SSH bastion for tests. Port forwarding only (direct-tcpip): no shell.
FROM alpine:3.20

RUN apk add --no-cache openssh-server \
 && adduser -D -s /bin/sh spelunk \
 # Alpine locks accounts without a password and sshd rejects them even with a key.
 && echo 'spelunk:*' | chpasswd -e \
 && mkdir -p /etc/ssh/authorized_keys \
 # No host keys in the image: each container generates its own at startup.
 && rm -f /etc/ssh/ssh_host_*

COPY sshd_config /etc/ssh/sshd_config

EXPOSE 22
# If host keys were generated in the image, all the bastions would share the same
# fingerprint; this way each one has its own, as in real life.
CMD ["/bin/sh", "-c", "ssh-keygen -A && exec /usr/sbin/sshd -D -e"]
