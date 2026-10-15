//! SSH integration (ADR 0014): target parsing, `ssh -G` resolution,
//! ControlMaster connection reuse, and a remote terminfo bootstrap with a
//! zero-install fallback.

/// How the pane's local shell reads a typed command line. ssh commands are
/// typed into the pane's shell, so their arguments are quoted for it: POSIX
/// shells take single quotes; cmd.exe neither knows single quotes nor `;` and
/// `clear`, and PowerShell has its own rules.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Syntax {
    Posix,
    Cmd,
    PowerShell,
}

impl Syntax {
    /// The syntax of a shell program, from its file name.
    pub fn of_shell(path: &str) -> Self {
        let base = path
            .rsplit(['/', '\\'])
            .next()
            .unwrap_or(path)
            .to_ascii_lowercase();
        let base = base.strip_suffix(".exe").unwrap_or(&base);
        match base {
            "cmd" => Syntax::Cmd,
            "pwsh" | "powershell" => Syntax::PowerShell,
            _ => Syntax::Posix,
        }
    }

    /// The syntax of the shell new panes run (mtty-core's default shell:
    /// `COMSPEC` on Windows, else a POSIX `$SHELL`).
    pub fn local() -> Self {
        if cfg!(windows) {
            Self::of_shell(&std::env::var("COMSPEC").unwrap_or_else(|_| "powershell.exe".into()))
        } else {
            Syntax::Posix
        }
    }

    /// One argument for this shell.
    pub fn quote(self, s: &str) -> String {
        match self {
            Syntax::Posix => shell_quote(s),
            Syntax::PowerShell => {
                let plain = !s.is_empty()
                    && s.chars()
                        .all(|c| c.is_ascii_alphanumeric() || "-_./:=+,".contains(c));
                if plain {
                    s.to_string()
                } else {
                    format!("'{}'", s.replace('\'', "''"))
                }
            }
            Syntax::Cmd => {
                let plain = !s.is_empty()
                    && !s
                        .chars()
                        .any(|c| c.is_whitespace() || "\"&|<>^()%!,;=".contains(c));
                if plain {
                    return s.to_string();
                }
                // The MSVCRT rules the target program parses with: backslashes
                // are literal unless they precede a quote.
                let mut out = String::from("\"");
                let mut slashes = 0;
                for c in s.chars() {
                    match c {
                        '\\' => slashes += 1,
                        '"' => {
                            out.push_str(&"\\".repeat(slashes * 2 + 1));
                            out.push('"');
                            slashes = 0;
                            continue;
                        }
                        _ => {
                            out.push_str(&"\\".repeat(slashes));
                            slashes = 0;
                            out.push(c);
                            continue;
                        }
                    }
                }
                out.push_str(&"\\".repeat(slashes * 2));
                out.push('"');
                out
            }
        }
    }

    /// `cmd` as typed into a pane: the screen is cleared first so the pane
    /// starts with the remote session (POSIX: a leading space also keeps it
    /// out of history).
    pub fn typed(self, cmd: &str) -> String {
        match self {
            Syntax::Posix => format!(" clear; {cmd}\r"),
            Syntax::Cmd => format!("cls & {cmd}\r"),
            Syntax::PowerShell => format!("clear; {cmd}\r"),
        }
    }
}

/// ssh's shared-connection options. Windows' OpenSSH has no ControlMaster
/// (it needs Unix sockets), so none there.
pub(crate) fn reuse_options() -> Vec<String> {
    if cfg!(windows) {
        return Vec::new();
    }
    [
        "-o".to_string(),
        "ControlMaster=auto".to_string(),
        "-o".to_string(),
        format!("ControlPath={}", control_path()),
        "-o".to_string(),
        "ControlPersist=60s".to_string(),
    ]
    .into()
}

/// A user-typed target: `[user@]host[:port]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub host: String,
    pub user: Option<String>,
    pub port: Option<u16>,
}

