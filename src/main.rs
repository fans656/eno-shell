use std::env;
use std::fs::{self, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{self, Child, ChildStdin, ChildStdout, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
struct Request {
    script: String,
    stdin: Vec<u8>,
}

#[derive(Serialize, Deserialize)]
struct Response {
    output: Vec<u8>,
    status: i32,
}

struct Conf {
    image: String,
    user: Option<String>,
    hostname: Option<String>,
    workdir: String,
    volumes: Vec<(PathBuf, String, String)>,
    environment: Vec<(String, String)>,
}

impl Conf {
    fn load(path: &Path) -> Result<Self, String> {
        let text = fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let doc = eno_filetype_eno::parse(&text, 0)
            .map_err(|e| format!("{}: {e}", path.display()))?;
        let value = doc
            .frontmatter
            .ok_or_else(|| format!("{}: no frontmatter", path.display()))?;
        let map = value
            .as_object()
            .ok_or_else(|| format!("{}: frontmatter is not a mapping", path.display()))?;
        let dir = realpath_lenient(path.parent().unwrap_or_else(|| Path::new(".")));

        let image = match map.get("image") {
            Some(value) => expand(value.as_str().ok_or("conf: `image` must be a string")?),
            None => "eno-base".to_string(),
        };
        let image = if image.rsplit('/').next().map(|part| part.contains(':')).unwrap_or(false) {
            image
        } else {
            format!("{image}:latest")
        };
        let user = match map.get("user") {
            Some(value) => {
                let value = expand(value.as_str().ok_or("conf: `user` must be a string")?);
                if value.is_empty() { None } else { Some(value) }
            }
            None => None,
        };
        let hostname = match map.get("hostname") {
            Some(value) => {
                let value = expand(value.as_str().ok_or("conf: `hostname` must be a string")?);
                if value.is_empty() { None } else { Some(value) }
            }
            None => None,
        };
        let workdir = match map.get("workdir") {
            Some(value) => {
                let value = expand(value.as_str().ok_or("conf: `workdir` must be a string")?);
                if value.is_empty() { "/".to_string() } else { value }
            }
            None => "/".to_string(),
        };
        let volumes = match map.get("volumes") {
            Some(value) => {
                let items = value.as_array().ok_or("conf: `volumes` must be a list")?;
                let mut out = Vec::with_capacity(items.len());
                for item in items {
                    let spec = item.as_str().ok_or("conf: a volume must be a string")?;
                    out.push(parse_volume(spec, &dir)?);
                }
                out
            }
            None => Vec::new(),
        };
        let environment = match map.get("environment") {
            Some(value) => {
                let items = value
                    .as_array()
                    .ok_or("conf: `environment` must be a list")?;
                let mut out = Vec::with_capacity(items.len());
                for item in items {
                    let spec = item
                        .as_str()
                        .ok_or("conf: an environment item must be a string")?;
                    out.push(parse_env(spec)?);
                }
                out
            }
            None => Vec::new(),
        };
        Ok(Self {
            image,
            user,
            hostname,
            workdir,
            volumes,
            environment,
        })
    }
}

fn parse_volume(spec: &str, dir: &Path) -> Result<(PathBuf, String, String), String> {
    let spec = expand(spec);
    let parts: Vec<&str> = spec.split(':').collect();
    let (source, target, mode) = if parts.len() >= 3
        && (parts[parts.len() - 1] == "ro" || parts[parts.len() - 1] == "rw")
    {
        (
            parts[..parts.len() - 2].join(":"),
            parts[parts.len() - 2].to_string(),
            parts[parts.len() - 1].to_string(),
        )
    } else if parts.len() >= 2 {
        (
            parts[..parts.len() - 1].join(":"),
            parts[parts.len() - 1].to_string(),
            "rw".to_string(),
        )
    } else {
        return Err(format!("conf: bad volume `{spec}`"));
    };
    let source = if Path::new(&source).is_absolute() {
        PathBuf::from(source)
    } else {
        dir.join(source)
    };
    let target = format!("/{}", target.trim_matches('/'));
    Ok((realpath_lenient(&source), target, mode))
}

fn parse_env(spec: &str) -> Result<(String, String), String> {
    let (key, value) = spec
        .split_once('=')
        .ok_or_else(|| format!("conf: bad environment `{spec}` (want KEY=VALUE)"))?;
    Ok((key.trim().to_string(), expand(value)))
}

fn expand(value: &str) -> String {
    let chars: Vec<char> = value.chars().collect();
    let mut out = String::new();
    let mut index = 0;
    while index < chars.len() {
        if chars[index] != '$' {
            out.push(chars[index]);
            index += 1;
            continue;
        }
        if index + 1 < chars.len() && chars[index + 1] == '{' {
            if let Some(offset) = chars[index + 2..].iter().position(|&c| c == '}') {
                let name: String = chars[index + 2..index + 2 + offset].iter().collect();
                if valid_name(&name) {
                    if let Ok(found) = env::var(&name) {
                        out.push_str(&found);
                        index += 3 + offset;
                        continue;
                    }
                }
            }
        } else if index + 1 < chars.len()
            && (chars[index + 1].is_ascii_alphabetic() || chars[index + 1] == '_')
        {
            let mut end = index + 2;
            while end < chars.len()
                && (chars[end].is_ascii_alphanumeric() || chars[end] == '_')
            {
                end += 1;
            }
            let name: String = chars[index + 1..end].iter().collect();
            if let Ok(found) = env::var(&name) {
                out.push_str(&found);
                index = end;
                continue;
            }
        }
        out.push('$');
        index += 1;
    }
    out
}

fn valid_name(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn realpath_lenient(path: &Path) -> PathBuf {
    if let Ok(found) = fs::canonicalize(path) {
        return found;
    }
    match (path.parent(), path.file_name()) {
        (Some(parent), Some(name)) => realpath_lenient(parent).join(name),
        _ => path.to_path_buf(),
    }
}

fn is_executable(path: &Path) -> bool {
    fs::metadata(path)
        .map(|meta| meta.is_file() && (meta.permissions().mode() & 0o111 != 0))
        .unwrap_or(false)
}

fn find_in_path(name: &str) -> Option<String> {
    let path = env::var_os("PATH")?;
    for dir in env::split_paths(&path) {
        let candidate = dir.join(name);
        if is_executable(&candidate) {
            return Some(candidate.to_string_lossy().into_owned());
        }
    }
    None
}

fn runtime() -> String {
    let configured = env::var("ENO_SHELL_RUNTIME")
        .or_else(|_| env::var("ENO_SHELL_DOCKER"))
        .unwrap_or_else(|_| "docker".to_string());
    if configured.contains('/') {
        return configured;
    }
    if let Some(found) = find_in_path(&configured) {
        return found;
    }
    if configured == "docker" {
        for candidate in [
            "/opt/homebrew/bin/docker",
            "/usr/local/bin/docker",
            "/usr/bin/docker",
            "/bin/docker",
        ] {
            if is_executable(Path::new(candidate)) {
                return candidate.to_string();
            }
        }
    }
    configured
}

fn docker(args: &[String]) -> Result<String, String> {
    let runtime = runtime();
    let output = Command::new(&runtime)
        .args(args)
        .output()
        .map_err(|e| format!("{runtime}: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "{runtime} {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn list_containers(rid: &str, sid: Option<&str>) -> Result<Vec<String>, String> {
    let mut args = vec![
        "ps".to_string(),
        "-aq".to_string(),
        "--filter".to_string(),
        format!("label=eno-shell={rid}"),
    ];
    if let Some(sid) = sid {
        args.push("--filter".to_string());
        args.push(format!("label=eno-session={sid}"));
    }
    Ok(docker(&args)?
        .split_whitespace()
        .map(str::to_string)
        .collect())
}

fn run_id() -> Result<String, String> {
    env::var("ENO_WEAVE_TMP")
        .ok()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "ENO_WEAVE_TMP is not set".to_string())
}

fn session_id() -> Result<String, String> {
    env::var("ENO_SHELL_SESSION")
        .ok()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "ENO_SHELL_SESSION is not set (ENO_WEAVE_MATERIAL?)".to_string())
}

fn session_dir(rid: &str, sid: &str) -> PathBuf {
    Path::new(rid).join(".sessions").join(sid)
}

fn env_defaults() {
    if env::var_os("ENO_HOST_UID").is_none() {
        env::set_var("ENO_HOST_UID", unsafe { libc::getuid() }.to_string());
    }
    if env::var_os("ENO_HOST_GID").is_none() {
        env::set_var("ENO_HOST_GID", unsafe { libc::getgid() }.to_string());
    }
    if env::var_os("ENO_SHELL_SESSION").is_none() {
        if let Ok(material) = env::var("ENO_WEAVE_MATERIAL") {
            let sid = match material.rsplit_once('.') {
                Some((stem, _)) => stem.to_string(),
                None => material,
            };
            env::set_var("ENO_SHELL_SESSION", sid);
        }
    }
}

fn default_conf() -> PathBuf {
    if let Ok(exe) = env::current_exe() {
        if let Some(host) = exe.ancestors().nth(4) {
            return host.join("shell.eno");
        }
    }
    PathBuf::from("shell.eno")
}

fn spawn_daemon(conf_path: &Path, session: &Path) -> Result<(), String> {
    let log = session.join("daemon.log");
    let stderr = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log)
        .map_err(|e| format!("{}: {e}", log.display()))?;
    let exe = env::current_exe().map_err(|e| format!("current exe: {e}"))?;
    let mut command = Command::new(exe);
    command.arg("--conf").arg(conf_path).arg("serve");
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(stderr));
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                Err(io::Error::last_os_error())
            } else {
                Ok(())
            }
        });
    }
    command.spawn().map_err(|e| format!("spawn daemon: {e}"))?;
    Ok(())
}

