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
`$ENO_WEAVE_TMP/.sessions/<id>`; containers carry
`eno-shell=<ENO_WEAVE_TMP>` and `eno-session=<id>`.

The daemon holds `docker exec -i <cid> bash`. Per command it writes
`cmd.N` / `in.N` into the session dir (mounted at `/session`), writes ONE line
to the bash's stdin:

```
eval "$(cat /session/cmd.N)" > /session/out.N 2>&1 < /session/in.N; echo "done N $?"
```

The data is in `out.N`; the bash's stdout is the control channel
(`done N <status>`); there is no polling. The shell's cwd/env/functions
persist. One command runs at a time. A bash EOF/`exit` fails the client
immediately.

## Runtime

`ENO_SHELL_RUNTIME` selects the container CLI (default `docker`;
`ENO_SHELL_DOCKER` is the older spelling). The command shape is the same for a
later podman backend.

## `--stop`

Kills the per-session daemons, removes their containers, and deletes
`.sessions`; idempotent.