impl Target {
    pub fn parse(input: &str) -> Option<Self> {
        let input = input.trim();
        if input.is_empty() {
            return None;
        }
        let (user, rest) = match input.split_once('@') {
            Some((u, r)) if !u.is_empty() => (Some(u.to_string()), r),
            _ => (None, input),
        };
        // Split host/port, keeping `[ipv6]:port` intact.
        let (host, port) = if let Some(stripped) = rest.strip_prefix('[') {
            match stripped.split_once(']') {
                Some((h, tail)) => (
                    format!("[{h}]"),
                    tail.strip_prefix(':').and_then(|p| p.parse().ok()),
                ),
                None => return None,
            }
        } else if let Some((h, p)) = rest.rsplit_once(':') {
            match p.parse::<u16>() {
                Ok(port) => (h.to_string(), Some(port)),
                Err(_) => (rest.to_string(), None),
            }
        } else {
            (rest.to_string(), None)
        };
        if host.is_empty() {
            return None;
        }
        Some(Target { host, user, port })
    }

    /// `[user@]host[:port]` as ssh accepts it on the command line.
    pub fn destination(&self) -> String {
        match &self.user {
            Some(user) => format!("{user}@{}", self.host),
            None => self.host.clone(),
        }
    }
}

/// The subset of `ssh -G` output we use.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Resolved {
    pub hostname: Option<String>,
    pub user: Option<String>,
    pub port: Option<u16>,
}

/// Parse `ssh -G <host>` output (lowercased `key value` lines).
pub fn parse_ssh_g(text: &str) -> Resolved {
    let mut out = Resolved::default();
    for line in text.lines() {
        let Some((key, value)) = line.split_once(char::is_whitespace) else {
            continue;
        };
        let value = value.trim();
        match key.trim().to_ascii_lowercase().as_str() {
            "hostname" if !value.is_empty() => out.hostname = Some(value.to_string()),
            "user" if !value.is_empty() => out.user = Some(value.to_string()),
            "port" => out.port = value.parse().ok(),
            _ => {}
        }
    }
    out
}

/// A new SSH session for user input: the tab title and the command to type
/// into a fresh pane. The target is passed to ssh exactly as typed: ssh
/// applies `~/.ssh/config` itself, and options under `Host <alias>` only match
/// the alias, never the resolved hostname. Nothing here blocks on a process
/// beyond a cached local `infocmp`.
pub fn session_command(input: &str) -> Option<(String, String)> {
    let target = Target::parse(input)?;
    let cmd = command(&target, &bootstrap("xterm-256color"));
    Some((target.destination(), cmd))
}

/// The command for a saved host (B3.1): connection reuse, the host's own
/// options (`-p`, `-J`), the destination and the terminfo bootstrap.
pub fn host_command(destination: &str, options: &[String]) -> String {
    host_command_with(
        Syntax::local(),
        destination,
        options,
        &bootstrap("xterm-256color"),
    )
}

/// Reuse the exact connection options of a plain SSH command, changing only
/// its bootstrap's initial directory. tmux/mosh reconnect commands retain
/// their own session semantics and are returned unchanged.
pub fn command_in_directory(command: &str, directory: Option<&str>) -> String {
    let Some(directory) = directory.filter(|dir| dir.starts_with('/')) else {
        return command.to_owned();
    };
    let boot = bootstrap("xterm-256color");
    let Some(body) = boot
        .strip_prefix("sh -c '")
        .and_then(|body| body.strip_suffix('\''))
    else {
        return command.to_owned();
    };
    let body = body.replacen(
        "export TERM=",
        &format!("cd {} || exit; export TERM=", shell_quote(directory)),
        1,
    );
    let new_boot = format!("sh -c {}", shell_quote(&body));
    let syntax = Syntax::local();
    let result = command.replacen(&syntax.quote(&boot), &syntax.quote(&new_boot), 1);
    if result.len() <= 1000 {
        result
    } else {
        command.to_owned()
    }
}

fn host_command_with(syn: Syntax, destination: &str, options: &[String], boot: &str) -> String {
    let mut cmd = String::from("ssh -t");
    for arg in reuse_options().iter().chain(options) {
        cmd.push(' ');
        cmd.push_str(
            &if arg.contains('%') || arg.starts_with('-') && arg.len() == 2 {
                arg.clone()
            } else {
                syn.quote(arg)
            },
        );
    }
    cmd.push(' ');
    cmd.push_str(&syn.quote(destination));
    cmd.push(' ');
    cmd.push_str(&syn.quote(boot));
    cmd
}

