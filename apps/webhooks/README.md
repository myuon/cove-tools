# The webhook lab

A Cove app on cove-host (issue #2). It hands out **receive URLs** that keep
every request sent to them, and **admin pages** to read what was received.
Handling and HTML are Cove. Persistence, the clock, randomness and the admin
credential check are the host's typed modules.

| path (below `/webhooks`) | what | auth |
| --- | --- | --- |
| `ANY /in/<endpoint>[/...]` | stores the request: method, the path below the receive URL, query, headers, body, and the time it arrived. Answers `200 {"ok":true,"endpoint":…,"event":…}` | none; the endpoint id (16 random hex digits) is the secret part of the URL |
| `GET /admin` | lists the endpoints, with a form to create one | admin |
| `POST /admin/endpoints` | creates an endpoint from `name`, `retention`, `maxBody` and `mask` fields. Answers 303 to its page, or 201 JSON with `Accept: application/json` | admin |
| `GET /admin/e/<endpoint>` | the endpoint's history, newest first, 50 per page (`?before=`). `?format=json` gives the same as JSON | admin |
| `GET /admin/e/<endpoint>/events/<event>` | one request in full: summary, query, headers, the body (also pretty-printed when it is JSON), and a copy button. `?view=json` gives the stored event as JSON; `?view=raw` gives the body alone, as plain text | admin |
| `POST /admin/e/<endpoint>/events/<event>/resend` | resends the stored request to the form's `url` (one `[fetch] allow` admits) and keeps what came back; `?view=resends` on the request lists the attempts as JSON | admin |
| `POST /admin/e/<endpoint>/clear`, `…/delete`, `…/events/<event>/delete` | deletes the endpoint's history, the endpoint with its history, or one request | admin |

## Running it

```console
$ export WEBHOOKS_ADMIN_TOKEN=change-me      # the admin secret; the app is refused without it
$ ./target/checked/cove-host serve --apps apps
$ curl -s -u admin:change-me -H 'accept: application/json' -d 'name=github' \
    http://127.0.0.1:8080/webhooks/admin/endpoints
{"id":"fd9f7a5017d31e7e","name":"github","url":"http://127.0.0.1:8080/webhooks/in/fd9f7a5017d31e7e"}
$ curl -s -H 'content-type: application/json' -H 'x-github-event: push' \
    -H 'authorization: Bearer ghs_secret' -d '{"ref":"refs/heads/main"}' \
    'http://127.0.0.1:8080/webhooks/in/fd9f7a5017d31e7e/push?delivery=1'
{"endpoint":"fd9f7a5017d31e7e","event":"1791171122814503-09b5","ok":true}
$ curl -s -u admin:change-me 'http://127.0.0.1:8080/webhooks/admin/e/fd9f7a5017d31e7e?format=json'
{
  "endpoint": {
    "created": 1791171122675,
    "id": "fd9f7a5017d31e7e",
    "mask": true,
    "maxBody": 65536,
    "name": "github",
    "retention": 100
  },
  "events": [
    {
      "bytes": 25,
      "contentType": "application/json",
      "id": "1791171122814503-09b5",
      "method": "POST",
      "path": "/push",
      "receivedAt": "2026-10-05T03:32:02.814Z"
    }
  ]
}
```

Stop the host (Ctrl-C) and start it again: the history is still there, because
it lives in the app's store, `data/webhooks/kv.sqlite3`. Then resend it to a
second endpoint:

```console
$ curl -s -u admin:change-me -H 'accept: application/json' -d 'name=staging' \
    http://127.0.0.1:8080/webhooks/admin/endpoints
{"id":"22e210190123a8df","name":"staging","url":"http://127.0.0.1:8080/webhooks/in/22e210190123a8df"}
$ curl -s -u admin:change-me -H 'accept: application/json' \
    --data-urlencode 'url=http://127.0.0.1:8080/webhooks/in/22e210190123a8df/replayed' \
    http://127.0.0.1:8080/webhooks/admin/e/fd9f7a5017d31e7e/events/1791171122814503-09b5/resend
{
  "at": 1791171532766,
  "atIso": "2026-10-05T03:38:52.766Z",
  "body": "{\"endpoint\":\"22e210190123a8df\",\"event\":\"1791171532766856-a40e8c42\",\"ok\":true}\n",
  "bytes": 78,
  "error": "",
  "headers": { … },
  "id": "1791171532766257",
  "method": "POST",
  "millis": 1,
  "ok": true,
  "status": 200,
  "truncated": false,
  "url": "http://127.0.0.1:8080/webhooks/in/22e210190123a8df/replayed"
}
$ curl -s -u admin:change-me -H 'accept: application/json' -d 'url=http://example.org/x' \
    http://127.0.0.1:8080/webhooks/admin/e/fd9f7a5017d31e7e/events/1791171122814503-09b5/resend | grep -e '"ok"' -e error
  "error": "`http://example.org:80` is not on app `webhooks`'s fetch allowlist (http://127.0.0.1:8080, http://localhost:8080)",
  "ok": false,
```

`staging` now holds the request at `/replayed`: same method, body and
headers. In the browser, the request's page has a **Resend** form and the
table of earlier attempts. In a browser, open
`http://127.0.0.1:8080/webhooks/admin` and log in with any user name and the
secret as the password.

## Admin access

On this host an app checks a credential with `auth.check(secret, header)`. The
host compares the `Authorization` header it is given against the secret named
in `app.toml` under `[secrets]`: `admin = { env = "WEBHOOKS_ADMIN_TOKEN" }`
here (`file = "..."` and, for tests, `value = "..."` also work). It accepts
`Bearer <secret>` and `Basic <base64(any-user:secret)>`, so a browser's login
prompt works. The comparison is constant-time, and the secret never enters the
app's run: the code can check a credential but cannot read one, so it can't
log it or leak it in an error.

Receive URLs and admin pages are separate paths. Only `/admin/` asks for the
secret, and nothing under `/in/` can read or change what is stored.

**Cross-site forms.** A browser re-sends a stored Basic login with any request
to the site, including a form posted from another site. So a request that
changes something is refused (403) when `Sec-Fetch-Site` says it came from
another site, or when `Origin` names another origin. curl sends neither header
and is allowed, because it holds the secret itself.

**Page headers.** Every admin page is sent with a `Content-Security-Policy`
(no script but the page's own copy buttons, no framing, forms to the lab
only), `X-Content-Type-Options: nosniff`, `X-Frame-Options: DENY`, no referrer
and `Cache-Control: no-store`.

## What is stored, and how much

- **Retention:** each endpoint keeps its last `retention` requests (1–1000,
  default 100). The oldest are deleted as new ones arrive.
- **Body size:** each endpoint stores at most `maxBody` bytes of a body (up to
  1 MiB, default 64 KiB), cut at a character boundary. The event records the
  full size and that it was truncated.
- **Host limits:** the host refuses a body over the app's `max_request_bytes`
  (1 MiB) with 413 before the app runs. The app's KV quotas (`[kv]` in
  `app.toml`) bound the whole store. A write past a quota answers the sender
  507 and stores nothing.
