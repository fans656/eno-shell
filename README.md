# eno/shell

A session shell for a machine. One persistent `bash` inside a per-session
container; the host client talks to a per-session daemon over a unix socket.

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
hand-written YAML.

```
---
image: eno-base            # bare = latest
user: 501:20
hostname: sen
network: eno-net
alias: eno.run
workdir: /home/dav
volumes:
  - $HOME/work:/home/dav
environment:
  - HOME=/home/dav
---
```

- `image`, `user`, `hostname`, `workdir` — scalars; `workdir` is the shell's
  initial cwd.
- `network`, `alias` — a docker network to join and a DNS alias to carry on
  it; absent, the container runs on the runtime's default network. The
  network is the caller's to create and remove; `--stop` leaves it alone.
- A numeric `user` gets a passwd entry named after `workdir`'s basename, so a
  prompt's `\u` resolves to that name (the container's uid has no login name).
- `volumes` — `host:container[:ro|rw]`; a relative host path is relative to
  the conf file.
- `environment` — `KEY=VALUE` entries for the container.

`$VAR` is expanded from the host env (`$HOME`, `$ENO_WEAVE_TMP`, …).

## Sessions

One session per material (`ENO_SHELL_SESSION`, defaulted from
`ENO_WEAVE_MATERIAL` by stripping the extension). The session dir is
`$ENO_WEAVE_TMP/.sessions/<id>` and holds only the daemon's socket and pid
file; nothing is mounted into the container, so the same client can later
drive `ssh -T host bash`. Containers carry `eno-shell=<ENO_WEAVE_TMP>` and
`eno-session=<id>`.

The daemon holds `docker exec -i <cid> bash` and, per command, writes ONE
wrapper to the bash's stdin — a fresh random heredoc delimiter, the script
verbatim, and the request's stdin base64-decoded into the eval:

```
eval "$(cat <<'ENO_<nonce>'
<script>
ENO_<nonce>
)" < <(printf %s '<base64>' | base64 -d) 2>&1
printf '\n\036ENO <nonce> %s\n' "$?"
```

The eval's stdout and stderr are the data channel, merged as bytes; the bash's
own stderr stays the daemon's. The marker line carries the status, and exactly
one trailing newline (the marker's prefix) is removed from the data. A command
cannot eat the channel (its stdin is always the decoded request), there is no
polling, and one command runs at a time. The shell's cwd/env/functions persist.
An `exit` in a command ends that shell: the command gets the shell's exit
status, and the next command starts a fresh shell in the same container.

## Runtime

`ENO_SHELL_RUNTIME` selects the container CLI (default `docker`;
`ENO_SHELL_DOCKER` is the older spelling). The command shape is the same for a
later podman backend.

## `--stop`

Kills the per-session daemons, removes their containers, and deletes
`.sessions`; idempotent.