fn connect_retry(sock: &Path, session: &Path) -> Result<UnixStream, String> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match UnixStream::connect(sock) {
            Ok(stream) => return Ok(stream),
            Err(error) if Instant::now() < deadline => {
                let _ = error;
                thread::sleep(Duration::from_millis(5));
            }
            Err(error) => {
                return Err(format!(
                    "daemon did not start ({error}); see {}",
                    session.join("daemon.log").display()
                ));
            }
        }
    }
}

fn run(conf_path: &Path, script: &str) -> Result<i32, String> {
    let rid = run_id()?;
    let sid = session_id()?;
    let session = session_dir(&rid, &sid);
    fs::create_dir_all(&session).map_err(|e| format!("{}: {e}", session.display()))?;
    let sock = session.join("sock");
    let mut stream = match UnixStream::connect(&sock) {
        Ok(stream) => stream,
        Err(_) => {
            spawn_daemon(conf_path, &session)?;
            connect_retry(&sock, &session)?
        }
    };

    let mut stdin = Vec::new();
    io::stdin()
        .read_to_end(&mut stdin)
        .map_err(|e| format!("stdin: {e}"))?;
    let request = Request {
        script: script.to_string(),
        stdin,
    };
    serde_json::to_writer(&mut stream, &request).map_err(|e| format!("socket: {e}"))?;
    stream
        .shutdown(std::net::Shutdown::Write)
        .map_err(|e| format!("socket: {e}"))?;
    let response: Response =
        serde_json::from_reader(&mut stream).map_err(|e| format!("socket: {e}"))?;
    let mut stdout = io::stdout().lock();
    stdout
        .write_all(&response.output)
        .map_err(|e| format!("stdout: {e}"))?;
    stdout.flush().map_err(|e| format!("stdout: {e}"))?;
    Ok(response.status)
}

