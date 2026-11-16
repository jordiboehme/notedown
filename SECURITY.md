# Security policy

## Report a vulnerability

Please don't open a public issue for a security problem. Use GitHub's private vulnerability reporting instead:

1. Open the [Security tab](https://github.com/jordiboehme/notedown/security) of this repository.
2. Click "Report a vulnerability".
3. Describe the problem and how to reproduce it. A sample file or the request that causes it helps a lot.

Only the maintainer can see your report.

## Supported versions

Only the latest release gets security fixes. Please check that the problem still exists there before you report it.

## The sync server

`notedown serve` is the Private Cloud of a Supernote device. It speaks plain HTTP and is made for the home network:

- Don't expose it to the internet directly. Use a VPN, or a reverse proxy with TLS, to reach it from outside.
- The server also has a REST API ([docs/rest-api.md](docs/rest-api.md)). The pipeline on another machine reads with it (`notedown connect`). It has the same exposure: plain HTTP on the home network. An account logs in with its password once. The password itself is never sent: the client proves it with a challenge. After that a token is the only key. It is like a password, so keep the file in `tokens/` private (mode 0600). The token itself crosses the network in clear text with every request over plain HTTP, so anyone who can read the traffic on the home network can copy it. To reach the API from outside the home network, use HTTPS through a reverse proxy. Revoke tokens with `notedown cloud account tokens`. A password change revokes every token.
- A token made for writing (`scope` `write`) can add, replace, rename, move and delete files, over the same plain HTTP. A delete goes to the recycle bin, but still: treat a write token like the password. The pipeline's token (`notedown connect`) is read-only. Give a write token only to a program that must change files. The path of an upload is part of the request line, so it is in the request log. The body is never logged.
- MCP is local over stdio only (`notedown mcp`). The server has no MCP endpoint and no sign-in page.

A way to read or change the files of an account without its password, or to get past a sign-in, is a security problem.

## How Notedown treats .note files

Notedown reads `.note` files from the device and from the sync, so it treats every `.note` file as untrusted input. A `.note` file that makes Notedown crash, hang, use a lot of memory or write outside its own folders is a security problem.

Please report these problems as described above.