/// A tmux session name mtty will put on a command line.
pub fn valid_session_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// The bootstrap, ending in `tmux new-session -A` (attach or create) instead
/// of a plain login shell when the host has tmux.
pub fn with_tmux(bootstrap: &str, session: &str) -> String {
    const SHELL: &str = "exec ${SHELL:-sh} -l";
    if !valid_session_name(session) {
        return bootstrap.to_string();
    }
    bootstrap.replacen(
        SHELL,
        &format!(
            "T=$(PATH=$PATH:/usr/local/bin:/opt/homebrew/bin command -v tmux); \
             if [ -n \"$T\" ]; then exec \"$T\" new-session -A -s {session}; else {SHELL}; fi"
        ),
        1,
    )
}

/// Whether a program is on PATH.
pub fn on_path(program: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| {
        let file = dir.join(program);
        file.is_file() || (cfg!(windows) && dir.join(format!("{program}.exe")).is_file())
    })
}

/// A saved host's command with its persistent-session choices (B3.6): the
/// shell inside a tmux session, and/or mosh instead of ssh. mosh takes the
/// ssh options through `--ssh` and runs the bootstrap as its remote command.
pub fn persistent_host_command(
    destination: &str,
    options: &[String],
    tmux: Option<&str>,
    mosh: bool,
) -> String {
    let mut boot = bootstrap("xterm-256color");
    if let Some(session) = tmux {
        boot = with_tmux(&boot, session);
    }
    // mosh has no native Windows client, and the command below relies on a
    // POSIX shell splitting the bootstrap after `--`, which cmd does not do:
    // on Windows the session uses ssh (tmux still keeps it alive).
    if !mosh || cfg!(windows) {
        return host_command_with(Syntax::local(), destination, options, &boot);
    }
    let mut cmd = String::from("mosh");
    if !options.is_empty() {
        let ssh: Vec<String> = std::iter::once("ssh".to_string())
            .chain(options.iter().map(|o| shell_quote(o)))
            .collect();
        cmd.push_str(&format!(" --ssh={}", shell_quote(&ssh.join(" "))));
    }
    cmd.push(' ');
    cmd.push_str(&shell_quote(destination));
    // The bootstrap is `sh -c '…'`: after `--` the local shell splits it into
    // the argv mosh-server runs.
    cmd.push_str(" -- ");
    cmd.push_str(&boot);
    cmd
}

/// Resolve a target through the user's ssh config, falling back to the typed
/// values when `ssh -G` is unavailable. Runs a process: keep it off the UI
/// thread, and connect with the typed target (see [`session_command`]).
pub fn resolve(target: &Target) -> Target {
    let output = mtty_platform::background_command("ssh")
        .args(["-G", &target.destination()])
        .output();
    let Ok(output) = output else {
        return target.clone();
    };
    if !output.status.success() {
        return target.clone();
    }
    let resolved = parse_ssh_g(&String::from_utf8_lossy(&output.stdout));
    Target {
        host: resolved.hostname.unwrap_or_else(|| target.host.clone()),
        user: resolved.user.or_else(|| target.user.clone()),
        port: resolved.port.or(target.port),
    }
}

/// The shared ControlMaster socket, using ssh's `%` tokens.
pub fn control_path() -> String {
    "~/.ssh/mtty-cm-%r@%h:%p".to_string()
}

/// Build the `ssh` invocation: connection reuse plus the remote bootstrap.
pub fn command(target: &Target, bootstrap: &str) -> String {
    let options: Vec<String> = target
        .port
        .map(|p| vec!["-p".to_string(), p.to_string()])
        .unwrap_or_default();
    host_command_with(Syntax::local(), &target.destination(), &options, bootstrap)
}

/// The remote bootstrap: install the local terminfo entry if the host lacks it,
/// else fall back to `xterm-256color`, then start a login shell.
pub fn bootstrap_with(terminfo_b64: Option<&str>, term: &str) -> String {
    let report = "printf \"\\033]7;file://mtty-remote%s\\033\\\\\" \"$(printf %s \"$PWD\" | sed \"s/%/%25/g\")\"";
    match terminfo_b64 {
        Some(b64) => format!(
            "sh -c 'if ! infocmp {term} >/dev/null 2>&1; then mkdir -p ~/.terminfo && \
             echo {b64} | base64 -d | tic -x -o ~/.terminfo - >/dev/null 2>&1; fi; \
             export TERM={term}; {report}; exec ${{SHELL:-sh}} -l'"
        ),
        None => format!("sh -c 'export TERM=xterm-256color; {report}; exec ${{SHELL:-sh}} -l'"),
    }
}

