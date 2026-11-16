# Supernote Private Cloud sync protocol

Status: protocol description for a clean-room reimplementation (Notedown server, Rust, AGPL-3.0). Written 2026-10-06 from Ratta's Private Cloud image `supernote/supernote-service:26.06.04`, its web frontend, its database schema, and four open-source implementations. Target device: Supernote A6 X2 Nomad, firmware Chauvet 3.29.42.

This document describes the protocol as a protocol. It contains no code from any implementation. Example request and response bodies are constructed from the field lists and from published device traces; ids, hashes and names in examples are made up unless stated otherwise.

## 0. Conventions

### 0.1 Source tags

Every statement carries one or more tags:

| Tag | Meaning |
|---|---|
| [ratta-server] | Seen in Ratta's server artifacts: route strings, DTO/VO field names, constant values, mapper SQL, `application.properties`, nginx config, database schema |
| [ratta-web] | Seen in Ratta's Vue 2 web frontend (`home/supernote/cloud/src`) |
| [community:allenporter] | allenporter/supernote (Python server, Apache-2.0) and its docs |
| [community:ultrabridge] | jdkruzr/ultrabridge (Go, Apache-2.0); its docs include device traces from a Nomad (`SN078C...`) |
| [community:opennotecloud] | k4z4n0v4/opennotecloud (Go, AGPL-3.0) |
| [community:sncloud] | rnbennett/supernote-cloud (continuation of julianprester/sncloud, Python, Apache-2.0), client of Ratta's public cloud |
| [community:supernote-sync] | dylanmazurek/supernote-sync (Go, MIT), client of Ratta's public cloud |
| [doc] | Ratta's public documentation (support.supernote.com) |
| [inferred] | Our conclusion from the evidence, not seen directly |
| [unknown] | Not known; see section 9 |
| [proposal] | Design proposal for Notedown, not observed protocol |
| [capture:nomad-3.29.42] | Observed by Notedown's capture server (`notedown serve --capture`) with a Nomad on firmware Chauvet 3.29.42, 2026-10-06; see 9.1 |

Since 2026-10-08, [ratta-server] statements about behaviour (what the server does, in which order, with which answer) come from an analysis of `supernote-service` 26.06.04; section 4 is based on it. Older [ratta-server] statements came from strings, names and configuration only. Sections 2, 3, 5, 6 and 7 were checked against the same build on 2026-10-08. A statement marked "[ratta-server] (analyzed)" rests on that deeper analysis. "Probably" marks a fact the analysis shows only indirectly, or a framework default.

"Device-confirmed" means a community project reports it from traffic of a real device. Facts confirmed by two independent sources are marked with both tags.

### 0.2 Secrets

Ratta's server contains hard-coded keys (JWT signing string, URL signing key, socket `sign` key, Redis and database passwords). They are deliberately **not** reproduced here. None of them is needed for interoperability (sections 2.5, 3.9, 4.2): the device does not validate the JWT, the server both creates and checks its own signed URLs, and the socket `sign` value can be accepted without checking. Notedown generates its own keys [inferred from community:ultrabridge, community:opennotecloud, community:allenporter].

### 0.3 Notation

- All REST calls are `POST` with a JSON body unless stated otherwise; `Content-Type: application/json; charset=UTF-8` [community:allenporter trace].
- Field names are given exactly as they appear on the wire. They mix camelCase (`equipmentNo`) and snake_case (`content_hash`, `path_display`, `to_path`); do not "fix" them [ratta-server] [community:ultrabridge].
- Times are milliseconds since the Unix epoch unless stated otherwise [ratta-server] [community:ultrabridge].
- "Envelope" means the three fields `success`, `errorCode`, `errorMsg` (section 7).

## 1. Overview

### 1.1 Components of Ratta's Private Cloud

| Component | Role | Source |
|---|---|---|
| `supernote-service` container | nginx (container port 8080, optional 443), Spring Boot backend on 19071, socket.io (netty-socketio) on 18072 (file, to-do and digest), a second socket.io server on 18073 without listeners, Node.js web UI on 9888 | [ratta-server] [doc] |
| MariaDB `supernotedb` | metadata: users, files, digests, tasks, devices, logs | [ratta-server] |
| Redis | tokens, random login codes, sync lock, socket message queues, counters | [ratta-server] [community:ultrabridge] |
| `notelib` container (TCP 6000) | renders `.note` to PDF/PNG for the web UI and conversion requests; length-prefixed JSON over raw TCP with business codes `T01` (note to PDF), `T02` (note to PNG), `T03` (page count) | [ratta-server] [community:allenporter] |
| Data volume `/home/supernote/data` | the user's files as real files, `/home/supernote/data/<email>/Supernote/...` | [ratta-server] |
| `/home/supernote/convert`, `/home/supernote/part`, `/home/supernote/recycle` | conversion output, chunk staging, recycle bin; the web recycle bin keeps files under `/home/supernote/recycle/recycle_bin/<email>/`, outside the data tree (6.4) | [ratta-server] |

### 1.2 Ports and routing

| Port | What | Source |
|---|---|---|
| 19072 (host) to 8080 (container nginx) | everything the device and browser use: `/` web UI (to 9888), `/api/` backend (to 19071), `/socket.io/` auto sync (to 18072) | [doc] [ratta-server nginx.conf] |
| 19071 | backend directly (also published by the compose file) | [ratta-server] |
| 18072 | socket.io for auto sync; Ratta's doc calls it optional ("needed only for automatic sync") | [doc] [ratta-server] |
| 18073 | second socket.io server; it starts but has no listeners, nginx does not route to it, the device never opens it | [ratta-server] [community:ultrabridge] |
| 443 / 19443 | HTTPS; Ratta ships nginx with an optional `listen 443 ssl` and recommends a reverse proxy with a public certificate | [doc] [ratta-server] |

nginx forwards `X-Forwarded-Host`, `X-Forwarded-Port` and `X-Forwarded-Proto` to the backend, and the backend builds absolute upload and download URLs (`<scheme>://<host>:<port>/api/oss/...`). It probably reads only the `Host` header and `X-Forwarded-Proto` for this, not `X-Forwarded-Host` or `X-Forwarded-Port` [ratta-server]. A reimplementation must therefore build URLs that the device can reach, from the request's Host header or from configuration [community:opennotecloud uses a configured base URL] [inferred].

`/socket.io/` is proxied with WebSocket upgrade and a 3600 s read timeout; `/api/` with 3000 s timeouts; `client_max_body_size` is effectively unlimited [ratta-server].

The device sends one base address (host and port, or an HTTPS domain) in its Private Cloud settings; REST and socket.io both go to that address [doc] [community:ultrabridge: all `/socket.io/*` arrives on the same external port]. A device accepts a public Let's Encrypt certificate (no pinning observed) [community:ultrabridge].

### 1.3 Transports

- REST over HTTP or HTTPS, JSON bodies, auth header `x-access-token` [ratta-server] [ratta-web] [community:allenporter trace].
- Binary transfer over signed URLs: `POST /api/oss/upload` (multipart), `POST /api/oss/upload/part` (multipart chunks), `GET /api/oss/download` (Range support) [ratta-server] [community:ultrabridge] [community:allenporter].
- Auto sync over socket.io (Engine.IO protocol 3), WebSocket transport, at `/socket.io/` [ratta-server] [community:ultrabridge] [community:allenporter].

The device runs several HTTP clients (user agents `okhttp/5.1.0` for file sync, `okhttp/4.12.0` for metadata, `okhttp/4.8.0` for task deletion, `okhttp/3.12.12` for the socket) [community:ultrabridge]; `okhttp/5.1.0` for sync is also seen by [community:allenporter].

### 1.4 Private cloud versus Ratta's public cloud

- Both use the same API family under `/api/...` (for example `/api/official/user/query/random/code`, `/api/official/user/account/login/new`, `/api/file/list/query`, `/api/file/upload/apply`) [community:sncloud] [community:supernote-sync] [ratta-server].
- The private server contains "Local" variants of the device file controller (`F_FileLocalController`: `/api/file/2/...` and `/api/file/3/...`) and a local object store (`/api/oss/...`). The DTOs of the public cloud variants (S3 / UFile signatures: `xAmzDate`, `s3Authorization`, `postAuthorization`, `fileServer` "0 ufile, 1 S3") are still in the jar but have no route in the private build [ratta-server]. The upload apply response of the private server keeps S3-style field names (`bucketName`, `xAmzDate`, `authorization`) [ratta-server] [community:ultrabridge].
- The public cloud adds email OTP login for new browsers (`/user/validcode/pre-auth`, `/official/user/sms/login`, CSRF cookie) [community:sncloud]. The private server has the MFA (TOTP) option instead (`/api/user/mfa/*`, `/api/official/user/account/login/mfa/verify`) for the web UI [ratta-server].
- Whether the device's public-cloud sync uses the same `/3/` endpoints with S3 URLs or other endpoints is [unknown]; it does not matter for a private server.

## 2. Account and device login

### 2.1 Endpoints the device calls versus web-only endpoints

| Endpoint | Caller | Auth | Source |
|---|---|---|---|
| `POST /api/terminal/equipment/unlink` | device (on logout, and before login in one trace) | none | [ratta-server] [community:ultrabridge] |
| `POST /api/official/user/check/exists/server` | device | none | [ratta-server] [community:ultrabridge] |
| `POST /api/official/user/query/random/code` | device, web | none | [ratta-server] [ratta-web] [community:ultrabridge] |
| `POST /api/official/user/account/login/equipment` | device | none | [ratta-server] [community:ultrabridge] |
| `POST /api/terminal/user/bindEquipment` | device, right after login | none | [ratta-server] [community:ultrabridge] |
| `POST /api/user/query` | device (first authenticated call), web | token | [ratta-server] [community:ultrabridge] |
| `POST /api/equipment/bind/status` | device (about 4 times per sync session), Partner app (every few seconds), web | token | [ratta-server] [ratta-web] [community:ultrabridge] |
| `GET /api/file/query/server` | device at boot (reachability); one trace shows `POST` | [unknown]; treat as public | [ratta-server] [community:ultrabridge] |
| `POST /api/user/query/token` | web after reload; device [unknown] | cookie or token | [ratta-server] [ratta-web] |
| `POST /api/user/logout` | web; device [unknown] | token | [ratta-server] |
| `POST /api/official/user/account/login/new` | web UI and Partner app | none | [ratta-server] [ratta-web] |
| `POST /api/official/system/base/param` | web before login | none | [ratta-server] [ratta-web] |
| `POST /api/user/mail/validcode`, `POST /api/user/check/validcode`, `POST /api/user/register` | web registration only | none | [ratta-server] [ratta-web] |
| `POST /api/official/user/retrieve/password` | web password reset | none | [ratta-server] [ratta-web] |
| `/api/save/email/config`, `/api/send/email/test`, `/api/query/email/config`, `/api/query/email/publickey` | web admin (SMTP settings) | token | [ratta-server] [ratta-web] |

The device does not register accounts and does not send verification codes. Registration with email verification exists only in the web UI [ratta-web] [community:ultrabridge: none of these endpoints seen in device traces] [inferred].

### 2.2 Why Ratta's server needs SMTP, and how to avoid it

Registration in the web UI runs: `POST /api/user/mail/validcode {email, language}` (the server mails a code and returns `validCodeKey`), then `POST /api/user/check/validcode {validCodeKey, validCode}`, then `POST /api/user/register {email, password, userName}` with `password` = lowercase hex MD5 of the plain password [ratta-web]. The server's register step checks that the email was validated before [ratta-server: register logic references the validated-code cache keys]. Password reset (`retrieve/password`) also depends on the mail code [ratta-server] [ratta-web]. Error `E0101`/`E0102` are expired/wrong codes [ratta-server].

A server without SMTP creates accounts out of band (admin CLI or a config file) and does not offer the registration or reset endpoints, or offers them with a different second factor [proposal]. allenporter disables public registration and password reset by default and creates users with a CLI; opennotecloud creates users with a CLI from an MD5 password hash [community:allenporter] [community:opennotecloud].

### 2.3 Password encoding

- Stored credential: `MD5_hex(plain_password)` (32 lowercase hex characters; column `u_user.password varchar(32)`) [ratta-server] [ratta-web: register sends MD5] [community:ultrabridge] [community:allenporter].
- Challenge: `POST /api/official/user/query/random/code` with `{"account": "<email>", "countryCode": null}` returns `randomCode` (8 characters in samples) and `timestamp` (ms) [ratta-server] [community:ultrabridge] [community:allenporter]. The device also sends `version` (for example `"202407"`) [community:ultrabridge].
- Login password: `SHA256_hex( MD5_hex(plain_password) + randomCode )`, lowercase hex, 64 characters [ratta-web] [community:ultrabridge, device-confirmed] [community:allenporter] [community:sncloud].
- The `timestamp` from the challenge is echoed in the login request but is not part of the hash [community:ultrabridge] [community:allenporter]. The server keeps the random code per account (Redis, one-time use, deleted after the check) [community:ultrabridge]. allenporter keys it by account and checks that the echoed timestamp matches [community:allenporter]. Ratta's random code has 8 characters and lives 300 s under the key `<countryCode>_<account>_<timestamp>` (`countryCode` can be the string `"null"`); it is deleted at the first password check, so it is used once, also when the password is wrong; the password is compared case-sensitive after trimming; an expired code is compared as `"null"` and gives `E0019` [ratta-server] (analyzed). The email lookup ignores case, emails have at most 50 characters [ratta-server].

Example challenge:

```json
POST /api/official/user/query/random/code
{"countryCode": null, "account": "jordi@example.org", "version": "202407"}

{"success": true, "errorCode": null, "errorMsg": null,
 "randomCode": "a1B2c3D4", "timestamp": 1791270000000}
```

### 2.4 Device login sequence

Observed sequences on a Nomad [community:ultrabridge]:

1. Variant A (fresh login after logout): `unlink` -> `check/exists/server` -> `query/random/code` -> `login/equipment` -> `bindEquipment` -> `POST /api/user/query` -> socket.io connect. Firmware 3.29.42 uses this variant; after a failed `check/exists/server` (`E0018`) the device starts again with `unlink` [capture:nomad-3.29.42].
2. Variant B: `check/exists/server` -> `query/random/code` -> `login/equipment` -> `bindEquipment` -> `user/query`.

The server must not depend on the order and must answer each call on its own [inferred].

`check/exists/server` request: `{"email": "...", "version": "..."}` or `{}`; response adds `dms` (`"ALL"`), `userId` (number) and `uniqueMachineId` (a server instance id) to the envelope [ratta-server: UserCheckVO fields dms, userId, uniqueMachineId] [community:ultrabridge]. allenporter answers success when the user exists and an error otherwise [community:allenporter].

`login/equipment` request (`LoginDTO`) [ratta-server] [community:ultrabridge]:

| Field | Value from device | Source |
|---|---|---|
| `account` | email | [ratta-server] [community:ultrabridge] |
| `password` | 64 hex, see 2.3 | same |
| `equipmentNo` | device serial, for example `SN078C1000xxxx` | same |
| `equipment` | `3` (number) = terminal; web sends `"1"`, Partner app `2`, user platform `4` | [ratta-server] [ratta-web] [community:ultrabridge] |
| `loginMethod` | `"2"` = email (`"1"` phone, `"3"` WeChat; the web UI sends `"1"`) | [ratta-server] [ratta-web] [community:ultrabridge] |
| `timestamp` | echoed from the challenge | same |
| `version` | firmware API version string | [community:ultrabridge] |
| `countryCode`, `browser`, `language` | optional, web only | [ratta-server] [ratta-web] |

