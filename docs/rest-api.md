# The REST API

The notedown server has a REST API under `/rest/v1`. It is for programs that work with an account's files over the network. Two programs use it today. The pipeline on a Mac reads notes and the journal with it (`notedown connect`). A Mac app browses the files of the account and manages them: it uploads, creates folders, renames, moves and deletes.

This page is for people who build such a client. It lists every route, its answers and its error codes. For the setup of a server, see the [getting started guide](getting-started.md). For the files and the journal on disk, see the [sync protocol](sync-protocol.md).

The API speaks plain HTTP on the home network, like the rest of the server. A token crosses the network in clear text. To reach the API from outside, use HTTPS through a reverse proxy. See [SECURITY.md](../SECURITY.md).

## The basics

- Every route is under `/rest/v1`. The answers are JSON with real HTTP status codes. Only a download answers with bytes.
- Every error body is `{"error": "<code>"}`. Some add a field, for example `existing`. A bad request also has a `message`.
- The token decides the account. No path names one.
- Ids are JSON numbers. An id stays the same when a file or folder is renamed or moved.
- A path is a wire path with `/` between the names, for example `NOTE/Note/a.note`. The empty path is the root. The server ignores a leading, a trailing and a doubled `/`.
- Names are exact. Another spelling of a name (other case, other Unicode form) is no entry. Names that start with a dot are hidden: they are never listed or served and never written.
- Every JSON request body is limited to 8 MiB (413 `too_large`). An upload is not.
- Unknown paths and methods answer 404 `no_route`.

## Log in

A client logs in once and keeps the token. The password is never sent. The client proves it with a challenge, as the device does.

1. `POST /login/code` with `{"email": "you@example.org"}` answers `{"randomCode": "...", "timestamp": ...}`. The code is good for one login.
2. Compute `passwordHash = sha256_hex(md5_hex(password) + randomCode)` with lowercase hex.
3. `POST /login` with the body below answers 201.

```json
{"email": "you@example.org", "passwordHash": "<hash>", "name": "mac app", "scope": "write"}
```

`name` is how the token is listed on the server. `scope` is `read` (the default) or `write`. Any other field, a clear `password` included, is a 400. The answer is:

```json
{"token": "<token>", "id": "<token id>", "slug": "me", "userId": 1234, "scope": "write"}
```

The server shows the token only now. Keep it private. Send it with every request as `Authorization: Bearer <token>`.

A wrong email, password or code and a locked account all give 401 `login_failed`. Too many requests from one address give 429 `too_many`.

Example:

```sh
BASE=http://192.168.1.10:19072/rest/v1
CODE=$(curl -s -X POST $BASE/login/code -d '{"email":"you@example.org"}' | jq -r .randomCode)
HASH=$(printf '%s' "$(printf '%s' "$PASSWORD" | md5 -q)$CODE" | shasum -a 256 | cut -d' ' -f1)
curl -s -X POST $BASE/login \
  -d "{\"email\":\"you@example.org\",\"passwordHash\":\"$HASH\",\"name\":\"mac app\",\"scope\":\"write\"}"
```

### Read and write tokens

A read token can use every read route. A write token can also use every write route. A write route with a read token answers 403 `read_only_token`. `notedown connect` makes a read token, so the pipeline can never change a file. A token is read-only unless it was made for writing.

A password change revokes every token. On the server, `notedown cloud account tokens list` shows each token's id, scope and name. `tokens revoke` removes it.

A refused token gets 401 `token_refused` with `WWW-Authenticate: Bearer`. A token of a removed account gets 410 `account_removed` with `userId` and `seq` of the removal.

## Routes

| Method and path | Scope | What it does |
| --- | --- | --- |
| `POST /login/code` | none | A random code for the login |
| `POST /login` | none | A new token |
| `GET /account` | read | `{"slug", "userId"}` |
| `GET /tree` | read | List a folder |
| `GET /entries/{id}` | read | One file or folder |
| `GET` and `HEAD /files/{id}` | read | Download a file |
| `GET /journal` | read | What changed since a `seq` |
| `GET /records`, `GET /records/{key}` | read | The page records of the pipeline |
| `GET /digests` | read | The digests |
| `GET` and `HEAD /note?path=` | read | One note file by its path |
| `PUT /files?path=` | write | Upload a file |
| `POST /folders` | write | Create a folder |
| `POST /move` | write | Rename or move a file or folder |
| `DELETE /entries/{id}` | write | Delete a file or folder |

The routes for the records, the digests and the note by path are made for the pipeline. This page does not describe their bodies. The sections below are for an app that browses and manages files.

## Browse

### `GET /tree?path=&recursive=`

Lists the children of the folder at `path` (the root by default). With `recursive=true` it lists all descendants. Anything else than `true` or `false` is a 400.

```json
{
  "seq": 812,
  "entries": [
    {"id": 41, "path": "NOTE/Note/a.note", "name": "a.note", "kind": "file",
     "size": 20480, "md5": "9e107d9d372bb6826bd81d3542a419d6",
     "mtimeMs": 1763290000000, "writable": true}
  ]
}
```

