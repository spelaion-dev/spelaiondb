# Search engine consoles

Elasticsearch, OpenSearch, Solr and Meilisearch use their HTTP JSON APIs. Their driver is the
first-party `spelaiondb-plugin-search` plugin, shared by the four engines and checked against
each one's own server. Open Distro is retired and is not offered.

## Connect

Choose the engine in Add connection and enter its host and HTTP port: normally 9200 for
Elasticsearch and OpenSearch, 8983 for Solr, or 7700 for Meilisearch. Choose the TLS mode of
that endpoint; HTTPS often uses another port. SSH tunnels, proxies and Kubernetes forwards
are opened by the core, and the plugin verifies TLS against the real host name.

Enter a user and password for HTTP Basic authentication. With no user, Elasticsearch's
password field takes its API key and Meilisearch's takes its bearer token. OpenSearch and
Solr currently support Basic authentication or an anonymous server. A password or token can
come from the keychain, a prompt or any existing secret reference; it never goes into the
connection file, a URL, a log or a command line.

The plugin's first official release must be pinned before its cards become available in a
shipped app. A development build can exercise the complete flow by pointing each
`SPELAION_DEV_PLUGIN_<ENGINE>` variable at its locally built executable. Release builds ignore
those overrides.

## Write requests

A request is an uppercase method and a path from `/`, followed by an optional JSON body:

```http
GET /orders/_search
{
  "query": {"match": {"name": "Spelaion"}},
  "size": 200
}

GET /orders/_count
```

Solr:

```http
GET /solr/orders/select?q=*%3A*&rows=200&wt=json
```

Meilisearch:

```http
POST /indexes/orders/search
{"q": "Spelaion", "limit": 200}
```

Each method line starts a new request outside its JSON body. Ctrl+Enter runs the current
request; Ctrl+Shift+Enter runs the script. The HTTP editor highlights methods and JSON and
suggests paths from the explorer's loaded index names. Format formats a single request's
JSON body; select one request before formatting a script.

Only paths on this connection's server are accepted. Full URLs, host headers, dot segments,
escaped path segments and duplicate query parameter names are refused. Query values support
percent encoding. Headers come from the connection, never from the editor. Bodies must be
JSON; SQL endpoints, NDJSON bulk scripts and arbitrary shell programs have no editor mode.
No HTTP redirect is followed and a request lost on the network is never retried.

## Browse and results

The explorer's `indexes` schema lists indexes as collections. Solr currently lists standalone
cores; SolrCloud collection discovery is not implemented. Opening an index opens a visible
request bounded to 200 hits in an editor tab. It does not open a relational table-data tab.
Index names that require escaped path segments cannot currently be opened this way.

Elasticsearch and OpenSearch hits have `_index`, `_id`, `_score`, then source fields. Numbers,
booleans and strings keep their types; nested values and aggregations are JSON. First-batch
column types remain fixed. The final `…` JSON column keeps late fields and values whose type
changes, so they are not lost. Results use the ordinary grid, stored-result filters, find,
copy, charts and cell viewer. Each response is bounded to 64 MiB; narrow a larger request.

## Safeguards and current limits

The core parses the request before sending it. Deletes, index drops, delete/update-by-query
and Solr's destructive administrative actions ask for confirmation. Unknown routes and
requests that run server scripts ask wherever the connection type confirms data changes;
they are never classified as reads. Production safeguards apply as they do to SQL.

These engines have no verified read barrier yet: read-only connections and agents are
unavailable, and a file that requests read-only mode or SQL session setup is refused.
Relational editing, imports, streaming exports, ER diagrams and manual transactions are
unavailable. This first plugin does not cancel a statement on the server or automatically
page beyond the submitted request. Limits and server-side cancellation need their own engine
work; the capability flags do not claim them.
