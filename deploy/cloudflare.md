# Cloudflare: the tunnel and Access in front of cove-tools

The host listens on `127.0.0.1:8790` only (`deploy/cove-tools.service`). The
public way in is the machine's existing **remotely-managed** Cloudflare
Tunnel (cloudflared runs as a system service with `--token`, so its routes
are edited in the dashboard, not in a file on the machine), and **Cloudflare
Access** sits in front of every path but the few that callers outside need.

```
browser ──https──▶ Cloudflare edge ──(Access: owner's login, or a bypass)──▶ tunnel ──▶ cloudflared ──http──▶ 127.0.0.1:8790 (cove-host, public)
  covtools.ramda.io, covtools-admin.ramda.io: one tunnel, one port; the host routes by Host header
                                                                                          127.0.0.1:8791 (cove-host, admin) ◀── ssh -L only
```

## 1. The public hostname

Zero Trust dashboard → **Networks → Tunnels** → the machine's tunnel →
**Configure** → **Public Hostname** (called *Published application routes*
in newer dashboards) → **Add a public hostname**:

| field | value |
| --- | --- |
| Subdomain | `covtools` |
| Domain | `ramda.io` |
| Path | *(empty)* |
| Service type | `HTTP` |
| URL | `localhost:8790` |

Leave *Additional application settings → HTTP Settings → HTTP Host Header*
empty. The host does not depend on what cloudflared forwards for the scheme
or the host: it is started with `--public-origin https://covtools.ramda.io`,
so the apps write `https://covtools.ramda.io/...` URLs and accept a form
whose `Origin` is that, whatever the request from cloudflared says.

Saving creates the proxied DNS record `covtools.ramda.io` for the tunnel.
**Do not save it before step 2**: until the Access application exists, the
hostname is open to everyone. (Or save it with the service pointed at
`http_status:404` and switch it once Access is in place.)