- `kind` is `file` or `folder`. A folder has no `md5`.
- `mtimeMs` is the time of the last change in milliseconds since 1970.
- `writable` says whether the app may rename, move or delete the entry. The tree roots (`NOTE/Note`, `NOTE/MyStyle`, `DOCUMENT/Document`, `EXPORT`, `SCREENSHOT` and `INBOX`) show `writable: false`: they cannot be renamed, moved or deleted. An upload or a new folder inside a root is allowed. Anything outside the trees is not writable either.
- 404 `not_found` for a path that is not there. 409 `not_a_folder` for a path that is a file.

Before it answers, the server scans what it shows. A file that someone put into the folder by hand is journaled and has its id before the app sees it.

**List by folder.** Do not list the whole tree with `recursive=true` on a big account. List the folder the user opens. Keep the `seq` of that answer and poll `GET /journal?from=<seq + 1>`. The server read `seq` under the same lock as the listing, so the listing holds every event up to `seq` and you miss nothing. When the journal answer says `changed`, list the open folder again.

### `GET /entries/{id}`

The same entry object as in `/tree`. 404 `not_found` for an unknown id, a file that is gone from disk and a hidden name. An id that is no number is a 400.

### `GET /files/{id}` and `HEAD /files/{id}`

The bytes of the file, streamed, with `Content-Length` and a strong `ETag: "<md5>"`. The server opens the file under the lock and streams it after, so a replace during a download never mixes old and new bytes.

- A `Range` header with one range answers 206, or 416 when it does not fit. `If-Range` is honored when it is the ETag.
- `If-None-Match` with the ETag answers 304.
- `HEAD` sends the headers and no body.
- There is no `Content-Disposition`. The client chooses the file name from the entry.
- 404 `not_found`. 409 `is_a_folder` for a folder.

### `GET /journal?from=&limit=`

Tells what changed in the account since the `seq` in `from` (0 by default). `limit` is at most 10000, which is also the default. The answer is a summary, not the events:

```json
{"changed": true, "removed": [], "next": 813, "end": 812, "more": false}
```

`changed` is true when the account has a new journal line in the range. `next` is one past the last `seq` read. Ask again from `next`. `more` means a page limit was hit: ask again at once. A number that does not parse is a 400.

Writes of the app have the origin `app` in the journal.

## Write

Every write follows the rules of `notedown cloud put` and takes the store lock. Every change is journaled with the origin `app` and no device, and the server pushes it to the connected devices as it does for any server-side change. The device sync and the app never see a half-done change of each other.

### Where the app may write

The app may write only inside the device folder trees: `NOTE/Note`, `NOTE/MyStyle`, `DOCUMENT/Document`, `EXPORT`, `SCREENSHOT` and `INBOX`. The roots of these trees stay as they are. Folders that are missing on the way are created. A name that differs from a sibling only in case or Unicode form is a collision. A symlink is never written through or replaced.

### `PUT /files?path=`

Uploads the raw bytes of a file as the request body.

| Header | Meaning |
| --- | --- |
| `Content-Length` | Required. A chunked upload is refused with 411 `length_required`. |
| `X-Notedown-Md5` | Optional. The MD5 of the body in hex. The server refuses the upload with 400 `bad_md5` when the bytes differ. |
| `If-None-Match: *` | Create only. 412 `precondition_failed` when the file is there. |
| `If-Match: "<md5>"` | Replace only when the file has this MD5. 412 `precondition_failed` when it has not. A weak tag (`W/"..."`) never matches. |

`If-Match` and `If-None-Match` together, a second header of the same kind, `If-None-Match` with anything but `*` and an `If-Match` that is no quoted MD5 are a 400. Identical bytes are always 200 `unchanged`, before the precondition is looked at.

The answer is 201 for a new file and 200 for a replace or for identical bytes:

```json
{"id": 41, "path": "NOTE/Note/a.note", "outcome": "created", "md5": "9e107d9d372bb6826bd81d3542a419d6",
 "size": 20480, "mtimeMs": 1763290000000, "seqs": [813]}
```

`outcome` is `created`, `replaced` or `unchanged`. A replace keeps the id and moves the old bytes to the recycle bin. `seqs` are the journal numbers of the change. An unchanged upload has none.

`mtimeMs` can be 0 when the server wrote the file but could not read its time afterwards. Treat 0 as unknown and read the entry again.

```sh
curl -X PUT "$BASE/files?path=NOTE/Note/a.note" \
  -H "Authorization: Bearer $TOKEN" -H "If-None-Match: *" \
  -H "X-Notedown-Md5: $(md5 -q a.note)" --data-binary @a.note
```

Limits and order of the checks:

- The largest file is 1 GiB (413 `too_large`).
- Before the body is read, the server checks the token, the path, the sync mode, the gates (423 `not_imported` and `scan_hold`), the device sync lock (423 `device_syncing`), `Content-Length` and the size. Then it checks the quota.
- Only an account with `quota_bytes` in its settings has a quota. Without it there is no check. When an upload would take such an account over, the answer is 507 `quota_exceeded`. Only what an upload adds counts, so a replace by fewer bytes always passes.
- The body goes to a staging folder first. A body shorter than `Content-Length` or a cut connection is 400 `bad_length`. The staging folder is removed in every case.
- Then the server takes the store lock once, checks everything again and commits.

