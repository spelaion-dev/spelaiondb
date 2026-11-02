# Test environment

PostgreSQL 17 behind two chained SSH bastions:

```
host ──127.0.0.1:2221──► bastion1 ──(mid network)──► bastion2 ──(data network)──► postgres:5432
```

- The bastions only allow port forwarding (`direct-tcpip`): they give no shell.
- `bastion2` and `postgres` only resolve from inside the chain.
- Postgres is also published on `127.0.0.1:15432` for tests without SSH.
- SSH user: `spelunk`. Postgres: `spelunk` / `spelunk`, database `spelunk`.
- The `big` table has 1 M rows with varied types.
- PostgreSQL accepts TLS (without requiring it) with a certificate signed by `keys/ca.crt`,
  valid for `localhost` and `postgres` (not for `127.0.0.1`): that way `verify-full` and
  `verify-ca` are tested, also through the bastions.

## Usage

```bash
bash testing/docker/gen-keys.sh
docker compose -f testing/docker/compose.yaml up -d --build --wait
```

Generated keys (test values, outside git):
- `keys/id_ed25519`: no passphrase.
- `keys/id_ed25519_pass`: passphrase `spelunk-test`.
- `keys/ca.crt`, `keys/server.crt`, `keys/server.key`: PostgreSQL TLS.

Manual check with OpenSSH:

```bash
ssh -i testing/docker/keys/id_ed25519 -J spelunk@127.0.0.1:2221 -p 22 spelunk@bastion2 -N -L 15432:postgres:5432
```