struct Shell {
    _child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl Shell {
    fn spawn(cid: &str) -> Result<Self, String> {
        let runtime = runtime();
        let mut command = Command::new(&runtime);
        command.arg("exec").arg("-i").arg(cid).arg("bash");
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        let mut child = command
            .spawn()
            .map_err(|e| format!("{runtime} exec: {e}"))?;
        let stdin = child.stdin.take().ok_or("bash: no stdin")?;
        let stdout = child.stdout.take().ok_or("bash: no stdout")?;
        Ok(Self {
            _child: child,
            stdin,
            stdout: BufReader::new(stdout),
        })
    }
}

fn ensure_container(conf: &Conf, rid: &str, sid: &str, session: &Path) -> Result<String, String> {
    for (source, _target, mode) in &conf.volumes {
        if mode == "rw" {
            fs::create_dir_all(source).map_err(|e| format!("{}: {e}", source.display()))?;
        }
    }
    let ids = list_containers(rid, Some(sid))?;
    if let Some(cid) = ids.first() {
        let state = docker(&[
            "inspect".to_string(),
            "-f".to_string(),
            "{{.State.Running}}".to_string(),
            cid.clone(),
        ])?;
        if state.trim() == "true" {
            ensure_user(conf, cid)?;
            return Ok(cid.clone());
        }
        docker(&["rm".to_string(), "-f".to_string(), cid.clone()])?;
    }

    let mut args = vec![
        "run".to_string(),
        "-d".to_string(),
        "--label".to_string(),
        format!("eno-shell={rid}"),
        "--label".to_string(),
        format!("eno-session={sid}"),
    ];
    if let Some(user) = &conf.user {
        args.push("--user".to_string());
        args.push(user.clone());
    }
    if let Some(hostname) = &conf.hostname {
        args.push("--hostname".to_string());
        args.push(hostname.clone());
    }
    args.push("-w".to_string());
    args.push(conf.workdir.clone());
    for (source, target, mode) in &conf.volumes {
        args.push("-v".to_string());
        args.push(format!("{}:{target}:{mode}", source.display()));
    }
    args.push("-v".to_string());
    args.push(format!("{}:/session:rw", realpath_lenient(session).display()));
    for (key, value) in &conf.environment {
        args.push("-e".to_string());
        args.push(format!("{key}={value}"));
    }
    args.push(conf.image.clone());
    args.push("sleep".to_string());
    args.push("infinity".to_string());
    let cid = docker(&args)?;
    let cid = cid.trim().to_string();
    ensure_user(conf, &cid)?;
    Ok(cid)
}

fn ensure_user(conf: &Conf, cid: &str) -> Result<(), String> {
    let Some(user) = conf.user.as_deref() else {
        return Ok(());
    };
    let Some((uid, gid)) = user.split_once(':') else {
        return Ok(());
    };
    let Some(name) = Path::new(&conf.workdir)
        .file_name()
        .and_then(|name| name.to_str())
    else {
        return Ok(());
    };
    let script = format!(
        "name={}; grep -q \"^${{name}}:\" /etc/passwd || printf '%s:x:%s:%s:%s:%s:/bin/bash\\n' \"$name\" {} {} \"$name\" {} >> /etc/passwd",
        shell_quote(name),
        shell_quote(uid),
        shell_quote(gid),
        shell_quote(&conf.workdir),
    );
    docker(&[
        "exec".to_string(),
        "-u".to_string(),
        "0".to_string(),
        cid.to_string(),
        "sh".to_string(),
        "-c".to_string(),
        script,
    ])?;
    Ok(())
}

fn shell_quote(value: &str) -> String {
    let mut out = String::from("'");
    for ch in value.chars() {
        if ch == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(ch);
        }
    }
    out.push('\'');
    out
}