`login/equipment` response (`LoginVO`, envelope plus): `token`, `counts` (failed attempts, string), `userName`, `avatarsUrl`, `lastUpdateTime` (a date, written as an ISO-8601 string on Ratta's wire, not ms (5.1) [ratta-server] (analyzed)), `isBind` (`"Y"`/`"N"`: user has any bound device), `isBindEquipment` (`"Y"`/`"N"`: this device is bound to this user), `soldOutCount` (int), `mfaRequired`, `mfaToken` [ratta-server] [community:ultrabridge]. Observed device-side answer: `{"success":true,"token":"eyJ...","counts":"0","userName":"<email>","isBind":"N","isBindEquipment":"N","soldOutCount":0}` [community:ultrabridge]. With `equipmentNo` in the request Ratta adds `counts` `"0"`, `userName`, `lastUpdateTime`, `isBindEquipment`, `isBind` (`"Y"` if bound) and `soldOutCount`; `login/new` sets `isBind` only when `equipmentNo` is null. With MFA on, a login with `equipment` 1 or 4 answers `mfaRequired` and an `mfaToken` that lives 300 s instead of a token [ratta-server] (analyzed).

Failures: `E0018` account not found, `E0019` wrong password, `E0045` locked; lockout after `MAX_ERR_COUNTS` (6) attempts [ratta-server] [community:ultrabridge]. The lock lasts `LOCK_TIME`, default 5 min (the parameter is not in the seeded data) [ratta-server]. In this build the lockout never triggers: the failure counter is always written as 1 (TTL 43,200 s), so `E0019` answers carry `counts` `"1"`, and `E0045` comes only if a lock key exists already or after 5 failed MFA tries in 300 s [ratta-server] (analyzed). `login/equipment` with an unknown account fails with the generic `"500"`, not with `E0018` [ratta-server] (analyzed). Required fields (missing -> `"400"`, 7.1): `account`, `password`, `equipment`, `loginMethod`; `query/random/code` requires `account` [ratta-server]. Which earlier tokens a login removes is described in 2.5.

`bindEquipment` request (`BindEquipmentDTO`) [ratta-server] [community:ultrabridge, device-confirmed values]:

```json
{"account": "jordi@example.org", "equipmentNo": "SN078C10000000", "flag": "1",
 "name": "Supernote Nomad", "totalCapacity": "25485312", "version": "202407",
 "label": ["DOCUMENT/Document", "NOTE/Note", "NOTE/MyStyle", "EXPORT", "SCREENSHOT", "INBOX"]}
```

`label` is the folder manifest the server creates for the user; the same six names and the capacity value `25485312` are constants in Ratta's user service [ratta-server]. Response: envelope only. Ratta's `bindEquipment` [ratta-server]:

- needs no token; required fields `equipmentNo`, `account`, `name`, `totalCapacity`, `label` (missing -> `"400"`);
- unknown account -> `E0502`;
- device already bound to the same account -> success, nothing changes;
- device bound to another account -> the old binding stays and a second binding row is added (a Ratta bug, do not copy);
- else it inserts the binding, sets the account's total capacity to `totalCapacity` x 1024 (the value is KiB: `"25485312"` is about 24.3 GiB), creates the `label` folders (names compared without case) and reconciles the disk (3.0.1);
- a `totalCapacity` that is not a number -> `"500"`.

The codes `E0075`, `E0077`, `E0083` and `E0503` exist in Ratta's enum but this build never sends them [ratta-server].

`unlink` request: `{"equipmentNo": "...", "version": "..."}`; response: envelope [ratta-server] [community:ultrabridge]. It needs no token and requires `equipmentNo`. An unknown device -> success. Else it deletes the binding and the device's history-sync rows; the files and the tokens stay [ratta-server].

`bind/status` response: envelope plus `bindStatus: true` [ratta-server] [community:ultrabridge] [community:allenporter]. It must always succeed for a logged-in device [community:ultrabridge]. Ratta answers `true` only if the account has at least one bound device [ratta-server]; after the device's `bindEquipment` that is always the case.

`/api/user/query` response (`UserQueryByIdVO`): `userId`, `userName`, `email`, `telephone`, `countryCode`, `avatarsUrl`, `sex`, `birthday`, `personalSign`, `hobby`, `education`, `job`, `address`, `totalCapacity` (string), `fileServer` (`"0"`), `isNormal` (`"Y"`) [ratta-server]. Which of these the device needs is [unknown]; return all with harmless values [proposal].

### 2.5 Tokens

- Header on every authenticated REST call: `x-access-token: <JWT>` [ratta-server] [ratta-web] [community:allenporter trace]. The device trace also carries an empty header `channel` [community:allenporter trace]; another source describes `channel` as `<JWT>_<equipmentNo>_<timestamp>` [community:ultrabridge]. Ignoring it works [community:opennotecloud] [inferred].
- Format: JWT, HS256 [ratta-server] [community:ultrabridge]. Ratta's claims: `userId` (string), `createTime` (seconds), `key` (`<userId>_<createTime>_<ms>` plus `_<equipmentNo>` for device tokens), `equipmentNo` (device tokens), and `exp` only on web tokens [ratta-server] [community:ultrabridge]. A captured socket handshake also showed an `exp`-bearing token without `equipmentNo` [community:ultrabridge]. The server must accept tokens without `exp` [community:ultrabridge]. Ratta's three kinds of token [ratta-server] (analyzed):
  - An internal system token per user in Redis under `<userId>_token`, with `userId`, `createTime` (ms), `aud` `"system"` and `iat`; it has no TTL.
  - A device token (request with `equipmentNo`): `userId`, `createTime` (seconds), `equipmentNo` and `key` = `<userId>_<createTime in s>_<createTime of the system token in ms>_<equipmentNo>`; no `exp`.
  - A token without `equipmentNo`: `userId`, `createTime` (seconds), `key` without the serial, and `exp` = now plus 30 days (`equipment` `"2"`, the Partner app: 3600 days).
- Validity on Ratta: the HS256 signature and `exp` (if present) are checked, then Redis under the `key` claim must hold exactly this token; a missing entry gives HTTP 401 (7.1) [ratta-server] (analyzed). The Redis entry lives as many days as the parameter `P012` says (default 30) when `equipment` is `"1"` or `"4"`, and has no TTL for other clients, so a device token never expires on its own. The 1 h, 8 h and 55 min constants of the token service are not used in this flow [ratta-server] (analyzed). Web tokens: 30 days [community:ultrabridge].
- Revocation: a new `login/equipment` for the same account displaces the device session [community:ultrabridge]. On Ratta a login revokes only the earlier tokens of the same serial (Redis keys `<userId>_*_<system time>_<equipmentNo>`, never for the serial `"WEB"`); tokens of other devices stay valid. A web logout deletes every Redis key that starts with `<userId>`: all device tokens, the system token and the sync lock; the prefix also matches other users whose id starts with the same digits (a Ratta bug, do not copy) [ratta-server] (analyzed).
- The device does not validate the token: it stores and echoes whatever `token` it receives; a token minted by a different server with a different key was accepted, and the device opened its socket with it [community:ultrabridge, device-confirmed]. So the claim layout and key are free for Notedown [inferred]. allenporter uses its own claims (`sub`, `equipment_no`, `iat`, `exp` with 10-year device lifetime) and syncs fine [community:allenporter].
- Refresh: there is no refresh call in the device flow; `POST /api/user/query/token` returns `token` (QueryTokenVO) for the web UI [ratta-server] [ratta-web] [community:sncloud]. If the token becomes invalid the device must log in again [inferred].
- Unauthenticated access: Ratta answers HTTP 401 with `{"success":false,"errorCode":"401","errorMsg":"Unauthorized"}` from its global error handler [ratta-server]; the earlier reading "HTTP 200 with `success:false`" was wrong. `E0712` ("You are not logged in or your login has expired") and `E0085` (token invalid) are in Ratta's enum but never sent [ratta-server]. ultrabridge and opennotecloud document `E0712` [community:ultrabridge] [community:opennotecloud]; opennotecloud and allenporter answer 401 [community:opennotecloud] [community:allenporter]. Which the device needs is still open (9, #11). The web UI treats `S0016`/`S0073` as "go to login" [ratta-web].

### 2.6 Device binding

- One device is bound to one account (`e_user_equipment`: `equipment_number`, `user_id`, `name`, `status` `Y`/`N`) [ratta-server].
- `bindEquipment` on login, `unlink` on logout; `query/by/equipmentno {equipmentNo}` returns `equipmentNumber`, `userId`, `name`, `status` [ratta-server]. For an unknown device it answers success with empty fields [ratta-server].
- `activateEquipment {equipmentNo}` exists; not seen in device traces [ratta-server] [community:ultrabridge]. It needs no token, adds the device to Ratta's inventory (`e_equipment`) if it is missing and always answers success [ratta-server]. opennotecloud warns that first-time activation of a never-registered tablet is untested [community:opennotecloud].
- `POST /api/terminal/send/password/unlock/code {equipmentNo, language, code, type}` mails a reset code for the device lock screen or file protection password; it needs email and is reachable from the device [ratta-server]. Not seen in traces [unknown].

## 3. File sync protocol

### 3.0 Edge cases a compatible server must handle

Items 1 to 5 are ordered by the risk of data loss; item 6 collects the delete behaviour. Each item states what Ratta's server does and how Notedown differs.

1. **Names are case-insensitive inside one folder.** Ratta's name column ignores case and every device lookup compares without case [ratta-server] (analyzed). An `upload/finish` onto a name that differs from an existing file only in case replaces that file: new content, MD5 and size, but the old id and the old spelling stay [ratta-server] (analyzed). `move_v3` allows a rename that changes only the case (no `E0322`); a move onto another entry that differs only in case is not refused, so two entries that differ only in case can result (a Ratta bug) [ratta-server] (analyzed). Autorename probably compares lower-cased names and makes `stem(N).ext` [ratta-server]. A case-only rename done on the disk is seen by Ratta's reconcile as delete plus add: it renames the disk file to `stem(1).ext` and gives it a new id (3.0.1) [ratta-server] (analyzed). A server that compares with case lists two files where the device expects one [inferred]. Do: compare names without case per folder, keep the first spelling on replace, allow case-only renames, refuse a move onto a case variant [inferred]. Notedown: names compare by Unicode caseless matching (full case folding and normalization) per folder, also for every parent folder on the way. Every device route first maps the path to the stored spelling, segment by segment: the entry with exactly this name, else the one entry that differs only in case or normalization. `upload/apply` and `upload/finish` onto a case variant of an existing file replace that file like any overwrite (old bytes to the recycle bin in protect mode, `file.modified`, page index); the id and the first spelling stay. A case variant of a folder at the target of an upload stays `E0322`. A case variant of a folder on the way of an upload, `create_folder_v2`, `move_v3` or `copy_v3` is that folder: no second folder is made, and `path_display` has the stored spelling. `create_folder_v2` for a case variant of an existing folder answers `E0322`, with `autorename` it makes `name(1)`. `move_v3` allows a case-only rename (same id) and refuses a move onto a case variant of another entry with `E0322`; autorename compares folded names and makes `stem(N).ext`. `list_folder` and `query/by/path_v3` find a path in any case and answer with the stored spelling. A folder can hold two entries that differ only in case (an old store, a change by hand or a case-sensitive file system): an exact name always wins, and a name that matches both without case answers `E0322` (log flag `case_ambiguous`) and changes nothing (`an_upload_over_a_case_variant_replaces_it_and_keeps_id_and_spelling`, `a_case_variant_upload_into_a_case_variant_folder_is_one_file`, `an_nfd_upload_over_the_nfc_name_replaces_it`, `list_folder_finds_a_folder_spelled_in_another_case`, `create_folder_under_a_case_variant_of_a_folder_uses_that_folder`, `move_and_copy_into_a_case_variant_of_a_folder_use_that_folder`, `a_rename_that_only_changes_case_keeps_the_file_and_the_id`, `two_stored_case_variants_are_never_picked_silently` on a case-sensitive file system).
2. **Writes outside the sync lock.** Ratta checks the lock only in `synchronous/start`. Uploads, deletes, moves and copies of another device while one device syncs are accepted, and so is the implicit upload of Auto Sync (9.3) [ratta-server] (analyzed). `E0079` is never sent [ratta-server]. A web delete gets `E0301` while the lock is held (6.4) [ratta-server] (analyzed); other web actions probably too [ratta-server]. A device delete checks no lock [ratta-server] (analyzed). Notedown: deliberately stricter. A write without a lock takes the free sync lock (log marker `implicit_lock`), and `E0079` comes only while another device holds the lock (9.3); with protect on, digest and to-do calls answer `E0079` until the import of the device's app data (5.4). An upload onto a case variant of an existing file (item 1) is an overwrite and follows the same lock rules.
3. **Empty path segments.** The device sends paths such as `NOTE/Note/Personal//IMG.jpg` (3.5). In Ratta an empty segment breaks the path lookup, and `create_folder_v2`, `move_v3` and `copy_v3` would create a folder named `""`; trailing slashes do no harm [ratta-server] (analyzed). Do: drop empty segments everywhere [inferred]. Notedown: every path is parsed with empty segments dropped (double, leading and trailing slashes), in all routes, so `INBOX//c.pdf` is `INBOX/c.pdf` and no folder named `""` can be created; `.`, `..`, NUL and `\` in a segment are refused (`ids_arrive_as_strings_or_numbers_and_paths_with_double_slashes`, `finish_joins_the_parent_folder_and_the_name_and_takes_the_size_as_string`).
4. **Name length.** Ratta's name column holds at most 255 characters; a longer name fails with the generic `"500"`, not with `E0325` (which is web only) [ratta-server]. Notedown: a name longer than 255 bytes in UTF-8 (the limit of most file systems) is refused with `E0324` (log flag `name_too_long`) at `upload/apply`, `upload/finish`, `create_folder_v2`, `move_v3` and `copy_v3` before anything is written. Only names that are new on disk count: every new folder on the way and a name that autorename made longer. A name that exists already passes (macOS APFS takes 255 characters), also in a case-only rename or a move that keeps it. A name of fewer than 255 characters with many non-ASCII letters can be refused where Ratta accepts it; the device keeps the file (`a_name_over_255_bytes_is_refused_before_anything_is_written`, `names_over_255_bytes_are_refused_by_create_move_and_copy`, `a_long_name_that_is_stored_already_can_be_renamed_in_case_and_moved`).
5. **Paths with missing folders.** `list_folder` for a path that does not exist answers success with no entries (Ratta resolves it to a placeholder folder id) [ratta-server] (analyzed); `query/by/path_v3` answers success with `entriesVO: null` (3.5). `create_folder_v2`, `upload/finish` and `move_v3` create missing parent folders on their own [ratta-server] (analyzed). Notedown: `list_folder` for a missing path (or a file) answers `E0081`, never an empty list, because the device could read an empty list as "everything deleted" (`list_folder_shows_the_root_with_native_folders_and_files`). `query/by/path_v3` answers success with `entriesVO: null`. `create_folder_v2`, `upload/finish` and `move_v3` create missing parent folders (`create_folder_creates_missing_parents_first`, `a_new_note_under_a_new_folder_lands_with_folder_file_and_page_events`, `a_move_keeps_the_id_creates_parents_and_prunes_the_source_folders`).
6. **Deletes.** A device delete is a hard delete on Ratta: no recycle bin, the bytes are gone; an unknown id gives `E0318` (3.5) [ratta-server] (analyzed). Only a web delete goes to Ratta's recycle bin, and a restore brings back the same ids (6.4) [ratta-server] (analyzed). Ratta probably never removes a folder that a delete left empty [ratta-server]. To-do deletes are hard deletes without tombstones (6.1); a digest group delete probably leaves its digests alive (5.7) [ratta-server]. Notedown: a device delete always moves the file or folder to the recycle bin (`recycle/<ms>/<path>` with an index line), also with protect off; retention keeps a deleted file 90 days by default (`a_deleted_file_goes_to_the_recycle_bin_with_an_index_line`). A delete of an unknown id, an id that is no number or an id whose file is gone answers `E0318`, as Ratta (`unknown_ids_get_their_error_codes_and_never_panic`). A delete or move that leaves user folders empty removes them in the same commit (`folder.deleted`, no recycle entry); native folders always stay (`a_folder_delete_arrives_per_file_and_prunes_the_emptied_user_folders`). Notedown: to-do deletes leave tombstones for 5 days, and a delete of a missing task, group or sort answers success (6.1). Ratta differs: no tombstones, and a delete of a missing task answers the generic `"500"`; a missing group answers success (6.1) [ratta-server] (analyzed).

### 3.0.1 Ratta's reconcile of out-of-band changes (`compareWithNas`)

Ratta keeps its catalog in step with the files on disk by a reconcile that compares the disk tree `<local.file.directory>/<email>/Supernote` (default `/home/supernote/data/<email>/Supernote`) with the catalog rows of the user. There is no file watcher: the watch service class is not wired up and only logs [ratta-server] (analyzed). All statements in this subsection above the lessons list are [ratta-server] (analyzed) unless tagged otherwise.

When it runs:

- At every login (all four login paths). The call is marked asynchronous but is made inside the same class, so it runs synchronously inside the login request.
- Before `list_folder`, `list_folder_v3`, `query_v3` and `query/by/path_v3`.
- In `move_v3` and `copy_v3` when the source is missing on disk, in `download_v3` when the disk size differs from the catalog (3.5), and in `bindEquipment` (2.4).
- It returns at once while `<userId>_lockCloud` exists (the sync lock of any device) or `upload_in_progress_<userId>` is `"true"` (3.5). Extending the lock never creates it. So during a device sync none of these calls reconciles; the next reconcile comes with the next lock-free call or login.

Root handling:

- The disk walk keeps only `Document`, `Note`, `MyStyle`, `EXPORT`, `SCREENSHOT` and `INBOX` (exact case) at the first level. The first-level test is "the parent path ends with `Supernote`", so the same filter also applies inside any folder whose name ends in `Supernote`.
- The catalog tree is mapped to the disk layout first: `DOCUMENT/Document`, `NOTE/Note` and `NOTE/MyStyle` move to the root, and the root rows `DOCUMENT` and `NOTE` are dropped (3.3).
- Any other root-level catalog row (a folder or file a device made at the root) counts as "in the catalog, missing on disk" and is hard-deleted with everything below it. Its directory stays on disk, orphaned.
- A missing user root (volume not mounted, email changed) wipes the catalog: the reconcile deletes the user's recycle-bin directory and all recycle rows, creates the six empty top folders again and then hard-deletes every catalog row, since none is on disk. The next device sync sees every id gone, and the device deletes its local copies (3.1).

Per folder, names are grouped without case:

- On disk, not in the catalog: a new row with a new snowflake id, the MD5 computed from the file, the size from disk, terminal edit time 0, create and update time now; folders are walked into. Dotfiles and temporary files are not filtered.
- In the catalog (active rows only), not on disk: hard delete, for a folder with every row below it (inactive ones too), and the used capacity goes down. No recycle row, no action row, no push.
- Same exact name, different size: the id stays; new MD5, new size, the storage name set to the name, update time now.
- Same exact name, same size: nothing, even if the content changed; the modification time is never read. The catalog keeps the old MD5, so a device that compares `content_hash` does not download the new content [inferred].
- A name that matches only without case (disk `foo.note`, catalog `Foo.note`): Ratta renames the user's disk file to `foo(1).note` (the suffix is chosen against case-insensitive catalog lookups), inserts it as a new row with a new id and deletes the `Foo.note` row. An out-of-band case-only rename becomes delete plus add with a new id, and the file on disk gets another name.
- An out-of-band rename or move: delete plus add with a new id; the two are not matched.
- A file on one side and a folder on the other: only logged. Catalog rows whose parent is missing are skipped.

The reconcile writes no action rows and sends no socket push. Devices learn about its result only at their next full listing. A large reconcile batch can also fail on an id collision (3.2).

[proposal] Lessons for Notedown's scan:

- A missing or empty root: refuse to scan and journal nothing. Treat a scan result that would delete most of the tree as suspect, since Ratta wipes the catalog in that case.
- No name allow-list for root entries; Ratta's allow-list silently deletes device-made root entries from the catalog.
- Ratta skips the reconcile while a sync runs. A scan that waits for the lock must run soon after it (at `synchronous/end` or on a timer) and must not race an upload.
- Ratta detects changes by size only. Size plus modification time, with a lazy re-hash, is stricter and correct.
- Ratta turns out-of-band renames and moves into delete plus add with a new id. Rename detection (same size and hash) is optional.
- Never rename user files on disk. A case conflict is journaled instead and follows the rule of 3.0 item 1 (an exact name wins, a name that matches two entries only without case answers `E0322`, `case_ambiguous`).
- Ratta's reconcile writes no action rows and sends no push. Journal entries with `origin:"scan"` (8.3) and a push go beyond Ratta and stay compatible with the device.
- Decide whether to filter hidden and temporary files (`._*`, `.DS_Store`, editor swap files); Ratta takes them into the catalog.
- Ratta's disk layout has `Document`, `Note` and `MyStyle` at the top of `Supernote/`, while the wire uses `DOCUMENT/Document`, `NOTE/Note` and `NOTE/MyStyle`. This matters when a Ratta data directory is imported.
- Keep the recycle bin outside the scanned tree (outside `files/`); Ratta's recycle bin is outside its data tree too.

Notedown: the scan (`notedown-cloud/src/store/scan.rs`) follows these lessons. It sees what a device listing sees: dot-names (so `.nd-*.tmp` temp files), names the wire can't carry, symlinks and special files are skipped, there is no allow-list for root entries and it never renames, moves or deletes anything on disk; a broken tree (a missing or replaced `files/`) is never scanned (`hidden_and_temp_files_are_never_seen`, `a_broken_tree_is_never_scanned`). A cached MD5 is reused only when size, mtime and ctime (both in ns) all match, so a tool that keeps size and mtime (`touch -r`, `rsync -t`) is still caught; a `hashes.json` row without a ctime is hashed once after the upgrade, with no event when the MD5 is the same (`md5_is_cached_and_recomputed_on_size_mtime_or_ctime_change`, `same_size_rewrite_with_old_mtime_is_hashed_again`, `row_without_ctime_is_hashed_once_then_reused`, `a_file_changed_by_hand_is_journaled_as_modified`, `a_file_without_a_cached_md5_is_hashed_silently`). A gone file and a new file with the same size and MD5 (the gone file's MD5 from the cache) are one move and keep the id, when the pair is unique on both sides; a case-only rename is such a move, two identical files moved at once are delete plus create and a folder is never paired: it is deleted and created while its files pair one by one (`a_file_moved_by_hand_keeps_its_id`, `a_case_only_rename_by_hand_keeps_its_id`, `a_folder_renamed_by_case_only_keeps_the_ids_of_its_files`, `two_identical_files_moved_at_once_are_not_paired`, `a_move_needs_the_cached_size_too`). A plan that removes files is not applied when fewer than `scan_min_files` files (default 1, 0 never holds) would be left in the whole account, also for a scoped scan; this covers an empty data folder and an unmounted volume. The scan then writes `scan-hold.json` and applies nothing. While it exists, every device file route and `synchronous/start` answer `E0079` with the log flag `scan_hold`, the push gate is closed, digest and to-do routes stay open and `serve` prints one warning, with the command that accepts the hold. The hold lifts by itself at the next whole-tree scan once the files are back; while an account is held, the watch scans its whole tree, so files put back by hand end the hold within seconds (`the_watch_lifts_a_hold_when_the_files_come_back_by_hand`). It also lifts at once with `notedown cloud scan --data <dir> --account <slug> --accept`; a plan that removes nothing never holds (`deleting_every_file_holds_and_applies_nothing`, `a_large_cleanup_that_leaves_one_file_never_holds`, `scan_min_files_counts_the_whole_account_in_a_scoped_scan`, `an_empty_files_folder_on_a_never_imported_account_holds`, `the_hold_is_released_when_the_files_come_back`, `accept_applies_the_mass_delete_and_removes_the_hold`, `sync_start_answers_scan_hold_after_a_mass_delete`, `no_push_while_a_hold_exists`, `the_hold_warning_names_the_account_and_the_way_out`). It runs at `synchronous/start` (under the store lock, before the lock is granted and not while another device holds the sync lock, checked again under the store lock), before every device route that touches files (for the listed folder, recursive when the listing is) and on a timer every `scan_interval` seconds (default 300, 0 = no timer; the first run is at start, and 0 also skips it), and, with `watch = true` (default), for the lowest folder that holds all paths the file system reports as changed, recursive, 2 s after the last change, so a file moved by hand between two folders keeps its id (`a_file_added_by_hand_is_journaled_by_the_watch`, `a_move_across_folders_keeps_its_id_in_a_watch_scan`), skipped while a device syncs, for an account without a `files/` folder and for an account that is not imported yet while protect is on (both checked again under the store lock). `notedown cloud put` scans the folders on its way first, as a device upload does, and refuses while the account is held; `notedown cloud journal snapshot` scans the whole tree first and refuses while held; `notedown cloud import` scans the whole tree first and never holds, because it replaces the tree (`a_put_journals_the_hand_changes_on_its_way_first`, `a_put_refuses_while_the_account_is_held`, `cloud_journal_snapshot_refuses_while_the_account_is_held`). So a device listing finds nothing new that the scan has not journaled: a file added by hand has a `file.created` event with `origin: "scan"` before the answer, and the follower pushes scan events like any other (`a_hand_added_file_is_journaled_before_the_device_lists_it`, `the_v3_listing_and_the_queries_journal_first_too`, `sync_start_journals_hand_deletes_and_grants_the_lock`, `sync_start_does_not_scan_while_another_device_syncs`, `the_timer_scan_skips_while_a_device_syncs`, `scan_interval_zero_turns_the_timer_off`, `a_scan_event_is_pushed`). A write that replaces a file (a device upload, `put`, `import`) drops the cached MD5 of that file, so the next scan hashes it without an event (`a_put_that_replaces_a_file_is_not_journaled_again_by_the_scan`, `a_scan_after_a_replace_finds_nothing`). A file that changes while the scan hashes it counts as there but gets no id and no event until a later scan reads it whole; a file that vanishes while it is hashed is gone (`a_file_that_changes_while_hashed_is_left_for_the_next_scan`, `a_file_that_vanishes_while_hashed_is_gone`). A file row above the scope of a scan whose path is a folder now is journaled as `file.deleted` (`a_file_above_the_scope_that_became_a_folder_is_journaled_as_deleted`). A crash between the journal append and the registry append can leave journaled ids that are never used again: the next scan journals the same change with new ids (`ids_journaled_before_a_crash_are_never_handed_out_again`).

### 3.1 The model

- The server is a passive file store with a catalog. The device drives every sync decision [community:opennotecloud] [community:ultrabridge] [community:allenporter].
- The device keeps its own sync database (last known server id, path and hash per file). On each sync it lists the whole server tree, classifies every file on both sides as no change, create, delete, modify, move or move+modify, and applies a reconciliation matrix [community:opennotecloud]:

| Local \ Cloud | no change | create | delete | modify | move |
|---|---|---|---|---|---|
| no change | skip | download | delete local | download | local move |
| create | upload | conflict | upload | upload | upload |
| delete | delete cloud | skip | skip | skip | skip |
| modify | upload | upload | upload | conflict | upload |
| move | cloud move | upload | upload | upload | conflict |

- Consequence: if the server stops listing a file that the device synced before and the device copy is unchanged, the device **deletes its local copy** [community:opennotecloud]. The device has safety checks: no delete if the file is open, locally changed, or if a PDF's `.mark` changed (then it re-uploads) [community:opennotecloud].
- Files are uploaded whole, no delta, no compression; change detection is by MD5 (`content_hash`) [community:ultrabridge].
- Ratta's server does not watch its data directory. It reconciles out-of-band changes into its catalog at login and before `list_folder`, `query_v3` and `query/by/path_v3`, but skips that while any device holds the sync lock or an upload is in progress, so during a device sync these calls do not reconcile (3.0.1) [ratta-server] (analyzed). An earlier reading of the watch service class as a live watcher was wrong.

### 3.2 What identifies a file

| Item | Meaning | Source |
|---|---|---|
| `id` | server id of a file or folder (Ratta: 64-bit snowflake; any stable integer works). Stable across rename and move; a copy gets a new id. Sent by the device as a JSON **string** (`"id":"819922212654940160"`), returned as string in `EntriesVO` | [ratta-server] [community:ultrabridge, device-confirmed] |
| `path_display` | path relative to the user's root, for example `NOTE/Note/Work/x.note` (Ratta returns it without leading slash in listings) | [ratta-server] [community:ultrabridge] |
| `parent_path` | parent directory with leading slash, `/NOTE/Note/Work` [community:ultrabridge]. Ratta's `list_folder` (v2) puts the requested `path` (leading `/` and trailing `/` removed) into the direct children; in a recursive listing the deeper entries get their real parent folder, built from the requested path, so a root listing gives a leading `/` (`/NOTE/Note`). `list_folder_v3`, `query_v3` and `query/by/path_v3` send no `parent_path` [ratta-server] (analyzed) | [community:ultrabridge] [ratta-server] |
| `content_hash` | lowercase hex MD5 of the bytes | [ratta-server] [community:ultrabridge] [community:allenporter] |
| `size` | bytes | same |
| `lastUpdateTime` | ms; in Ratta's `list_folder` (v2) null for the direct children and set for the deeper entries of a recursive listing; set in `list_folder_v3`, `query_v3`, move and copy [ratta-server] (analyzed) | same |
| `innerName` | storage name chosen at upload apply (Ratta: equal to the file name in observed traffic). Ratta's name on disk is the file name of the multipart part, not `innerName` or `fileName` (3.5) | [ratta-server] [community:ultrabridge] |
| root | id `0` (or null) means the user root | [ratta-server] [community:ultrabridge] |

Folders exist as catalog rows (`is_folder` `Y`) with size 0; in listings a folder has `content_hash` `""` and `size` 0 [ratta-server] (analyzed). File and folder names compare without case inside one folder (3.0) [ratta-server].

Id collisions on Ratta [ratta-server] (analyzed): every id comes from a newly created snowflake generator with worker, datacenter and sequence 0, so an id is only the ms since 2020-01-01 shifted left by 22 bits, and two ids made in the same ms are equal. The file id is the primary key, so a folder copy or a reconcile batch (3.0.1) that makes two rows in one ms fails with a duplicate key; the copy runs in one transaction, is rolled back and answers the generic `"500"`. Digests and digest groups get their ids the same way. The insert of an action row retries with the same id, so a colliding action row is lost. How often this happens depends on timing and is not known.

### 3.3 Which folders the device syncs

- Server-side folder manifest, sent by `bindEquipment` and also hard-coded in Ratta: `NOTE/Note`, `NOTE/MyStyle`, `DOCUMENT/Document`, `EXPORT`, `SCREENSHOT`, `INBOX` [ratta-server] [community:ultrabridge, device-confirmed].
- Wire paths use the two-level form (`/NOTE/Note/...`, `/DOCUMENT/Document/...`) [community:allenporter trace] [community:ultrabridge]. Ratta maps between this form and a flat form (`Supernote/Note`, `Supernote/Document`, `Supernote/MyStyle`) in several places [ratta-server] [community:opennotecloud maps `NOTE/Note` to disk `Supernote/Note`]. On disk Ratta uses the flat form: `Document`, `Note`, `MyStyle`, `EXPORT`, `SCREENSHOT` and `INBOX` directly under `Supernote/`; its reconcile maps the catalog to this form and drops everything else at the root (3.0.1) [ratta-server] (analyzed).
- Ratta's on-disk root is `/home/supernote/data/<email>/Supernote/` [ratta-server].
- The user selects which folders sync in the device settings [community:allenporter README].
- PDF/EPUB annotations live in sidecar files `<document>.mark` (for example `book.pdf.mark`) next to the document and sync as normal files [community:ultrabridge]. `.doc` files written by the device's Word app sync like other files [community:ultrabridge].
- Allowed file types (reference parameter `FILE_TYPE`): `note, mark, pdf, epub, doc, docx, txt, png, jpg, jpeg, webp, bmp, gif, tif, tga, psd, cbz, fb2, xps, chm, rtf, ppt, pptx, xls, xlsx, zip, rar, gz, tar.gz, zz, apk, snstk, snbak, read, spd, otf, ttf, ttc, eot, dfont, woff`; max file size 1 GiB (`FILE_MAX`) [community:ultrabridge, read from `/api/official/system/base/param`]. Whether the device enforces this list is [unknown].
- Digests are not files in this tree; they sync over the summary API (section 5) [ratta-server] [community:ultrabridge].

### 3.4 Sequence of a sync run

Observed on devices [community:allenporter trace] [community:ultrabridge 0b trace, Nomad]:

```
POST /api/file/2/files/synchronous/start      lock
POST /api/file/2/files/list_folder            {"path":"/","recursive":true}  whole tree
  per file to upload:
    POST /api/file/3/files/query/by/path_v3   exists?
    POST /api/file/2/files/create_folder_v2   for new folders, before their files
    POST /api/file/3/files/upload/apply       get signed URLs
    POST /api/oss/upload                      bytes (or /api/oss/upload/part chunks)
    POST /api/file/2/files/upload/finish      commit
  per file to download:
    POST /api/file/3/files/download_v3        get signed URL
    GET  /api/oss/download                    bytes
  per local delete / move / copy:
    POST /api/file/3/files/query_v3           source exists?
    POST /api/file/3/files/delete_folder_v3 | move_v3 | copy_v3
POST /api/file/2/files/synchronous/end        {"flag":"Y"} success, {"flag":"N"} failure
```

Around the file sync the device also calls `bind/status`, `capacity/query`, `query/summary/hash|group|id`, `schedule/group/all`, `schedule/task/all` and `PUT schedule/task/list` [community:ultrabridge 0b endpoint counts].

One community reference states that device firmware does not call `synchronous/start|end` and lists with `/api/file/3/files/list`; both device traces contradict this, and Ratta's server has no `/3/files/list` route [community:allenporter trace] [community:ultrabridge 0b] [ratta-server]. Treat that statement as wrong.

### 3.5 Endpoint reference (device file API)

All under `F_FileLocalController`, prefix `/api/file`, token required, every body carries `equipmentNo` [ratta-server]. No file DTO requires `equipmentNo`; a missing token gives HTTP 401 (7.1) [ratta-server].

#### synchronous/start

```json
POST /api/file/2/files/synchronous/start
{"equipmentNo": "SN078C10000000"}

{"success": true, "errorCode": null, "errorMsg": null,
 "equipmentNo": "SN078C10000000", "synType": true}
```

- `synType` true = normal compare; false = the device should upload all its files without comparing (used when the server is empty) [ratta-server field description] [community:allenporter returns false for an empty account]. Ratta answers false exactly when the account has no folder at the root [ratta-server] (analyzed).
- Locking: one sync per account. Another device starting a sync while the lock is held gets `E0078` ("Sync in progress, please wait until it's completed before proceeding!") [ratta-server] [community:allenporter]. allenporter uses a 5 min lock [community:allenporter]. Ratta's lock [ratta-server] (analyzed):
  - Redis key `<userId>_lockCloud`, value = `equipmentNo` plus the socket channel; "held by this device" means the value contains the device's `equipmentNo`. Start also stores the start time under `<equipmentNo>_<userId>` for 1440 min.
  - `E0109` if the account has neither phone nor email; `E0078` only if the lock exists and does not contain this `equipmentNo`. The same device may start again while it holds the lock.
  - Start sets the lock to `lock_cloud_time` = 30 s.
  - Only `synchronous/start` checks the lock (3.0). Other calls only extend it, and only for the device that holds it (the `equipmentno` header first, then the body's `equipmentNo`): to at least 120 s (a value of 30 s or less becomes 120 s), for files above 61,440,000 bytes to 120 s plus 10 s per started 5,120,000 bytes, at most 600 s; operations without a size give 120 s.
  - `upload_in_progress_<userId>` is set by `upload/apply` for 2 min and deleted at the end of the same call and again by `upload/finish`.
- Ratta sends no push on `synchronous/start` (`WAITING` is never sent). It pushes `STARTSYNC` on `synchronous/end` (4.5) [ratta-server].

#### synchronous/end

`{"equipmentNo": "...", "flag": "Y"|"N"}` -> envelope + `equipmentNo`. Releases the lock [ratta-server] [community:allenporter trace shows `flag:"N"` after a failed sync]. Ratta ignores `flag`, deletes the lock only if this device holds it, and answers the generic `"500"` to an `end` without a `start` (the start time key is missing) [ratta-server] (analyzed).

#### list_folder (v2, used by the device)

```json
POST /api/file/2/files/list_folder
{"path": "/", "recursive": true, "equipmentNo": "SN078C10000000"}

{"success": true, "errorCode": null, "errorMsg": null, "equipmentNo": "SN078C10000000",
 "entries": [
  {"tag": "folder", "id": "101", "name": "NOTE", "path_display": "NOTE", "size": 0},
  {"tag": "folder", "id": "102", "name": "Note", "path_display": "NOTE/Note", "size": 0,
   "parent_path": "/NOTE"},
  {"tag": "file", "id": "4711", "name": "20261006_090000 Bike.note",
   "path_display": "NOTE/Note/20261006_090000 Bike.note",
   "content_hash": "0cc175b9c0f1b6a831c399e269772661", "size": 1135077,
   "lastUpdateTime": 1791270408000, "parent_path": "/NOTE/Note",
   "is_downloadable": true}
 ]}
```

- Request `ListFolderV2DTO`: `equipmentNo`, `path` (empty or `/` for root), `recursive` [ratta-server] [community:allenporter trace].
- Entry `EntriesVO`: `tag` (`"file"`/`"folder"`), `id`, `name`, `path_display`, `content_hash`, `is_downloadable`, `size`, `lastUpdateTime`, `parent_path` [ratta-server]. On Ratta's wire the downloadable flag appeared as `_downloadable` (a serializer artifact of the field name) [community:ultrabridge]; Ratta's serializer always writes `_downloadable`, never `is_downloadable`, and writes null fields [ratta-server]. Other servers send `is_downloadable` and the device syncs [community:allenporter] [community:opennotecloud]. Sending both is harmless [inferred].
- The listing returns the whole tree, flat, O(n) [community:ultrabridge]. Ratta: root is `""`, a leading `/` is removed; `path_display` is computed per entry without a leading slash; `parent_path` is the requested path for direct children and the real parent folder for deeper entries of a recursive listing (3.2); `lastUpdateTime` is null for direct children and set (ms) for deeper entries of a recursive listing; `path_display` is the full parent path plus the name; `_downloadable` is true; entries come in database order; a missing path answers success with no entries (3.0) [ratta-server] (analyzed). The disk reconcile runs first unless a sync lock or upload flag is set (3.0.1) [ratta-server] (analyzed).
- Hidden server folders (recycle, staging, digest blobs) must not be listed [community:ultrabridge] [inferred].

#### list_folder_v3

`{"equipmentNo", "id", "recursive"}` -> same `entries` list for the folder with that id [ratta-server] [community:allenporter]. `id` is a number in Ratta's DTO; a digit string is converted, anything else gives `"422"` [ratta-server]. Ratta's v3 entries have no `parent_path` and carry `lastUpdateTime` in ms [ratta-server] (analyzed). Firmware 3.29.42 did not call it in the first syncs; it used `2/files/list_folder` (9.2) [capture:nomad-3.29.42]. Implement it anyway [proposal].

#### query/by/path_v3

```json
POST /api/file/3/files/query/by/path_v3
{"path": "/NOTE/Note/Work", "equipmentNo": "SN078C10000000"}

{"success": true, "errorCode": null, "errorMsg": null,
 "equipmentNo": "SN078C10000000", "entriesVO": null}
```

`entriesVO` is null when the path does not exist, else one `EntriesVO` [ratta-server] [community:ultrabridge]. It must answer HTTP 200 with `entriesVO: null`; a 404 for an unknown path made the device abort the sync (`synchronous/end` with `flag:"N"`) [community:allenporter failed-sync trace]. Ratta: an empty path gives `E0334`; leading and trailing `/` are removed; a file wins over a folder of the same name; the answer has no `parent_path`; the disk reconcile runs first unless a sync lock or upload flag is set (3.0.1) [ratta-server] (analyzed).

#### query_v3

`{"equipmentNo", "id": "<string>"}` -> envelope + `equipmentNo` + `entriesVO` (null if unknown) [ratta-server] [community:ultrabridge]. The device uses it before delete and move, and for files it has locally that may not exist on this server [community:ultrabridge]. Ratta reads `id` as a string and converts it to a number: an id that is not a number gives the generic `"500"`; an unknown or deleted id gives success with `entriesVO: null`. The answer has `path_display` = parent path + `/` + name (a root item gives `/NAME`, with a leading slash, unlike the listings), no `parent_path`, and `lastUpdateTime` in ms. The disk reconcile runs first unless a sync lock or upload flag is set (3.0.1) [ratta-server] (analyzed).

#### create_folder_v2

```json
POST /api/file/2/files/create_folder_v2
{"equipmentNo": "SN078C10000000", "path": "/NOTE/Note/Work", "autorename": false}

{"success": true, "errorCode": null, "errorMsg": null, "equipmentNo": "SN078C10000000",
 "metadata": {"tag": "folder", "id": "5120", "name": "Work", "path_display": "NOTE/Note/Work"}}
```

The response must contain a real `metadata.id`: the device needs it for the following uploads; with an empty stub it ends the sync right after this call [community:ultrabridge, device-confirmed]. Name collision with `autorename:false` -> `E0322` [community:ultrabridge] [ratta-server enum]. The device first calls `query/by/path_v3` for the folder [community:ultrabridge]. Ratta creates missing parent folders, answers `Name(1)` for a collision with `autorename:true`, and gives a real id and `path_display` without a leading slash [ratta-server] (analyzed). The `409` resubmit check on this route never fires (3.7).

#### upload/apply

```json
POST /api/file/3/files/upload/apply
{"equipmentNo": "SN078C10000000", "path": "/NOTE/Note/Work/Bike.note",
 "fileName": "Bike.note", "size": 1135077}

{"success": true, "errorCode": null, "errorMsg": null,
 "equipmentNo": "SN078C10000000", "bucketName": "Bike.note", "innerName": "Bike.note",
 "xamzDate": "1791270408879", "authorization": "<signature>",
 "fullUploadUrl": "https://notes.example.org/api/oss/upload?signature=<sig>&timestamp=1791270408879&nonce=<uuid>&path=<b64url>",
 "partUploadUrl": "https://notes.example.org/api/oss/upload/part?signature=<sig>&timestamp=1791270408879&nonce=<uuid>&path=<b64url>"}
```

- Request `FileUploadApplyLocalDTO`: `equipmentNo`, `path`, `fileName`, `size` [ratta-server]. On the wire `path` is the **full destination path including the file name**, and `size` is a JSON **number** [community:ultrabridge, device-confirmed]. Ratta cuts the last segment off `path`; it reads `size` as a string (empty = 0, not a number -> `"500"`); it does no lock, capacity or name check; `innerName` = `fileName` [ratta-server] (analyzed).
- Response `FileUploadApplyLocalVO`: `equipmentNo`, `bucketName`, `innerName`, `xAmzDate`, `authorization`, `fullUploadUrl`, `partUploadUrl` [ratta-server]. Ratta's wire spelling of the date field is `xamzDate` [community:ultrabridge] [inferred: serializer lowercases the leading capitals]. `bucketName` and `innerName` equal the file name in Ratta's traffic [community:ultrabridge]; allenporter returns the file name as `bucketName` because a reference check expects that [community:allenporter].
- In Ratta's URLs `path` is the base64url (no padding) of the target **directory** (for example `L05PVEUvTm90ZQ` = `/NOTE/Note`) [community:ultrabridge]. Notedown can encode anything it can later resolve, since it signs and verifies its own URLs [community:ultrabridge] [community:allenporter] [community:opennotecloud].

#### Byte upload: /api/oss/upload

- `POST <fullUploadUrl>`, `multipart/form-data` with one part named `file` (filename = file name) [ratta-server: request parameters `file`, `signature`, `timestamp`, `nonce`, `path`] [community:ultrabridge] [community:allenporter].
- Response `UploadFileVO`: envelope + `innerName`, `md5` (Ratta returned both null in traffic) [ratta-server] [community:ultrabridge].
- Ratta's handler also reads `x-access-token` if present [ratta-server string]; the signature is the real gate [community:ultrabridge] [community:opennotecloud] [inferred]. Ratta takes the user of `/api/oss/upload` and `/api/oss/upload/part` from `x-access-token` (except for the conversion directory, user `"0"`); the download has no token check (see Byte download) [ratta-server] (analyzed).
- Ratta writes the upload straight to its final path, with no temporary file, so the previous content is overwritten before `upload/finish`. The name on disk is the file name of the multipart part, not the `fileName` of apply or finish; if they differ, catalog and disk disagree. A chunked upload writes `<dir>/<multipart file name>` after all parts are in [ratta-server] (analyzed).
- Ratta's signature: SHA-256 hex over base64url path + timestamp + nonce + file size (`0`) + a server key, valid 30 min; download signature is the same without size, valid 24 h; no nonce replay tracking [community:ultrabridge, reproduced byte for byte from device traffic] [ratta-server]. A timestamp in the future passes [ratta-server].
- Multipart limit: 1024 MB per file and per request; a multipart that cannot be parsed gives HTTP 500 `FILE_UPLOAD_FAILED`; a failed upload answers "File upload failed." (`E1305`) [ratta-server]. Notedown may use any scheme (for example HMAC with its own key, single-use nonces) [inferred].
- A bad signature on Ratta's OSS endpoints yields HTTP 500 with plain text, not a JSON envelope [community:ultrabridge]. The text is "Signature verification failed."; the code `E1306` itself is never sent [ratta-server].

#### Chunked upload: /api/oss/upload/part

- `POST <partUploadUrl>&uploadId=<id>&partNumber=<n>&totalChunks=<N>`, multipart part `file` [ratta-server: parameters `partNumber`, `totalChunks`, `uploadId`] [community:allenporter] [community:opennotecloud].
- Used for files of 8 MiB and more, chunk size 8 MiB, `uploadId` = `<ms>_<uuid>`, `partNumber` 1-based [community:ultrabridge] [community:allenporter].
- Response `FileChunkVO`: `uploadId`, `partNumber`, `totalChunks`, `chunkMd5`, `status` [ratta-server].
- The server merges when all chunks are present (Ratta keeps a status file per upload, skips chunks already received, checks the merged size) [ratta-server strings]. allenporter merges when `partNumber == totalChunks` [community:allenporter]; ultrabridge tolerates retries and out-of-order parts [community:ultrabridge]. There is no separate "merge" call [ratta-server] [inferred]. Ratta's status file is `upload_status.txt`, chunks are `*_chunk_<n>`, and the merge checks only the total size; chunk folders older than 2 days are deleted every day at 12:00 [ratta-server].
- Error `E1305` / `E0355`..`E0357` on chunk failure [ratta-server].

#### upload/finish

```json
POST /api/file/2/files/upload/finish
{"equipmentNo": "SN078C10000000", "path": "/NOTE/Note/Work/", "fileName": "Bike.note",
 "content_hash": "0cc175b9c0f1b6a831c399e269772661", "innerName": "Bike.note", "size": "1135077"}

{"success": true, "errorCode": null, "errorMsg": null, "equipmentNo": "SN078C10000000",
 "path_display": "NOTE/Note/Work/Bike.note", "id": "4711", "size": 1135077,
 "name": "Bike.note", "content_hash": "0cc175b9c0f1b6a831c399e269772661"}
```

- Request `FileUploadFinishLocalDTO`: `equipmentNo`, `path`, `size` (string), `fileName`, `content_hash`, `innerName` [ratta-server]. Here `path` is the **parent directory with a trailing slash** and `size` a **string** [community:ultrabridge, device-confirmed]. Target = parent + `fileName`; joining the apply path with the file name creates `X.note/X.note` [community:ultrabridge].
- The server creates or replaces the catalog entry. Replacing keeps the id [ratta-server: finish updates an existing row by name and folder] [community:allenporter]. Ratta does **not** check MD5, size or the file on disk: it takes `content_hash` and `size` as sent [ratta-server] (analyzed). It removes a leading `/`, creates missing parent folders, and looks the name up without case (3.0): an existing file gets the new `innerName`, MD5 and size and keeps its id and spelling, else a new row is made. A `size` that is not a number -> `"500"`. Answer: `id` (string), `name`, `size` (number), `content_hash`, `path_display` = request path without leading `/` + `fileName` [ratta-server] (analyzed). Required fields (missing -> `"400"`): `fileName`, `content_hash`, `innerName` [ratta-server]. No capacity check; the used capacity is adjusted [ratta-server].
- Errors: ultrabridge uses `E0324` (cannot be uploaded) for a hash mismatch [community:ultrabridge]. On Ratta `E0324`, `E0325` (name too long) and `E0309` (capacity) come only from the web upload; `E0310`, `E0333` and `E0082` are never sent [ratta-server].
- Ratta pushes a socket message to the user's other devices after finish (section 4) [ratta-server].
- One reference names `/api/file/3/files/upload/confirm`; no such route exists in Ratta's server, devices use `2/files/upload/finish` [ratta-server] [community:ultrabridge 0b].

#### download_v3

```json
POST /api/file/3/files/download_v3
{"equipmentNo": "SN078C10000000", "id": "4711"}

{"success": true, "errorCode": null, "errorMsg": null, "equipmentNo": "SN078C10000000",
 "id": "4711", "url": "https://notes.example.org/api/oss/download?path=<b64url>&signature=<sig>&timestamp=1791270408879&nonce=<uuid>&pathId=<userId>",
 "name": "Bike.note", "path_display": "NOTE/Note/Work/Bike.note",
 "content_hash": "0cc175b9c0f1b6a831c399e269772661", "size": 1135077, "is_downloadable": true}
```

`id` arrives as a string; a server that parses it only as a number fails every download (`E0321`) [community:ultrabridge, device-confirmed]. Unknown id -> `E0321` [community:ultrabridge] [community:opennotecloud]. Ratta: `id` is a number in the DTO (a digit string is converted, anything else gives `"422"`) [ratta-server]; an unknown or deleted id gives `E0321`, and so does a row whose file is missing on disk, at once and without a reconcile. Ratta compares only the size: when the disk size differs from the catalog, it computes the MD5 of the disk file (failures map to the disk codes `E0342`..`E0357`), stores it (not the size), answers the new `content_hash` with the old catalog `size`, and runs the reconcile (3.0.1); when the size is equal, it answers the catalog MD5 without reading the file. It extends the sync lock by the file size, only for the device that holds it [ratta-server] (analyzed).

#### Byte download: GET /api/oss/download

- Query `path`, `signature`, `timestamp`, `nonce`, `pathId` [ratta-server]. No token needed; the signature is the auth [community:ultrabridge] [community:allenporter]. On Ratta `pathId` carries the user id and selects the user's email directory, not the file id [ratta-server] (analyzed). A path that disappeared between `download_v3` and the `GET` ends in the generic error handler (7.1) [ratta-server] (analyzed).
- Supports `Range: bytes=a-b` with `206`, `Content-Range`, `Accept-Ranges: bytes`, `Content-Disposition: attachment; filename*=UTF-8''...` [ratta-server strings] [community:allenporter]. A failed download answers HTTP 500 with plain text [ratta-server]. The Partner app always sends `Range: bytes=0-` [community:ultrabridge].
- Device paths can contain double slashes (`NOTE/Note/Personal//IMG.jpg`); tolerate them everywhere [community:ultrabridge, device-confirmed]. Ratta itself does not (3.0).

#### delete_folder_v3

`{"equipmentNo", "id"}` -> envelope + `equipmentNo` + `metadata {tag, id, name, path_display}` [ratta-server] [community:ultrabridge]. Works for files and folders [community:ultrabridge]. The device deletes a folder by deleting each contained file by id and never sends a delete for the folder itself; an emptied folder that stays listed comes back on the device as a "zombie" folder, so the server must prune folders a delete or move left empty (but never the six native folders) [community:ultrabridge, device-confirmed]. Unknown id -> `E0318` [community:ultrabridge] [ratta-server enum]. Ratta [ratta-server] (analyzed):

- A device delete is a **hard delete**, as [community:ultrabridge] says, and checks no sync lock. A file: the catalog row and the file on disk are deleted and the used capacity goes down. A folder: its row and every active row below it are deleted (capacity per file), and the directory on disk recursively. Ratta writes an action row and sends a socket push (4.5). Disk errors are only logged; the answer is success. Nothing goes to the recycle bin; only the web UI feeds it (6.4). This answers unknown #4.
- An unknown or deleted id gives `E0318`. The answer to a folder delete carries `metadata.id` as the string `"null"` (a Ratta bug).
- Ratta probably never removes a folder that a delete left empty [ratta-server]; the zombie pruning above is community advice from device tests, not Ratta behaviour.
- `id` is a number in the DTO; a digit string is converted, anything else gives `"422"` [ratta-server].

#### move_v3 (also rename)

```json
POST /api/file/3/files/move_v3
{"equipmentNo": "SN078C10000000", "id": "4711",
 "to_path": "/NOTE/Note/Archive/Bike 2026.note", "autorename": false}
```

- Response: envelope + `equipmentNo` + `entriesVO` [ratta-server].
- `to_path` is the **full destination path including the new name**, despite the field description "target parent directory" [community:ultrabridge, device-confirmed]. The id stays the same [community:ultrabridge].
- The device calls `query_v3` (source) and `query/by/path_v3` (target) first [community:ultrabridge].
- Errors: `E0320` source missing, `E0322` name exists with `autorename:false`, `E0358` folder into itself [ratta-server] [community:ultrabridge].
- Ratta [ratta-server] (analyzed): a missing or deleted source gives `E0320`; missing target folders are created; a folder into its own subtree gives `E0358`; a collision with `autorename:false` gives `E0322`, but not for a rename that changes only the case, and not for a target that differs only in case (3.0); a target in the same folder is a rename, else a move; disk errors map to `E0342`..`E0357`, IO errors to `E0343`/`E0344`; the id stays.
- `to_path` is not validated: a missing `to_path` gives the generic `"500"`; `id` is required [ratta-server].
- When the source is in the catalog but missing on disk, Ratta runs the disk reconcile, unless a sync lock or upload flag is set (3.0.1) [ratta-server] (analyzed).

#### copy_v3

Same request as `move_v3`; the copy gets a new id [ratta-server] [community:ultrabridge]. A missing source gives `E0320`, not `E0308` [ratta-server] (analyzed). A collision with `autorename:false` gives `E0322`, an IO error `E0345` [ratta-server] (analyzed). The copy and every row below it get new snowflake ids; the answer's `entriesVO.id` is the **new** id of the copy (an earlier reading said the source id) and `lastUpdateTime` is now in ms. Copied rows keep MD5, size and storage name; the used capacity goes up. Ratta writes the catalog rows before it copies on disk, outside a transaction: a disk failure answers a disk error code while the rows stay without files, until a later reconcile deletes them (3.0.1). A folder copy can fail on an id collision with `"500"` (3.2). A source missing on disk runs the reconcile as in `move_v3` [ratta-server] (analyzed). `to_path` is required (missing -> `"400"`) [ratta-server]. With `autorename:true` Ratta-style names are `name(1)`, `name(2)` [community:opennotecloud]; probably `stem(N).ext` [ratta-server].

#### Capacity

- `POST /api/file/2/users/get_space_usage {"equipmentNo"}` -> envelope + `equipmentNo`, `used` (bytes), `allocationVO {tag, allocated}` [ratta-server]. Ratta uses tag `individual` [ratta-server string near the capacity handler]; allenporter sends `personal` and 10 GiB [community:allenporter]. Ratta reads both from its `f_capacity` row; without a row, `used` and `allocationVO` are null [ratta-server] (analyzed).
- `POST /api/file/capacity/query` (no body) -> envelope + `usedCapacity`, `totalCapacity`; the Nomad called this one in a sync session [ratta-server] [community:ultrabridge 0b]. Must return sane numbers; opennotecloud reports real filesystem numbers [community:opennotecloud].

### 3.6 Conflicts

Conflict resolution happens on the device (matrix in 3.1); on a conflict the device renames with a conflict suffix or re-uploads [community:opennotecloud]. The server only refuses name collisions (`E0322`) [ratta-server]. The enum also has `E0304` (same name), `E0303` (already deleted), `E0305` (already moved), `E0306` (target deleted) and `E0342` ("data has expired, re-sync"), but the device file service probably never sends `E0303` to `E0306`, and `E0342` only as a disk error code [ratta-server].

### 3.7 Request deduplication

Ratta marks `create_folder_v2`, `upload/apply`, the to-do group add and the task add with a resubmit check of 1 s (`409`, "Please do not resubmit the data") [ratta-server] [community:ultrabridge]. The check catches its own error, so it never refuses anything and `409` is never sent [ratta-server]. Not required for the device [inferred].

### 3.8 Older and other upload paths

`/api/file/terminal/upload/apply|finish` (fields `filePath`, `fileSize`, `md5`, `modifyTime`, `uploadTime`) exist but the Nomad does not use them [ratta-server] [community:ultrabridge 0b]. The web UI uses `/api/file/upload/apply` (`directoryId`, `fileName`, `size`, `md5`) + `/api/oss/upload` + `/api/file/upload/finish` (`directoryId`, `fileName`, `fileSize`, `md5`, `innerName`, `type`) [ratta-server] [ratta-web] [community:ultrabridge].

### 3.9 Signed URL key

The device never computes a signature; it uses the URLs as given [community:ultrabridge] [community:allenporter] [community:opennotecloud]. So Notedown uses its own key and scheme [inferred].

## 4. Auto sync (socket.io)

Statements tagged [ratta-server] in this section describe behaviour of Ratta's server as read in its analyzed code (0.1). Class names are given as evidence.

### 4.1 Transport

- Engine.IO protocol 3 (socket.io 2.x wire), WebSocket transport, path `/socket.io/`, default namespace `/` only, no rooms [ratta-server: netty-socketio 2.0.3] [community:ultrabridge] [community:allenporter] [community:opennotecloud]. Ratta selects recipients by Redis keys, not by rooms [ratta-server].
- One socket server on port 18072 carries all listeners: file, to-do and digest (`SocketIoConfiguration`, `SocketIOEventHandler`) [ratta-server]. On Ratta's setup it is reached through the same external port as the REST API (nginx `location /socket.io/`) [ratta-server] [doc].
- A second server on 18073 (`socketIOServerStask`) is started but has no listeners. It is not a task channel [ratta-server]. The device opens only one socket [community:ultrabridge].
- Ratta settings: ping interval 5000 ms, ping timeout 25000 ms [ratta-server]. opennotecloud announces the values swapped (25000/5000) and still works [community:opennotecloud]; use Ratta's values [inferred].
- Ack mode is the library default `AUTO_SUCCESS_ONLY`: after a listener ran without error, the server sends an empty ack `43<id>[]`, but only if the client packet carried an ack id [ratta-server: `Configuration`, `Namespace`, `AckRequest`].
- permessage-deflate is not needed [community:ultrabridge, device-confirmed].

### 4.2 Handshake and session

```
GET /socket.io/?token=<JWT>&type=<equipmentNo>&random=<ms>&sign=<sig>&EIO=3&transport=websocket
```

| Query parameter | Meaning | Source |
|---|---|---|
| `token` | the login JWT (in the query string, not a header) | [ratta-server] [community:ultrabridge] [community:allenporter] |
| `type` | device serial in device traces; `ANDROID<uuid>` in another trace (probably another client app) | [community:ultrabridge] [community:allenporter] |
| `random` | client time in ms, used as nonce | same |
| `sign` | keyed hash over the string `<token>_<type>_<random>` | [ratta-server: `SignVerifierSocketIO`] |
| `EIO`, `transport` | `3`, `websocket` | same |

- The server speaks first: Engine.IO open packet, then the Socket.IO connect packet `40` [ratta-server: `AuthorizeHandler`]. The open packet lists `"upgrades":["websocket"]` on Ratta [ratta-server]. A device-confirmed open packet is `0{"sid":"<id>","upgrades":[],"pingInterval":5000,"pingTimeout":25000}` [community:ultrabridge] [community:opennotecloud]. The device does not send its own `40` for the default namespace; without the server's `40` it never considers itself connected and reconnects with growing back-off (30/60/120/240 s) [community:ultrabridge]. If the client sends `40` anyway, answer `40` [community:ultrabridge].
- Sign check: on failure the server emits `ServerMessage` with a JSON string (`code` `"403"` and a Chinese `msg`), then disconnects [ratta-server]. The key for `sign` is a Ratta secret and is not given here (0.2). `sign` can be accepted and ignored [community:ultrabridge] [community:opennotecloud].
- On connect the token is only decoded: `userId` is read from the JWT claim `userId` without a signature check. If it is missing: the same 403 and disconnect [ratta-server: `JwtTokenUtil`]. The full token check runs later on every `ratta_ping`, `status`, to-do and digest event (4.3).
- On success Ratta writes three Redis keys with a 10 min TTL, one per channel: `<userId>_fileSocket_<type>_<random>`, `<userId>_todoSocket_<type>_<random>` and `<userId>_digestSocket_<type>_<random>`. Each value names the session (`<session>_<userId>_<type>_file`, `_todo`, `_digest`) and is also the key of that session's message queue (a Redis sorted set) [ratta-server].
- Nothing is sent on connect, and the queue is not flushed on connect [ratta-server].
- Online rule: a session is online while its key `<userId>_fileSocket_<type>_<random>` exists. Only `ratta_ping` and `ClientMessage "status"` refresh it (10 min). Engine.IO pings do not [ratta-server].
- Disconnect: all six keys (three channel keys, three queues) are deleted. Queued and unacknowledged messages are lost. If `<userId>_lockCloud` holds this session's token, type and random, the sync lock is released [ratta-server].
- After login the device retries the socket about every 5 s until it connects. Connections last minutes and get re-established [community:ultrabridge].

### 4.3 Events and answers

Token check (used below): the JWT is verified, its `key` claim is looked up in Redis, and the stored value must equal the token. On failure the server disconnects and deletes the session keys [ratta-server].

| Client sends | Server does | Source |
|---|---|---|
| Engine.IO ping `2` | pong `3` (library) | [ratta-server] [community:ultrabridge] [community:opennotecloud] |
| `42["ratta_ping", ...]` (any or no argument) | token check; `42["ratta_ping","Received"]`; refresh the channel keys and queue TTLs; flush the file queue (`ServerMessage`), then the to-do queue, then the digest queue; ack `43<id>[]` if an ack id was given | [ratta-server: `SocketIOEventHandler`, `SocketIOTaskEventHandler`, `SetocketIODigestEventHandler`] |
| `42["ClientMessage","status"]` | token check; `42["ServerMessage","true"]`; refresh the file keys; flush the file queue only | [ratta-server] |
| `42["ClientMessage","Received"]` | removes the sent entries from the file queue; ack `43<id>[]` if an ack id was given (`430[]` for `420[...]`) | [ratta-server] |
| `42["ClientMessage","<other string>"]` | ack `43<id>["Received"]` if an ack id was given | [ratta-server] |
| `to-do` `"Received"` / `digest` `"Received"` | token check; removes the sent entries from that queue; ack `43<id>[]` if an ack id was given | [ratta-server] |

Firmware 3.29.42 sends, every 5 s, both an Engine.IO `2` and `42["ratta_ping"]` (no argument), right after each other; it sends no `ClientMessage "status"` and no `40` [capture:nomad-3.29.42]. It acknowledges a `ServerMessage` push with `420["ClientMessage","Received"]`, an event with ack id 0 (9.3); Ratta answers that with `430[]` [ratta-server].

### 4.4 From change to push

| Channel | Event (server to device) | `msgType` | Source |
|---|---|---|---|
| file | `ServerMessage` | `FILE-SYN` | [ratta-server] [community:ultrabridge] [community:allenporter] |
| to-do | `to-do` | `TASK-SYN` | [ratta-server] [community:ultrabridge] |
| digest | `digest` | `DIGEST-SYN` | [ratta-server] [community:ultrabridge] |

- All three events go over the one socket on 18072 [ratta-server]. Task pushes on the `to-do` event over the main socket work [community:ultrabridge, device-confirmed].
- The event name decides the handler on the device, not `msgType`: the device's task service listens on `to-do` and starts a task sync on any message [community:ultrabridge].
- The payload is one **string** argument holding JSON (`42["ServerMessage","{\"code\":\"200\",...}"]`); the device parses `args[0]` [ratta-server] [community:ultrabridge].

Producer [ratta-server: `SocketIoMessageInfo`, `AsyncConfig`]:

- A REST handler calls an asynchronous method (pool of 20 to 50 threads). It sleeps 1 s, then puts the message into the queues of the recipients. Nothing is sent at the time of the change.
- No Redis pub/sub. The queues are Redis sorted sets, used from one JVM.

Recipients [ratta-server: `SocketIOEventHandler`; to-do and digest the same]:

- The REST header `channel` is parsed as `<token>_<type>_<random>`. It gives the sender's channel and its `type`. A missing or empty header gives none.
- All `<userId>_fileSocket_*` keys of the user are listed. Without a sender channel, every session of the user gets the message, including the session of the device that caused it. With a sender channel, every session whose queue key contains the sender's `type` is skipped (substring match).
- Device file-sync calls carry an empty `channel` header (9.1), so the device that caused a file change is not excluded. Digest and to-do calls carry a `channel` header, so their sender is excluded.
- One queue entry per recipient, with status "not sent". Queue TTL: 10 min for file, 30 min for to-do and digest.

Delivery, only inside the `ratta_ping` and `status` handlers [ratta-server: `pushMessageToChannel`]:

1. If an entry with status "sent" was sent less than 5 s ago, nothing is sent now. The message is deferred, not dropped.
2. Otherwise all entries are merged into one item list and stored back as one entry with status "sent" and the time of now.
3. If Redis holds no valid token for the user, nothing is sent.
4. The server emits `ServerMessage` with one string argument (the envelope JSON of 4.5).
5. `ClientMessage "Received"` removes the sent entries. Without it, the merged items are sent again at the next heartbeat that comes at least 5 s later, merged with any new items.

- Latency: about 1 s plus up to one `ratta_ping` interval.
- To-do and digest have no 5 s gate: they are sent again on every `ratta_ping` until `"Received"`.
- No gating by firmware, version, device type or serial pattern in the socket code.

Notedown [Notedown's current behaviour]: with `sync = "accept"` the server pushes to the bound and connected devices of the account after a file or folder change by `put`, an import or another device: at most once per 5 s per device and never to the device that caused the change. With `detail` every pending change is sent, without a cap, as Ratta does. The pending changes are collapsed per file or folder, so memory stays bounded by the entries of the account plus those deleted since the last push: a file created and then changed is one `DOWNLOADFILE` item with the latest content, a file created and deleted again gives no item, two moves give one move from the first place to the last and a file the device knows that moved and changed gives the move and then `DOWNLOADFILE`. Each live entry is named at its path in the store when the push is built, and a path the device knows is moved along with the moves of its parent folders in the same push. The items go out in an order the device can follow: new and moved folders first (parents before children), then copies, then the other file items, then the deletes in journal order. Each folder item names the folder's place on the device at the moment the device runs it: a path is moved along only with the folder moves sent before it. A copy of a file that is new or changed in the same push goes out as `DOWNLOADFILE` with the source's content instead of `COPYFILE`. A sync start that saw only some changes of an entry leaves its newest change, with the path before it as the path the device knows. With two or more changes of the entry after that sync start, the older ones count as seen: that gap is left. All items go out in one `ServerMessage`, as with Ratta. If the id registry can't be read, the push stays pending and is tried again after about one heartbeat (5 s). The log gets one line when this starts and one when it ends. A push is handed over only while no sync lock is held; for `startsync` and `detail` this is checked when the push is queued, not when the session sends it (Ratta checks no lock at delivery either). A session that sent no frame for more than 15 s (three heartbeats) counts as not connected: its push waits for the next session. The `push` key of `server.toml` switches the form: `detail` (default), `startsync` or `off`. The older value `reconnect` (close the device's WebSocket so it connects again and runs a full sync, 9.3) is gone. A file that still has it loads as `detail`, and `notedown serve` prints a warning. `startsync` and `detail` deliver as Ratta does: the push goes into the queue of the device's session, and the session sends it right after its reply to the next `ratta_ping` or `ClientMessage "status"` (before the ack), all queued items merged into one `ServerMessage`. Without `ClientMessage "Received"` the sent items go out again at the first heartbeat at least 5 s after the last send; `Received` ends that. While the sent items wait for `Received`, Notedown queues no new push for the session: the changes keep collapsing in the planner and go out after the ack. So the queue doesn't grow while the device doesn't answer. A push queued in a session that sends no heartbeat (a half-open socket) is never sent. The queue drops no item and ends with its session; a new session of the device starts with an empty queue. The message has Ratta's form (4.5): envelope `code`, `timestamp` (send time), `msgType` and `data`, no `msg`, no null field. `equipmentNo` is `"<serial>-null"` of the device that caused the change, or `"null-null"` for a change no device made (`put`, an import). `detail` sends one item per changed file or folder, with the item `timestamp` of the time Notedown builds it: `DOWNLOADFILE` for a new or changed file (with `originalName` = `newName` and `originalPath` = `newPath`), `MODIFYFILE` or `MODIFYFOLDER` for a rename within its folder, `MOVEFILE` or `MOVEFOLDER` with `goDirectoryId` for a move, `COPYFILE` with `newId`, `DELETEFILE`, `ADDFOLDER` (`size` 0) and `DELETEFOLDER`; a field Notedown cannot fill is left out. `startsync` sends one `STARTSYNC` item; several pending ones merge into one. Notedown acks events as 4.3 describes (`430[]` for `420["ClientMessage","Received"]`); an event no listener handles gets no ack. Firmware 3.29.42 acknowledged the earlier message forms (sent at once, with `"msg":""` and null fields, and not acked by Notedown) but synced on neither. A `detail` push in Ratta's form made the Nomad download a new file without a reconnect (9.3) [capture:nomad-3.29.42].

### 4.5 Payloads

All payloads are serialized with plain Gson: fields that are null are left out, and there is no `msg` key [ratta-server]. The top-level `timestamp` is the send time; an item `timestamp` is the time the item was built [ratta-server].

File channel (`SocketMessageData`, items `SocketIoMessageTemplate`) [ratta-server]. Example, a web upload:

```json
{"code": "200", "timestamp": 1791270409912, "msgType": "FILE-SYN",
 "data": [{"messageType": "DOWNLOADFILE", "equipmentNo": "null-null", "fileType": "FILE",
           "id": 4711, "originalName": "Bike.note", "newName": "Bike.note",
           "md5": "0cc175b9c0f1b6a831c399e269772661", "size": 1135077, "directoryId": 102,
           "originalPath": "NOTE/Note/", "newPath": "NOTE/Note/", "timestamp": 1791270408879}]}
```

- Envelope: `code` `"200"` (string), `timestamp` (number), `msgType` `"FILE-SYN"`, `data[]`.
- Item fields: `messageType`, `equipmentNo`, `fileType` (`FILE` or `FOLDE`, sic), `id`, `newId`, `originalName`, `newName`, `md5`, `size`, `directoryId`, `goDirectoryId`, `originalPath`, `newPath`, `timestamp`. `id`, `newId`, `size`, `directoryId`, `goDirectoryId` and `timestamp` are numbers. Each message type fills only some of them.
- `equipmentNo` is always `"<x>-<y>"`: the `equipmentNo` argument of the call, a hyphen, and the `type` from the `channel` header. A web action gives `"null-null"`; a device call with an empty `channel` header gives `"<serial>-null"` [ratta-server] [community:ultrabridge saw "null-null" for web actions].

| `messageType` | Trigger | Fields | Source |
|---|---|---|---|
| `DOWNLOADFILE` | upload finish from the web or the device, terminal upload, restore from the recycle bin, a converted image finished | `fileType` `FILE`, `id`, `md5`, `originalName` = `newName`, `originalPath` = `newPath`, `directoryId`, `size`; no `goDirectoryId`, no `newId` | [ratta-server: `FileLocalWebServiceImpl`, `FileServiceImpl`, `FileUploadServiceImpl`] |
| `ADDFOLDER` | folder created on the web or the device, also the missing parents of a device upload | `fileType` `FOLDE`, `size` 0 | [ratta-server] |
| `MODIFYFILE` / `MODIFYFOLDER` | rename | | [ratta-server] [community:ultrabridge web rename] |
| `MOVEFILE` / `MOVEFOLDER` | move; a file move carries `goDirectoryId`. A device move mixes `ADDFOLDER`, `MODIFY*` and `MOVE*` items | | [ratta-server] |
| `COPYFILE` / `COPYFOLDER` | copy | with `newId` and `goDirectoryId` | [ratta-server] [community:ultrabridge] |
| `DELETEFILE` / `DELETEFOLDER` | delete | no `newName` | [ratta-server] |
| `STARTSYNC` | only on a device's `synchronous/end` and at the end of a history sync | only `messageType`, `equipmentNo`, `timestamp` | [ratta-server: `FileLocalServiceImpl`, `FinishSynchronousServiceImpl`] |
| `WAITING` | never sent (the code path returns at once) | | [ratta-server] |

- On a restore, the item timestamps are now plus a running index [ratta-server].
- `STARTSYNC` means "a device just finished a sync: the lock is free, the cloud may have changed". It is never sent for a web change; web and device file operations send per-item notices [ratta-server]. Because device file-sync calls carry an empty `channel` header, every device, including the one that just synced, gets `STARTSYNC` with `"<own serial>-null"`, and the uploader gets its own `DOWNLOADFILE` and `ADDFOLDER` items [ratta-server]. The firmware therefore cannot obey `STARTSYNC` without a condition: it filters on its own serial, or it treats `STARTSYNC` as "lock free, retry", for example after `E0078` [inferred].
- Community projects send `STARTSYNC` to make a device sync [community:ultrabridge] [community:opennotecloud]. Ratta never sends `STARTSYNC` with `"null-null"` [ratta-server]. Firmware 3.29.42 did not sync on such a push (9.3) [capture:nomad-3.29.42].

To-do channel (`SocketTaskMessageData`, items `ScheduleMessageTemplate`) [ratta-server]:

- Envelope: `code` `"200"`, `msgType` `"TASK-SYN"`, `timestamp`, `data[]`.
- Item: `messageType` always `"QUERY"`, `equipmentNo` the bare `type` of the channel (no suffix), `taskListId` (string), `taskId` (string, absent when null), `timestamp`.
- Every write of a task, a group or the sort order triggers it (`ScheduleServiceImpl`).
- `taskListId` must be numeric-looking (the device parses it as a long) [community:ultrabridge].

Digest channel (`SocketDigestMessageData`, items `DigestMessageTemplate`) [ratta-server]:

- Envelope: `code` `"200"`, `msgType` `"DIGEST-SYN"`, `timestamp`, `data[]`.
- Item: `messageType` `ADD_DIGEST`, `UPDATE_DIGEST` or `DELETE_DIGEST`; `dataType` `"1"` group, `"2"` digest, `"3"` tag (`SummaryServiceImpl`); `equipmentNo` the bare `type`; `id`; `timestamp`. Add and update also carry the digest record fields of section 5; delete carries only these fields.
- A delete with only `messageType`, `dataType`, `equipmentNo`, `timestamp` and `id` was honored by a Nomad; the device keys on `id` and did not care about `equipmentNo` or the `dataType` value [community:ultrabridge, device-confirmed]. ultrabridge saw no acknowledgement for a digest push [community:ultrabridge]; Ratta's server handles a `digest` `"Received"` (4.3) [ratta-server].

### 4.6 Direction summary

- Device to server: only heartbeats and acknowledgements on the socket. All changes go over REST [ratta-server] [community:ultrabridge] [community:allenporter].
- Server to device: hints only. If a hint is lost nothing breaks, the device's next REST sync reconciles [community:allenporter]. Ratta itself loses queued hints on disconnect (4.2) [ratta-server]. Exception: a server-side digest delete is only learned through a `DELETE_DIGEST` push, so it must be queued durably per device until sent (5.7) [community:ultrabridge].

### 4.7 Recommended push [proposal]

- Result: a push in Ratta's exact form works on 3.29.42 (device test 2026-10-08, 9.3). Notedown sends it by default (`push = "detail"`).
- Ack every client event that has an ack id: `43<id>[]` for `ClientMessage "Received"`, to-do `"Received"`, digest `"Received"` and `ratta_ping`; `43<id>["Received"]` for any other `ClientMessage` string.
- On `ratta_ping` reply `42["ratta_ping","Received"]`, then send the session's pending `ServerMessage`, `to-do` and `digest` pushes. On `status` reply `42["ServerMessage","true"]`, then the pending file pushes.
- A session is online while `ratta_ping` or `status` arrive. Keep a queue per session and merge pending file items into one push. Send an unacknowledged push again, merged with new items, at the next heartbeat at least 5 s after the last send. Drop the queue when the session ends.
- File push: wait about 1 s, queue, deliver after the next `ratta_ping` reply. Frame `42["ServerMessage","<JSON string>"]`. Envelope `code` `"200"`, `timestamp`, `msgType` `"FILE-SYN"`, `data[]`, no `msg`. A new or changed file: `messageType` `"DOWNLOADFILE"`, `equipmentNo` `"null-null"`, `fileType` `"FILE"`, `id`, `md5`, `originalName` = `newName`, `originalPath` = `newPath` (device-form folder path, for example `"NOTE/Note/"`), `directoryId`, `size`, `timestamp`. No `goDirectoryId`, no `newId`, no nulls. The matching item types of 4.5 for folders, renames, moves, copies and deletes.
- No `STARTSYNC` for server-side changes. If ever: only after another device's `synchronous/end`, with `equipmentNo` `"<that serial>-null"` and the three fields of 4.5.
- Result: the exact push plus the ack fix made the Nomad download a new file, so no reconnect is needed. Notedown no longer has the reconnect mode.

## 5. Digests ("summary")

A digest is an excerpt the user makes on the device: lasso on handwriting in a note ("recognize as text") or a text selection in a PDF/EPUB, optionally with a typed comment and a handwritten comment, sorted into groups ("categories") [doc via our note-format.md 7.4] [community:ultrabridge]. The server stores them as records (`t_summary`); the handwritten comment is a separate `.mark` file [ratta-server] [community:ultrabridge].

### 5.1 Record

`SummaryDO`, the record returned by queries [ratta-server]; all names camelCase on the wire, no snake_case [community:ultrabridge]:

| Field | Type | Meaning | Source |
|---|---|---|---|
| `id` | long | server id (Ratta snowflake) | [ratta-server] |
| `fileId` | long | unused, always null | [ratta-server] [community:ultrabridge] |
| `name` | string | group name (groups only) | [ratta-server] |
| `userId` | long | owner | [ratta-server] |
| `uniqueIdentifier` | string | 32-hex id; set on groups; **empty on items** from the device | [ratta-server] [community:ultrabridge, device-confirmed] |
| `parentUniqueIdentifier` | string | group of an item; empty = no group | [ratta-server] [community:ultrabridge] |
| `content` | string | excerpt text | [ratta-server] [community:ultrabridge] |
| `sourcePath` | string | source file, relative to the device storage root, for example `Note/x.note` or `Document/Books/y.epub` | [community:ultrabridge] |
| `dataSource` | string | [unknown] | [ratta-server] |
| `sourceType` | int | `1` document (PDF/EPUB), `2` note | [community:ultrabridge] |
| `isSummaryGroup` | `"Y"`/`"N"` | group or item | [ratta-server] [community:ultrabridge] |
| `description` | string | group description | [ratta-server] |
| `tags` | string | comma-separated tag names | [ratta-server] [community:allenporter] |
| `md5Hash` | string | 32-hex hash of the record, computed by the device, changes on every edit; used for diffing | [ratta-server] [community:ultrabridge] |
| `metadata` | string | JSON object as a string, see 5.2 | [ratta-server] [community:ultrabridge] |
| `commentStr` | string | typed comment | same |
| `commentHandwriteName` | string | file name of the handwritten comment on the device, `<32hex>.mark` | same |
| `handwriteInnerName` | string | server name of the `.mark` blob (same value in traffic) | same |
| `handwriteMD5` | string | MD5 of the `.mark` bytes | same |
| `creationTime`, `lastModifiedTime` | long ms | device times | same |
| `isDeleted` | `"Y"`/`"N"` | soft delete | [ratta-server] |
| `createTime`, `updateTime` | ms numbers on the wire [community:ultrabridge, device-confirmed]; in Ratta they are dates, written as ISO-8601 strings on Ratta's wire (no custom date format; probably Jackson's default shape `yyyy-MM-ddTHH:mm:ss.SSS+0000`, UTC) [ratta-server] (analyzed) | server times | [community:ultrabridge, device-confirmed] [ratta-server] |
| `author` | string | author (the device puts it in `metadata.author`) | [ratta-server] [community:ultrabridge] |

### 5.2 Metadata and the link to the source

Note digest (`sourceType` 2) [community:ultrabridge]:

```json
{"author": "Jordi", "note_page": "3",
 "note_fileId": "F20261006090648...", "note_pageId": "P20261006090648...",
 "unique_identifier": "56672a0bcc5e4955552e2e45b18c0dfc"}
```

- `note_fileId` is the `FILE_ID` header value of the `.note`, `note_pageId` the page's `PAGEID`, `note_page` the 1-based page number as a string [community:ultrabridge] [our note-format.md 7.4].
- The `.note` side: the text box that replaced the handwriting carries `{"digest_source":"1","digest_id":"<32 hex>","digest_time":"<ms>"}` [our note-format.md 7.4, verified]. That `digest_id` has the form of `metadata.unique_identifier`; that they are equal is [inferred], not yet verified.
- `FILE_ID` and `PAGEID` survive rename and move; `sourcePath` does not [community:ultrabridge task links analogy] [inferred].

Document digest (`sourceType` 1) [community:ultrabridge]:

```json
{"document_location_data": "[{\"chapter\":2,\"endPosition\":995,\"page\":0,\"startPosition\":940}]",
 "source_size": 3344196, "unique_identifier": "f6444dce52bc2460c932d42aba0f7133", "author": "Jordi"}
```

`document_location_data` is itself a JSON string; EPUB addresses by `chapter` (page 0), PDF by 0-based `page` (chapter 0); positions are character offsets; `source_size` is the document size in bytes. The link to the document is `sourcePath` plus `source_size` [community:ultrabridge].

- The item's stable identity is `metadata.unique_identifier`, not the top-level `uniqueIdentifier` [community:ultrabridge, device-confirmed].
- `metadataMap` in the hash query must keep numbers as numbers (`source_size: 18992668`, not `1.8992668e+07`) [community:ultrabridge, device-confirmed].

### 5.3 Endpoints

All under `/api/file`, token required [ratta-server]. Note that two deletes are HTTP `DELETE` with a JSON body.

| Method and path | Request fields | Response (envelope plus) | Source |
|---|---|---|---|
| `POST /add/summary` | `uniqueIdentifier`, `fileId`, `parentUniqueIdentifier`, `content`, `dataSource`, `sourcePath`, `sourceType`, `tags`, `md5Hash`, `metadata`, `commentStr`, `commentHandwriteName`, `handwriteInnerName`, `handwriteMD5`, `creationTime`, `lastModifiedTime`, `author` | `id` | [ratta-server] [community:ultrabridge] |
| `PUT /update/summary` | `id` (required) and the same mutable fields, no `creationTime`/`fileId`/`uniqueIdentifier`; the device sends the full record each time | envelope | same |
| `DELETE /delete/summary` | `{"id": N}` | envelope (soft delete) | same |
| `POST /query/summary` | `page`, `size`, `parentUniqueIdentifier`, `ids` | `totalRecords`, `totalPages`, `currentPage`, `pageSize`, `summaryDOList` | same |
| `POST /query/summary/hash` | same as above | paging fields + `summaryInfoVOList` of `{id, userId, md5Hash, handwriteMd5, commentHandwriteName, lastModifiedTime, metadataMap}` (note lowercase `Md5` here); Ratta's `metadataMap` holds only `author` [ratta-server] (analyzed) | [ratta-server] [community:ultrabridge] |
| `POST /query/summary/id` | `{"ids": [..]}` | `summaryDOList` | same |
| `POST /add/summary/group` | `uniqueIdentifier`, `name`, `description`, `md5Hash` (required), `creationTime`, `lastModifiedTime` | `id` | [ratta-server] |
| `PUT /update/summary/group` | `id`, `uniqueIdentifier`, `name`, `description`, `metadata`, `commentStr`, `commentHandwriteName`, `handwriteInnerName`, `md5Hash`, `lastModifiedTime` | envelope | [ratta-server] |
| `DELETE /delete/summary/group` | `{"id": N}` | envelope; soft-deletes the group and its items [community:ultrabridge]. Ratta probably soft-deletes only the group row: its digests stay live and point to a deleted group (the cascade mapper is unused) [ratta-server] | [ratta-server] [community:ultrabridge] |
| `POST /query/summary/group` | `page`, `size` | paging fields + `summaryDOList` (groups) | [ratta-server] |
| `POST /add/summary/tag` | `name` | `id` | [ratta-server] |
| `PUT /update/summary/tag` | `id`, `name` | envelope | [ratta-server] |
| `DELETE /delete/summary/tag` | `id` | envelope | [ratta-server] |
| `GET /query/summary/tag` | none | `summaryTagDOList` of `{id, name, userId, uniqueIdentifier, createdAt}`; `uniqueIdentifier` is always null (no column) | [ratta-server] |
| `POST /upload/apply/summary` | `fileName`, `equipmentNo` | `fullUploadUrl`, `partUploadUrl`, `innerName` | [ratta-server] [community:ultrabridge] |
| `POST /download/summary` | `id` (digest id) | `url` | [ratta-server] [community:ultrabridge] |

- `page` is 1-based; queries return only `isDeleted = 'N'` rows and order by creation time descending [ratta-server mapper]. The device's paging sizes are [unknown] (servers in the community use 20).
- Errors: `E0335` tag name exists, `E0336` tag missing, `E0338` unique identifier exists, `E0339` group missing, `E0340` digest missing, `E0109` [ratta-server]. ultrabridge treats re-adds with a known identity as updates (idempotent) to survive full re-syncs [community:ultrabridge].
- Ratta details [ratta-server]:
  - Digests and groups get snowflake ids, tags auto-increment ids. Only rows with `isDeleted = 'N'` are returned.
  - `page` 0 or `size` 0 returns all live rows in database order with `totalPages` 1, `currentPage` 1, and `pageSize` and `totalRecords` both the row count; with paging the order is `creationTime` descending (analyzed).
  - `createTime`, `updateTime` and `createdAt` are ISO-8601 strings, not ms (5.1) (analyzed).
  - `update/summary` for an unknown or deleted id gives `E0340`, `update/summary/group` `E0339`; neither creates nor revives a record. Both work on any row: the update of a digest id through the group route, or the reverse, is not refused. `delete/summary` and `delete/summary/group` for a missing id answer success and send no push (analyzed).
  - An update changes only the fields that are not null, and never `fileId`, `uniqueIdentifier`, `isSummaryGroup`, `creationTime` or `isDeleted`. A delete is a soft delete (`isDeleted` `"Y"`) (analyzed).
  - `add/summary/group` with the `uniqueIdentifier` of a live group gives `E0338`; `add/summary` checks no uniqueness and always inserts. `creationTime` and `lastModifiedTime` become now (ms) when they are null or 0 (analyzed).
  - Tags: adding an existing name gives `E0335`, updating an unknown id `E0336`. The update compares ids by object identity, so for tag ids of 128 and more an update to a name that exists, also its own, gives `E0337` (a Ratta bug). An update rewrites `createdAt` and `userId`. A tag delete is not limited to the user and always answers success (analyzed).
  - Pushes: `ADD_DIGEST`, `UPDATE_DIGEST` and `DELETE_DIGEST` with `dataType` `"3"` tag, `"1"` group, `"2"` digest (4.5) (analyzed).
  - Required fields (missing -> `"400"`): group add `uniqueIdentifier`, `name`, `md5Hash`; digest update `id`; group update `id`, `md5Hash`; `upload/apply/summary` `fileName`; `download/summary` `id`.
  - `download/summary` for an unknown id gives `E0340` (analyzed); for a digest without handwriting it probably does not answer `E0321` (compare step 5 of 5.4).
  - Tag names compare without case. Field limits: `sourcePath` 1000, `commentStr` 2000, `tags` 255, handwriting names 100, `author` 200 characters.
  - The `.mark` blobs are stored under `<data>//digest/`; `upload/apply/summary` answers `innerName` = the request's `fileName` (5.5) (analyzed).
- Whether the device uses tags at all is [unknown]; no trace shows tag calls.

### 5.4 Sync flow

Observed on a Nomad [community:ultrabridge 0b and Phase D]:

1. `POST /query/summary/group` (all groups), `POST /query/summary/hash` (id + md5 of every item), then `POST /query/summary/id` for items whose hash differs or that are new on the server.
2. New item on the device: `POST /add/summary` (empty handwriting fields, top-level `uniqueIdentifier` empty) -> `id`.
3. Handwritten comment: `POST /upload/apply/summary {"fileName": "<32hex>.mark", "equipmentNo": ...}` -> `fullUploadUrl` + `innerName`; `POST <fullUploadUrl>` multipart `file`; then `PUT /update/summary` with `commentHandwriteName` = `handwriteInnerName` = the `.mark` name and `handwriteMD5`. A 29 KB `.mark` used the single-shot URL; `partUploadUrl:""` was accepted [community:ultrabridge, device-confirmed].
4. Each later edit (typed comment, group change, author) sends a full `PUT /update/summary` [community:ultrabridge].
5. Pulling a handwritten comment: `POST /download/summary {"id": N}` -> `url`, then `GET` [community:ultrabridge, device-confirmed]. No handwriting -> `E0321` [community:ultrabridge].
6. Delete on the device: `DELETE /delete/summary {"id": N}` [community:ultrabridge, device-confirmed].

The device polls `query/summary/hash` every sync, so the round trip works without socket pushes [community:ultrabridge].

- Notedown: paging. `page` is 1-based and `page <= 0` reads as 1; `size <= 0` means all rows, so the device's `{"page":0,"size":0}` group query (9.1) gets every live group. Rows are ordered by `creationTime` descending, ties by `id` descending. `totalPages` is at least 1; `currentPage` and `pageSize` are answered as sent (0 stays 0) [proposal]. Ratta differs for `page` 0 or `size` 0: database order, `currentPage` 1 and `pageSize` = the row count (5.3) [ratta-server] (analyzed).
- Notedown: ids. Before the first digest sync `notedown cloud import-app-data` fills the account from the device's own `.snbak` backup and keeps the cloud id the device holds (`serviceId` in `knowledge.json`) as the record `id`, so `query/summary/hash` answers the ids and `md5Hash` values the device already has. A digest or group without a cloud id (it never reached a cloud) is not imported: the device adds it itself with `add/summary` (or `add/summary/group`). Only these add routes give new ids: the largest id of all digests and groups of the account plus one, never above 2^63 - 1; tags count in their own space. Ids stay JSON integers and never pass through a float. Until the import, every digest and to-do call answers `E0079` (flag `app_data_refused`) when protect is on [proposal].
- Notedown: identity. An `add/summary` whose `metadata.unique_identifier` matches a live digest updates that digest and answers its `id`. An `update/summary` (or `update/summary/group`) for an id the server does not know creates the record under that id (flags `digest_adopted`, `digest_group_adopted`, 5.7) without checking `metadata.unique_identifier`. After an import of an old backup this can leave two records for one digest, and the device may download a duplicate. An `update/summary` for a soft-deleted digest brings it back: a second device that has not seen the delete can revive a digest with an ordinary edit [proposal]. Ratta differs in all three cases: `add/summary` checks no uniqueness and inserts a second record, and `update/summary` for an unknown or deleted id answers `E0340` and neither creates nor revives (5.3) [ratta-server] (analyzed).
- Notedown: handwritten comments. `upload/apply/summary` answers a signed `fullUploadUrl` for `POST /api/oss/upload/summary`, `partUploadUrl: ""` and `innerName` = `fileName`; `download/summary` answers a signed `url` for `GET /api/oss/download/summary`, or `E0321` when the digest names no comment. The target in these signed URLs is distinct from every file path (it contains a backslash, which no wire path can), so a mark URL never reaches a file and a file URL never reaches a mark. A failed mark upload (account or store error) answers HTTP 500 with the plain text `E1305`, a failed mark download `E1307`, as the file transfers do (7.1) [proposal].

### 5.5 The handwritten comment file (`.mark`)

- Name: `<32 hex>.mark`. In the one published sample the name differs from `handwriteMD5`, so the name is not the MD5 of the file content [community:ultrabridge sample]. opennotecloud returns the device's `fileName` as `innerName` [community:opennotecloud]. Ratta does not rename: `innerName` is the request's `fileName` [ratta-server] (analyzed); returning the device's name keeps `commentHandwriteName == handwriteInnerName` as observed [community:ultrabridge].
- Ratta stores them in a per-server `digest` directory (`.../data//digest/` in a decoded upload URL: note the double slash) [ratta-server: constant `/digest/`] [community:ultrabridge].
- Its internal format is the Supernote `.mark` annotation format [unknown details]; transfer is opaque bytes.
- Distinct from document sidecars `<doc>.pdf.mark`, which sync as normal files in the file tree (3.3) [community:ultrabridge].

### 5.6 Which device files hold digests

Digests are records of the device's Digest app, not files in the synced tree [community:ultrabridge] [our note-format.md 7.4]. The device-side storage location is [unknown].

### 5.7 Deletion semantics

- Server-side: soft delete (`isDeleted:"Y"`). Ratta probably never purges soft-deleted rows, although its mapper has a purge by date [ratta-server]. A group delete probably does not cascade to the group's digests (5.3) [ratta-server].
- Device-authoritative: if the server deletes an item on its own, the device sees it missing in `query/summary/hash` and pushes it back with `PUT /update/summary`. To make a server-side delete stick, the server must send `DELETE_DIGEST` on the `digest` event; ultrabridge keeps a durable per-device queue and sends it on the next heartbeat after reconnect, and the Nomad removed the digest [community:ultrabridge, device-confirmed]. Group delete pushes are [unknown].

## 6. Other device-facing features

### 6.1 Schedules and tasks (To-Do app)

Tasks sync as records, not files, over REST polling plus the `to-do` socket nudge [ratta-server] [community:ultrabridge, device-confirmed]. All under `/api/file`, token required [ratta-server].

| Method and path | Request | Response (envelope plus) | Source |
|---|---|---|---|
| `POST /schedule/group/all` | `maxResults` (string, default 20), `pageToken` | `pageToken`, `scheduleTaskGroup[]` of `{taskListId, userId, title, lastModified, isDeleted, createTime}` | [ratta-server] [community:ultrabridge] |
| `POST /schedule/group` | `taskListId`, `title`, `lastModified`, `createTime` | `taskListId` | [ratta-server] |
| `PUT /schedule/group` | `taskListId`, `title`, `lastModified` | envelope | [ratta-server] |
| `DELETE /schedule/group/{taskListId}` | none | envelope | [ratta-server] |
| `POST /schedule/group/clear` | `taskListId`, `lastModified` | envelope | [ratta-server] |
| `GET /schedule/group/{taskListId}` | none | group fields | [ratta-server] |
| `POST /schedule/task/all` | `maxResults`, `nextPageTokens` (plural), `nextSyncToken` (ms) | `nextPageToken` (singular), `nextSyncToken`, `scheduleTask[]` | [ratta-server] [community:ultrabridge] |
| `POST /schedule/task` | task fields | `taskId` | [ratta-server] [community:ultrabridge] |
| `PUT /schedule/task` | task fields | `taskId` | [ratta-server] |
| `PUT /schedule/task/list` | `{taskListId, updateScheduleTaskList: [task, ...]}` | envelope | [ratta-server] [community:ultrabridge] |
| `DELETE /schedule/task/{taskId}` | none | envelope (hard delete, no tombstone) | [ratta-server] [community:ultrabridge] |
| `GET /schedule/task/{taskId}` | none | task fields + `scheduleRecurTask[]` | [ratta-server] |
| `POST /schedule/sort`, `PUT /schedule/sort` | `taskListId`, `title`, `lastModify` (no "d"), `content` | envelope | [ratta-server] [community:ultrabridge] |
| `DELETE /schedule/sort/{taskListId}` | none | envelope | [ratta-server] |
| `POST /query/schedule/sort` | `nextIndexNumber` (null on first call) | `taskListId`, `title`, `lastModify`, `content`, `nextIndexNumber` (null = end) | [ratta-server] |

Task fields: `taskId` (32 hex, made by the device), `taskListId`, `recurrenceId`, `title`, `detail`, `lastModified`, `recurrence` (RFC 5545 rule), `isReminderOn`, `status` (`needsAction`/`completed`), `importance`, `dueTime`, `completedTime`, `links` (base64 of JSON `{appName, fileId, filePath, page, pageId}` pointing to a note page), `isDeleted`, `sort`, `sortCompleted`, `planerSort`, `allSort`, `allSortCompleted`, `sortTime`, `planerSortTime`, `allSortTime` [ratta-server] [community:ultrabridge].

- Incremental sync: with a recent `nextSyncToken` the server returns changes, an old one -> `E0330` and the device does a full pull [ratta-server: `next.sync.tiamout=5`, enum E0330] [community:ultrabridge]. Ratta counts whole days between the token and now and answers `E0330` when they are more than 5, so from 6 full days on; a token in the future passes [ratta-server] (analyzed). Without a token there is no check; a token that is not a number probably gives `"422"` [ratta-server].
- Paging on Ratta: `nextSyncToken` (now, ms) comes only on the last page; every other page carries `nextPageToken` = page + 1. Defaults: page 1, `maxResults` 20 [ratta-server] (analyzed).
- Ratta details [ratta-server]:
  - Types: `maxResults`, `nextPageTokens` and `pageToken` are strings, `nextSyncToken` is a number.
  - Deletes leave **no tombstones**. `DELETE /schedule/task/{taskId}` removes the row and its recurring instances. A group delete removes the group and its tasks (their recurring instances stay). `isDeleted` becomes `"Y"` only when the device sends it in a `PUT`, and `task/all` and `group/all` also return such rows. Deleting a missing task answers the generic `"500"` (the push code reads the missing row), not `E0329`; deleting a missing group answers success (analyzed).
  - A group update checks no existence and answers success. `GET` of an unknown task or group answers `"500"` (analyzed).
  - The incremental pull selects rows whose `lastModified`, `sortTime` or `planerSortTime` is at or after the token, but Ratta's query does not put these three conditions in brackets after the user condition, so it can return tasks of other users (a Ratta bug, do not copy). The count for paging uses `lastModified` only. Rows are ordered by `lastModified` ascending and paged by limit and offset.
  - `taskId` and `taskListId` are primary keys across all users. Adding a task whose `taskId` exists for any user appends `"0"` until the id is free, inserts a second row and answers the new id; so a retried add makes a duplicate. An empty `taskId` or `taskListId` becomes the MD5 hex of `title` plus `lastModified` (`"null"` when `lastModified` is missing, which is then stored as 0) (analyzed).
  - Adding a task with `recurrenceId` set always fails with `"500"`: that branch inserts a row without `task_id`, which the primary key refuses (analyzed).
  - `PUT /schedule/task/list` updates one task after the other; the first missing task answers `E0329`, and the updates before it stay (no rollback). A single task update: missing task `E0329`, missing group `E0328` (analyzed).
  - Adding a task probably needs an existing group (`E0328`).
  - A task update overwrites `title`, `detail`, `status`, `importance`, `dueTime`, `completedTime`, `links` and `taskListId`; `dueTime` must not be null (missing -> `"500"`); `lastModified` 0 is ignored; the sort fields and `isDeleted` change only when they are sent.
  - Required fields (missing -> `"400"`): task add `title`; task update `taskId`; group add `title`; group update `taskListId`, `title`; group clear `taskListId`, `lastModified`.
  - Sort: the query does not read `content` and an update never writes it, so `query/schedule/sort` answers `content: null` (a Ratta bug). `POST /schedule/sort` for a `taskListId` that has a sort gives `E0331`; `query/schedule/sort` returns the sorts ordered by id, one per call, stepping with `nextIndexNumber` (analyzed).
  - The resubmit check on group add and task add never fires (3.7).
- Tasks with `lastModified` 0 or without sort fields are hidden on the device [community:ultrabridge]. Empty page tokens must be omitted, an empty string made the device loop [community:ultrabridge].
- The Nomad called `task/all` 7 times, `group/all` 3 times and `PUT task/list` twice in one session [community:ultrabridge 0b].
- Notedown: ids and records. `taskId` and `taskListId` are kept as the device sends them (a JSON number is stored as its decimal string); the `.snbak` backup holds no to-dos, so an account starts without any. A task delete leaves a tombstone (`isDeleted: "Y"`) for 5 days, so incremental pulls see it; a group delete makes tombstones of its tasks and removes its sort. Deleting a task, group or sort that is not there answers success: an error would fail every to-do sync of the device [proposal]. Ratta has no tombstones, answers `"500"` for a missing task and success for a missing group [ratta-server] (analyzed).
- Notedown: sync tokens. `nextSyncToken` is the server time in ms. A pull with a token answers every task the server wrote at or after that ms (`>=`, so a write in the same ms comes again instead of never), tombstones included. A token older than 5 days, in the future or not a number answers `E0330`; an absent, `null`, `""` or `0` token gives a full pull of the live tasks [proposal]. Ratta differs: `E0330` only from 6 full days on, and a token in the future passes [ratta-server] (analyzed).
- Notedown: paging. Tasks are ordered by `taskId`, groups by `taskListId`, and paged by offset; `maxResults` defaults to 20. A page with more rows after it carries `nextPageToken` (or `pageToken` for groups); the last page omits it. Every page of one pull answers the first page's time as `nextSyncToken` (Ratta sends `nextSyncToken` only on the last page [ratta-server] (analyzed)). A change between the page calls can be skipped or repeated on that pass; the next incremental pull repairs it. `pageToken`, `nextPageTokens`, `maxResults` and `nextIndexNumber` are read leniently (a string, a number or a one-element array); any other shape means the first page or the default of 20, never `422`, since an error would fail every to-do poll [proposal].

### 6.2 Equipment

See 2.4 and 2.6. Minimal answers: `bindEquipment`, `unlink`, `activateEquipment` -> envelope success; `bind/status` -> `bindStatus: true`; `query/by/equipmentno` -> device row [ratta-server] [community:allenporter] [community:ultrabridge] [inferred for activate].

### 6.3 Capacity

See 3.5. Ratta's default quota per user is a value in `f_capacity`; the device's own `totalCapacity` arrives in `bindEquipment` [ratta-server]. `bindEquipment` sets the account's total capacity to `totalCapacity` x 1024 bytes (the device sends KiB) (2.4). Device uploads are not checked against the quota; only the used capacity is counted up [ratta-server]. A device delete and a reconcile delete lower the used capacity, a copy raises it; a web delete does not lower it [ratta-server] (analyzed).

### 6.4 Recycle bin

`/api/file/recycle/list/query` (`order`, `sequence`, `pageNo`, `pageSize` -> `total`, `recycleFileVOList[]` of `{fileId, isFolder, fileName, size, updateTime}`), `/recycle/clear`, `/recycle/delete {idList}`, `/recycle/revert {idList}` [ratta-server] [ratta-web]. Web UI only; never seen from the device [community:ultrabridge]. Device deletes do not feed it: they are hard deletes (3.5) [ratta-server] (analyzed). A revert pushes file messages to devices [ratta-server]. `E0311` already restored or deleted [ratta-server].

Ratta's web delete and restore [ratta-server] (analyzed):

- Web delete: `E0301` while a sync lock is held; `E0305` if an id is no longer in the given directory. Otherwise a soft delete: the row and every row below it become inactive, a recycle row is written, an action row too, and a socket push goes out (4.5). The used capacity stays (6.3). The file moves to `/home/supernote/recycle/recycle_bin/<email>/<name>_<epoch ms>`, outside the data tree, and its storage name is set to that name.
- Restore (`revert`): the same ids become active again, a name collision is solved by autorename, inactive parent folders come back too, the file moves back and a socket push goes out. A device that dropped the id after the delete sees it come back with the same id.
- Permanent delete (`delete`) and `clear`: hard delete of the rows, the files are deleted on disk later (asynchronously).
- The disk reconcile reads only active rows, so recycled files are never touched by it (3.0.1); a missing user root, however, deletes the whole recycle bin of the user.

### 6.5 Sharing

`POST /api/file/share/record/add {fileId, shareWay}` (`0` PDF, `1` PNG) only records a share [ratta-server]. Device-side share and the device conversion DTOs (`TerminalUploadConvert*`, share URL with `ttl`) exist in the jar without routes in this build [ratta-server]. The Nomad never calls the share route (9.6) [capture:nomad-3.29.42]. Its "Share via QR Code" goes to Ratta's own service, not to the Private Cloud: the code holds a `https://viewer.supernote.com/share/?shareId=<id>&fileType=<n>` link that is valid for one day, and the server gets no request for it. Notedown has nothing to record.

### 6.6 File conversion requests

| Path | Request | Response | Source |
|---|---|---|---|
| `POST /api/file/note/to/pdf` | `id`, `host`, `pageNoList` | `url` | [ratta-server] [ratta-web] |
| `POST /api/file/note/to/png` | `id`, `host` | `pngPageVOList[]` of `{pageNo, url}` | [ratta-server] [ratta-web] |
| `POST /api/file/pdfwithmark/to/pdf` | `id` | `url` (PDF with its `.mark` annotations merged) | [ratta-server] [ratta-web] [community:ultrabridge] |

Ratta delegates to `notelib`; errors `E0314` parse failure, `E0315` wrong type, `E0316` already converting [ratta-server]. Not seen during device syncs [community:ultrabridge 0b]. The Nomad's export renders on the device and does not call them either (9.6) [capture:nomad-3.29.42]. A server without a renderer can answer with an error envelope [inferred]; Notedown can render `.note` itself [proposal].

### 6.7 Server info and parameters

- `GET /api/file/query/server` -> envelope; reachability check at boot [ratta-server] [community:ultrabridge].
- `POST /api/official/system/base/param` (no auth) -> `paramList[]` of `{serial, name, value}` (`COPY_MAX`, `DOWNLOAD_MAX_NUMBER`, `EMAIL_CODE_TIME`, `FILE_MAX`, `FILE_TYPE`, `MAX_ERR_COUNTS`, `UPLOAD_MAX`) and `random` [ratta-server] [community:ultrabridge]. Used by the web UI [ratta-web]; device use [unknown].

### 6.8 Web management UI API

The web UI uses `/api/file/list/query` (by `directoryId`, paged, `order` filename/time/size), `/folder/list/query`, `/capacity/query`, `/delete {idList, directoryId}`, `/folder/add {fileName, directoryId}`, `/move` and `/copy {idList, directoryId, goDirectoryId}`, `/rename {id, newName}`, `/list/search`, `/path/query {id}` -> `{path, idPath}`, `/download/url {id, type}` -> `{url, md5}`, the upload trio of 3.8, the recycle bin, conversions, user profile (`/api/user/query`, `/update`, `/update/name`, `PUT /password`, `PUT /email`, `/account/clear`), MFA (`/api/user/mfa/setup|enable|disable|recovery/regenerate|status`), registration restriction (`GET|PUT /api/user/register/restriction`), email server config and log level (`GET|PUT /api/system/log/level`) [ratta-server] [ratta-web]. Web file actions push `FILE-SYN` messages to devices with `equipmentNo` `"null-null"` [ratta-server] [community:ultrabridge].

## 7. Response envelope and errors

### 7.1 Envelope

Every JSON response object carries three fields at the top level, payload fields beside them (never under a `data` key) [ratta-server] [community:ultrabridge]:

```json
{"success": false, "errorCode": "E0322", "errorMsg": "A file with the same name already exists"}
```

- `errorCode` and `errorMsg` are null (or empty) on success [community:ultrabridge traces].
- Business errors come with HTTP 200 on Ratta [community:ultrabridge] [ratta-server]. Null fields are written. Ratta's global error handler [ratta-server]:
  - not logged in or token invalid: HTTP **401**, `errorCode` `"401"`, `errorMsg` `"Unauthorized"` (2.5);
  - a required field missing or invalid: HTTP 200, `"400"`, the messages joined with `;`;
  - JSON that cannot be read, or a value of the wrong type (for example a non-numeric `id`): HTTP 200, `"422"`, "Request Parameter Serialisation Exception";
  - wrong `Content-Type`: HTTP 200, `"403 Forbidden"`;
  - any other exception: HTTP 200, `"500"` with a long English message;
  - a multipart that cannot be parsed: HTTP 500, `FILE_UPLOAD_FAILED`;
  - failed OSS signature checks and failed downloads: HTTP 500 with plain text (3.5).
- `409` (resubmitted) exists but is never sent (3.7) [ratta-server].
- Unknown JSON fields are probably ignored [ratta-server].

### 7.2 Error codes relevant to the device

The column "Sent" says whether Ratta's 26.06.04 build sends the code to the device [ratta-server]; "probably" marks codes the analysis shows only indirectly.

| Code | Message (shortened) | Where | Sent | Source |
|---|---|---|---|---|
| `E0018` | account does not exist / wrong account or password | login | yes, probably; not by `login/equipment`, which answers `"500"` for an unknown account (2.4) | [ratta-server] (analyzed) |
| `E0019` | password error | login | yes, also for an expired random code; `counts` is always `"1"` | [ratta-server] (analyzed) [community:ultrabridge] |
| `E0045` | user locked | login | only if a lock key exists already or after 5 failed MFA tries; the password lockout never triggers (2.4) | [ratta-server] (analyzed) |
| `E0075` | device already bound to this account | bind | never | [ratta-server] |
| `E0077` | logged-in account differs from bound account | bind/login | never | [ratta-server] |
| `E0078` | another device is synchronizing | sync start | yes | [ratta-server] [community:allenporter] |
| `E0079` | synchronization in progress, wait | file ops | never (3.0) | [ratta-server] |
| `E0081` | path does not exist | file ops | never | [ratta-server] |
| `E0082` / `E0310` | identical MD5 exists, no need to upload | upload | `E0082` never; `E0310` web only | [ratta-server] |
| `E0083` / `E0503` | device bound to another account | bind | never | [ratta-server] |
| `E0085` | token invalid | any | never; HTTP 401 instead (7.1) | [ratta-server] |
| `E0101` / `E0102` | verification code expired / wrong | web registration | web only | [ratta-server] [ratta-web] |
| `E0109` | account has neither phone nor email | sync start, digests, to-dos | yes | [ratta-server] (analyzed) |
| `E0301` | sync in progress | web file ops | web only; web delete yes (6.4), other web actions probably | [ratta-server] (analyzed) |
| `E0303` / `E0305` / `E0306` | already deleted / moved / target deleted | file ops | probably not by the device file service; `E0305` by the web delete for an id no longer in the given directory (6.4) | [ratta-server] (analyzed) |
| `E0304` / `E0322` | same name exists | create/move/copy | `E0322` yes | [ratta-server] [community:ultrabridge] |
| `E0307` / `E0309` / `E0333` | capacity exceeded | copy/upload | `E0309` web only; `E0333` never | [ratta-server] |
| `E0308` | file does not exist | copy | not for a device copy (`E0320` instead) | [ratta-server] (analyzed) |
| `E0321` | file does not exist | download | yes | [ratta-server] [community:ultrabridge] |
| `E0312` | device not linked to an account | file ops | never (only in an unused class) | [ratta-server] |
| `E0318` / `E0320` | delete / move or copy source missing | delete / move / copy | yes | [ratta-server] (analyzed) [community:ultrabridge] |
| `E0324` / `E0325` | file cannot be uploaded / name too long | upload finish | web only | [ratta-server] [community:ultrabridge] |
| `E0328` / `E0329` / `E0330` / `E0331` | task group missing / task missing / sync token too old / sort exists | tasks | yes; a delete of a missing task gives `"500"`, not `E0329` (6.1) | [ratta-server] (analyzed) [community:ultrabridge] |
| `E0332` | upgrade to the latest version to sync | sync | never | [ratta-server] |
| `E0334` | path empty | `query/by/path_v3` | yes | [ratta-server] (analyzed) |
| `E0335`..`E0340` | tag/unique id/group/digest errors | digests | yes; deletes of a missing digest or group answer success; `E0337` also wrongly for tag ids of 128 and more (5.3) | [ratta-server] (analyzed) |
| `E0342` | data expired, re-sync | file ops | only as a disk error code | [ratta-server] |
| `E0343`..`E0358` | private storage disk errors, IO errors, chunk errors, folder into itself | file ops | yes | [ratta-server] (analyzed) |
| `E0502` | account does not exist | `bindEquipment` | yes | [ratta-server] |
| `E0712` | not logged in or login expired | auth | never; HTTP 401 instead (7.1) | [ratta-server] [community:ultrabridge] [community:opennotecloud] |
| `E1301`..`E1307` | OSS delete/URL/missing/empty/upload/signature/download failures | `/api/oss/*` | `E1305` as text; `E1306` never (plain text message instead) | [ratta-server] |
| `E9999` | network error | any | never | [ratta-server] |

How the device reacts to each code is [unknown] except: `E0078` and any failure make it end the sync with `flag:"N"` [community:allenporter trace] [inferred]; `E0330` triggers a full task pull [community:ultrabridge].

### 7.3 Device wire quirks (implement exactly)

| Quirk | Source |
|---|---|
| File ids arrive as JSON strings (`"id":"16"`) in `download_v3`, `query_v3`, move, copy, delete | [community:ultrabridge, device-confirmed] |
| `upload/apply`: `path` = full path incl. name, `size` = number. `upload/finish`: `path` = parent dir with trailing slash, `size` = string | [community:ultrabridge, device-confirmed] |
| `move_v3`/`copy_v3` `to_path` = full destination path incl. name | [community:ultrabridge, device-confirmed] |
| Paths with double slashes (`Personal//IMG.jpg`); Ratta does not handle them (3.0) | [community:ultrabridge, device-confirmed] [ratta-server] (analyzed) |
| Names compare without case inside a folder; a replace keeps the id and the first spelling (3.0) | [ratta-server] (analyzed) |
| `query/by/path_v3` for a missing path must be 200 + `entriesVO:null`, never 404 | [community:allenporter trace] [community:ultrabridge] |
| `create_folder_v2` must return a real `metadata.id` | [community:ultrabridge, device-confirmed] |
| Folder delete arrives as one `delete_folder_v3` per contained file; the server prunes emptied folders (Ratta probably does not prune, 3.5; Notedown: prunes emptied user folders after a delete or move, never a native folder) | [community:ultrabridge, device-confirmed] [ratta-server] |
| File DTOs mix snake_case (`content_hash`, `path_display`, `to_path`, `parent_path`, `is_downloadable`, on Ratta's wire `_downloadable`) and camelCase (`equipmentNo`, `lastUpdateTime`, `entriesVO`, `xamzDate`) | [ratta-server] [community:ultrabridge] |
| Digest DTOs are camelCase; `handwriteMD5` in requests and records, `handwriteMd5` in `summaryInfoVOList` | [ratta-server] [community:ultrabridge] |
| Digest item identity = `metadata.unique_identifier`; top-level `uniqueIdentifier` empty | [community:ultrabridge, device-confirmed] |
| `metadataMap` numbers must stay numbers | [community:ultrabridge, device-confirmed] |
| Y/N strings, not booleans: `isSummaryGroup`, `isDeleted`, `isBind`, `isBindEquipment`, `isReminderOn` | [ratta-server] [community:ultrabridge] |
| Task: `nextPageTokens` in, `nextPageToken` out; `lastModify` in sort DTOs; `taskListId` numeric | [ratta-server] [community:ultrabridge] |
| `bindEquipment` sends the six-folder `label` manifest and `totalCapacity "25485312"` | [ratta-server] [community:ultrabridge] |
| Socket: server sends `40` right after the open packet | [community:ultrabridge] [community:opennotecloud] |
| Socket push payload is a JSON string argument | [community:ultrabridge] |
| Never list a file the server cannot serve; never drop a listed file unless it was deleted (the device deletes its local copy) | [community:opennotecloud] [community:ultrabridge] |

## 8. Open-source implementations and implications

### 8.1 Survey

| Project | Kind, language | License | Last commit seen | Storage | Source |
|---|---|---|---|---|---|
| allenporter/supernote | server + client + MCP + AI layer (Gemini), Python 3.13, aiohttp, python-socketio | Apache-2.0 | 2026-10-05 | SQLite (SQLAlchemy, Alembic) for catalog and records; blobs stored content-addressed by MD5 | [community:allenporter] |
| jdkruzr/ultrabridge | multi-device note bridge; "SPC server mode" for Supernote, Go | Apache-2.0 | 2026-09-08 | device tree as real files under a configured root, with hidden `.staging/`, `.recycle/`, `.digests/`; SQLite for id registry, digests, tasks, tombstones | [community:ultrabridge] |
| k4z4n0v4/opennotecloud | tablet-only sync server, Go (~2900 lines, stdlib) | AGPL-3.0 | 2026-03-20 | files on disk `<data>/<email>/Supernote/...`; SQLite for catalog, digests, tasks | [community:opennotecloud] |
| rnbennett/supernote-cloud (sncloud) | client of Ratta's public cloud, Python | Apache-2.0 | 2026-06-08 | none | [community:sncloud] |
| dylanmazurek/supernote-sync | client of Ratta's public cloud (listing), Go | MIT | 2025-01-04 | none | [community:supernote-sync] |

Protocol coverage of the servers:

| Area | allenporter | ultrabridge | opennotecloud |
|---|---|---|---|
| device login, bind, unlink | yes | yes, device-validated | yes (fresh activation untested) |
| web login, registration | yes (registration off by default) | partial (Partner/web out of scope) | no (CLI users) |
| sync lock start/end | yes (in-memory, 5 min) | yes | yes |
| list_folder v2/v3, query, by-path | yes | yes, device-validated | yes |
| create folder, move, copy, delete | yes | yes, device-validated (prunes empty folders) | yes (permanent delete) |
| upload apply/oss/finish, chunked | yes, implicit merge on last part | yes, staging + verify; chunking built, rarely exercised | yes, chunked |
| download with Range | yes | yes | yes |
| capacity | yes (fixed 10 GiB) | yes (du-style) | yes (filesystem) |
| socket.io heartbeat | yes | yes, device-validated | yes |
| socket pushes | FILE-SYN on changes | to-do task push, FILE-SYN, durable DELETE_DIGEST | none |
| digests items/groups/tags/.mark | yes | yes, device-validated both directions | yes |
| schedules/tasks | yes | yes (CalDAV bridge), device-validated | yes |
| recycle bin | web API | soft delete to `.recycle/`, web restore partial | no |
| conversion note to PDF/PNG | yes (own renderer) | partial | no |
| web UI | own UI | own UI | none |
| sharing | stub | no | no |

Known compatibility notes:

- allenporter: a sync failed when `query/by/path_v3` answered 404 for a missing path (`failed-sync.md`) [community:allenporter].
- ultrabridge: its wire gotchas (7.3) were all found on a Nomad; several analyzed field descriptions were wrong (`path`, `to_path`) [community:ultrabridge]. The device firmware version of its tests is not stated.
- opennotecloud: tested by moving a device that was already registered with Ratta's private cloud; a brand-new device may fail on activation endpoints [community:opennotecloud].
- No source reports firmware Chauvet 3.29.42 specifically [unknown].

Most useful for the clean-room spec: ultrabridge's docs (`docs/spc-protocol.md`, `docs/PRIVATE_CLOUD_REFERENCE.md`), because they hold device traces from a Nomad and record each wire surprise; its reference document contains a few statements that its own later traces correct (3.4, 3.5). For a compact picture of the device-facing surface, opennotecloud (AGPL-3.0, same license as Notedown; still clean-room rules apply).

### 8.2 Minimal endpoint set for a Nomad sync

[inferred from 2 to 6 and the device traces]

- Login: `check/exists/server`, `query/random/code`, `login/equipment`, `terminal/user/bindEquipment`, `terminal/equipment/unlink`, `user/query`, `equipment/bind/status`, `file/query/server`.
- Files: `synchronous/start|end`, `2/files/list_folder` (and `3/files/list_folder_v3`), `3/files/query_v3`, `3/files/query/by/path_v3`, `2/files/create_folder_v2`, `3/files/upload/apply`, `oss/upload`, `oss/upload/part`, `2/files/upload/finish`, `3/files/download_v3`, `oss/download`, `3/files/delete_folder_v3`, `3/files/move_v3`, `3/files/copy_v3`, `file/capacity/query`, `2/users/get_space_usage`.
- Digests: `query/summary/group|hash|id` (must never 404), `add|update|delete summary`, `upload/apply/summary`, `download/summary`, group add/update/delete.
- Tasks: `schedule/group/all`, `schedule/task/all`, `schedule/task` (POST/PUT), `PUT schedule/task/list`, `DELETE schedule/task/{id}`.
- Socket: `/socket.io/` with open + `40`, pong, `ratta_ping` and `ClientMessage` answers.

### 8.3 Proposal: a full Private Cloud on plain files

Goal set by Jordi: every device-facing feature of Ratta's server, no email, no database, no Redis, no other service; state mostly in files that a person can read. Everything in this subsection is [proposal].

#### 8.3.1 Principles

1. The user's device tree is stored as real files with their real names and paths. A backup of the data directory is a backup of everything.
2. Every record type is one JSON file per entity (rewritten atomically: write temp, fsync, rename), or an append-only JSONL log when the data is a history.
3. Short-lived state (random login codes, the sync lock, socket sessions, upload progress counters) lives in memory with a TTL. Losing it on restart only forces a re-login or a retried upload.
4. Durable state that the protocol needs and that a file tree cannot express: the id registry (ids survive moves, copies get new ids) and the per-device push queue (digest deletes).
5. Data-loss rule: never list a file that cannot be served, and never stop listing a file unless it was really deleted. The device deletes its local copy in both cases (3.1). Out-of-band changes (a person edits the tree) are detected by a scan (at sync start, before device listings and on a timer; no file watcher), assigned ids, and journaled (3.0.1).
6. A change journal (8.3.5) records every change for consumers such as the Crystalline ingestion pipeline.

#### 8.3.2 On-disk layout

```
<data>/
  server.toml                       listen address, external base URL, socket on/off, limits
  keys/                             url-signing key, token key (mode 0600, generated on first start)
  accounts/<account>/               <account> = email address (as Ratta), or a slug
    account.json                    userId, email, userName, passwordMd5, admin flag, quotaBytes,
                                    createdAt, locked, failedLogins
    devices/<equipmentNo>.json      name, totalCapacity, label manifest, boundAt, lastLogin,
                                    tokenGeneration, status
    files/                          the device tree, real files
      NOTE/Note/...  NOTE/MyStyle/...  DOCUMENT/Document/...  EXPORT/  SCREENSHOT/  INBOX/
    ids.jsonl                       id registry log: {"id":4711,"kind":"file","path":"NOTE/Note/x.note"}
                                    plus move/delete entries; compacted into ids.json on start
    hashes.json                     cache path -> {size, mtimeNs, md5} to avoid rehashing
    digests/
      groups/<uniqueIdentifier>.json
      tags.json                     [{id, name, uniqueIdentifier, createdAt}]
      notes/<note FILE_ID>/         one folder per source note (stable across renames)
        source.json                 last sourcePath, FILE_ID, page ids seen
        <unique_identifier>.json    the digest record (wire fields + server id + isDeleted)
        <unique_identifier>.mark    handwritten comment, if any (original name kept in the JSON)
      documents/<docKey>/           docKey = sanitized sourcePath (+ source_size), see note below
        source.json
        <unique_identifier>.json
        <unique_identifier>.mark
      unassigned/                   digests whose source cannot be resolved
    tasks/
      groups/<taskListId>.json
      items/<taskId>.json
      sort/<taskListId>.json
    recycle/<deletedAtMs>/<original relative path>   deleted files, restorable
    recycle/index.jsonl             {id, path, deletedAt, by}
    outbox/<equipmentNo>.jsonl      pending socket pushes (DELETE_DIGEST and others), cleared when sent
    logins.jsonl                    login history (optional)
    staging/<uploadId>/             chunks and single uploads before finish (hidden from listings)
    convert/                        cached PDF/PNG renderings (deletable)
  journal/
    journal-<firstSeq>.jsonl        change journal, all accounts
    consumers/<name>.cursor         {"seq": 1234, "updatedAt": ...}
```

Notes:
- Document digests have no stable file id in the protocol; the key is the `sourcePath` (plus `source_size`); a rename of the PDF makes a new key, so `source.json` records the old path and the server can re-key on `move_v3` of the document.
- The digest's server `id` (a long the device uses in `update/summary`, `delete/summary`, `download/summary` and `query/summary/hash`) is stored inside each digest JSON; an in-memory index id -> file is built at start.
- `.mark` blobs are stored under the digest's `unique_identifier` so the pair JSON + `.mark` sits together per source; the device-facing name (`commentHandwriteName`/`handwriteInnerName`) stays in the JSON and is returned unchanged.
- Notedown: built differently from this proposal. Digests are flat, one file per server id (`digests/items/<id>.json`, `digests/groups/<id>.json`, `digests/tags.json`), and the `.mark` files sit in `digests/marks/` under the device's name; the source link stays in the records and the journal (design spec, "Known limits"). The `outbox/` is not used yet: the server never deletes a digest on its own.

#### 8.3.3 Feature table

| Feature | Endpoints | Persistent state and file mapping | Ephemeral state |
|---|---|---|---|
| Accounts (no email) | `check/exists/server`, `random/code`, `login/equipment`, `login/new`, `user/query`, `user/query/token`, `logout`, `PUT user/password`, `user/update`, `update/name` | `account.json` with `passwordMd5`; created by an admin CLI (`notedown-server user add <email>`) or a `[[users]]` list in `server.toml`; password reset by the admin CLI; registration, `mail/validcode`, `check/validcode`, `retrieve/password` answer with an error envelope (or registration only for the first user) | random codes (5 min, one-time), failed-login counters |
| Tokens | all authenticated calls, socket `token` | `devices/<n>.json tokenGeneration`; token = JWT with userId, equipmentNo, generation; logout/unlink/new login increments the generation; no `exp` for device tokens | none |
| Device binding | `bindEquipment`, `unlink`, `activateEquipment`, `bind/status`, `query/by/equipmentno` | `devices/<equipmentNo>.json`; `bindEquipment` creates the `label` folders under `files/` | none |
| File tree | 3.5 endpoints | real files under `files/`; ids in `ids.jsonl`; MD5 cache in `hashes.json`; folders are real directories | sync lock per account (TTL, extended by each call), upload progress |
| Uploads | `upload/apply`, `oss/upload`, `oss/upload/part`, `upload/finish` | `staging/<uploadId>/`, then atomic rename into `files/`; journal event after rename | signed URL nonces |
| Downloads | `download_v3`, `oss/download` | read from `files/` | signed URL nonces |
| Delete, move, copy | `delete_folder_v3`, `move_v3`, `copy_v3`, web `file/delete`, `move`, `copy`, `rename`, `folder/add` | delete = move into `recycle/<ms>/...` + `recycle/index.jsonl`; move keeps the id; copy mints an id; empty user folders are pruned | none |
| Recycle bin | `recycle/list/query`, `revert`, `delete`, `clear` | `recycle/` + `recycle/index.jsonl`; revert moves back and journals a create | none |
| Capacity | `get_space_usage`, `capacity/query` | `account.json quotaBytes`; usage computed from `files/` (+ `recycle/`) and cached | usage cache |
| Auto sync | `/socket.io/` | `outbox/<equipmentNo>.jsonl` for pushes that must not be lost | connection registry |
| Digests | 5.3 endpoints | `digests/` tree, one JSON per record, `.mark` beside it, `groups/`, `tags.json`; soft delete = `isDeleted:"Y"` kept until purged | id index (rebuilt at start) |
| Tasks | 6.1 endpoints | `tasks/groups`, `tasks/items`, `tasks/sort`; each JSON = the wire record; `nextSyncToken` = max `lastModified` served | none |
| Sharing | `share/record/add` | not needed: the Nomad never calls it (9.6); the route stays refused | none |
| Conversion | `note/to/pdf`, `note/to/png`, `pdfwithmark/to/pdf` | Notedown's own `.note` renderer; outputs cached in `convert/` and served by signed `oss/download` URLs | job state |
| System parameters | `official/system/base/param`, `file/query/server` | constants from `server.toml` | none |
| Web UI API | 6.8 endpoints | same state as above; web MFA (TOTP) secrets, if offered, in `account.json` | web sessions |
| Login history | none required | `logins.jsonl` | none |
| Email server config, SMS, WeChat, server migration, avatars | `save/email/config` and others | not offered; answer with an error envelope | none |

#### 8.3.4 Integrity rules

- A file is listed only after it is completely written and renamed into place [proposal].
- The server hashes files lazily and caches by size and mtime; a changed file found by the scan gets a `file.modified` journal entry (a cached MD5 is reused only when size, mtime and ctime all match) and appears with its new hash at the next device listing [proposal].
- One account, one writer: the sync lock serializes device syncs; web actions take the same lock briefly [proposal; Ratta itself never sends `E0079` and checks the lock only in `synchronous/start`, 3.0].
- Out-of-band deletes are dangerous (the device deletes its copy); the scan journals them as `file.deleted` with `origin:"scan"`, an empty `recycle` and no copy in `recycle/`, because it never has the bytes. A scan that would leave fewer than `scan_min_files` files in the account is held instead of applied (3.0.1): nothing is journaled, device sync answers `E0079` (`scan_hold`) until the files are back or `notedown cloud scan --data <dir> --account <slug> --accept` runs. The operator documentation says that a delete by hand is a real delete [proposal].

#### 8.3.5 Change journal for ingestion

Append-only JSONL, one event per line, written after the change is durable (data file fsynced and renamed, then the journal line appended and fsynced) [proposal]:

```json
{"seq": 1842, "ts": 1791270408879, "account": "jordi@example.org", "origin": "device",
 "device": "SN078C10000000", "type": "file.modified", "kind": "note",
 "id": 4711, "path": "NOTE/Note/Work/Bike.note",
 "md5": "0cc175b9c0f1b6a831c399e269772661", "prevMd5": "92eb5ffee6ae2fec3ad71c777531578f",
 "size": 1135077}
{"seq": 1843, "ts": 1791270409120, "account": "jordi@example.org", "origin": "device",
 "device": "SN078C10000000", "type": "digest.created",
 "digestId": 9001, "uniqueIdentifier": "56672a0bcc5e4955552e2e45b18c0dfc",
 "sourceType": 2, "sourcePath": "Note/Work/Bike.note",
 "noteFileId": "F20261006090648...", "notePageId": "P20261006090648...", "notePage": 3,
 "record": "accounts/jordi@example.org/digests/notes/F20261006090648.../56672a0bcc5e4955552e2e45b18c0dfc.json",
 "mark": null, "md5Hash": "ca055a657650fc93c1b2927e1400eb56"}
```

- Event types: `file.created`, `file.modified`, `file.moved` (`path`, `oldPath`), `file.copied` (`id`, `sourceId`), `file.deleted`, `file.restored`, `folder.created`, `folder.moved`, `folder.deleted`, `digest.created`, `digest.updated`, `digest.deleted`, `digest.mark.stored`, `digest.group.created|updated|deleted`, `digest.tag.*`, `task.created|updated|deleted`, `device.bound`, `device.unlinked`.
- `origin` is one of `device`, `cli`, `scan`, `repair`, `snapshot`, `retention` or `app` (a write over the REST API by the Mac app). A reader reads an origin it does not know as `unknown` and keeps the line. `scan` is emitted for changes made by hand in `files/` (one `opId` per scan, no `device`; order: `folder.created`, `file.created`, `file.modified`, `file.moved`, `file.deleted`, `folder.deleted`). Events of `journal snapshot` carry `origin: snapshot`: they describe the current state, not a change, and a consumer that reacts to changes (such as the push of 4.4) ignores them.
- `kind` for files: `note`, `document` (pdf, epub, doc...), `mark` (document sidecar), `image`, `other`, derived from the extension.
- `seq` is a server-wide counter, strictly increasing, never reused; it is recovered at start from the last line of the newest journal file.
- Rotation: a new file `journal-<firstSeq>.jsonl` when the current one passes a size (for example 64 MiB) or at a month change; old files are kept until all registered consumers passed them (then they may be compressed or removed by the admin).
- Consumers: each consumer has `journal/consumers/<name>.cursor` with the last fully processed `seq`, written atomically after processing. A consumer reads from `cursor + 1`, processes in order, and may re-process after a crash, so it must be idempotent: it keys notes by `id` + `md5` and digests by `uniqueIdentifier` + `md5Hash` and skips work it already has. When an event points to a file whose current MD5 differs from the event's (a newer change happened), the consumer skips it and waits for the newer event.
- Bootstrap: a consumer with no cursor starts at the oldest kept `seq`; the server can write a `snapshot` set of `file.created`/`digest.created` events on demand (`notedown-server journal snapshot`) so a new consumer sees the full state.
- For Crystalline: the pipeline tails the journal, ingests new or changed notes (using the Notedown parser), documents and digests; it links a note digest to its note by `noteFileId` (= the note's `FILE_ID`) and page id, and a document digest by `sourcePath`; deletes retire the engram or mark it superseded.

#### 8.3.6 Which Ratta tables the device features need

From the mapper files and the endpoints above [ratta-server] [inferred]:

| Table | Used for | Device needs it | File-based replacement [proposal] |
|---|---|---|---|
| `u_user` | accounts, password MD5 | yes | `account.json` |
| `e_user_equipment` | device to user binding | yes | `devices/<n>.json` |
| `e_equipment` | device inventory (joined by equipment queries) | no (Ratta inventory) | none |
| `e_user_equipment_record` | bind history | no | journal `device.*` events |
| `f_user_file` | file catalog (id, parent, name, inner name, md5, size, folder flag) | yes | real files + `ids.jsonl` + `hashes.json` |
| `f_file_action` | per-file change log (A, D, U, M, R...) | indirectly (pushes, history) | journal |
| `f_capacity` | quota and usage | yes | `account.json quotaBytes` + computed usage |
| `f_sync_record` | sync statistics | no | optional journal events |
| `t_summary` | digests and digest groups | yes | `digests/` tree |
| `t_summary_tag` | digest tags | yes (API exists; device use unknown) | `digests/tags.json` |
| `t_schedule_task`, `t_schedule_task_group`, `t_schedule_sort`, `t_schedule_recur_task` | tasks | yes | `tasks/` tree |
| `u_login_record` | login history | no | `logins.jsonl` |
| `f_recycle_file` | recycle bin | web only | `recycle/` |
| `f_share_record`, `f_terminal_share_file` | shares | no (9.6) | none |
| `f_file_convert`, `f_terminal_file_convert` | conversion jobs | web, device use unknown | in-memory jobs + `convert/` cache |
| `f_file_his_sync` | legacy history sync | no (no route) | none |
| `f_file_server_change` | migration between Ratta storage backends | no | none |
| `t_machine_id` | server instance id (`uniqueMachineId` in `check/exists/server`) | maybe | one value in `server.toml` |
| `t_user_mfa` | web MFA | web only | `account.json` if MFA is offered |
| `u_email_config` | SMTP settings | web only | none |
| `u_sensitive_record`, `u_commonly_area`, `u_commonly_equipment`, `u_user_history`, `u_user_sold_out`, `u_data_migration_record` | security audit, login areas, history, closed accounts, migration | no | none |
| `b_*` (`b_user`, `b_role`, `b_resource`, `b_dictionary`, `b_reference`, `b_pwd_his`, `b_schedule_*`) | Ratta's admin backend, dictionaries, parameters (`b_reference` feeds `system/base/param`) | no (parameters only for the web UI) | constants |
| `e_equipment_*` (authorize, log, manual, warranty), `e_language`, `e_task`, `e_task_his`, `e_temp`, `e_temp_now` | device logs, manuals, warranty, language packs, device command queue, staging tables | no route in this build | none |

Two mappers reference tables that the shipped schema does not create (`f_composite_image`, `f_deleted_user_file`, plus `t_language*`), which suggests unused code paths [ratta-server].

## 9. Unknowns and how to resolve them

| # | Unknown | How to resolve |
|---|---|---|
| 1 | Exact first requests and their order on Nomad firmware 3.29.42 (unlink first or check-exists first; the `version` value; whether `query/token` or `activateEquipment` is called) | Logging stub server: answers every request with a plausible success envelope, logs method, path, headers and body; point the Nomad at it and log in |
| 2 | Heartbeat: EIO `2`, `ratta_ping`, or `ClientMessage "status"`; push delivery timing | The stub's socket endpoint logs every frame for 10 minutes of idle connection |
| 3 | `list_folder` vs `list_folder_v3`; what the device expects at the root (the `NOTE` and `DOCUMENT` parent entries) | Capture against Ratta's own test stack (`~/supernote-private-cloud-test`, its nginx JSON log has a `req_body` field) and against the stub |
| 4 | Whether device deletes go to Ratta's recycle bin | Answered in 3.5 (`delete_folder_v3`): no, a device delete is a hard delete [ratta-server] (analyzed) |
| 5 | `.mark` name for digest handwriting (device name echoed or server name), `digest_id` in the `.note` = `metadata.unique_identifier`? | Create a note digest with a handwritten comment on the Nomad, sync to the Ratta stack, compare the `.note` text box JSON with `t_summary` and the `digest` directory |
| 6 | Device use of digest tags, paging sizes, group delete push | Same capture, plus deleting a group in the web UI (Ratta web UI has no digest view: use the database) |
| 7 | Device use of share and conversion endpoints and of `send/password/unlock/code` | Partly answered in 9.6: export and "Share via QR Code" call neither share nor conversion routes. Still open: `send/password/unlock/code` ("forgot lock password" on the device during a capture) |
| 8 | Which HTTP status codes the device accepts for errors (200 + `success:false` vs 4xx) | Stub returns chosen errors; observe retries and `flag` |
| 9 | Moving an already synced device from Ratta's server to Notedown: the device's sync database holds Ratta ids | Test with a copy of the data: the device should see new ids and treat files by path and hash; confirm no deletes happen (opennotecloud reports this works) |
| 10 | Device-side storage of digests and tasks | Not needed for the server; optional via ADB file listing |
| 11 | When the device drops a token (logout, server says `E0712`, `E0085`) and how it re-logs in | Partly answered in 2.5: Ratta never sends `E0712` or `E0085`, it answers HTTP 401 with `errorCode` `"401"` [ratta-server]. Still open: the device's reaction. Stub returns that 401 once and logs the following requests |
| 12 | HTTP vs HTTPS: Ratta's doc shows plain `http://IP:19072`; does 3.29.42 require HTTPS for private cloud? | Try plain HTTP first with the stub |
| 13 | Firmware 3.29.42 differences from the firmware used by the community traces (not stated there) | All of the above captures on Jordi's device |
| 14 | The real body of the device's first `schedule/task/all` request (which `maxResults`, whether `nextSyncToken` and `nextPageTokens` are absent, `null` or empty, and their JSON types) | Answered in 9.4: `{"maxResults":"200"}`, no tokens |

Plan [proposal]: build the logging stub first (it is also the skeleton of the server), capture items 1, 2, 3, 8, 11, 12 in one session, then run the Ratta test stack for items 4 to 7 and save the captures as test fixtures with personal data removed.

### 9.1 Capture results [capture:nomad-3.29.42]

Session of 2026-10-06 (about 15 minutes, one login, one manual sync) against the capture server, which answers login and server calls and refuses every sync call with `E0079`. Plain HTTP on `http://<lan ip>:19072` was used.

Answered unknowns:

| # | Answer |
|---|---|
| 1 | Variant A of 2.4: `GET /api/file/query/server` when the address is entered, then `unlink` -> `check/exists/server` -> `query/random/code` -> `login/equipment` -> `bindEquipment`, then (after the account dialog, see below) `POST /api/user/query` -> socket connect. `query/token` and `activateEquipment` were not called. `version` is `"202407"` in bodies and in a `version` header; it is an API version, not the firmware version. `bindEquipment` carried the documented `label` list, `name` `"Supernote"` and `totalCapacity` `"25485312"`; it was sent with the login token. |
| 2 | Heartbeat every 5 s: EIO `2` plus `42["ratta_ping"]` (see 4.3). The connection stayed up for the whole session (127 heartbeat pairs, no reconnect) with the open packet values of 4.2. |
| 8 | HTTP 200 with `success:false` is accepted. For `E0079` on `synchronous/start` the device shows "Private Cloud Sync Failed" with the text "Synchronization in progress, please wait", identical to the `errorMsg` sent; whether the device shows the server's `errorMsg` or its own text for the code is still open. Digest and to-do refusals show a separate "App Data Sync Failed: Sync errors occurred". |
| 12 | Plain HTTP works on 3.29.42; HTTPS is not required. |

Further observations:

- Before logging in to a new account the device asks how to handle digest and to-do data of the previous account: "Retain locally" or "Delete locally" (the latter removes the cloud-synced digests and to-dos from the device). Keep "Retain locally" when switching servers.
- Right after the socket connects, and again after every manual sync, the device calls `POST /api/file/query/summary/group` (`{"page":0,"size":0}`) and `POST /api/file/schedule/group/all` (`{"maxResults":"200"}`): digests and to-dos are a second sync channel next to files.
- A manual sync sends `POST /api/file/2/files/synchronous/start` with `{"equipmentNo": "..."}`. After `E0079` the device did not retry on its own (8 minutes observed); only `user/query` followed when settings screens were opened.
- Socket handshake query, in this order: `sign`, `random` (ms), `EIO=3`, `transport=websocket`, `type=<equipmentNo>`, `token`.
- The device uses several HTTP clients, told apart by `User-Agent`:
  - login and account calls: `okhttp/4.12.0` with a `version` header;
  - socket: `okhttp/3.12.12`;
  - digest and to-do calls: `okhttp/4.8.0` with `equipmentno`, `version` and a `channel` header (228 characters on digest calls);
  - file sync: `okhttp/5.1.0` with an empty `channel` header and no `version` header.
- `x-access-token` is sent on every call after login; `user/query` also sends `language: en`.
- No request outside the documented endpoints was seen.

Still open after this session: 3 (the file listing calls, not reached because `synchronous/start` was refused), 4 to 7, 9, 11 and 13. They need a server that accepts the sync (milestone 3, with protect mode) or the Ratta test stack.

### 9.2 Switchover results [capture:nomad-3.29.42]

First real syncs on 2026-10-06 against Notedown (protect on, after an import of a fresh Browse & Access backup of the same device):

| # | Answer |
|---|---|
| 3 | The device lists with `POST /api/file/2/files/list_folder`, `recursive: true`, the whole tree in one call. While the user picks the sync folders it sends `path: ""` with a `version` field; during a sync it sends `path: "/"` without `version`. `list_folder_v3` was not called. |
| 9 | Moving from Ratta's cloud works with new server ids. After the import the device compared all 56 files by path and hash, found no difference and ended the sync with `flag: "Y"`: no upload, no download, no delete. The Ratta ids in the device's sync database did no harm. |
| 13 | Partly answered: these syncs showed no difference to the community traces. Deletes, moves, digests and to-dos are not checked yet. |

Further observations:

- After a login to a new private cloud account, no sync folders are selected. A manual sync then sends `synchronous/start`, gets success, and sends `synchronous/end` with `flag: "N"` about 30 ms later, without any listing. The device shows "Private Cloud Sync Failed: No folders selected to sync. Select Now". The user must select the folders after the login.
- The refused digest and to-do calls (`E0079`) do not stop the file sync. The device shows "App Data Sync Failed" and syncs the files.
- Sync run: `synchronous/start` -> `list_folder` -> per changed file `3/files/upload/apply` -> `POST /api/oss/upload` -> `2/files/upload/finish` -> `synchronous/end` `Y`. The device also calls `2/users/get_space_usage` (not `capacity/query`) when the folder selection opens. The digest and to-do calls run in parallel with the file sync.
- `upload/finish` sends back exactly the `innerName` that `upload/apply` answered (the server answered the file name, as Ratta does). `path` is the parent with a trailing slash and `size` a string, as in 7.3.
- A one-sentence edit on one page of a 1.4 MB note uploaded the whole file (no delta), as 3.1 says.
- Browse & Access (port 8089) lists the SD card at the root under the card's own name (`"name": "Extended"`) with `"uri": "/REMOVABLE0/Extended"`, not as `REMOVABLE0`.

Still open: 4 to 7, 10 and 11. They need a sync with deletes and moves on the device, digests and to-dos (milestone 5), and a token loss test.

### 9.3 Auto Sync results [capture:nomad-3.29.42]

Device tests on 2026-10-07 with "Auto Sync" on, against Notedown with socket pushes:

- A `ServerMessage` push with one `STARTSYNC` item (`equipmentNo` `"null-null"`) was acknowledged within 0.2 s with `420["ClientMessage","Received"]` (an event with ack id 0). No `synchronous/start` followed in the next four minutes. Twice the same result.
- A `ServerMessage` push with one `DOWNLOADFILE` item (`id`, `originalName`, `md5`, `size`, `directoryId`, `originalPath`) was acknowledged the same way within 0.4 s. No sync followed either. Neither push form makes firmware 3.29.42 sync.
- A server restart (all sockets closed by the server) makes the device reconnect within seconds and run a full sync. This is how a file added on the server side reached the device in every test.
- A WebSocket close by the server (code 1001, reason `sync`, no socket.io disconnect packet, Notedown's former `push = "reconnect"`, removed after the `detail` test below) makes the device connect again and run a full sync that downloads the new file. In the first test the new connection came 26 s after the close (12:51:49 close, 12:52:15 open packet, 12:52:16 `synchronous/end` `Y`). The client's reconnect delay is not known yet; more tests will show whether 26 s is typical.
- Every new socket connection (after a server restart or a wifi reconnect) is followed within 0.2 s by a full sync run (`synchronous/start`, `list_folder`, transfers, `synchronous/end` `Y`). This is how the device picked up files added on the server side.
- The `DOWNLOADFILE` test push was not in Ratta's form (4.5): the envelope carried `"msg":""`, the item carried null fields and lacked `fileType`, `newName` and `newPath`. Notedown then left the device's `420["ClientMessage","Received"]` without the `430[]` ack that Ratta sends; it now sends that ack (4.4). The protocol log shows no REST call after the push, only pings. The next device test, cheapest first, is done: Notedown does all four with `push = "detail"` (4.4), and the test of 2026-10-08 below passed:
  1. Ack the device's `420["ClientMessage","Received"]` with `430[]`.
  2. Do not send `STARTSYNC` for a server-side change; Ratta never sends `STARTSYNC` with `"null-null"`.
  3. Send a complete `DOWNLOADFILE` item as in 4.7 (with `fileType`, `newName`, `newPath`, no `msg`, no nulls).
  4. Send the push right after the `ratta_ping` reply, as Ratta does.
- An edit of a note makes the device send it on its own within a few minutes, without `synchronous/start`: `POST /api/file/3/files/query_v3` with the file's `id`, then `3/files/upload/apply` for the same path (and then, presumably, the byte upload and `upload/finish`). A server that requires the sync lock for writes refuses this upload; Notedown now takes the free sync lock for such a write (log marker `implicit_lock`) and answers `E0079` only while another device holds the lock.
- Device test on 2026-10-08 with `push = "detail"`: a file added with `put` downloaded on the Nomad without a reconnect. Notedown sends this push by default now and no longer has the reconnect mode.


### 9.4 Digest and to-do results [capture:nomad-3.29.42]

Device test on 2026-10-07 (times UTC), "Auto Sync" on, against Notedown with digest and to-do sync:

- A fresh `.snbak` held 7 digests and 6 `.mark` files. All 7 digests had no `serviceId` (they were made after the switch to the Private Cloud), so `import-app-data` imported none and only opened app-data sync. The backup held no to-dos.
- First full sync after the server restart (18:34:18 to 18:34:21): `query/summary/group` and `query/summary/hash` with `{"page":0,"size":0}`, then for each local digest `add/summary` followed by `upload/apply/summary` and the signed `.mark` upload, then `schedule/group/all` and `schedule/task/all`, then `schedule/group` and `schedule/task` for the device's one to-do group and its 2 tasks. Result: 7 digests, 6 `.mark` files, 1 group, 2 tasks. No delete, no duplicate, every request answered with success. The device showed "App Data Sync Completed".
- The device's `schedule/task/all` body is `{"maxResults":"200"}`, the same on every pull; `schedule/group/all` sends the same. The device never sent `nextSyncToken`, `nextPageTokens` or `pageToken` in this test, so every to-do pull is a full pull (answers unknown 14).
- No tag call and no `query/schedule/sort` call was seen.
- With "Auto Sync" on, digest changes go out on their own within seconds, without `synchronous/start`: a new note digest went out as `add/summary` at 18:36:05; a handwritten comment as `upload/apply/summary`, the `.mark` upload and `update/summary` with `commentHandwriteName`, `handwriteInnerName` and `handwriteMD5` (18:36:36); a new category as `add/summary/group` (18:36:53); moving the digest into it as `update/summary` with `parentUniqueIdentifier` (18:36:57). The stored `.mark` matched `handwriteMD5`. The device's `fileName` is used as the `.mark` name and echoed as `innerName`; the device accepted that (part of unknown 5). The author set on the device travels in `metadata.author`.
- A new to-do (19:06:07) went out as `schedule/group/all` and `schedule/task/all` (pull first), then `POST schedule/task` for the new task and `PUT schedule/task/list` with the other tasks of the group (their `sort` and `sortTime` changed).
- Device paging for digests is `{"page":0,"size":0}` for both group and hash queries (part of unknown 6).

### 9.5 Scan results [capture:nomad-3.29.42]

Device test on 2026-10-08 against Notedown with the scan (3.0.1) and the scan follow-ups. Three changes were made by hand in the data folder while the server ran, then the device synced:

- A PDF added to `INBOX` reached the device.
- A file renamed only in case on the disk showed up on the device under the new spelling. The scan kept its id, so the device did not download it again as a new file.
- A deleted file was deleted on the device too. The sync was not held: far more files were left than `scan_min_files`.

All three passed.

### 9.6 Detail push, render and share results [capture:nomad-3.29.42]

Device test on 2026-10-09 (times UTC), "Auto Sync" on, `push = "detail"`, folder watch on. All changes were made by hand in the data folder while the server ran, with no `cloud scan`:

- Each change was pushed 5 to 8 seconds after it was made, and the Nomad applied it without a manual sync: `ADDFOLDER` for a new folder, `DOWNLOADFILE` for a new note, `MODIFYFILE` for a rename, `MOVEFILE` for a move into the parent folder, `DELETEFILE` and `DELETEFOLDER`.
- A copy made in Finder is a new file to the server and goes out as `DOWNLOADFILE`. `COPYFILE` is only sent for a copy made through the API, so it stays untested with one device.
- When the Nomad had closed its socket (sleep), it synced on reconnect and fetched the new file itself. The push for that file still went out after the sync; the Nomad answered it with one `query_v3` and did nothing else.
- A folder with 30 notes, moved in at once: one push with one `ADDFOLDER` and 30 `DOWNLOADFILE` items (12:18:23). The Nomad fetched each file once and was done at 12:18:28. Deleting the folder: one push with 30 `DELETEFILE` items and one `DELETEFOLDER` (12:19:35). The file deletes are not folded into the folder delete.
- `render_dir`: the first pass wrote 41 PDFs for 41 notes. A note changed on the Nomad was rendered again 2 seconds after its upload; the other PDFs stayed untouched.
- Export to PDF and to PNG renders on the device. The result lands in `EXPORT/` (one PDF, or one PNG per page named `<note>_Page_<n>.png`) and syncs as a normal upload. No conversion route is called.
- "Share via QR Code", tried twice (the second time after a restart of the Nomad): no request reached Notedown. The QR code held a `viewer.supernote.com` link with a new `shareId` each time (`fileType=2`, then `fileType=1`), valid for one day. So sharing goes through Ratta's service even when the device uses a Private Cloud, and `share/record/add` is never called.