**Never add a route to `localhost:8791`.** That is the admin listener:
updates with a bearer token, and the operations views (`/_host/stats`,
`/_host/ui`, an app's JSON and logs) without one. It is reached from the
owner's machine only, over SSH:

```console
$ ssh -L 8791:127.0.0.1:8791 whisky
# then http://localhost:8791/_host/ui in a browser
```

The public listener answers 404 for everything under `/_host/`
(`--ops-listener admin`), so nothing of it is reachable through the tunnel
even by the owner.

## 2. Access: the owner, everywhere

Zero Trust → **Access → Applications** → **Add an application** →
**Self-hosted**:

- Application name: `cove-tools`
- Session duration: `24 hours` (or what you prefer)
- Application domain: subdomain `covtools`, domain `ramda.io`, path *empty*
  (the whole hostname)
- Identity providers: the one you log in with (One-time PIN works with no
  setup)
- Policy: name `owner`, action **Allow**, *Include* → **Emails** → the
  owner's address (only that one)

Everything under `https://covtools.ramda.io/` now needs that login first:
the webhook lab's admin pages, the ledger's pages, the algorithm playground.
The webhook lab's admin pages ask for their own secret on top
(`WEBHOOKS_ADMIN_TOKEN` as the password of the browser's login prompt, any
user name).

## 3. The paths callers outside need

Access matches the most specific application path first, so each of these
is an application of its own on a path under the hostname.

### Webhook receive URLs: `/webhooks/in/` — Bypass

Senders (GitHub, Stripe, ...) cannot log in or add headers, so this path has
to be open.

- **Add an application** → Self-hosted, name `cove-tools webhooks receive`
- Application domain: `covtools` . `ramda.io`, path **`webhooks/in`**
  (Access covers the path and everything below it; `webhooks/in/*` is the
  same)
- Policy: name `senders`, action **Bypass**, *Include* → **Everyone**

Its own authentication: **none but the URL**. A receive URL is
`/webhooks/in/<id>`, and the id is 16 random hex digits (64 bits); anyone who
has the URL can post to it, which is what a webhook URL is. An endpoint stores
at most its own `maxBody` of a request and keeps its `retention` newest
requests; the app answers 404 for an unknown id. Delete an endpoint (its admin
page) to revoke its URL. Nothing else in the webhook lab is under
`/webhooks/in/`: the admin pages are `/webhooks/admin/...` and stay behind
the owner's login.

If Cloudflare's *Bot Fight Mode* or a WAF rule challenges a sender (a
`403`/challenge page in the sender's delivery log), add a WAF *Skip* rule for
`http.host eq "covtools.ramda.io" and starts_with(http.request.uri.path,
"/webhooks/in/")`.

### The ledger's posting API: `/ledger/api/runs` — a service token (preferred) or Bypass

`POST /ledger/api/runs` stores a run and `DELETE /ledger/api/runs/<id>`
deletes one; both need `Authorization: Bearer <LEDGER_TOKEN>` from the app
itself. But under the same path `GET /ledger/api/runs` and
`GET /ledger/api/runs/<id>` answer the run list and every stored run **with
no secret** (`apps/ledger/README.md`), and Access matches paths, not methods.
So:

- **Preferred — a service token.** The poster is CI, which can send two more
  headers. Zero Trust → **Access → Service auth → Service Tokens** →
  **Create**, name `ledger-poster`; keep the Client ID and Secret. Then
  **Add an application** → Self-hosted, name `cove-tools ledger API`,
  domain `covtools` . `ramda.io`, path **`ledger/api/runs`**, with two
  policies: `poster`, action **Service Auth**, *Include* → **Service Token**
  → `ledger-poster`; and `owner`, action **Allow**, the owner's email as in
  step 2. The poster sends

  ```console
  $ curl -X POST https://covtools.ramda.io/ledger/api/runs \
      -H "CF-Access-Client-Id: $CF_ACCESS_CLIENT_ID" \
      -H "CF-Access-Client-Secret: $CF_ACCESS_CLIENT_SECRET" \
      -H "Authorization: Bearer $LEDGER_TOKEN" \
      -H 'content-type: application/json' --data @run.json
  ```

  and the run list stays private.

- **Or Bypass**, if the poster cannot send the Access headers: the same
  application with one policy, action **Bypass**, *Include* → **Everyone**.
  Posting and deleting are still guarded by `LEDGER_TOKEN`; but the run list
  and every run's JSON become public, read-only. Decide whether that is
  acceptable for the benchmark data you post.

## 4. The admin UI: `covtools-admin.ramda.io`

`apps/admin` is reached by hostname only (`[route] hosts` in its
`app.toml`): a request with `Host: covtools-admin.ramda.io` reaches it with the
whole path, and `https://covtools.ramda.io/admin/` is a 404 from the host. It
has its own hostname, its own Access application and its own secret, so the
main hostname's bypass paths (`/webhooks/in`, `/ledger/api/runs`) never apply
to it.

**Order matters, as in section 1: create the Access application first, then
the public hostname** (or save the hostname with the service pointed at
`http_status:404` and switch it once Access is in place).

### Access: the owner, and nothing else

Zero Trust → **Access → Applications** → **Add an application** →
**Self-hosted**:

- Application name: `cove-tools admin` (a separate application from
  `cove-tools`)
- Session duration: short, `1 hour` or `2 hours`
- Application domain: subdomain `covtools-admin`, domain `ramda.io`, path
  *empty* (the whole hostname)
- Identity providers: the one you log in with; optionally allow only a
  specific login method
- Policy: name `owner`, action **Allow**, *Include* → **Emails** → the
  owner's address (only that one)

**No Bypass, no service token, no other policy, and no application on a path
under this hostname.**

### The public hostname

Zero Trust → **Networks → Tunnels** → the tunnel → **Public Hostname** →
**Add a public hostname**:

| field | value |
| --- | --- |
| Subdomain | `covtools-admin` |
| Domain | `ramda.io` |
| Path | *(empty)* |
| Service type | `HTTP` |
| URL | `localhost:8790` |

The same public listener as section 1: the host picks the app by the `Host`
header. Leave *HTTP Host Header* empty, so that cloudflared forwards
`Host: covtools-admin.ramda.io`; that is what routes to the admin app. Never
`localhost:8791`.

### The second lock

The app itself demands `ADMIN_UI_TOKEN`: a browser login prompt (any user
name, the token as the password), or `Authorization: Bearer <token>`. Without
it every page is a 401. Read it on the machine:

```console
$ grep ADMIN_UI_TOKEN ~/cove-tools/env
```

### Check it

```console
$ curl -sI https://covtools-admin.ramda.io/ | head -3   # 302 to <team>.cloudflareaccess.com
$ curl -sI https://covtools.ramda.io/admin/ | head -3   # 302 (Access); and 404 behind it: the admin app is not on this hostname
```

and on the machine:

```console
$ curl -s -o /dev/null -w '%{http_code}' -H 'Host: covtools-admin.ramda.io' http://127.0.0.1:8790/   # 401
```

### Emergency exits

If the UI breaks or locks itself out (an app disabled by mistake, the admin
app itself), the changes it made are in `~/cove-tools/data/_host/overrides.json`
(history in `changes.jsonl`). On the machine:

```console
$ ~/cove-tools/current/cove-host enable|disable|reset <app> \
    --token-file ~/cove-tools/data/admin.token --admin 127.0.0.1:8791
```

or edit (or delete) `~/cove-tools/data/_host/overrides.json` and
`sudo systemctl restart cove-tools`.

## 5. Check it from outside

From a machine that is not logged in:

```console
$ curl -sI https://covtools.ramda.io/ | head -3                 # 302 to <team>.cloudflareaccess.com: Access in front
$ curl -sI https://covtools.ramda.io/_host/stats | head -3      # 302 as well; and 404 behind it
$ curl -s -X POST https://covtools.ramda.io/webhooks/in/nope   # the app's own 404, not a login page
$ curl -s https://covtools.ramda.io/ledger/api/runs             # 302 (service token) or the JSON list (Bypass)
```

and, logged in, a form on the webhook lab's admin pages (create or delete an
endpoint) goes through: its `Origin: https://covtools.ramda.io` is the origin
the host was told.

## Later, not now

- **Verify Access in the host.** Cloudflare adds a signed
  `Cf-Access-Jwt-Assertion` header to every request it lets through. The host
  could verify that JWT (the team's public keys at
  `https://<team>.cloudflareaccess.com/cdn-cgi/access/certs`, the
  application's AUD tag) and refuse a request without it, so that a
  misconfigured tunnel or a local process on the machine could not skip
  Access. Today the host trusts that only cloudflared reaches `127.0.0.1:8790`.
- **Security statement.** Apps run in one process: each request is its own
  Cove isolate with its own heap and budget, and an app reaches only the host
  modules it is granted, but same-process isolation is a fault and resource
  boundary, **not a guarantee against untrusted or malicious code** (README,
  *Security*). Only the owner's own apps run on this host; do not deploy an
  app you did not write or review.