After an early refusal the server may close the connection while the client still sends the body. A client can see a broken pipe or a reset instead of the answer. It should read the answer first when it has one, and retry otherwise.

A large upload or a replace holds the store lock for a while, most of all on a Raspberry Pi. Other requests of the account, a device sync included, wait for it.

### `POST /folders`

Body `{"path": "NOTE/Note/projects/2026"}`. Creates the folder and every missing folder above it, parents first, one `folder.created` each.

The answer is 201 `{"id", "path", "seqs"}`. When the folder is there, it is 200 with `seqs` empty. A file at the path is 409 `name_taken` and `existing` holds the name that is in the way. A file on the way is 409 `not_a_folder`.

### `POST /move`

Body `{"id": 41, "to": "NOTE/Note/b.note"}`. Renames or moves the file or folder `id` to the path `to`. The id stays. Missing folders on the way are created.

```json
{"id": 41, "path": "NOTE/Note/b.note", "seqs": [814], "removed": []}
```

- A move to the path it has is 200 with `seqs` and `removed` empty.
- A change of case only (`a.note` to `A.note`) is allowed.
- Any other entry at `to`, also one that differs only in case, is 409 `name_taken` with `existing`.
- A folder into itself or into one of its own folders is 400 `into_itself`.
- An unknown id is 404 `not_found`. A tree root or anything outside the trees as a source is 403 `not_writable`.

`removed` lists the folders that the move left empty and that the server removed, deepest first, as `{"id", "path"}`. Device changes do the same.

### `DELETE /entries/{id}`

Moves the file, or the folder with everything in it, to the recycle bin. Nothing is lost for good. The answer is:

```json
{"id": 41, "path": "NOTE/Note/b.note", "seqs": [815], "removed": []}
```

`removed` lists the folders that the delete left empty, as for a move. An unknown id is 404 `not_found`. That includes a delete that is retried after it worked. A tree root is 403 `not_writable`.

## While a device syncs

While a device holds the sync lock of the account, every write answers 423 `device_syncing` with a `Retry-After` header (5 seconds). The app waits that long and tries again. Reads always work. A write also waits (423) while a scan holds the account (`scan_hold`) and while the import of the account is missing (`not_imported`). The device then has priority: the app never changes files in the middle of a sync.

## Error codes

| Status | Code | When |
| --- | --- | --- |
| 400 | `bad_request` | A body, a query or a header that does not parse. `message` says what is wrong. |
| 400 | `bad_name` | The path is no valid path, a name starts with a dot, or the path is the root. |
| 400 | `name_too_long` | A name has more than 255 bytes. |
| 400 | `into_itself` | A folder is moved into itself. |
| 400 | `bad_length` | The body is shorter than `Content-Length`, or the connection broke. |
| 400 | `bad_md5` | The MD5 of the body is not `X-Notedown-Md5`. |
| 401 | `login_failed` | A wrong email, password or code, or a locked account. |
| 401 | `token_refused` | No token, a wrong token or a revoked one. |
| 403 | `read_only_token` | A write with a read token. |
| 403 | `not_writable` | The path is outside the device trees or a tree root. |
| 404 | `not_found` | No entry with this id or path. |
| 404 | `no_route` | No such route or method. |
| 404 | `no_record` | No record for this key (`/records/{key}`). |
| 409 | `name_taken` | Another entry has the name (also in another case or Unicode form), or a file is at the target of a new folder. `existing` is the name that is in the way. |
| 409 | `not_a_folder` | A file is on the way, or the path of a listing is a file. |
| 409 | `is_a_folder` | A folder is at the target of an upload, or a download is asked for a folder. |
| 409 | `sync_refused` | The server runs with `sync = "refuse"` and takes no files. |
| 410 | `account_removed` | The token belongs to a removed account. |
| 411 | `length_required` | An upload without `Content-Length`. |
| 412 | `precondition_failed` | `If-None-Match` or `If-Match` does not hold. |
| 413 | `too_large` | A file over 1 GiB, or a JSON body over 8 MiB. |
| 423 | `device_syncing` | A device syncs. `Retry-After` says when to retry. |
| 423 | `scan_hold` | A scan holds the account. See `scan_min_files` in the getting started guide. |
| 423 | `not_imported` | The account has no import yet. |
| 429 | `too_many` | Too many logins or codes from one address. |
| 500 | `internal` | A failure on the server. The detail is in its log, never in the answer. |
| 503 | `store_damaged` | The files of the account are damaged. The reason is in the server log. |
| 507 | `quota_exceeded` | The upload would take the account over its `quota_bytes`. Only for an account with a quota. |

The server logs every write in the protocol log, with the path in the request line. A body is never logged.

## Update order

Update the Mac before the Pi. The new server writes journal lines with the origin `app`. An older pipeline does not know it and skips such lines, so it would miss the change. A new pipeline reads the origin as it should.
