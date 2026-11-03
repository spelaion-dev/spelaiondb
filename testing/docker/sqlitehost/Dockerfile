# A server holding a SQLite file, for "SQLite over SSH" (ADR-018): unlike the bastions it
# lets the client run commands, and it has the sqlite3 program.
FROM alpine:3.20

RUN apk add --no-cache openssh-server sqlite \
 && adduser -D -s /bin/sh spelunk \
 && echo 'spelunk:*' | chpasswd -e \
 && mkdir -p /etc/ssh/authorized_keys \
 && rm -f /etc/ssh/ssh_host_*

COPY sshd_config /etc/ssh/sshd_config
COPY shop.sql /tmp/shop.sql
# Two copies: tests that write and tests that only read do not share a file.
RUN su spelunk -c "sqlite3 /home/spelunk/shop.db < /tmp/shop.sql && cp /home/spelunk/shop.db /home/spelunk/other.db" \n && rm /tmp/shop.sql

EXPOSE 22
CMD ["/bin/sh", "-c", "ssh-keygen -A && exec /usr/sbin/sshd -D -e"]