fn parse_done(line: &str, n: u64) -> Option<i32> {
    let line = line.trim_end_matches(|c| c == '\r' || c == '\n');
    let prefix = format!("done {n} ");
    line.strip_prefix(&prefix)?.parse().ok()
}

fn handle(
    shell: &mut Shell,
    session: &Path,
    n: &mut u64,
    stream: UnixStream,
) -> Result<(), String> {
    let request: Request =
        serde_json::from_reader(&stream).map_err(|e| format!("request: {e}"))?;
    *n += 1;
    let n = *n;
    let cmd = session.join(format!("cmd.{n}"));
    fs::write(&cmd, request.script.as_bytes())
        .map_err(|e| format!("{}: {e}", cmd.display()))?;
    let input = session.join(format!("in.{n}"));
    fs::write(&input, &request.stdin).map_err(|e| format!("{}: {e}", input.display()))?;
    let line = format!(
        "eval \"$(cat /session/cmd.{n})\" > /session/out.{n} 2>&1 < /session/in.{n}; echo \"done {n} $?\"\n"
    );
    shell
        .stdin
        .write_all(line.as_bytes())
        .map_err(|e| format!("bash: {e}"))?;
    shell.stdin.flush().map_err(|e| format!("bash: {e}"))?;
    let status = loop {
        let mut line = String::new();
        let read = shell
            .stdout
            .read_line(&mut line)
            .map_err(|e| format!("bash: {e}"))?;
        if read == 0 {
            return Err("bash exited".to_string());
        }
        if let Some(status) = parse_done(&line, n) {
            break status;
        }
    };
    let out = session.join(format!("out.{n}"));
    let output = fs::read(&out).map_err(|e| format!("{}: {e}", out.display()))?;
    let response = Response { output, status };
    serde_json::to_writer(&stream, &response).map_err(|e| format!("socket: {e}"))?;
    Ok(())
}

