# HeidiSQL 12.22 fixture

`heidisql-12.22.txt` is the unmodified settings file written by the official
64-bit Windows portable HeidiSQL 12.22 release (portable ZIP SHA-256
`7b36821fa3e16cc37b511d41747efa4c8b980d1e3fa937e52c5931bcc4ee79b4`).
The executable's Authenticode signature was verified against Ansgar Becker.

In a dedicated temporary directory, we created `portable.lock` and an empty
portable settings file, then seeded the session settings in that file by
commands using entirely fictitious example values. We started HeidiSQL hidden
with `--psettings` pointing to that file and connected `Examples\\Local` to
our disposable MySQL test container with its public test credentials. HeidiSQL
loaded those seeded sessions and rewrote the file, adding the successful
connection metadata. The seeded SSH session was not connected. We stopped only
that process and removed its exact `HeidiSQL Portable <PID>` temporary registry
key. No personal HeidiSQL sessions or registry key were read or modified.
The sessions were seeded by commands, not created in the session manager.

The seeded settings were `Updatecheck=0`, `DoUsageStatistics=0`,
`RestoreTabs=0`, folder `Examples`, and these two sessions:

- `Local`: MySQL TCP (`NetType=0`), `Host=127.0.0.1`, `Port=13306`,
  `User=spelaion`, `Databases=spelaion`.
- `SSH`: MySQL over SSH (`NetType=2`, `SSHtunnelActive=1`),
  `Host=database.example.test`, `Port=3306`, `User=reader`,
  `SSHtunnelHost=bastion.example.test`, `SSHtunnelHostPort=2222`,
  `SSHtunnelUser=tunnel-user`, and `SSHtunnelPrivateKey=C:\fixture\id_ed25519`.

The initial settings were serialized in HeidiSQL's `path<|||>type<|||>value`
format, with type `1` for strings and `3` for integers. We launched the binary
with `--psettings=<isolated settings file> -d=Examples\Local -p=spelaion`,
confirmed the process-specific portable registry key existed and saw an active
connection from that PID to the disposable MySQL server, then stopped that PID.
The database password is the test container's disposable password; it was passed
on the command line for that connection and did not enter this file.

The checked-in file has SHA-256
`3ead9131f0a3ac4a1a7f9091df82b573c478b385caeaeb78b1be2cf88f6b3a98`.
The path, user names and SSH key path in it are examples and no private key is
included. No passwords are stored in this particular output; separate parser
tests use synthetic, non-secret encoded values. Its output format and session keys can be checked against HeidiSQL's
[`apphelpers.pas`](https://github.com/HeidiSQL/HeidiSQL/blob/v12.22/source/apphelpers.pas)
and [`dbconnection.pas`](https://github.com/HeidiSQL/HeidiSQL/blob/v12.22/source/dbconnection.pas).