/// Compute the bootstrap, embedding the local terminfo entry when it is small.
///
/// A PTY input line is capped by the kernel (`MAX_CANON`, ~1 KB on macOS), so a
/// large base64 blob on the command line would be truncated and leave the shell
/// with an unterminated quote. Only embed short entries; otherwise fall back to
/// the plain `xterm-256color` bootstrap.
pub fn bootstrap(term: &str) -> String {
    const MAX_EMBED: usize = 384;
    // The local terminfo entry does not change while we run: look it up once.
    static CACHE: std::sync::Mutex<Vec<(String, Option<String>)>> =
        std::sync::Mutex::new(Vec::new());
    let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    let b64 = match cache.iter().find(|(t, _)| t == term) {
        Some((_, b64)) => b64.clone(),
        None => {
            let entry = mtty_platform::background_command("infocmp")
                .args(["-x", term])
                .output()
                .ok();
            let b64 = entry
                .filter(|o| o.status.success() && !o.stdout.is_empty())
                .map(|o| base64_encode(&o.stdout))
                .filter(|s| s.len() <= MAX_EMBED);
            cache.push((term.to_string(), b64.clone()));
            b64
        }
    };
    bootstrap_with(b64.as_deref(), term)
}

/// Standard base64 (RFC 4648) with padding.
pub fn base64_encode(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(TABLE[(n >> 18) as usize & 63] as char);
        out.push(TABLE[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            TABLE[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

/// Cap on how much of a remote file we read into the reader.
pub const MAX_REMOTE_BYTES: u64 = 2 * 1024 * 1024;

/// The `ssh` argv for reading a remote file (`cat`), reusing the ControlMaster.
pub fn read_args(dest: &str, path: &str) -> Vec<String> {
    let mut args = base_args();
    args.push(dest.to_string());
    args.push(format!(
        "head -c {} -- {}",
        MAX_REMOTE_BYTES + 1,
        shell_quote(path)
    ));
    args
}

/// The `ssh` argv for writing a remote file (`cat >`), reusing the ControlMaster.
pub fn write_args(dest: &str, path: &str) -> Vec<String> {
    let mut args = base_args();
    args.push(dest.to_string());
    args.push(format!("cat > {}", shell_quote(path)));
    args
}

fn base_args() -> Vec<String> {
    let mut args = vec!["-o".to_string(), "BatchMode=yes".to_string()];
    args.extend(reuse_options());
    args
}

/// Read a remote file over ssh (bounded). Errors carry the ssh stderr.
pub fn read_remote(dest: &str, path: &str) -> std::io::Result<Vec<u8>> {
    let out = mtty_platform::background_command("ssh")
        .args(read_args(dest, path))
        .output()?;
    if !out.status.success() {
        return Err(std::io::Error::other(
            String::from_utf8_lossy(&out.stderr).trim().to_string(),
        ));
    }
    if out.stdout.len() > MAX_REMOTE_BYTES as usize {
        return Err(std::io::Error::other(
            "remote file exceeds the editable size limit",
        ));
    }
    Ok(out.stdout)
}

/// The `ssh` argv for probing a remote file's length and mtime, reusing the
/// ControlMaster. GNU `stat -c` first, BSD/macOS `stat -f` as the fallback.
pub fn stat_args(dest: &str, path: &str) -> Vec<String> {
    let q = shell_quote(path);
    let mut args = base_args();
    args.push(dest.to_string());
    args.push(format!(
        "stat -c '%s %Y' -- {q} 2>/dev/null || stat -f '%z %m' -- {q}"
    ));
    args
}

/// Parse `len mtime` from a `stat` probe (bytes and whole seconds since the
/// epoch); `None` on anything else.
pub fn parse_stat(text: &str) -> Option<(u64, i64)> {
    let mut fields = text.split_whitespace();
    let len = fields.next()?.parse().ok()?;
    let mtime = fields.next()?.parse().ok()?;
    Some((len, mtime))
}

/// A remote file's length and modification time over ssh, so an editor pane
/// can notice external changes without reading the file. Errors carry the
/// ssh stderr; callers treat a failure as a silent no-op.
pub fn stat_remote(dest: &str, path: &str) -> std::io::Result<(u64, i64)> {
    let out = mtty_platform::background_command("ssh")
        .args(stat_args(dest, path))
        .output()?;
    if !out.status.success() {
        return Err(std::io::Error::other(
            String::from_utf8_lossy(&out.stderr).trim().to_string(),
        ));
    }
    parse_stat(&String::from_utf8_lossy(&out.stdout))
        .ok_or_else(|| std::io::Error::other("unexpected stat output"))
}

/// Write a remote file over ssh (`cat >`), feeding `data` on stdin.
pub fn write_remote(dest: &str, path: &str, data: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut child = mtty_platform::background_command("ssh")
        .args(write_args(dest, path))
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()?;
    if let Some(stdin) = child.stdin.as_mut() {
        stdin.write_all(data)?;
    }
    let out = child.wait_with_output()?;
    if !out.status.success() {
        return Err(std::io::Error::other(
            String::from_utf8_lossy(&out.stderr).trim().to_string(),
        ));
    }
    Ok(())
}

/// A content-checked, same-directory replacement. The original stays intact
/// if transfer or decoding fails. This is optimistic concurrency: external
/// writers do not share a lock, so a comparison cannot be an atomic CAS.
fn checked_write_script(path: &str, expected: &[u8], data: &[u8]) -> String {
    format!(
        "set -eu\np={}\ncase $p in /*) ;; *) p=./$p ;; esac\n\
         links=0\nwhile [ -L \"$p\" ]; do\n\
         links=$((links + 1)); [ $links -le 40 ] || exit 1\n\
         link=$(readlink \"$p\"); case $link in /*) p=$link ;; *) p=$(dirname \"$p\")/$link ;; esac\n\
         done\n\
         dir=$(dirname \"$p\")\n\
         tmp=$(mktemp \"$dir/.mtty-save.XXXXXX\")\n\
         old=$(mktemp \"$dir/.mtty-base.XXXXXX\")\n\
         trap 'rm -f \"$tmp\" \"$old\"' EXIT HUP INT TERM\n\
         if base64 -d </dev/null >/dev/null 2>&1; then decode=-d; else decode=-D; fi\n\
         base64 $decode >\"$old\" <<'MTTY_BASE'\n{}\nMTTY_BASE\n\
         if ! cmp -s \"$p\" \"$old\"; then echo 'remote file changed; review before saving' >&2; exit 73; fi\n\
         cp -p \"$p\" \"$tmp\"\n\
         base64 $decode >\"$tmp\" <<'MTTY_DATA'\n{}\nMTTY_DATA\n\
         if ! cmp -s \"$p\" \"$old\"; then echo 'remote file changed during save' >&2; exit 73; fi\n\
         mv -f \"$tmp\" \"$p\"\n",
        shell_quote(path), base64_encode(expected), base64_encode(data),
    )
}

pub fn write_remote_checked(
    dest: &str,
    path: &str,
    expected: &[u8],
    data: &[u8],
) -> std::io::Result<()> {
    use std::io::Write;
    let mut args = base_args();
    args.extend([dest.to_owned(), "sh -s".to_owned()]);
    let mut child = mtty_platform::background_command("ssh")
        .args(args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()?;
    let transfer = child
        .stdin
        .take()
        .unwrap()
        .write_all(checked_write_script(path, expected, data).as_bytes());
    let out = child.wait_with_output()?;
    if !out.status.success() {
        let kind = if out.status.code() == Some(73) {
            std::io::ErrorKind::AlreadyExists
        } else {
            std::io::ErrorKind::Other
        };
        return Err(std::io::Error::new(
            kind,
            String::from_utf8_lossy(&out.stderr).trim().to_owned(),
        ));
    }
    transfer
}

pub fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    #[test]
    fn bootstrap_fits_a_pty_line() {
        let b = super::bootstrap("xterm-256color");
        let t = super::Target::parse("dingzg@host").unwrap();
        let cmd = super::command(&t, &b);
        assert!(
            cmd.len() < 400,
            "ssh command too long for a PTY input line: {} bytes",
            cmd.len()
        );
    }

    use super::*;

    #[test]
    fn host_commands_quote_options_and_destination() {
        let opts = ["-p".into(), "2222".into(), "-J".into(), "bastion".into()];
        let cmd = host_command_with(Syntax::Posix, "deploy@203.0.113.7", &opts, "sh -c 'x'");
        assert!(
            cmd.contains(r#" -p '2222' -J 'bastion' 'deploy@203.0.113.7' 'sh -c '\''x'\'''"#),
            "{cmd}"
        );
        // Windows' OpenSSH has no ControlMaster.
        assert_eq!(
            host_command("h", &[]).contains("ControlMaster=auto"),
            !cfg!(windows)
        );
        let boot = "sh -c 'exec ${SHELL:-sh} -l'";
        let win = host_command_with(Syntax::Cmd, "administrator@127.0.0.1", &opts, boot);
        assert!(
            win.ends_with(
                r#" -p 2222 -J bastion administrator@127.0.0.1 "sh -c 'exec ${SHELL:-sh} -l'""#
            ),
            "{win}"
        );
    }

    #[test]
    fn persistent_sessions_use_tmux_and_mosh() {
        let plain = host_command("h", &[]);
        assert_eq!(persistent_host_command("h", &[], None, false), plain);
        let tmux = persistent_host_command("h", &["-p".into(), "2222".into()], Some("mtty"), false);
        assert!(
            tmux.starts_with("ssh -t") && tmux.contains("new-session -A -s mtty"),
            "{tmux}"
        );
        assert!(
            tmux.contains("else exec ${SHELL:-sh} -l; fi"),
            "falls back without tmux"
        );
        // A name that is not safe on a command line is ignored.
        let bad = persistent_host_command("h", &[], Some("x; rm -rf ~"), false);
        assert_eq!(bad, plain);
        let mosh = persistent_host_command(
            "deploy@h",
            &["-p".into(), "2222".into()],
            Some("mtty"),
            true,
        );
        assert!(mosh.len() < 1000, "fits a PTY input line");
        if cfg!(windows) {
            assert!(mosh.starts_with("ssh "), "{mosh}");
            assert!(mosh.contains("new-session -A -s mtty"), "{mosh}");
        } else {
            assert!(mosh.starts_with("mosh --ssh="), "{mosh}");
            assert!(mosh.contains(" 'deploy@h' -- sh -c '"), "{mosh}");
        }
    }

    /// The tmux bootstrap really runs: with a fake `tmux` on PATH it is
    /// called as `new-session -A -s mtty`; without one, the login shell runs.
    #[test]
    #[cfg(unix)]
    fn tmux_bootstrap_runs_in_sh() {
        let dir = std::env::temp_dir().join(format!("mtty-tmux-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let fake = dir.join("tmux");
        std::fs::write(&fake, "#!/bin/sh\necho \"tmux $*\"\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        let boot = with_tmux(&bootstrap_with(None, "xterm-256color"), "mtty");
        let script = boot
            .strip_prefix("sh -c '")
            .unwrap()
            .strip_suffix('\'')
            .unwrap();
        let run = |path: &str| {
            let out = mtty_platform::background_command("/bin/sh")
                .args(["-c", script])
                .env("PATH", path)
                .env("SHELL", "/bin/echo")
                .output()
                .unwrap();
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };
        assert!(run(&format!("{}:/usr/bin:/bin", dir.display()))
            .ends_with("tmux new-session -A -s mtty"));
        // The fallback: PATH is an empty directory (the script needs only
        // builtins); the bootstrap also looks in Homebrew's directories.
        let empty = dir.join("empty");
        std::fs::create_dir_all(&empty).unwrap();
        let installed = ["/usr/local/bin/tmux", "/opt/homebrew/bin/tmux"]
            .iter()
            .any(|p| std::path::Path::new(p).exists());
        if !installed {
            assert!(run(&empty.to_string_lossy()).ends_with("-l"));
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn arguments_are_quoted_for_the_local_shell() {
        assert_eq!(
            Syntax::of_shell(r"C:\Windows\system32\cmd.exe"),
            Syntax::Cmd
        );
        assert_eq!(Syntax::of_shell("pwsh"), Syntax::PowerShell);
        assert_eq!(Syntax::of_shell("powershell.exe"), Syntax::PowerShell);
        assert_eq!(Syntax::of_shell("/bin/zsh"), Syntax::Posix);
        // cmd.exe: bare when safe, otherwise MSVCRT double-quote rules.
        assert_eq!(Syntax::Cmd.quote("user@host"), "user@host");
        assert_eq!(Syntax::Cmd.quote("a b"), r#""a b""#);
        assert_eq!(Syntax::Cmd.quote(r#"say "hi""#), r#""say \"hi\"""#);
        assert_eq!(Syntax::Cmd.quote(r"C:\dir\"), r"C:\dir\");
        assert_eq!(Syntax::Cmd.quote(r"C:\my dir\"), r#""C:\my dir\\""#);
        assert_eq!(Syntax::Cmd.quote(""), r#""""#);
        // PowerShell: single quotes, doubled inside.
        assert_eq!(Syntax::PowerShell.quote("it's here"), "'it''s here'");
        assert_eq!(Syntax::PowerShell.quote("-p"), "-p");
        // What is typed into a pane.
        assert_eq!(Syntax::Cmd.typed("ssh h"), "cls & ssh h\r");
        assert_eq!(Syntax::PowerShell.typed("ssh h"), "clear; ssh h\r");
        assert_eq!(Syntax::Posix.typed("ssh h"), " clear; ssh h\r");
        if !cfg!(windows) {
            assert_eq!(Syntax::local(), Syntax::Posix);
        }
    }

    #[test]
    fn sessions_connect_with_the_typed_alias() {
        // `Host work` options (IdentityFile, ProxyJump, …) only apply when ssh
        // is given the alias itself, so it must not be replaced by a hostname.
        let (title, cmd) = session_command("deploy@work:2200").unwrap();
        assert_eq!(title, "deploy@work");
        let syn = Syntax::local();
        assert!(
            cmd.contains(&format!(" -p {} ", syn.quote("2200"))),
            "{cmd}"
        );
        assert!(cmd.contains(&syn.quote("deploy@work")), "{cmd}");
        assert!(session_command("  ").is_none());
    }

    #[test]
    fn parses_targets() {
        assert_eq!(
            Target::parse("alice@example.com"),
            Some(Target {
                host: "example.com".into(),
                user: Some("alice".into()),
                port: None
            })
        );
        assert_eq!(
            Target::parse("example.com:2222"),
            Some(Target {
                host: "example.com".into(),
                user: None,
                port: Some(2222)
            })
        );
        assert_eq!(Target::parse("[::1]:2200").unwrap().port, Some(2200));
        assert!(Target::parse("  ").is_none());
    }

    #[test]
    fn parses_ssh_g() {
        let text = "hostname example.com\nuser bob\nport 2222\ncontrolmaster auto\n";
        let r = parse_ssh_g(text);
        assert_eq!(r.hostname.as_deref(), Some("example.com"));
        assert_eq!(r.user.as_deref(), Some("bob"));
        assert_eq!(r.port, Some(2222));
    }

    #[test]
    fn builds_command_with_reuse_and_bootstrap() {
        let t = Target {
            host: "h".into(),
            user: Some("u".into()),
            port: Some(2200),
        };
        let cmd = command(&t, "sh -c 'x'");
        assert!(cmd.starts_with("ssh -t "));
        assert_eq!(cmd.contains("ControlMaster=auto"), !cfg!(windows));
        let syn = Syntax::local();
        assert!(
            cmd.contains(&format!(" -p {} ", syn.quote("2200"))),
            "{cmd}"
        );
        assert!(cmd.contains(&syn.quote("u@h")), "{cmd}");
    }

    #[test]
    fn bootstrap_variants() {
        assert!(bootstrap_with(None, "mtty").contains("xterm-256color"));
        let with = bootstrap_with(Some("QUJD"), "mtty");
        assert!(with.contains("base64 -d"));
        assert!(with.contains("QUJD"));
        assert!(with.contains("export TERM=mtty"));
    }

    #[test]
    fn remote_args_reuse_control_master_and_quote() {
        let r = read_args("u@h", "/tmp/a b.txt");
        assert!(r.iter().any(|a| a == "BatchMode=yes"));
        // Windows' OpenSSH has no ControlMaster.
        assert_eq!(
            r.iter().any(|a| a.starts_with("ControlPath=")),
            !cfg!(windows)
        );
        assert_eq!(
            r.last().unwrap(),
            &format!("head -c {} -- '/tmp/a b.txt'", super::MAX_REMOTE_BYTES + 1)
        );
        let w = write_args("u@h", "x");
        assert_eq!(w.last().unwrap(), "cat > 'x'");
    }

    #[test]
    fn stat_args_quote_the_path_and_fall_back_to_bsd_stat() {
        let s = stat_args("u@h", "/tmp/a b.txt");
        let cmd = s.last().unwrap();
        assert!(cmd.contains("stat -c '%s %Y' -- '/tmp/a b.txt'"), "{cmd}");
        assert!(cmd.contains("stat -f '%z %m' -- '/tmp/a b.txt'"), "{cmd}");
    }

    #[test]
    fn parse_stat_reads_length_and_mtime_or_nothing() {
        assert_eq!(parse_stat("123 1700000000\n"), Some((123, 1_700_000_000)));
        assert_eq!(parse_stat("  42\t7  "), Some((42, 7)));
        assert_eq!(parse_stat(""), None);
        assert_eq!(parse_stat("not numbers"), None);
        assert_eq!(parse_stat("12"), None);
    }

    #[test]
    fn base64_matches_known_vectors() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    #[cfg(unix)]
    fn checked_save_executes_without_truncating_conflicts_and_preserves_symlinks() {
        use std::io::Write;
        use std::os::unix::fs::{symlink, PermissionsExt};
        let dir = std::env::temp_dir().join(format!("mtty-checked-save-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("a ' quoted.txt");
        std::fs::write(&file, b"original\0bytes").unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o640)).unwrap();
        let link = dir.join("link.txt");
        symlink(file.file_name().unwrap(), &link).unwrap();
        let run = |expected: &[u8], data: &[u8]| {
            let mut child = std::process::Command::new("/bin/sh")
                .stdin(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .unwrap();
            child
                .stdin
                .take()
                .unwrap()
                .write_all(checked_write_script(&link.to_string_lossy(), expected, data).as_bytes())
                .unwrap();
            child.wait_with_output().unwrap()
        };
        assert_eq!(run(b"stale", b"must not overwrite").status.code(), Some(73));
        assert_eq!(std::fs::read(&file).unwrap(), b"original\0bytes");
        let out = run(b"original\0bytes", b"saved\0new\n");
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(std::fs::read(&file).unwrap(), b"saved\0new\n");
        assert!(std::fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(
            std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
            0o640
        );
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 2);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    #[cfg(unix)]
    fn split_bootstrap_preserves_options_and_really_changes_quoted_directory() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("mtty-ssh-dir-{}", std::process::id()));
        let cwd = dir.join("space ' quote % percent");
        std::fs::create_dir_all(&cwd).unwrap();
        let ssh = dir.join("ssh");
        std::fs::write(
            &ssh,
            "#!/bin/sh\nfor arg do last=$arg; done\nexec /bin/sh -c \"$last\"\n",
        )
        .unwrap();
        let shell = dir.join("shell");
        std::fs::write(&shell, "#!/bin/sh\npwd\n").unwrap();
        for file in [&ssh, &shell] {
            std::fs::set_permissions(file, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let cmd = host_command("example.invalid", &["-p".into(), "2222".into()]);
        let changed = command_in_directory(&cmd, Some(&cwd.to_string_lossy()));
        assert!(changed.contains(" -p '2222' "));
        let out = std::process::Command::new("/bin/sh")
            .args(["-c", &changed])
            .env("PATH", format!("{}:/usr/bin:/bin", dir.display()))
            .env("SHELL", &shell)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let text = String::from_utf8(out.stdout).unwrap();
        assert!(text.contains("file://mtty-remote"));
        assert!(text.contains("%25 percent"));
        assert!(text.ends_with(&format!("{}\n", cwd.display())), "{text}");
        assert_eq!(command_in_directory(&cmd, None), cmd);
        let tmux = persistent_host_command("example.invalid", &[], Some("mtty"), false);
        assert_eq!(command_in_directory(&tmux, Some("/tmp")), tmux);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
