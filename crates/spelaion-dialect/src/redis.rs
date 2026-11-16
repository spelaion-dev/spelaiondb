//! Redis's language, as the core reads it (ADR-056): one command per line, its arguments
//! quoted as `redis-cli` quotes them. Each line becomes the canonical request the plugin is
//! sent, the argument list: `{"args": ["HGETALL", "user:1"]}`. Valkey speaks the same.
//!
//! What is read:
//! - Arguments separated by spaces; `"…"` with `redis-cli`'s escapes (`\n`, `\r`, `\t`, `\b`,
//!   `\a`, `\xHH`, and a backslash before any other character is that character); `'…'` with
//!   only `\'` as an escape. A closing quote must be followed by a space or the end of the line,
//!   as in `redis-cli`. An argument whose bytes are not UTF-8 is refused for now.
//! - A line whose first character is `#` is a comment; blank lines are skipped.
//!
//! What is refused before anything is sent, because it would change the connection itself
//! rather than the data, or hold it: `AUTH`, `HELLO`, `SELECT` (the tab's database is chosen
//! above the editor), `MULTI`/`EXEC`/`WATCH`, the `SUBSCRIBE` family, `MONITOR`, `QUIT`,
//! `RESET`, replication's own commands, `CLIENT REPLY`/`TRACKING`, and the commands that block
//! until data arrives (`BLPOP` and its kin, `XREAD … BLOCK`, `WAIT`).
//!
//! The guard reads the argument list ([`request_class`]): lists compiled here name the reads
//! and the writes; `FLUSHDB` and `FLUSHALL` delete everything; what reconfigures, stops or
//! empties the server is a drop; `KEYS`, scripts (`EVAL`, `FCALL`) and every command not
//! listed are unknown, never a read.

use serde_json::{Value, json};
use spelaion_driver_api::{Dialect, RequestClass, RequestError, StatementSpan};

/// Redis's language.
#[derive(Debug, Clone, Copy, Default)]
pub struct RedisDialect;

impl Dialect for RedisDialect {
    fn name(&self) -> &'static str {
        "redis"
    }

    fn split_statements(&self, script: &str) -> Vec<StatementSpan> {
        split(script)
    }

    /// A key or any argument, quoted as `redis-cli` would need it.
    fn quote_identifier(&self, ident: &str) -> String {
        quote_arg(ident)
    }

    /// Redis has no placeholders: a `:` is part of keys (`user:1`), so nothing is code.
    fn code_ranges(&self, _script: &str) -> Vec<std::ops::Range<usize>> {
        Vec::new()
    }

    fn reads_requests(&self) -> bool {
        true
    }

    fn to_request(&self, statement: &str, _database: &str) -> Option<Result<Value, RequestError>> {
        // The database is the connection's, chosen above the editor: `SELECT` is refused.
        Some(to_request(statement))
    }

    fn request_class(&self, request: &Value) -> RequestClass {
        request_class(request)
    }

    fn browse_statement(&self, object: &spelaion_driver_api::ObjectInfo) -> Option<String> {
        Some(browse_statement(&object.name, object.comment.as_deref()))
    }
}

/// How many elements opening a key shows: never the whole of a big hash, list or set.
pub const BROWSE_LIMIT: u32 = 200;

/// The statement that shows a key, chosen by its type (the driver lists it as the object's
/// comment): bounded for every type that holds many elements.
pub fn browse_statement(key: &str, key_type: Option<&str>) -> String {
    let key = quote_arg(key);
    let last = BROWSE_LIMIT - 1;
    match key_type {
        Some("string") => format!("GET {key}"),
        Some("hash") => format!("HSCAN {key} 0 COUNT {BROWSE_LIMIT}"),
        Some("list") => format!("LRANGE {key} 0 {last}"),
        Some("set") => format!("SSCAN {key} 0 COUNT {BROWSE_LIMIT}"),
        Some("zset") => format!("ZRANGE {key} 0 {last} WITHSCORES"),
        Some("stream") => format!("XRANGE {key} - + COUNT {BROWSE_LIMIT}"),
        // A module's type (JSON, a time series…) or one the driver did not say: what it is.
        _ => format!("TYPE {key}"),
    }
}

