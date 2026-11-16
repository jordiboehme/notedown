# The REST API

The Notedown server has a REST API under `/rest/v1`. It is for programs that work with an account's files over the network. Two programs use it today. The pipeline on a Mac reads notes and the journal with it (`notedown connect`). A Mac app browses the files of the account, follows their changes and manages them: it uploads, creates folders, renames, moves and deletes.

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
| `GET /account` | read | `{"slug", "userId", "devices"}` |
| `GET /tree` | read | List a folder |
| `GET /entries/{id}` | read | One file or folder |
| `GET` and `HEAD /files/{id}` | read | Download a file |
| `GET /journal` | read | What changed since a `seq` |
| `GET /events` | read | The file and folder changes since a `seq` |
| `GET /records`, `GET /records/{key}` | read | The page records of the pipeline |
| `GET /digests` | read | The digests |
| `GET` and `HEAD /note?path=` | read | One note file by its path |
| `PUT /files?path=` | write | Upload a file |
| `POST /folders` | write | Create a folder |
| `POST /move` | write | Rename or move a file or folder |
| `DELETE /entries/{id}` | write | Delete a file or folder |
| `GET /recycle` | read | List the recycle bin |
| `POST /recycle/restore` | write | Bring an item of the recycle bin back |
| `POST /recycle/empty` | write | Remove every deleted file and folder from the recycle bin |

The routes for the records, the digests and the note by path are made for the pipeline. This page does not describe their bodies. The sections below are for an app that browses and manages files.

## Paging

`GET /tree` and `GET /recycle` answer in pages. Ask with `limit` and `cursor`:

- `limit` is the number of items in one page. Each route has a default and a largest value. A larger number counts as the largest, 0 counts as 1. A `limit` that is no number is 400 `bad_request`.
- The answer has the list and `"next"`. `next` is the cursor of the next page, or `null` on the last page.
- To get the next page, ask again with the same query and `cursor` set to `next`. Leave the other values as they were. A cursor is only valid with the same `path` and `recursive` (and `kind`) as the page it came from.
- A cursor is opaque. It starts with its route and version (`t1:` for the tree, `r1:` for the recycle bin) and is at most 8192 characters long. Never build one and never look inside.
- A cursor that is garbled, that belongs to another route or that points outside the folder of the request is 400 `bad_cursor`. Start again from the first page.
- A cursor names the last item of its page. When that item is gone since, the next page starts after its place in the order. An item that stays where it is comes once and is never left out. A change in the middle of the pages comes as an event (for the tree, see "Many pages" under `GET /tree`).

Every route with a list:

| Route | Limit | Default | Why |
| --- | --- | --- | --- |
| `GET /tree` | 1000 | 500 | Paged with `cursor` |
| `GET /recycle` | 500 | 500 | Paged with `cursor` |
| `GET /events` | 1000 | 1000 | Its own cursor: `from` and `next` are journal numbers |
| `GET /journal` | 10000 | 10000 | Its own cursor: `from` and `next` are journal numbers |
| `GET /account` | none | none | Not paged: an account has a handful of devices |
| `GET /records`, `GET /digests` | none | none | Not paged yet: routes of the pipeline. Paging them is planned for later |

## Account

### `GET /account`

Tells whose token this is and which devices the account has.

```json
{
  "slug": "jordi",
  "userId": 1000001,
  "devices": [
    {"serial": "SN078C10000000", "name": "Jordis Nomad", "bound": true,
     "lastLoginMs": 1763290000000, "lastSyncMs": 1763290900000,
     "syncing": false, "online": true}
  ]
}
```

- `lastSyncMs` is the time of the last sync that the device ended with success. It is `null` until then. Servers of an older version do not record it, so it stays `null` until the device syncs once on a server of this version. An app that talks to an older server gets no `devices` at all and treats that as an empty list.
- `lastLoginMs` is the last login of the device. A device keeps its token across syncs, so this is not the time of a sync.
- `syncing` is true while this device holds the sync lock of the account.
- `online` is true while the device has an open auto-sync connection that is alive.
- The list is sorted by `lastSyncMs`, newest first. Devices that never synced come last.
- `name` is `null` for a device that never told its name.

## Browse

Every read can also answer 423 `scan_hold` (a scan holds the account), 423 `not_imported` (the import of the account is missing) and 503 `store_damaged` (the files of the account are damaged).

