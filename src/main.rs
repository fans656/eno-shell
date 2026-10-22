use std::env;
use std::fs::{self, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Write};
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
    user: String,
    host: Option<String>,
    workdir: Option<String>,
    environment: Vec<(String, String)>,
    shell: String,
}

fn scalar(value: &serde_json::Value) -> Option<String> {
    if let Some(text) = value.as_str() {
        return Some(text.to_string());
    }
    value.as_i64().map(|number| number.to_string())
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
        let user = match map.get("user") {
            Some(value) => expand(&scalar(value).ok_or("conf: `user` must be a string")?),
            None => return Err("conf: `user` is required".to_string()),
        };
        if user.is_empty() {
            return Err("conf: `user` is required".to_string());
        }
        let host = match map.get("host") {
            Some(value) => {
                let value = expand(&scalar(value).ok_or("conf: `host` must be a string")?);
                if value.is_empty() { None } else { Some(value) }
            }
            None => None,
        };
        let workdir = match map.get("workdir") {
            Some(value) => {
                let value = expand(&scalar(value).ok_or("conf: `workdir` must be a string")?);
                if value.is_empty() { None } else { Some(value) }
            }
            None => None,
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
        let shell = match map.get("shell") {
            Some(value) => {
                let value = expand(&scalar(value).ok_or("conf: `shell` must be a string")?);
                if value.is_empty() { "bash".to_string() } else { value }
            }
            None => "bash".to_string(),
        };
        Ok(Self {
            user,
            host,
            workdir,
            environment,
            shell,
        })
    }
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

fn space_path() -> Result<PathBuf, String> {
    match env::var("ENO_SPACE") {
        Ok(value) if !value.is_empty() => Ok(PathBuf::from(value)),
        _ => Err("ENO_SPACE is not set (the space unit path)".to_string()),
    }
}

fn eno_bin() -> Result<PathBuf, String> {
    if let Some(value) = env::var_os("ENO_BIN").filter(|v| !v.is_empty()) {
        return Ok(PathBuf::from(value));
    }
    if let Some(path) = env::var_os("PATH") {
        for dir in env::split_paths(&path) {
            let candidate = dir.join("eno");
            if candidate.is_file() {
                return Ok(candidate);
            }
        }
    }
    Err("ENO_BIN is not set and `eno` is not on PATH".to_string())
}

fn space_command(space: &Path) -> Result<Command, String> {
    let mut command = Command::new(eno_bin()?);
    command.arg("run").arg(space);
    Ok(command)
}

fn space_name() -> Result<String, String> {
    env::var("ENO_SPACE_ID")
        .ok()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "ENO_SPACE_ID is not set".to_string())
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
    space_path()?;
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
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    dead: bool,
}

impl Shell {
    fn spawn(space: &Path, conf: &Conf) -> Result<Self, String> {
        let mut command = space_command(space)?;
        command.arg("exec");
        command.arg(space_name()?);
        if let Some(host) = &conf.host {
            command.arg("--host").arg(host);
        }
        command.arg("--user").arg(&conf.user);
        if let Some(workdir) = &conf.workdir {
            command.arg("--cwd").arg(workdir);
        }
        for (key, value) in &conf.environment {
            command.arg("-e").arg(format!("{key}={value}"));
        }
        command.arg("-i").arg("--").arg(&conf.shell);
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        let mut child = command
            .spawn()
            .map_err(|e| format!("{} exec: {e}", space.display()))?;
        let stdin = child.stdin.take().ok_or("bash: no stdin")?;
        let stdout = child.stdout.take().ok_or("bash: no stdout")?;
        Ok(Self {
            child,
            stdin,
            stdout: BufReader::new(stdout),
            dead: false,
        })
    }
}

