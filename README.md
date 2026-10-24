# eno/shell

A session shell for a machine. One persistent shell in a space host; the host
client talks to a per-session daemon over a unix socket. The shell owns no
containers: it drives `ENO_SPACE`.

## CLI

```
eno-shell [--conf <path>] -c <script>
eno-shell [--conf <path>] --stop
eno-shell [--conf <path>] serve     # internal: the detached daemon
```

`--conf` defaults to the `shell.eno` beside the binary's installed tree.
`serve` is what the first `-c` spawns; users never call it.

## Conf

One eno document; its frontmatter is the conf mapping, parsed with
`eno/filetype/eno` (`eno_filetype_eno::parse`; `Doc.frontmatter`). No
hand-written YAML. It is a SESSION conf: the containers belong to the space.

```
---
user: dav
host: sen
workdir: /home/dav
environment:
  - PS1=\u@\h:\W$ 
shell: bash
---
```

- `user` — required; a name or a numeric uid (the space's spec resolves it).
- `host` — optional; a name is resolved to its space host, a numeric uid
  needs it.
- `workdir` — the session's cwd; absent, the user's home.
- `environment` — `KEY=VALUE` entries for the session. The space sets
  `HOME`, `USER`, and `LOGNAME` from the user unless a key is overridden here.
- `shell` — the persistent shell (default `bash`).

`$VAR` is expanded from the host env (`$HOME`, `$ENO_WEAVE_TMP`, …).

## Sessions

One session per material (`ENO_SHELL_SESSION`, defaulted from
`ENO_WEAVE_MATERIAL` by stripping the extension). The session dir is
`$ENO_WEAVE_TMP/.sessions/<id>` and holds only the daemon's socket and pid
file; nothing is mounted anywhere. One space id scopes the network, the hosts,
and their users, so sessions share a host's container and never own one.

The transport is the `ENO_SPACE` executable (loud when unset): `serve` runs
`ENO_SPACE up` (idempotent), then holds
`ENO_SPACE exec [--host <h>] --user <u> [--cwd <w>] [-e K=V]... -i -- <shell>`.
Per command it writes ONE wrapper to the shell's stdin — a fresh random heredoc
delimiter, the script verbatim, and the request's stdin base64-decoded into the
eval:

```
eval "$(cat <<'ENO_<nonce>'
<script>
ENO_<nonce>
)" < <(printf %s '<base64>' | base64 -d) 2>&1
printf '\n\036ENO <nonce> %s\n' "$?"
```

The eval's stdout and stderr are the data channel, merged as bytes; the shell's
own stderr stays the daemon's. The marker line carries the status, and exactly
one trailing newline (the marker's prefix) is removed from the data. A command
cannot eat the channel (its stdin is always the decoded request), there is no
polling, and one command runs at a time. The shell's cwd/env/functions persist.
An `exit` in a command ends that shell: the command gets the shell's exit
status, and the next command starts a fresh shell in the same host.

The request carries a fresh 64-bit random nonce. The daemon forwards the data
to the client socket as it arrives, then ends the stream with
`\0ENO <nonce> <status>\n`, echoing that request nonce. The client writes the
data to stdout as it arrives, strips that exact trailer, and exits with the
recorded status. The trailer cannot collide with command output: it starts
with NUL and carries the unguessable request nonce, which is separate from the
container wrapper's nonce and never reaches the command. A command that prints
its own `\0ENO ...` line is still just output unless it reproduces the exact
nonce. A client that goes away mid-command does not poison the session: the
daemon drains the command to its marker, then accepts the next one. If the
shell dies, the daemon logs `bash exited` after forwarding the output it
already received.

## `--stop`

Kills the per-session daemons and deletes `.sessions`; idempotent. It does not
touch containers — those are the space's (`ENO_SPACE down`).
