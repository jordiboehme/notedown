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
- The server also has a read-only REST API for a pipeline on another machine (`notedown connect`). It has the same exposure: plain HTTP on the home network. An account logs in with its password once. The password itself is never sent: the client proves it with a challenge. After that a token is the only key. It is like a password, so keep the file in `tokens/` private (mode 0600). Revoke tokens with `notedown cloud account tokens`. A password change revokes every token.
- MCP is local over stdio only (`notedown mcp`). The server has no MCP endpoint and no sign-in page.

A way to read or change the files of an account without its password, or to get past a sign-in, is a security problem.

## How notedown treats .note files

notedown reads `.note` files from the device and from the sync, so it treats every `.note` file as untrusted input. A `.note` file that makes notedown crash, hang, use a lot of memory or write outside its own folders is a security problem.

Please report these problems as described above.