- **Masking:** on an endpoint that masks (the default), the values of
  `authorization`, `proxy-authorization`, `cookie`, `set-cookie`,
  `x-api-key`, `x-auth-token` and `x-access-token` are replaced by
  `[masked: N characters]` **before anything is stored**, and the event lists
  which headers were masked. An endpoint created with `mask=off` (or with the
  checkbox unticked) stores every header as sent.
- **Body encoding:** bodies must be UTF-8 (the host answers 400 otherwise).
  Binary webhooks are out of scope for now.

## Resending

From a request's page (or `POST …/events/<event>/resend` with `url=`), the
stored request goes out again with `fetch.request`:

- **What is sent:** the same method and body, and the headers less the ones
  that belong to the connection it arrived on (`host`, `content-length`,
  `connection`, `transfer-encoding`, …), the host's own `x-forwarded-*`,
  `accept-encoding` (the app could not decode a compressed answer), and any
  header that was **masked**. Only a placeholder of a masked header was
  stored, and sending that as though it were the value would be wrong.
- **Where it may go:** only a URL that `[fetch] allow` in `app.toml` admits.
  The shipped config allows this host on port 8080, which is what the
  walkthrough uses; add the services you replay to. A URL off the list is
  refused by the host before anything is sent.
- **While it is out**, the run is parked: it holds no worker. `[fetch]
  timeout = "8s"` keeps a slow target below the run's 10 s deadline, so it is
  stored as a failure rather than answered 504.
- **What is kept:** every attempt, as one record under the request:
  - when it was sent, the URL, and how long it took;
  - for a response (any status): its status, headers and body (truncated to
    the endpoint's `maxBody`);
  - for no response: why (refused by the allowlist, could not connect, timed
    out, too large).

  The newest 20 per request are kept. Deleting the request, its endpoint's
  history or the endpoint deletes its attempts too. A response is shown as
  text, escaped, like everything else.
- Resending is manual, one request at a time. Automatic retries or forwarding
  on receipt are not part of this version.

## Display and escaping

Every value a sender controls goes through `text.escapeHtml` before it becomes
part of a page: the endpoint name, the path, the query, the header names and
values, and the body. The page's own markup is the only markup on it. The raw
view is `text/plain` with `nosniff`, so a body that is HTML is shown as text,
not rendered. The tests send `<script>` and `<img onerror>` in every one of
those places.

## Layout

| file | what |
| --- | --- |
| `webhooks.cove` | routing, receiving, the admin actions |
| `store.cove` | the records in `kv`, as JSON: `endpoint:<id>`; `index:<id>:<event>` (a summary for listing and trimming); `event:<id>:<event>` (the request in full). An event id is its arrival time in microseconds, zero-padded to 16 digits and strictly increasing (`time.nowMicros()`), plus a random suffix, so keys sort by arrival |
| `resend.cove` | resending, and the attempts, `resend:<id>:<event>:<n>` |
| `pages.cove` | the HTML |
| `json/` | a JSON parser and renderer, adapted from the Cove repository's `examples/cq/json` (with `\u` escapes and an indented renderer) |
| `text/` | HTML escaping, form decoding (`+`, `%XX`, UTF-8), ISO 8601 times, byte sizes, truncation at a character boundary |

`cove-host test webhooks` runs the Cove tests in `json/` and `text/`. The Rust
tests in `crates/cove-host/tests/webhooks.rs` drive the app over HTTP.
