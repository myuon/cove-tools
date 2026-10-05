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
The webhook lab's admin pages also have the host verify the login itself
(section 6), so they ask for nothing more.

(The deployment's application is named `covtools`; its AUD tag is what
section 6 puts in `COVTOOLS_ACCESS_AUD`.)

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
  `cove-tools`; the deployment's is named `covtools-admin`, and its AUD tag
  goes in `COVTOOLS_ADMIN_ACCESS_AUD`, section 6)
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

The host verifies the Access login itself (section 6): every page needs the
token Access adds to the request, for this application. Without it — a
request that did not come through Access — every page is a 403, and
`ADMIN_UI_TOKEN` does not help (`fallback = "none"`). The token is the way
in only when Access is off for the app (`ACCESS_TEAM_DOMAIN` or
`COVTOOLS_ADMIN_ACCESS_AUD` empty), as on a developer's machine.

### Check it

```console
$ curl -sI https://covtools-admin.ramda.io/ | head -3   # 302 to <team>.cloudflareaccess.com
$ curl -sI https://covtools.ramda.io/admin/ | head -3   # 302 (Access); and 404 behind it: the admin app is not on this hostname
```

and on the machine:

```console
$ curl -s -o /dev/null -w '%{http_code}' -H 'Host: covtools-admin.ramda.io' http://127.0.0.1:8790/   # 403: no Access token
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

## 6. The host verifies Access

Cloudflare adds a signed `Cf-Access-Jwt-Assertion` header (and a
`CF_Authorization` cookie) to every request an Access application lets
through. The host verifies it for the apps that ask (`auth.identity`; main
README, *Cloudflare Access*): the admin UI and the webhook lab's admin pages.
So they know who you are from the Access login alone — no second password
prompt — and a request that skipped Access (a misconfigured tunnel route, a
process on the machine talking to `127.0.0.1:8790`) is refused. The receive
URLs and every other app are unaffected.

What the host needs, all in `~/cove-tools/env` (`deploy/env.example` has the
deployment's values, and `install.sh` appends whichever an existing env
lacks, leaving the others alone):

| key | what | where to find it |
| --- | --- | --- |
| `ACCESS_TEAM_DOMAIN` | `ioijoi.cloudflareaccess.com`: the issuer (`https://ioijoi.cloudflareaccess.com`) and where the keys are (`https://ioijoi.cloudflareaccess.com/cdn-cgi/access/certs`) | Zero Trust → **Settings** → *Team name and domain*; also the host of the login page Access redirects to |
| `COVTOOLS_ACCESS_AUD` | `a48226d2b0b956230ccb78ac4b9452d5d32dfb94b112d0a8e84712476d42568a`: the AUD tag of the application `covtools` (covtools.ramda.io, the whole host) — the webhook lab | Zero Trust → **Access → Applications** → `covtools` → **Configure** → **Overview** → *Application Audience (AUD) Tag* |
| `COVTOOLS_ADMIN_ACCESS_AUD` | `12a48ba5cc9c31a4411138b999190250ba5dd6df203e7630c45a1a1ed5d9d2bc`: the AUD tag of `covtools-admin` (covtools-admin.ramda.io) — the admin UI | the same, for `covtools-admin` |
| `ACCESS_ALLOWED_EMAILS` | `ioi.joi.koi.loi@gmail.com,ioijoikoiloi@gmail.com`: only these, even if a policy lets someone else through. Empty: whoever the policy allows | the Access policies' *Include → Emails* |

The bypass application on `covtools.ramda.io/webhooks/in` (its AUD tag is
`1d489de7e96253ea0c7edbffe84db8cc2906773c6c2c94fe8fd1f84ca0c24442`) needs no
entry: a Bypass application adds no token, and the receive URLs do not ask
for one. Each app accepts only its own application's AUD tag, so a token
for `covtools` does not open the admin UI, and one for `covtools-admin` does
not open the webhook lab.

To take it into use on a running deployment:

```console
$ bash install.sh v0.2.1          # appends the four keys to ~/cove-tools/env
$ grep '^ACCESS_\|^COVTOOLS_' ~/cove-tools/env
$ sudo systemctl restart cove-tools
```

No unit change: `EnvironmentFile=` already reads the env. Then, in a browser,
`https://covtools-admin.ramda.io/` and `https://covtools.ramda.io/webhooks/admin`
open after the Access login with no other prompt, and the admin UI's
history records your email. On the machine:

```console
$ curl -s -w '%{http_code}\n' -H 'Host: covtools-admin.ramda.io' http://127.0.0.1:8790/
the admin pages are reached through Cloudflare Access: no Cloudflare Access token (`Cf-Access-Jwt-Assertion`): ...
403
$ journalctl -u cove-tools | grep -i 'access'   # a failed key fetch, or "Access is off" at startup, is logged here
```

**If the keys cannot be fetched** (the machine cannot reach
`ioijoi.cloudflareaccess.com`), every login is refused — the host fails
closed — and the failure is logged per app (`~/cove-tools/data/<app>/log.txt`,
`/_host/apps/<app>/logs` on the admin listener). Keys already held are used
for up to six hours. **To fall back to the tokens** (Cloudflare broken, say),
empty `ACCESS_TEAM_DOMAIN=` in the env and restart: Access is then off for
both apps, they log that it is, and they prompt for `ADMIN_UI_TOKEN` /
`WEBHOOKS_ADMIN_TOKEN` again — still only through the tunnel, behind the
Access login at the edge.

A script cannot use these pages through Access without an Access service
token, and a service token carries no email, so it is not an identity here:
use the admin listener (`cove-host enable|disable|reset`) from the machine.

## Later, not now

- **Security statement.** Apps run in one process: each request is its own
  Cove isolate with its own heap and budget, and an app reaches only the host
  modules it is granted, but same-process isolation is a fault and resource
  boundary, **not a guarantee against untrusted or malicious code** (README,
  *Security*). Only the owner's own apps run on this host; do not deploy an
  app you did not write or review.