/// `arg` as `redis-cli` reads it back: bare when it holds nothing that needs quoting,
/// otherwise in double quotes with its escapes.
pub fn quote_arg(arg: &str) -> String {
    let bare = !arg.is_empty()
        && !arg.starts_with('#')
        && arg
            .chars()
            .all(|c| !c.is_whitespace() && !c.is_control() && !matches!(c, '"' | '\'' | '\\'));
    if bare {
        return arg.to_string();
    }
    let mut out = String::with_capacity(arg.len() + 2);
    out.push('"');
    for c in arg.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() && (c as u32) < 0x80 => {
                out.push_str(&format!("\\x{:02x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

// ------------------------------------------------------------------------------ splitting

/// One statement per line that holds a command: blank lines and `#` comments are skipped,
/// and a statement ends before its line's trailing spaces.
fn split(script: &str) -> Vec<StatementSpan> {
    let mut spans = Vec::new();
    let mut start = 0;
    for line in script.split_inclusive('\n') {
        let end = start + line.len();
        let text = line.trim_end_matches(['\n', '\r']);
        let lead = text.len() - text.trim_start().len();
        let body = text.trim();
        if !body.is_empty() && !body.starts_with('#') {
            spans.push(StatementSpan {
                start: start + lead,
                end: start + lead + body.len(),
            });
        }
        start = end;
    }
    spans
}

// ------------------------------------------------------------------------------ arguments

fn error(position: usize, message: impl Into<String>) -> RequestError {
    RequestError {
        position,
        message: message.into(),
    }
}

/// The arguments of one line, as `redis-cli` splits them (its `sdssplitargs`), with the byte
/// where each starts.
pub fn split_args(line: &str) -> Result<Vec<(usize, Vec<u8>)>, RequestError> {
    let b = line.as_bytes();
    let mut args = Vec::new();
    let mut i = 0;
    loop {
        while i < b.len() && b[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= b.len() {
            return Ok(args);
        }
        let start = i;
        let mut current = Vec::new();
        loop {
            match b.get(i) {
                None => break,
                Some(c) if c.is_ascii_whitespace() => break,
                Some(b'"') => {
                    let open = i;
                    i += 1;
                    loop {
                        match b.get(i) {
                            None => return Err(error(open, "this double quote is never closed")),
                            Some(b'\\') => {
                                let Some(&next) = b.get(i + 1) else {
                                    return Err(error(open, "this double quote is never closed"));
                                };
                                if next == b'x'
                                    && let (Some(h), Some(l)) = (b.get(i + 2), b.get(i + 3))
                                    && let (Some(h), Some(l)) = (hex(*h), hex(*l))
                                {
                                    current.push(h * 16 + l);
                                    i += 4;
                                    continue;
                                }
                                current.push(match next {
                                    b'n' => b'\n',
                                    b'r' => b'\r',
                                    b't' => b'\t',
                                    b'b' => 0x08,
                                    b'a' => 0x07,
                                    other => other,
                                });
                                i += 2;
                            }
                            Some(b'"') => {
                                i += 1;
                                if b.get(i).is_some_and(|c| !c.is_ascii_whitespace()) {
                                    return Err(error(
                                        i,
                                        "a closing quote must be followed by a space",
                                    ));
                                }
                                break;
                            }
                            Some(&c) => {
                                current.push(c);
                                i += 1;
                            }
                        }
                    }
                    break;
                }
                Some(b'\'') => {
                    let open = i;
                    i += 1;
                    loop {
                        match b.get(i) {
                            None => return Err(error(open, "this single quote is never closed")),
                            Some(b'\\') if b.get(i + 1) == Some(&b'\'') => {
                                current.push(b'\'');
                                i += 2;
                            }
                            Some(b'\'') => {
                                i += 1;
                                if b.get(i).is_some_and(|c| !c.is_ascii_whitespace()) {
                                    return Err(error(
                                        i,
                                        "a closing quote must be followed by a space",
                                    ));
                                }
                                break;
                            }
                            Some(&c) => {
                                current.push(c);
                                i += 1;
                            }
                        }
                    }
                    break;
                }
                Some(&c) => {
                    current.push(c);
                    i += 1;
                }
            }
        }
        args.push((start, current));
    }
}

fn hex(c: u8) -> Option<u8> {
    (c as char).to_digit(16).map(|d| d as u8)
}

// ------------------------------------------------------------------------------ requests

/// Commands that change the connection rather than the data, or hold it: the core keeps the
/// connection's state (its database, its replies) as the session knows it.
const REFUSED: &[(&str, &str)] = &[
    ("AUTH", "the connection logs in with its own credentials"),
    (
        "HELLO",
        "the connection chooses its own protocol and credentials",
    ),
    ("SELECT", "choose the database above the editor"),
    ("MULTI", "transactions are not supported yet"),
    ("EXEC", "transactions are not supported yet"),
    ("DISCARD", "transactions are not supported yet"),
    ("WATCH", "transactions are not supported yet"),
    ("UNWATCH", "transactions are not supported yet"),
    ("SUBSCRIBE", "subscriptions are not supported yet"),
    ("PSUBSCRIBE", "subscriptions are not supported yet"),
    ("SSUBSCRIBE", "subscriptions are not supported yet"),
    ("UNSUBSCRIBE", "subscriptions are not supported yet"),
    ("PUNSUBSCRIBE", "subscriptions are not supported yet"),
    ("SUNSUBSCRIBE", "subscriptions are not supported yet"),
    (
        "MONITOR",
        "it streams every command of the server without end",
    ),
    ("QUIT", "close the tab or disconnect instead"),
    ("RESET", "it resets the connection's own state"),
    ("SYNC", "it is replication's own command"),
    ("PSYNC", "it is replication's own command"),
    ("REPLCONF", "it is replication's own command"),
    ("READONLY", "it changes the connection's cluster mode"),
    ("READWRITE", "it changes the connection's cluster mode"),
    ("BLPOP", "it holds the connection until data arrives"),
    ("BRPOP", "it holds the connection until data arrives"),
    ("BLMOVE", "it holds the connection until data arrives"),
    ("BRPOPLPUSH", "it holds the connection until data arrives"),
    ("BLMPOP", "it holds the connection until data arrives"),
    ("BZPOPMIN", "it holds the connection until data arrives"),
    ("BZPOPMAX", "it holds the connection until data arrives"),
    ("BZMPOP", "it holds the connection until data arrives"),
    ("WAIT", "it holds the connection until replicas answer"),
    (
        "WAITAOF",
        "it holds the connection until the file is written",
    ),
];

/// `CLIENT` subcommands that change how the connection is answered.
const REFUSED_CLIENT: &[&str] = &[
    "REPLY", "TRACKING", "CACHING", "SETINFO", "NO-EVICT", "NO-TOUCH",
];

/// The argument list `statement` sends, or why it is refused.
pub fn to_request(statement: &str) -> Result<Value, RequestError> {
    let args = split_args(statement)?;
    let Some((_, name)) = args.first() else {
        return Err(error(0, "a statement is a Redis command"));
    };
    let mut texts = Vec::with_capacity(args.len());
    for (position, arg) in &args {
        match String::from_utf8(arg.clone()) {
            Ok(text) => texts.push(Value::String(text)),
            Err(_) => {
                return Err(error(
                    *position,
                    "an argument that is not UTF-8 text is not supported yet",
                ));
            }
        }
    }
    let command = String::from_utf8_lossy(name).to_ascii_uppercase();
    if let Some((_, why)) = REFUSED.iter().find(|(c, _)| *c == command) {
        return Err(error(0, format!("`{command}` is not run here: {why}")));
    }
    let sub = |n: usize| {
        args.get(n)
            .map(|(_, a)| String::from_utf8_lossy(a).to_ascii_uppercase())
    };
    if command == "CLIENT"
        && let Some(sub) = sub(1)
        && REFUSED_CLIENT.contains(&sub.as_str())
    {
        return Err(error(
            args[1].0,
            format!("`CLIENT {sub}` is not run here: it changes how the connection is answered"),
        ));
    }
    // `XREAD … BLOCK n` and `XREADGROUP … BLOCK n` wait for entries.
    if matches!(command.as_str(), "XREAD" | "XREADGROUP")
        && let Some((position, _)) = args
            .iter()
            .skip(1)
            .find(|(_, a)| a.eq_ignore_ascii_case(b"BLOCK"))
    {
        return Err(error(
            *position,
            "`BLOCK` is not run here: it holds the connection until data arrives",
        ));
    }
    Ok(json!({ "args": texts }))
}

/// Commands that only read.
const READS: &[&str] = &[
    "GET",
    "MGET",
    "GETRANGE",
    "SUBSTR",
    "STRLEN",
    "LCS",
    "EXISTS",
    "TYPE",
    "TTL",
    "PTTL",
    "EXPIRETIME",
    "PEXPIRETIME",
    "DBSIZE",
    "SCAN",
    "RANDOMKEY",
    "DUMP",
    "TOUCH",
    "HGET",
    "HMGET",
    "HGETALL",
    "HKEYS",
    "HVALS",
    "HLEN",
    "HEXISTS",
    "HSTRLEN",
    "HRANDFIELD",
    "HSCAN",
    "HTTL",
    "HPTTL",
    "HEXPIRETIME",
    "HPEXPIRETIME",
    "LRANGE",
    "LINDEX",
    "LLEN",
    "LPOS",
    "SMEMBERS",
    "SISMEMBER",
    "SMISMEMBER",
    "SCARD",
    "SRANDMEMBER",
    "SINTER",
    "SUNION",
    "SDIFF",
    "SINTERCARD",
    "SSCAN",
    "ZRANGE",
    "ZRANGEBYSCORE",
    "ZREVRANGE",
    "ZREVRANGEBYSCORE",
    "ZRANGEBYLEX",
    "ZREVRANGEBYLEX",
    "ZSCORE",
    "ZMSCORE",
    "ZRANK",
    "ZREVRANK",
    "ZCARD",
    "ZCOUNT",
    "ZLEXCOUNT",
    "ZRANDMEMBER",
    "ZINTER",
    "ZUNION",
    "ZDIFF",
    "ZINTERCARD",
    "ZSCAN",
    "XRANGE",
    "XREVRANGE",
    "XLEN",
    "XPENDING",
    "XREAD",
    "GETBIT",
    "BITCOUNT",
    "BITPOS",
    "BITFIELD_RO",
    "PFCOUNT",
    "GEOPOS",
    "GEODIST",
    "GEOHASH",
    "GEOSEARCH",
    "GEORADIUS_RO",
    "GEORADIUSBYMEMBER_RO",
    "PING",
    "ECHO",
    "TIME",
    "INFO",
    "LASTSAVE",
    "ROLE",
    "LOLWUT",
];

/// Subcommands that only read, by their command.
const READ_SUBCOMMANDS: &[(&str, &[&str])] = &[
    (
        "OBJECT",
        &["ENCODING", "FREQ", "IDLETIME", "REFCOUNT", "HELP"],
    ),
    (
        "MEMORY",
        &["USAGE", "STATS", "DOCTOR", "MALLOC-STATS", "HELP"],
    ),
    ("XINFO", &["STREAM", "GROUPS", "CONSUMERS", "HELP"]),
    ("CONFIG", &["GET", "HELP"]),
    (
        "COMMAND",
        &[
            "COUNT",
            "INFO",
            "DOCS",
            "LIST",
            "GETKEYS",
            "GETKEYSANDFLAGS",
            "HELP",
        ],
    ),
    (
        "CLIENT",
        &[
            "LIST",
            "INFO",
            "GETNAME",
            "ID",
            "GETREDIR",
            "TRACKINGINFO",
            "HELP",
        ],
    ),
    ("SLOWLOG", &["GET", "LEN", "HELP"]),
    (
        "LATENCY",
        &["LATEST", "HISTORY", "GRAPH", "DOCTOR", "HISTOGRAM", "HELP"],
    ),
    ("SCRIPT", &["EXISTS", "HELP"]),
    ("FUNCTION", &["LIST", "DUMP", "STATS", "HELP"]),
    (
        "ACL",
        &[
            "WHOAMI", "LIST", "USERS", "CAT", "GETUSER", "LOG", "DRYRUN", "HELP",
        ],
    ),
    (
        "CLUSTER",
        &[
            "INFO",
            "NODES",
            "SLOTS",
            "SHARDS",
            "KEYSLOT",
            "MYID",
            "MYSHARDID",
            "COUNTKEYSINSLOT",
            "GETKEYSINSLOT",
            "LINKS",
            "HELP",
        ],
    ),
    ("MODULE", &["LIST", "HELP"]),
    (
        "PUBSUB",
        &[
            "CHANNELS",
            "NUMSUB",
            "NUMPAT",
            "SHARDCHANNELS",
            "SHARDNUMSUB",
            "HELP",
        ],
    ),
];

/// Commands that change keys they name.
const WRITES: &[&str] = &[
    "SET",
    "SETNX",
    "SETEX",
    "PSETEX",
    "MSET",
    "MSETNX",
    "GETSET",
    "GETDEL",
    "GETEX",
    "APPEND",
    "SETRANGE",
    "INCR",
    "INCRBY",
    "INCRBYFLOAT",
    "DECR",
    "DECRBY",
    "DEL",
    "UNLINK",
    "EXPIRE",
    "PEXPIRE",
    "EXPIREAT",
    "PEXPIREAT",
    "PERSIST",
    "RENAME",
    "RENAMENX",
    "COPY",
    "MOVE",
    "RESTORE",
    "HSET",
    "HSETNX",
    "HMSET",
    "HDEL",
    "HINCRBY",
    "HINCRBYFLOAT",
    "HEXPIRE",
    "HPEXPIRE",
    "HEXPIREAT",
    "HPEXPIREAT",
    "HPERSIST",
    "HGETDEL",
    "HGETEX",
    "HSETEX",
    "LPUSH",
    "RPUSH",
    "LPUSHX",
    "RPUSHX",
    "LPOP",
    "RPOP",
    "LSET",
    "LINSERT",
    "LREM",
    "LTRIM",
    "LMOVE",
    "LMPOP",
    "RPOPLPUSH",
    "SADD",
    "SREM",
    "SPOP",
    "SMOVE",
    "SINTERSTORE",
    "SUNIONSTORE",
    "SDIFFSTORE",
    "ZADD",
    "ZREM",
    "ZINCRBY",
    "ZPOPMIN",
    "ZPOPMAX",
    "ZMPOP",
    "ZREMRANGEBYRANK",
    "ZREMRANGEBYSCORE",
    "ZREMRANGEBYLEX",
    "ZRANGESTORE",
    "ZINTERSTORE",
    "ZUNIONSTORE",
    "ZDIFFSTORE",
    "XADD",
    "XDEL",
    "XTRIM",
    "XACK",
    "XCLAIM",
    "XAUTOCLAIM",
    "XGROUP",
    "XREADGROUP",
    "XSETID",
    "SETBIT",
    "BITOP",
    "BITFIELD",
    "PFADD",
    "PFMERGE",
    "GEOADD",
    "GEOSEARCHSTORE",
    "GEORADIUS",
    "GEORADIUSBYMEMBER",
    "SORT",
    "PUBLISH",
    "SPUBLISH",
];

/// What drops, reconfigures, stops or empties the server, by command and, where it is one of
/// several, subcommand (`None`: the whole command).
const DROPS: &[(&str, Option<&[&str]>)] = &[
    ("SHUTDOWN", None),
    ("SWAPDB", None),
    ("DEBUG", None),
    ("REPLICAOF", None),
    ("SLAVEOF", None),
    ("FAILOVER", None),
    ("CONFIG", Some(&["SET", "REWRITE", "RESETSTAT"])),
    ("SCRIPT", Some(&["FLUSH", "KILL"])),
    ("FUNCTION", Some(&["FLUSH", "DELETE", "RESTORE", "KILL"])),
    ("MODULE", Some(&["LOAD", "LOADEX", "UNLOAD"])),
    ("ACL", Some(&["SETUSER", "DELUSER", "LOAD", "SAVE"])),
    ("CLIENT", Some(&["KILL", "PAUSE", "UNPAUSE", "UNBLOCK"])),
    (
        "CLUSTER",
        Some(&[
            "RESET",
            "FAILOVER",
            "FORGET",
            "FLUSHSLOTS",
            "DELSLOTS",
            "DELSLOTSRANGE",
            "SETSLOT",
            "ADDSLOTS",
            "ADDSLOTSRANGE",
            "MEET",
            "REPLICATE",
        ]),
    ),
    ("LATENCY", Some(&["RESET"])),
    ("SLOWLOG", Some(&["RESET"])),
];

/// What the guard makes of an argument list [`to_request`] wrote.
pub fn request_class(request: &Value) -> RequestClass {
    let Some(args) = request.get("args").and_then(Value::as_array) else {
        return RequestClass::Unknown;
    };
    let word = |n: usize| {
        args.get(n)
            .and_then(Value::as_str)
            .map(str::to_ascii_uppercase)
    };
    let Some(command) = word(0) else {
        return RequestClass::Unknown;
    };
    let sub = word(1);
    if matches!(command.as_str(), "FLUSHDB" | "FLUSHALL") {
        return RequestClass::DeleteAll;
    }
    for (name, subs) in DROPS {
        if *name == command {
            match subs {
                None => return RequestClass::Drop,
                Some(subs) if sub.as_deref().is_some_and(|s| subs.contains(&s)) => {
                    return RequestClass::Drop;
                }
                Some(_) => {}
            }
        }
    }
    if let Some((_, subs)) = READ_SUBCOMMANDS.iter().find(|(c, _)| *c == command) {
        return match sub {
            Some(s) if subs.contains(&s.as_str()) => RequestClass::Read,
            _ => RequestClass::Unknown,
        };
    }
    if READS.contains(&command.as_str()) {
        // `SORT … STORE` writes; `SORT_RO` is the read.
        return RequestClass::Read;
    }
    if command == "SORT_RO" {
        return RequestClass::Read;
    }
    if WRITES.contains(&command.as_str()) {
        return RequestClass::Write;
    }
    // `KEYS` walks the whole keyspace; scripts (`EVAL`, `FCALL`…) run code the core cannot
    // see; and anything not listed here is never taken for a read.
    RequestClass::Unknown
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(line: &str) -> Vec<String> {
        split_args(line)
            .unwrap()
            .into_iter()
            .map(|(_, a)| String::from_utf8(a).unwrap())
            .collect()
    }

    fn class(line: &str) -> RequestClass {
        request_class(&to_request(line).unwrap())
    }

    #[test]
    fn a_line_is_split_as_redis_cli_splits_it() {
        assert_eq!(args("HGETALL user:1"), ["HGETALL", "user:1"]);
        assert_eq!(args("  SET   k   v  "), ["SET", "k", "v"]);
        assert_eq!(
            args(r#"SET "a key" "line\none""#),
            ["SET", "a key", "line\none"]
        );
        assert_eq!(
            args(r#"SET k "tab\there \"q\" \\ \x41\x62""#),
            ["SET", "k", "tab\there \"q\" \\ Ab"]
        );
        assert_eq!(args(r"SET k 'it\'s \n raw'"), ["SET", "k", r"it's \n raw"]);
        assert_eq!(args(r#"SET k "ñandú""#), ["SET", "k", "ñandú"]);
        // A quote inside a bare argument opens a quoted part of the same argument, which
        // must end the argument.
        assert_eq!(args(r#"SET a"b c" x"#), ["SET", "ab c", "x"]);
        assert_eq!(args(r#"SET k """#), ["SET", "k", ""]);
        // An escape that is not one keeps the character.
        assert_eq!(args(r#"SET k "\q\xZZ""#), ["SET", "k", "qxZZ"]);
    }

    #[test]
    fn what_redis_cli_refuses_is_refused_at_its_position() {
        let at = |line: &str| split_args(line).unwrap_err().position;
        assert_eq!(at(r#"SET k "open"#), 6);
        assert_eq!(at("SET k 'open"), 6);
        assert_eq!(at(r#"SET k "a"b"#), 9);
        assert_eq!(at("SET k 'a'b"), 9);
        assert_eq!(at(r#"SET a"b c"d"#), 10);
        assert_eq!(at(r#"SET k "ends\"#), 6);
        assert_eq!(
            to_request(r#"SET k "\xff""#).unwrap_err().message,
            "an argument that is not UTF-8 text is not supported yet"
        );
    }

    #[test]
    fn a_statement_is_a_line_and_comments_and_blank_lines_are_skipped() {
        let script = "# keys of the demo\nSET a 1\r\n\n   GET a   \n#GET b\nDEL a";
        let texts: Vec<&str> = split(script).iter().map(|s| s.text(script)).collect();
        assert_eq!(texts, ["SET a 1", "GET a", "DEL a"]);
        assert!(split("\n  \n# only a comment\n").is_empty());
    }

    #[test]
    fn the_request_is_the_argument_list() {
        assert_eq!(
            to_request("hset user:1 name \"Ana María\"").unwrap(),
            json!({"args": ["hset", "user:1", "name", "Ana María"]})
        );
        assert!(to_request("   ").is_err());
    }

    #[test]
    fn what_changes_or_holds_the_connection_is_refused_before_it_is_sent() {
        for line in [
            "AUTH user secret",
            "auth secret",
            "HELLO 3",
            "select 2",
            "MULTI",
            "EXEC",
            "WATCH k",
            "SUBSCRIBE news",
            "psubscribe n*",
            "MONITOR",
            "QUIT",
            "RESET",
            "BLPOP q 0",
            "BZPOPMIN z 0",
            "WAIT 1 0",
            "CLIENT REPLY OFF",
            "client tracking on",
            "XREAD COUNT 1 BLOCK 0 STREAMS s $",
            "XREADGROUP GROUP g c BLOCK 100 STREAMS s >",
        ] {
            let refused = to_request(line).unwrap_err();
            assert!(
                refused.message.contains("is not run here"),
                "{line}: {refused}"
            );
        }
        // The secret of a refused AUTH is not repeated in its error.
        assert!(
            !to_request("AUTH user hunter2")
                .unwrap_err()
                .message
                .contains("hunter2")
        );
        // Their harmless neighbours are not.
        assert_eq!(class("CLIENT LIST"), RequestClass::Read);
        assert_eq!(class("XREAD COUNT 10 STREAMS s 0"), RequestClass::Read);
    }

    #[test]
    fn the_guard_reads_the_command() {
        for line in [
            "GET k",
            "hgetall h",
            "SCAN 0 MATCH user:* COUNT 100",
            "ZRANGE z 0 -1 WITHSCORES",
            "INFO keyspace",
            "CONFIG GET maxmemory",
            "OBJECT ENCODING k",
            "XINFO STREAM s",
            "SORT_RO l",
        ] {
            assert_eq!(class(line), RequestClass::Read, "{line}");
        }
        for line in [
            "SET k v",
            "del a b",
            "HSET h f v",
            "EXPIRE k 10",
            "XADD s * f v",
            "RENAME a b",
        ] {
            assert_eq!(class(line), RequestClass::Write, "{line}");
        }
        for line in ["FLUSHDB", "flushall async"] {
            assert_eq!(class(line), RequestClass::DeleteAll, "{line}");
        }
        for line in [
            "CONFIG SET maxmemory 1",
            "config rewrite",
            "SCRIPT FLUSH",
            "FUNCTION DELETE lib",
            "SHUTDOWN NOSAVE",
            "SWAPDB 0 1",
            "DEBUG SLEEP 10",
            "CLIENT KILL ID 3",
            "ACL SETUSER u on",
            "REPLICAOF host 6379",
            "MODULE LOAD x.so",
        ] {
            assert_eq!(class(line), RequestClass::Drop, "{line}");
        }
        // Never taken for reads: the whole keyspace, code on the server, what is not listed,
        // and a subcommand of a listed command that is not.
        for line in [
            "KEYS *",
            "EVAL \"return 1\" 0",
            "EVAL_RO \"return 1\" 0",
            "FCALL f 0",
            "FCALL_RO f 0",
            "JSON.GET k",
            "NOSUCH x",
            "CONFIG NOSUCH",
            "OBJECT",
            "SORT l STORE d",
        ] {
            assert_ne!(class(line), RequestClass::Read, "{line}");
        }
        assert_eq!(request_class(&json!({})), RequestClass::Unknown);
        assert_eq!(request_class(&json!({"args": []})), RequestClass::Unknown);
    }

    #[test]
    fn a_key_opens_with_a_bounded_command_of_its_type() {
        assert_eq!(
            browse_statement("user:1", Some("hash")),
            "HSCAN user:1 0 COUNT 200"
        );
        assert_eq!(browse_statement("q", Some("list")), "LRANGE q 0 199");
        assert_eq!(browse_statement("s", Some("set")), "SSCAN s 0 COUNT 200");
        assert_eq!(
            browse_statement("z", Some("zset")),
            "ZRANGE z 0 199 WITHSCORES"
        );
        assert_eq!(
            browse_statement("x", Some("stream")),
            "XRANGE x - + COUNT 200"
        );
        assert_eq!(browse_statement("k", Some("string")), "GET k");
        assert_eq!(browse_statement("j", Some("ReJSON-RL")), "TYPE j");
        assert_eq!(browse_statement("j", None), "TYPE j");
        // A key that needs quoting is quoted, and reads back as itself.
        let statement = browse_statement("a key \"q\"\n", Some("string"));
        assert_eq!(statement, r#"GET "a key \"q\"\n""#);
        assert_eq!(args(&statement), ["GET", "a key \"q\"\n"]);
        // Every browse statement is a read.
        for t in ["string", "hash", "list", "set", "zset", "stream", "other"] {
            assert_eq!(
                class(&browse_statement("k", Some(t))),
                RequestClass::Read,
                "{t}"
            );
        }
    }

    #[test]
    fn a_quoted_argument_reads_back_as_itself() {
        for arg in [
            "plain",
            "user:1",
            "",
            "#tag",
            "a b",
            "q\"uote",
            "it's",
            "back\\slash",
            "tab\tline\nret\r",
            "\u{1}ctl",
            "ñandú 🦀",
        ] {
            let quoted = quote_arg(arg);
            assert_eq!(args(&format!("GET {quoted}")), ["GET", arg], "{quoted}");
        }
    }
}