fn serve(conf_path: &Path) -> Result<i32, String> {
    let conf = Conf::load(conf_path)?;
    let rid = run_id()?;
    let sid = session_id()?;
    let session = session_dir(&rid, &sid);
    fs::create_dir_all(&session).map_err(|e| format!("{}: {e}", session.display()))?;
    let sock = session.join("sock");
    fs::remove_file(&sock).ok();
    let listener =
        UnixListener::bind(&sock).map_err(|e| format!("bind {}: {e}", sock.display()))?;
    fs::write(session.join("daemon.pid"), format!("{}\n", process::id()))
        .map_err(|e| format!("daemon.pid: {e}"))?;
    let cid = ensure_container(&conf, &rid, &sid, &session)?;
    let mut shell = Shell::spawn(&cid)?;
    let mut n = 0u64;
    for stream in listener.incoming() {
        let stream = stream.map_err(|e| format!("accept: {e}"))?;
        if let Err(error) = handle(&mut shell, &session, &mut n, stream) {
            eprintln!("eno-shell: {error}");
            return Err(error);
        }
    }
    Ok(0)
}

fn stop() -> Result<i32, String> {
    let rid = run_id()?;
    let sessions = Path::new(&rid).join(".sessions");
    if let Ok(entries) = fs::read_dir(&sessions) {
        for entry in entries.flatten() {
            let pid_path = entry.path().join("daemon.pid");
            if let Ok(text) = fs::read_to_string(&pid_path) {
                if let Ok(pid) = text.trim().parse::<i32>() {
                    unsafe {
                        libc::kill(pid, libc::SIGTERM);
                    }
                }
            }
        }
    }
    let ids = list_containers(&rid, None)?;
    if !ids.is_empty() {
        let mut args = vec!["rm".to_string(), "-f".to_string()];
        args.extend(ids);
        docker(&args)?;
    }
    fs::remove_dir_all(&sessions).ok();
    Ok(0)
}

fn real_main() -> Result<i32, String> {
    env_defaults();
    let mut conf: Option<PathBuf> = None;
    let mut script: Option<String> = None;
    let mut stop_flag = false;
    let mut serve_flag = false;
    let mut args = env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--conf" => {
                conf = Some(PathBuf::from(
                    args.next()
                        .ok_or_else(|| "usage: eno-shell --conf <path>".to_string())?,
                ));
            }
            "-c" => {
                script = Some(
                    args.next()
                        .ok_or_else(|| "usage: eno-shell -c <script>".to_string())?,
                );
            }
            "--stop" => stop_flag = true,
            "serve" => serve_flag = true,
            other => {
                return Err(format!(
                    "usage: eno-shell [--conf <path>] -c <script> | --stop (unknown `{other}`)"
                ));
            }
        }
    }
    let conf = conf.unwrap_or_else(default_conf);
    if serve_flag {
        serve(&conf)
    } else if stop_flag {
        stop()
    } else if let Some(script) = script {
        run(&conf, &script)
    } else {
        Err("usage: eno-shell [--conf <path>] -c <script> | --stop".to_string())
    }
}