### `GET /tree?path=&recursive=&kind=&limit=&cursor=`

Lists the children of the folder at `path` (the root by default), one page at a time. With `recursive=true` it lists all descendants. Anything else than `true` or `false` is a 400. With `kind=folder` it lists only folders, for example for a sidebar with the folder tree. Any other `kind` is a 400. `limit` is at most 1000. The default is 500. See [Paging](#paging).

```json
{
  "seq": 812,
  "entries": [
    {"id": 41, "path": "NOTE/Note/a.note", "name": "a.note", "kind": "file",
     "size": 20480, "md5": "9e107d9d372bb6826bd81d3542a419d6",
     "mtimeMs": 1763290000000, "writable": true}
  ],
  "next": "t1:Tk9URS9Ob3RlL2Eubm90ZQ"
}
```

- `kind` is `file` or `folder`. A folder has no `md5`.
- `mtimeMs` is the time of the last change in milliseconds since 1970.
- `writable` says whether the app may rename, move or delete the entry. The tree roots (`NOTE/Note`, `NOTE/MyStyle`, `DOCUMENT/Document`, `EXPORT`, `SCREENSHOT` and `INBOX`) show `writable: false`: they cannot be renamed, moved or deleted. An upload or a new folder inside a root is allowed. Anything outside the trees is not writable either.
- 404 `not_found` for a path that is not there. 409 `not_a_folder` for a path that is a file. 400 `bad_cursor` for a cursor that does not fit (see [Paging](#paging)).

**The order.** Entries come in the order of their paths, name by name. Each name is compared by its bytes. So a folder comes right before what is in it: `a`, `a/x`, `a b`. This is not the order of the paths as whole strings. Upper case comes before lower case (`Zebra` before `apple`), `a10` comes before `a2`, and files and folders are mixed. The app shows the entries in this order while it loads a folder. It can sort them in its own way once it has all pages. A sort by size or by time is only possible then.

**The cost.** The first page (no `cursor`) scans the folder before it answers. A file that someone put into the folder by hand is journaled and has its id before the app sees it. The next pages do not scan the folder again. They read only the names of the folder (with `recursive=true`, of the folders on the way) and look only at the entries of the page. An entry that was made or changed by hand after the first page is scanned on its own when a page shows it, so its event is in the journal before the app sees it.

**List by folder.** Do not list the whole tree with `recursive=true` on a big account. List the folder the user opens. Keep the `seq` of that answer and poll `GET /events?from=<seq + 1>`. The server read `seq` under the same lock as the page, so the page holds every event up to `seq` and you miss nothing. See [Following changes](#following-changes) for what to do with the events.

**Many pages.** Each page has its own `seq`. Follow the events of a folder from the lowest `seq` of its pages, plus one. When a later page answers 404 `not_found` or 409 `not_a_folder`, the folder was removed or replaced while the app read it. Drop the pages and list the folder again from the first page.

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

`changed` is true when the account has a new journal line in the range. That includes lines that are no file change (digests, tasks, devices and scans). An app that follows files uses `GET /events` instead. `next` is one past the last `seq` read. Ask again from `next`. `more` means a page limit was hit: ask again at once. A number that does not parse is a 400.

Writes of the app have the origin `app` in the journal.

## Following changes

### `GET /events?from=&limit=`

Gives the file and folder changes of the account since the `seq` in `from` (0 by default). An app lists once, keeps the `seq` of that listing and follows on from `seq + 1`. It does not need to list again, except after a `gap`.

`limit` is the number of events in one page. The default and the largest is 1000. A larger number is 1000 and 0 is 1. A number that does not parse is a 400. Any valid token can read it, a read token too.

```json
{
  "events": [
    {"seq": 813, "ts": 1763290000123, "type": "file.created", "origin": "app",
     "opId": "3f2c1d7a-8b4e-4c2a-9d1f-6e5a7b8c9d0e",
     "id": 41, "path": "NOTE/Note/a.note", "fileKind": "note",
     "md5": "9e107d9d372bb6826bd81d3542a419d6", "size": 20480}
  ],
  "next": 814, "end": 813, "more": false, "gap": false
}
```

- `next` is one past the last `seq` read. Ask again from `next`. A page without events for the account can still move `next`.
- `end` is the highest `seq` of the journal as far as the server can tell. It is 0 for an empty journal. For a token of a removed account it is at most the `seq` of the removal.
- `more` means the page stopped at its limit: ask again at once. Then `next` is larger than `from`. A page also stops after 10000 journal lines, whoever they belong to, so a page can be short or empty and still have `more`.
- `gap` means events may be missing between `from` and `next`. See below.

Each event has these fields. A field that does not apply is left out.

| Field | Meaning |
| --- | --- |
| `seq` | The journal number. |
| `ts` | The time of the change in milliseconds since 1970. It is not the time of the file. Take `mtimeMs` from `GET /entries/{id}`. |
| `type` | One of the types below. |
| `origin` | Who made the change: `device`, `cli`, `scan`, `repair`, `snapshot`, `retention`, `app` or `unknown`. |
| `device` | The device, when there is one. |
| `opId` | The same for all events of one change. |
| `id` and `path` | The entry. Not for `account.removed`. |
| `oldPath` | For a move: the path before. |
| `sourceId` | For a copy: the id of the file that was copied. |
| `fileKind` | For a created or modified file: `note`, `document`, `mark`, `image` or `other`. This is the kind of the journal, not the `kind` of the tree. |
| `md5` and `size` | For a created or modified file. |
| `prevMd5` | For a modified file: the MD5 before. |
| `userId` | For `account.removed`. |

The types are `file.created`, `file.modified`, `file.moved`, `file.copied`, `file.deleted`, `folder.created`, `folder.moved` and `folder.deleted`. Other journal lines (pages, digests, tasks, devices and the recycle bin) are not shown. The place of a deleted file in the recycle bin is not shown either.

The origins are the same as in the journal. A scan or a snapshot can create an entry that the app already has, so a create must not fail when the id is known. The app sees its own writes again, with the `seqs` that the write answered.

### What the app does with each event

| Event | What the app does |
| --- | --- |
| `file.created`, `folder.created` | Set the entry with this `id`. Take `path` from the event. For a file also take `md5`, `size` and `fileKind`. A folder has none of these. A different entry at the same `path` is replaced. |
| `file.modified` | Set `md5` and `size` of the entry. |
| `file.moved` | Set the new `path`. |
| `folder.moved` | Set the new `path` of the folder. Change the `oldPath` prefix of every entry below it to the new path. |
| `file.copied` | Add the entry `id` at `path`. Take `md5` and `size` from the entry `sourceId`, or ask `GET /entries/{id}`. `/tree` and `/entries` have no `fileKind`: the app has it only from an earlier event of the source. |
| `file.deleted` | Remove the entry. |
| `folder.deleted` | Remove the folder and every entry below it. |
| `account.removed` | The account is gone. Stop. |

- An app that listed several folders follows on from the lowest of their `seq` numbers plus one. It applies the events in order and treats each create as an upsert, so an event that a later listing already holds does no harm.
- An event can name an id that the app does not hold, for example in a folder it never listed. Ask `GET /entries/{id}` for it, or ignore it when it is outside what the app shows.
- A copy whose source is in a hidden path has a `sourceId` that the app cannot read. Ask `GET /entries/{id}` for the new id.

### Gap

When `gap` is true, the app lists again and follows on from the `seq` of that listing. The server sets `gap` when events may be missing:

- `from` is past the end of the journal.
- A journal file was removed, so the `seq` numbers do not follow on.
- A journal line of the account is of a type this version does not know, or does not parse.
- A journal line does not parse and the server cannot read its account.
- `from` is at or below the removal of an earlier account that had the same name.
- A move came from a hidden path into sight. The app has nothing of what came in.

### Hidden paths

An event with a hidden path (a name that starts with a dot) is not shown. A move from a visible to a hidden path comes as a `file.deleted` or `folder.deleted` of the old path. A move from a hidden to a visible path sets `gap`.

The recycle bin does not list an item with a hidden path either. Empty removes it all the same.

### Accounts

An app never sees the events of another account. This includes an earlier account with the same name: its events are left out, and `gap` tells the app that it may have missed some.

A token of a removed account reads up to the removal. The last event is then `account.removed` with `userId`. A token that is still valid while its account is removed gets the same: the page ends with the removal.

A removal by an older server, from before the journal had the `account.removed` record, sets no such limit: a name used again since then can show events of the old account.

### Limits

- The first call for an account name after each server start reads the whole journal once. The server keeps what it found in memory, so the next calls read only what is new. The whole journal is also read when a token's account is removed while it polls. That is fine, but it is slow on a very large journal.
- When the server fails to write a batch of journal lines, it cuts the batch. A poll that read a part of it before the cut has moved past numbers that are given out again. `/journal` has the same limit. A listing fixes it.

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

A large upload or a replace holds the store lock for a while, most of all on a small machine such as a Raspberry Pi. Other requests of the account, a device sync included, wait for it.

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

## Recycle bin

A delete moves the file or folder to the recycle bin of the account. The bin lists each thing the user deleted once. A deleted folder is one item with everything that was in it.

The bin also keeps the old versions of replaced files. They are not listed. When a delete leaves a folder with only dot-files, the server moves that folder to the bin as well. The user did not delete it, so it is not listed either. By default the server removes an item 90 days after the delete. The [getting started guide](getting-started.md) has the settings (`recycle_keep_deleted_days`, `recycle_prune`). Empty removes every deleted item at once.

### `GET /recycle?limit=&cursor=`

Lists the items of the bin, newest first. Items of the same time come in the order of their paths.

```json
{
  "items": [
    {"key": "1763290000000/NOTE/Note/b.note", "path": "NOTE/Note/b.note", "name": "b.note",
     "kind": "file", "deletedAt": 1763290000000, "size": 20480,
     "md5": "9e107d9d372bb6826bd81d3542a419d6"},
    {"key": "1763280000000/NOTE/Note/projects", "path": "NOTE/Note/projects",
     "name": "projects", "kind": "folder", "deletedAt": 1763280000000, "size": 81920,
     "files": 4}
  ],
  "next": null
}
```

- `key` names the item. It is `<deletedAt>/<path>`. It is not used for another path.
- `path` is the path the item had before the delete. `name` is its last name.
- `kind` is `file` or `folder`.
- `deletedAt` is the time of the delete in milliseconds since 1970.
- `size` is the number of bytes in the bin for the item. For a folder it is the sum of its files.
- `md5` is only there for a file, when the server knows it. `files` is only there for a folder: the number of files that were in it.

`limit` is at most 500, which is also the default. 0 counts as 1. When `next` is not `null`, there are more items. Ask again with `cursor` set to `next`. The next page starts after the last item of the page, also when that item has left the bin since. See [Paging](#paging). A `limit` that is no number is 400 `bad_request`, a cursor that does not fit is 400 `bad_cursor`. The server cuts each page from the whole bin index. That is fine at the size of a bin.

The bin is not in `/events`. List it again after a delete, a restore or Empty, and on a `file.deleted` or `folder.deleted` while the app shows it.

An item whose bytes are gone from the bin is not listed. A line of the bin index that the server cannot read is skipped. The list answers 423 `scan_hold` and `not_imported` like the other reads.

### `POST /recycle/restore`

Body `{"key": "1763290000000/NOTE/Note/b.note", "path": "NOTE/Note/b.note"}`. Brings the item `key` of the bin back at `path`. `path` is optional. Without it the item comes back at the path it had. The answer is:

```json
{"id": 52, "path": "NOTE/Note/b.note", "kind": "file", "seqs": [816]}
```

- The item gets a new id. A folder comes back with everything that was in it, and each file and folder in it gets a new id too.
- The events are `file.created` and `folder.created` with the new ids, parents first. Missing folders on the way are created first. Dot-files in a folder come back without an event, as everywhere else.
- The devices see a new file. They do not know that it was there before.
- The old versions of a restored file stay in the bin and now belong to the new id.
- A note comes back with its text. It is not read again.
- The item leaves the bin. A key already restored is 404 `not_found`, as is a key that the server pruned or that Empty removed. Once an item has left the bin, a later delete of the same path in the same millisecond (the same `deletedAt`) gets its key again, so an old key can then name that newer delete.

The rules for the target are those of a write:

- Anything at `path`, also with a name that differs only in case or Unicode form, is 409 `name_taken` with `existing`. A folder is never merged into a folder that is there. Choose another `path` instead.
- A file on the way is 409 `not_a_folder`.
- A path outside the device trees or a tree root is 403 `not_writable`. A name that starts with a dot is 400 `bad_name`. A name with more than 255 bytes is 400 `name_too_long`. This also holds for the old path, when the body has none.
- An item whose bytes in the bin are damaged (a symlink in them or on the way to them) is 409 `slot_damaged`. Nothing is changed.
- The bytes of the item count against the quota as for an upload: 507 `quota_exceeded` for an account with `quota_bytes`.
- A restore waits like every write: 423 `device_syncing` with `Retry-After`, `scan_hold` and `not_imported`. The item stays in the bin.

### `POST /recycle/empty`

No body. Removes every deleted file and folder from the bin for good. The answer is:

```json
{"removed": 2, "bytes": 102400, "kept": 0}
```

- `removed` is the number of items that left the bin. `bytes` is their size in the bin before Empty.
- Empty also removes what a delete put in the bin but the bin does not list: hidden paths and folders that a delete left with only dot-files.
- The old versions of replaced files stay. That includes the old versions of a file that was restored. The server prunes them by its rules as before. After Empty, the old versions of a deleted file are removed 90 days after the newest of them, which can be right away.
- `kept` counts the lines of the bin index for deleted files and folders that Empty could not remove, for example because there is a symlink in their bytes or on the way to them. They stay in the bin. It is usually 0.
- The journal gets one `recycle.pruned` event per removed line of the bin index, with `origin` `app`. `/events` does not show them.
- An empty bin gives `{"removed": 0, "bytes": 0, "kept": 0}` and no event.
- Empty waits like every write: 423 `device_syncing` with `Retry-After`, `scan_hold` and `not_imported`. A read token is 403 `read_only_token`.

## While a device syncs

While a device holds the sync lock of the account, every write answers 423 `device_syncing` with a `Retry-After` header (5 seconds). The app waits that long and tries again. Device sync does not stop reads. A read can still answer 423 `scan_hold` while a scan holds the account and 423 `not_imported` while the import of the account is missing. It answers 503 `store_damaged` when the files of the account are damaged. A write also waits (423) while a scan holds the account (`scan_hold`) and while the import of the account is missing (`not_imported`). The device then has priority: the app never changes files in the middle of a sync.

## Error codes

| Status | Code | When |
| --- | --- | --- |
| 400 | `bad_request` | A body, a query or a header that does not parse. `message` says what is wrong. |
| 400 | `bad_cursor` | A cursor that is garbled, of another route, or outside the folder of the request. Start again from the first page. |
| 400 | `bad_name` | The path is no valid path, a name starts with a dot, or the path is the root. |
| 400 | `name_too_long` | A name has more than 255 bytes. |
| 400 | `into_itself` | A folder is moved into itself. |
| 400 | `bad_length` | The body is shorter than `Content-Length`, or the connection broke. |
| 400 | `bad_md5` | The MD5 of the body is not `X-Notedown-Md5`. |
| 401 | `login_failed` | A wrong email, password or code, or a locked account. |
| 401 | `token_refused` | No token, a wrong token or a revoked one. |
| 403 | `read_only_token` | A write with a read token. |
| 403 | `not_writable` | The path is outside the device trees or a tree root. |
| 404 | `not_found` | No entry with this id or path, or no item of the recycle bin with this key. |
| 404 | `no_route` | No such route or method. |
| 404 | `no_record` | No record for this key (`/records/{key}`). |
| 409 | `name_taken` | Another entry has the name (also in another case or Unicode form), a file is at the target of a new folder, or anything is at the target of a move or a restore. `existing` is the name that is in the way. In rare cases, when the file system itself reports the clash, it is a short description instead of a name. |
| 409 | `not_a_folder` | A file is on the way, or the path of a listing is a file. |
| 409 | `is_a_folder` | A folder is at the target of an upload, or a download is asked for a folder. |
| 409 | `slot_damaged` | The bytes of a recycle bin item are damaged: there is a symlink in them or on the way to them. |
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
| 507 | `quota_exceeded` | The upload or the restore would take the account over its `quota_bytes`. Only for an account with a quota. |

The server logs every write in the protocol log, with the path in the request line. The body of an upload is never logged. A JSON body is logged with its secrets masked.

## Update order

Update the Mac before the server. The new server writes journal lines with the origin `app`. An older pipeline does not know it and skips such lines, so it would miss the change. A new pipeline reads the origin as it should. A pipeline that reads over REST works with either order, because `notedown connect` sends no scope. The Mac first is still the safe order.
