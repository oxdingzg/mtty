# Security

[简体中文](SECURITY.zh-CN.md)

## Reporting a security issue

Use the GitHub Security Advisory
["Report a Vulnerability"](https://github.com/mttydev/mtty/security/advisories/new)
tab. It stays private until it is published.

You will get an answer saying what happens next, and after that, how the fix is
going. There is no security team here and no response-time commitment - if a
week passes with no reply, ask again on the same thread.

## Threat model

mtty is a terminal: it runs the programs you tell it to, with your permissions.
It does not sandbox them, and it is not meant to. Two consequences of that are
worth spelling out, because in both the boundary is deliberate rather than an
oversight.

### The control plane runs commands in your shell

MTP - `mtty-cli`, and anything else speaking the protocol - can start commands in
a pane. That is what it is for, and it is why there is a token.

- The socket is **a per-user endpoint with owner-only permissions** (mode 0600;
  a named pipe on Windows). Other local users cannot connect, which matters
  precisely because `pane.run` executes commands in your shell.
- **Serving it over TCP requires `remote-listen`, and that refuses to start
  without `MTTY_MTP_TOKEN`** - it returns an error rather than carrying on with a
  warning. Every request must then carry the token, and `MTTY_MTP_ALLOW` can
  narrow what is accepted at all.

A **leaked token is a secret you are holding, not a vulnerability in mtty** - the
same as a leaked SSH key. What would be a vulnerability is a way to reach the
control plane without the token, or a socket another user can open.

### Private keys are read here, not used here

`mtty-keys` parses OpenSSH and .ppk `.ppk` private keys and re-encodes them
between the two formats. It does not sign or decrypt with them: the workspace
performs no private-key operation, and the SSH connections themselves are made
by your own OpenSSH.

A key file, and any passphrase you type, stay on the machine.

### Out of scope

| Category | Rationale |
| --- | --- |
| What a program you asked mtty to run does | It runs as you; that is what a terminal is |
| A leaked control-plane token | Custody of a secret, as above |
| A TCP listener you enabled and left reachable | `remote-listen` is opt-in and refuses to start unauthenticated |
| Anything on a remote host you connected to | The host's security, not mtty's |
