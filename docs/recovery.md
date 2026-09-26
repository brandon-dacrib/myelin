# Recovering administrator access

When nobody can sign in as an administrator, this is the way back in. It is one command, run
where the server keeps its signing key, and one link.

## The short version

| Where the server runs | Run | Then |
| --- | --- | --- |
| Kubernetes (the Helm chart) | `kubectl -n <namespace> exec <release>-hs-0 -- hs recover` | open the link it prints |
| Docker (the image's quickstart) | `docker exec <container> hs recover` | open the link it prints |
| A host (`hs serve --data-dir ./data`) | `hs recover --data-dir ./data` | open the link it prints |

The link is `<public base URL>/admin/recover#token=...`. Opening it shows the administrator
accounts, asks for a new password for one of them, signs out every session that account had,
and signs you in as it. It works once and expires fifteen minutes after it was printed; running
`hs recover` again replaces it.

If the server has no active administrator at all (none was ever made, or the only one was
deactivated), `hs recover` prints the first-run **setup** link instead, which creates one.

## What `hs recover` needs

- **The server's signing key.** It looks in `--signing-key <file or directory>`, else
  `HS__SERVER__SIGNING_KEY_PATH` (the chart sets it in cluster mode, to the mounted Secret),
  else `keys/` under `--data-dir` or `HS_DATA_DIR` (the image and the chart set that). Inside
  the pod or the container there is nothing to pass. On a host, pass what `hs serve` was given.
- **The server's address.** `--server` defaults to `http://127.0.0.1:8008`, right from inside the
  pod or the container. From anywhere else, name the address the server is reached at; the key
  is what proves the request, not where it came from, so `hs recover --signing-key ./signing.key
  --server https://matrix.example.org` works from a laptop that holds the key.

Only the link goes to standard output, so `$(hs recover)` is the link; what it is and when it
expires go to standard error.

## Why the signing key is the credential

Every other way of administering the server goes through an administrator's session, which is
the one thing a locked-out operator does not have. Something else has to prove the request is
the operator's, and the signing key is the right something:

- It is the one secret the server could not function without, and it is already where the
  operator can reach it and nobody else can: the data volume, the mounted Secret, the directory
  `hs serve` was given. Reading it needs the same access as reading the server's log, which is
  what the setup link already relies on.
- Holding it is *already* being the server. Its holder can sign events and federation requests
  as this server. A recovery link adds nothing to that.
- It is there in every mode. A separate operator secret would be one more thing to make, keep
  and (in cluster mode) mount; a flag at start-up would cost a restart and, left set, re-offer a
  link at every restart after; a loopback-only endpoint with no signature would let any local
  process on a shared host mint a link.

`hs recover` signs a short message (a fixed purpose string, the time, a random nonce) with the
key and posts it to `/api/v1/recovery/links`. The server accepts it only if the signature
verifies under its own current key, the time is within five minutes of its clock, and the nonce
has not been seen before; a refusal says only that it was refused, and the reason goes to the
server's log. A request that verifies is spent: the same one again is a replay.

## What keeps the link safe

- The token in the link is forty random characters, in the URL *fragment*, which browsers do
  not send: it cannot land in an access log, a proxy's log or a `Referer` header.
- It is stored with its expiry and checked before anything else, so a caller without it learns
  nothing: not which accounts exist, not what the password policy is.
- The reset consumes it atomically before changing anything, so of any number of requests
  carrying the right token exactly one proceeds. If the reset then fails for a fault of the
  server's, the token is put back rather than leaving you with a spent link.
- Every session of the recovered account is signed out first, then the password is set: a
  session that survived a failed sign-out would still be one the old password had opened.
- Issuing a link and using it are both in the audit log (`recovery.link_issued` by the key,
  `recovery.password_reset` by the account) and the server log, neither with the token or the
  password.

## What it does not do

- It resets the password of an **active administrator**. It does not reactivate a deactivated
  one (that is the setup link's job, above), does not create accounts, and does not touch
  ordinary users.
- It does not rotate or replace the signing key. If the key itself is what was lost, see the
  chart's `secrets.signingKey` and `hs generate-signing-key`; a new key is a new server identity
  to the federation.
- It is not a password-reset email. Ordinary users reset their passwords through the client
  API, and administrators reset anybody's from the Users page.

## When it says no

| It says | It means |
| --- | --- |
| `nowhere to look for the server's signing key` | Not running where the key is, and no `--signing-key` or `--data-dir`. |
| `no ed25519 signing key at <path>` | The path exists but holds no key. `hs serve` writes `hs.signing.key` into `<data-dir>/keys` on its first start. |
| `could not reach http://127.0.0.1:8008` | Not running inside the server's pod or container, or the server listens elsewhere: `--server`. |
| `the server refused (401 ...)` | The key is not the server's current one, or the two clocks are more than five minutes apart. The server's log says which. |
| The page says `No recovery link is open` | The link was used, expired, or replaced by a newer `hs recover`. Run it again. |
| The page says `This is not this server's recovery link` | The token in the link is not the one outstanding. Copy the link again, or run `hs recover` again. |