fn main() {
    match real_main() {
        Ok(code) => process::exit(code),
        Err(error) => {
            eprintln!("eno-shell: {error}");
            process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    fn conf(dir: &Path, body: &str) -> PathBuf {
        let path = dir.join("shell.eno");
        fs::write(&path, format!("---\n{body}---\n")).unwrap();
        path
    }

    #[test]
    fn conf_frontmatter_parses_the_contract() {
        let dir = tempdir().unwrap();
        let d = dir.path();
        fs::create_dir_all(d.join("data")).unwrap();
        let path = conf(
            d,
            "image: eno-base\nuser: 501:20\nhostname: sen\nworkdir: /home/dav\nvolumes:\n  - data:/data:rw\n  - ../outside:/out:ro\nenvironment:\n  - HOME=/home/dav\n  - K=V\n",
        );
        let loaded = Conf::load(&path).unwrap();
        assert_eq!(loaded.image, "eno-base:latest");
        assert_eq!(loaded.user.as_deref(), Some("501:20"));
        assert_eq!(loaded.hostname.as_deref(), Some("sen"));
        assert_eq!(loaded.workdir, "/home/dav");
        assert_eq!(loaded.volumes[0].0, fs::canonicalize(d.join("data")).unwrap());
        assert_eq!(loaded.volumes[0].1, "/data");
        assert_eq!(loaded.volumes[0].2, "rw");
        assert_eq!(loaded.volumes[1].0.file_name().unwrap(), "outside");
        assert_eq!(loaded.volumes[1].1, "/out");
        assert_eq!(loaded.volumes[1].2, "ro");
        assert_eq!(
            loaded.environment,
            vec![
                ("HOME".to_string(), "/home/dav".to_string()),
                ("K".to_string(), "V".to_string()),
            ]
        );
    }

    #[test]
    fn conf_shape_errors_are_loud() {
        let dir = tempdir().unwrap();
        for body in [
            "image: [x]\n",
            "volumes: x\n",
            "environment:\n  - K\n",
            "user: [x]\n",
        ] {
            let path = conf(dir.path(), body);
            assert!(Conf::load(&path).is_err(), "{body}");
        }
        let path = dir.path().join("no.eno");
        fs::write(&path, "no frontmatter\n").unwrap();
        assert!(Conf::load(&path).is_err());
        let path = dir.path().join("broken.eno");
        fs::write(&path, "---\nimage: [\n---\n").unwrap();
        assert!(Conf::load(&path).is_err());
    }

    #[test]
    fn volume_and_env_parse() {
        let dir = tempdir().unwrap();
        let d = dir.path();
        fs::create_dir_all(d.join("data")).unwrap();
        let (source, target, mode) = parse_volume("data:/data:ro", d).unwrap();
        assert_eq!(source, fs::canonicalize(d.join("data")).unwrap());
        assert_eq!(target, "/data");
        assert_eq!(mode, "ro");
        let (_, target, mode) = parse_volume("data:/data", d).unwrap();
        assert_eq!(target, "/data");
        assert_eq!(mode, "rw");
        assert!(parse_volume("data", d).is_err());
        assert_eq!(
            parse_env("K=V").unwrap(),
            ("K".to_string(), "V".to_string())
        );
        assert!(parse_env("K").is_err());
    }

    #[test]
    fn done_lines_parse() {
        assert_eq!(parse_done("done 1 0\n", 1), Some(0));
        assert_eq!(parse_done("done 2 7\r\n", 2), Some(7));
        assert_eq!(parse_done("done 1 0\n", 2), None);
        assert_eq!(parse_done("done 1\n", 1), None);
        assert_eq!(parse_done("ready\n", 1), None);
    }

    #[test]
    fn expand_reads_env_and_keeps_unknown() {
        std::env::set_var("ENO_SHELL_TEST_VAR", "abc");
        assert_eq!(expand("$ENO_SHELL_TEST_VAR/x"), "abc/x");
        assert_eq!(expand("${ENO_SHELL_TEST_VAR}y"), "abcy");
        assert_eq!(expand("$ENO_SHELL_MISSING/x"), "$ENO_SHELL_MISSING/x");
    }
}
