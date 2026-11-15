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
- The MCP endpoint for claude.ai is the one part that is made for the internet. notedown serves it only when `external_base_url` is an `https://` address. Put it behind a reverse proxy with TLS that forwards only `/mcp`, `/oauth/` and `/.well-known/` from the internet. The device API stays on the home network. [Getting started](docs/getting-started.md) shows the setup.

A way to read or change the files of an account without its password, to get past a sign-in, or to reach the device API through the MCP setup above is a security problem.

## How notedown treats .note files

notedown reads `.note` files from the device and from the sync, so it treats every `.note` file as untrusted input. A `.note` file that makes notedown crash, hang, use a lot of memory or write outside its own folders is a security problem.

Please report these problems as described above.