fn nonce() -> Result<String, String> {
    let mut bytes = [0u8; 8];
    fs::File::open("/dev/urandom")
        .and_then(|mut file| file.read_exact(&mut bytes))
        .map_err(|e| format!("/dev/urandom: {e}"))?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn base64_encode(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let second = chunk.get(1).copied().unwrap_or(0);
        let third = chunk.get(2).copied().unwrap_or(0);
        let bits = ((chunk[0] as u32) << 16) | ((second as u32) << 8) | third as u32;
        out.push(TABLE[(bits >> 18) as usize & 63] as char);
        out.push(TABLE[(bits >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            TABLE[(bits >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[bits as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

fn wrapper(nonce: &str, script: &str, stdin: &[u8]) -> String {
    let mut body = script.to_string();
    if !body.ends_with('\n') {
        body.push('\n');
    }
    format!(
        "eval \"$(cat <<'ENO_{nonce}'\n{body}ENO_{nonce}\n)\" < <(printf %s '{}' | base64 -d) 2>&1\nprintf '\\n\\036ENO {nonce} %s\\n' \"$?\"\n",
        base64_encode(stdin),
    )
}

fn marker_status(chunk: &[u8], nonce: &str) -> Result<Option<i32>, String> {
    let prefix = format!("\x1eENO {nonce} ");
    let Some(rest) = chunk.strip_prefix(prefix.as_bytes()) else {
        return Ok(None);
    };
    let text = std::str::from_utf8(rest).map_err(|e| format!("marker: {e}"))?;
    text.trim_end_matches(['\r', '\n'])
        .parse::<i32>()
        .map(Some)
        .map_err(|e| format!("marker status: {e}"))
}

fn strip_marker_newline(data: &mut Vec<u8>) {
    if data.last() == Some(&b'\n') {
        data.pop();
    }
}

fn handle(shell: &mut Shell, stream: UnixStream) -> Result<(), String> {
    if shell.dead {
        return Err("bash exited".to_string());
    }
    let request: Request =
        serde_json::from_reader(&stream).map_err(|e| format!("request: {e}"))?;
    let nonce = nonce()?;
    let line = wrapper(&nonce, &request.script, &request.stdin);
    shell
        .stdin
        .write_all(line.as_bytes())
        .map_err(|e| format!("bash: {e}"))?;
    shell.stdin.flush().map_err(|e| format!("bash: {e}"))?;
    let mut output = Vec::new();
    let status = loop {
        let mut chunk = Vec::new();
        let read = shell
            .stdout
            .read_until(b'\n', &mut chunk)
            .map_err(|e| format!("bash: {e}"))?;
        if read == 0 {
            shell.dead = true;
            let code = shell.child.wait().ok().and_then(|status| status.code());
            break code.ok_or_else(|| "bash exited".to_string())?;
        }
        match marker_status(&chunk, &nonce)? {
            Some(status) => {
                strip_marker_newline(&mut output);
                break status;
            }
            None => output.extend_from_slice(&chunk),
        }
    };
    let response = Response { output, status };
    serde_json::to_writer(&stream, &response).map_err(|e| format!("socket: {e}"))?;
    if shell.dead {
        return Err("bash exited".to_string());
    }
    Ok(())
}

fn serve(conf_path: &Path) -> Result<i32, String> {
    let space = space_path()?;
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
    let mut shell = Shell::spawn(&space, &conf)?;
    for stream in listener.incoming() {
        let stream = stream.map_err(|e| format!("accept: {e}"))?;
        if let Err(error) = handle(&mut shell, stream) {
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
    let conf = conf
        .or_else(|| env::var_os("ENO_SHELL_CONF").map(PathBuf::from))
        .unwrap_or_else(default_conf);
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
        let path = conf(
            dir.path(),
            "user: dav\nhost: sen\nworkdir: /home/dav\nenvironment:\n  - HOME=/home/dav\n  - K=V\nshell: bash\n",
        );
        let loaded = Conf::load(&path).unwrap();
        assert_eq!(loaded.user, "dav");
        assert_eq!(loaded.host.as_deref(), Some("sen"));
        assert_eq!(loaded.workdir.as_deref(), Some("/home/dav"));
        assert_eq!(loaded.shell, "bash");
        assert_eq!(
            loaded.environment,
            vec![
                ("HOME".to_string(), "/home/dav".to_string()),
                ("K".to_string(), "V".to_string()),
            ]
        );
    }

    #[test]
    fn conf_defaults_and_requirements() {
        let dir = tempdir().unwrap();
        let loaded = Conf::load(&conf(dir.path(), "user: 501\n")).unwrap();
        assert_eq!(loaded.user, "501");
        assert_eq!(loaded.host, None);
        assert_eq!(loaded.workdir, None);
        assert_eq!(loaded.shell, "bash");
        assert!(loaded.environment.is_empty());
        assert!(Conf::load(&conf(dir.path(), "shell: bash\n")).is_err());
    }

    #[test]
    fn conf_shape_errors_are_loud() {
        let dir = tempdir().unwrap();
        for body in [
            "user: [x]\n",
            "host: [x]\n",
            "workdir: [x]\n",
            "shell: [x]\n",
            "environment: x\n",
            "environment:\n  - K\n",
        ] {
            let path = conf(dir.path(), body);
            assert!(Conf::load(&path).is_err(), "{body}");
        }
        let path = dir.path().join("no.eno");
        fs::write(&path, "no frontmatter\n").unwrap();
        assert!(Conf::load(&path).is_err());
        let path = dir.path().join("broken.eno");
        fs::write(&path, "---\nuser: [\n---\n").unwrap();
        assert!(Conf::load(&path).is_err());
    }

    #[test]
    fn env_parses_and_expands() {
        std::env::set_var("ENO_SHELL_TEST_VAR", "abc");
        assert_eq!(
            parse_env("K=V").unwrap(),
            ("K".to_string(), "V".to_string())
        );
        assert_eq!(
            parse_env("K=$ENO_SHELL_TEST_VAR").unwrap(),
            ("K".to_string(), "abc".to_string())
        );
        assert!(parse_env("K").is_err());
    }

    #[test]
    fn marker_status_parses_the_channel() {
        assert_eq!(marker_status(b"\x1eENO abc123 0\n", "abc123"), Ok(Some(0)));
        assert_eq!(
            marker_status(b"\x1eENO abc123 7\r\n", "abc123"),
            Ok(Some(7))
        );
        assert_eq!(
            marker_status(b"\x1eENO abc123 130\n", "abc123"),
            Ok(Some(130))
        );
        assert_eq!(marker_status(b"\x1eENO abc123 0", "abc123"), Ok(Some(0)));
        assert_eq!(marker_status(b"\x1eENO abc123 7\n", "def456"), Ok(None));
        assert_eq!(marker_status(b"\x1eENO abc123\n", "abc123"), Ok(None));
        assert!(marker_status(b"\x1eENO abc123 \n", "abc123").is_err());
    }

    #[test]
    fn marker_newline_strips_exactly_once() {
        let mut data = b"hi".to_vec();
        strip_marker_newline(&mut data);
        assert_eq!(data, b"hi");

        let mut data = b"hi\n".to_vec();
        strip_marker_newline(&mut data);
        assert_eq!(data, b"hi");

        let mut data = b"hi\n\n".to_vec();
        strip_marker_newline(&mut data);
        assert_eq!(data, b"hi\n");

        let mut data = Vec::new();
        strip_marker_newline(&mut data);
        assert!(data.is_empty());
    }

    #[test]
    fn base64_encodes_the_request_stdin() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"hello\n"), "aGVsbG8K");
        assert_eq!(base64_encode(b"\x00\xff\n"), "AP8K");
    }

    #[test]
    fn expand_reads_env_and_keeps_unknown() {
        std::env::set_var("ENO_SHELL_TEST_VAR", "abc");
        assert_eq!(expand("$ENO_SHELL_TEST_VAR/x"), "abc/x");
        assert_eq!(expand("${ENO_SHELL_TEST_VAR}y"), "abcy");
        assert_eq!(expand("$ENO_SHELL_MISSING/x"), "$ENO_SHELL_MISSING/x");
    }
}
