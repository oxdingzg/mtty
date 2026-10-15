//! Shell integration: the spawned shell reports its cwd (OSC 7), command
//! boundaries (OSC 133 C / D;exit) and history (via `mtty-cli`) without the
//! user editing any dotfiles. Each shell gets a shim that loads the user's own
//! startup files first, so their setup is untouched:
//!
//! - zsh: a `ZDOTDIR` whose `.zshenv` restores the real `ZDOTDIR` (Ghostty's
//!   approach).
//! - bash: `--rcfile` pointing at a script that sources `~/.bashrc` (what an
//!   interactive non-login bash reads anyway), then adds `PROMPT_COMMAND` and
//!   `PS0` (bash 4.4+) or a DEBUG trap (older bash, e.g. macOS's 3.2).
//! - fish: a `vendor_conf.d` script found through `XDG_DATA_DIRS`, which it
//!   restores (Kitty's approach).
//! - PowerShell: `-NoExit -Command` dot-sources a script after the profile; it
//!   wraps `prompt` and PSReadLine's `PSConsoleHostReadLine`, and records
//!   history through `mtty-cli` on PowerShell 7+ and Windows PowerShell 5.1.

use std::io;
use std::path::{Path, PathBuf};

// The host exports `MTTY_CLI` as an absolute path: inside an app bundle the
// CLI is not on `PATH`.
const ZSHENV: &str = r#"# mtty shell integration (zsh) — auto-generated, do not edit.
if [[ -n "${MTTY_ZDOTDIR_ORIG+x}" ]]; then
  export ZDOTDIR="$MTTY_ZDOTDIR_ORIG"
  unset MTTY_ZDOTDIR_ORIG
else
  unset ZDOTDIR
fi
[[ -r "${ZDOTDIR:-$HOME}/.zshenv" ]] && source "${ZDOTDIR:-$HOME}/.zshenv"

_mtty_osc7() { printf '\033]7;file://%s%s\033\\' "${HOST:-localhost}" "$PWD"; }
_mtty_meta() {
  local c=$1
  c=${c//\%/%25}; c=${c//$'\e'/%1B}; c=${c//$'\a'/%07}
  c=${c//$'\n'/%0A}; c=${c//$'\r'/%0D}
  printf '\033]133;E;%s\007' "$c"
}
# OSC 133 semantic prompts: C when a command starts, D;<exit> when it ends,
# so the terminal can tell a command's output apart from the prompt.
_mtty_precmd() {
  local s=$?
  printf '\033]133;D;%s\007' "$s"
  _mtty_osc7
}
_mtty_preexec() {
  _mtty_meta "$1"
  printf '\033]133;C\007'
  local cli="${MTTY_CLI:-mtty-cli}"
  command -v "$cli" >/dev/null 2>&1 || return 0
  ( "$cli" history add --command "$1" --cwd "$PWD" >/dev/null 2>&1 & )
}
autoload -Uz add-zsh-hook 2>/dev/null
if (( $+functions[add-zsh-hook] )); then
  add-zsh-hook chpwd _mtty_osc7
  add-zsh-hook preexec _mtty_preexec
fi
# First in line, so it sees the command's own exit status.
precmd_functions=(_mtty_precmd ${precmd_functions[@]})
_mtty_osc7
"#;

const BASH_RC: &str = r#"# mtty shell integration (bash) — auto-generated, do not edit.
# A ConPTY console starts on the system locale's code page; make it UTF-8 so
# programs that write UTF-8 bytes (Bun, native renderers) are not mojibake.
case "$(uname -s 2>/dev/null)" in
  MINGW*|MSYS*|CYGWIN*) chcp.com 65001 >/dev/null 2>&1 ;;
esac
# bash reads this instead of ~/.bashrc (--rcfile), so load that first.
[[ -r ~/.bashrc ]] && source ~/.bashrc
[[ $- == *i* ]] || return 0

_mtty_osc7() { printf '\e]7;file://%s%s\e\\' "${HOSTNAME:-localhost}" "$PWD"; }
_mtty_meta() {
  local c=$1
  c=${c//\%/%25}; c=${c//$'\e'/%1B}; c=${c//$'\a'/%07}
  c=${c//$'\n'/%0A}; c=${c//$'\r'/%0D}
  printf '\e]133;E;%s\a' "$c"
}
_mtty_hist() { local h; h=$(HISTTIMEFORMAT= builtin history 1); [[ $h =~ ^\ *([0-9]+)\*?\ +(.*)$ ]] && printf '%s\n%s' "${BASH_REMATCH[1]}" "${BASH_REMATCH[2]}"; }
_mtty_hist_last=$(_mtty_hist | head -n 1)
_mtty_precmd() {
  local s=$?
  local h n
  h=$(_mtty_hist)
  n=${h%%$'\n'*}
  if [[ -n $n && $n != "$_mtty_hist_last" ]]; then _mtty_meta "${h#*$'\n'}"; fi
  printf '\e]133;D;%s\a' "$s"
  _mtty_osc7
  if [[ -n $n && $n != "$_mtty_hist_last" ]]; then
    _mtty_hist_last=$n
    local cli="${MTTY_CLI:-mtty-cli}"
    command -v "$cli" >/dev/null 2>&1 &&
      ( "$cli" history add --command "${h#*$'\n'}" --cwd "$PWD" >/dev/null 2>&1 & )
  fi
  return $s
}
# First in line, so it sees the command's own exit status.
if [[ $(declare -p PROMPT_COMMAND 2>/dev/null) == "declare -a"* ]]; then
  PROMPT_COMMAND=(_mtty_precmd "${PROMPT_COMMAND[@]}")
else
  PROMPT_COMMAND="_mtty_precmd${PROMPT_COMMAND:+;$PROMPT_COMMAND}"
fi
if (( BASH_VERSINFO[0] > 4 || (BASH_VERSINFO[0] == 4 && BASH_VERSINFO[1] >= 4) )); then
  PS0+=$'\e]133;C\a'
else
  # No PS0: the DEBUG trap fires before each simple command; the flag set
  # last in PROMPT_COMMAND limits it to the first one of a command line.
  _mtty_ready=
  _mtty_debug() { [[ -n $_mtty_ready ]] || return 0; _mtty_ready=; printf '\e]133;C\a'; }
  trap '_mtty_debug' DEBUG
  PROMPT_COMMAND="$PROMPT_COMMAND;_mtty_ready=1"
fi
"#;

const FISH_CONF: &str = r#"# mtty shell integration (fish) — auto-generated, do not edit.
if set -q MTTY_FISH_XDG_DATA_DIRS
    if test -n "$MTTY_FISH_XDG_DATA_DIRS"
        set -gx XDG_DATA_DIRS (string split : -- $MTTY_FISH_XDG_DATA_DIRS)
    else
        set -e XDG_DATA_DIRS
    end
    set -e MTTY_FISH_XDG_DATA_DIRS
end
status is-interactive; or exit 0

function __mtty_osc7 --on-variable PWD
    printf '\e]7;file://%s%s\e\\' $hostname "$PWD"
end
function __mtty_preexec --on-event fish_preexec
    printf '\e]133;E;%s\a' (string escape --style=url -- "$argv[1]")
    printf '\e]133;C\a'
    set -l cli $MTTY_CLI
    test -n "$cli"; or set cli mtty-cli
    if command -q $cli
        command $cli history add --command "$argv[1]" --cwd "$PWD" >/dev/null 2>&1 &
        disown 2>/dev/null
    end
end
function __mtty_postexec --on-event fish_postexec
    printf '\e]133;D;%s\a' $status
end
__mtty_osc7
"#;

const PWSH_SCRIPT: &str = r#"# mtty shell integration (PowerShell) — auto-generated, do not edit.
# Make the pane's console UTF-8. A ConPTY starts on the system locale's code
# page, so native programs that write UTF-8 bytes (instead of going through
# WriteConsoleW) would otherwise show non-ASCII text as mojibake.
try {
    [Console]::InputEncoding = [System.Text.UTF8Encoding]::new($false)
    [Console]::OutputEncoding = [System.Text.UTF8Encoding]::new($false)
    $OutputEncoding = [System.Text.UTF8Encoding]::new($false)
} catch {}
if (Test-Path variable:global:__mttyLoaded) { return }
$global:__mttyLoaded = $true
$global:__mttyRan = $false
$global:__mttyOrigPrompt = $function:prompt
# Quote one argument for a native command line. Windows PowerShell 5.1 runs on
# .NET Framework, whose ProcessStartInfo has no ArgumentList, so the command
# line is built by hand with CommandLineToArgvW's rules: double the backslashes
# that precede a quote or end the argument, then wrap it in quotes.
function global:__mttyQuoteArg([string]$a) {
    if ($a.Length -gt 0 -and $a -notmatch '[\s"]') { return $a }
    $sb = [System.Text.StringBuilder]::new()
    [void]$sb.Append('"')
    $slashes = 0
    foreach ($c in $a.ToCharArray()) {
        if ($c -eq '\') { $slashes++; continue }
        if ($c -eq '"') {
            [void]$sb.Append('\', ($slashes * 2 + 1))
            [void]$sb.Append('"')
            $slashes = 0
        } else {
            if ($slashes -gt 0) { [void]$sb.Append('\', $slashes); $slashes = 0 }
            [void]$sb.Append($c)
        }
    }
    if ($slashes -gt 0) { [void]$sb.Append('\', ($slashes * 2)) }
    [void]$sb.Append('"')
    $sb.ToString()
}
# Record the typed line in mtty's history through `mtty-cli`, detached so the
# next prompt is not delayed. PowerShell 7 and later pass the arguments through
# ProcessStartInfo.ArgumentList; Windows PowerShell 5.1 falls back to a quoted
# command line.
function global:__mttyRecord([string]$line) {
    $cli = if ($env:MTTY_CLI) { $env:MTTY_CLI } else { 'mtty-cli' }
    $cmd = Get-Command $cli -CommandType Application -ErrorAction SilentlyContinue
    if (-not $cmd) { return }
    $exe = $cmd.Path
    if (-not $exe) { $exe = $cmd.Source }
    try {
        $psi = [System.Diagnostics.ProcessStartInfo]::new()
        $psi.FileName = $exe
        $psi.UseShellExecute = $false
        $psi.RedirectStandardOutput = $true
        $psi.RedirectStandardError = $true
        $argv = @('history', 'add', '--command', $line, '--cwd', $PWD.ProviderPath)
        if ($psi.PSObject.Properties['ArgumentList']) {
            foreach ($a in $argv) { $psi.ArgumentList.Add($a) }
        } else {
            $psi.Arguments = (($argv | ForEach-Object { __mttyQuoteArg $_ }) -join ' ')
        }
        [void][System.Diagnostics.Process]::Start($psi)
    } catch {}
}
function global:__mttyWrapReadLine {
    if ($global:__mttyOrigReadLine) { return }
    $rl = Get-Command PSConsoleHostReadLine -CommandType Function -ErrorAction SilentlyContinue
    if (-not $rl) { return }
    $global:__mttyOrigReadLine = $rl.ScriptBlock
    function global:PSConsoleHostReadLine {
        $line = & $global:__mttyOrigReadLine
        if ($line -and $line.Trim()) {
            [Console]::Write("$([char]27)]133;E;$([Uri]::EscapeDataString($line))$([char]7)")
            [Console]::Write("$([char]27)]133;C$([char]7)")
            $global:__mttyRan = $true
            __mttyRecord $line
        }
        $line
    }
}
function global:prompt {
    $ok = $?
    $code = if ($ok) { 0 } elseif ($global:LASTEXITCODE) { $global:LASTEXITCODE } else { 1 }
    $e = [char]27
    $out = ''
    if ($global:__mttyRan) {
        $out += "$e]133;D;$code$([char]7)"
        $global:__mttyRan = $false
    }
    $loc = $executionContext.SessionState.Path.CurrentLocation
    if ($loc.Provider.Name -eq 'FileSystem') {
        $p = $loc.ProviderPath -replace '\\', '/'
        if ($p -notmatch '^/') { $p = '/' + $p }
        $out += "$e]7;file://$([System.Net.Dns]::GetHostName())$p$e\"
    }
    __mttyWrapReadLine
    $out + (& $global:__mttyOrigPrompt)
}
"#;

/// Which integration a shell gets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Zsh,
    Bash,
    Fish,
    PowerShell,
    CommandPrompt,
}

/// The shell kind from its path (`/bin/zsh`, `C:\…\pwsh.exe`, …).
pub fn kind(shell: &str) -> Option<Kind> {
    let base = shell.rsplit(['/', '\\']).next().unwrap_or(shell);
    let base = base
        .strip_suffix(".exe")
        .unwrap_or(base)
        .to_ascii_lowercase();
    match base.as_str() {
        "zsh" => Some(Kind::Zsh),
        "bash" => Some(Kind::Bash),
        "fish" => Some(Kind::Fish),
        "pwsh" | "powershell" => Some(Kind::PowerShell),
        "cmd" => Some(Kind::CommandPrompt),
        _ => None,
    }
}

/// Finder launches with a minimal PATH. A login zsh loads macOS's path_helper
/// and the user's login profile before the interactive configuration.
pub fn startup_args(shell: &str) -> &'static [&'static str] {
    if cfg!(target_os = "macos") && kind(shell) == Some(Kind::Zsh) {
        return &["-l"];
    }
    // A ConPTY console starts on the system locale's code page (936, 932, …),
    // and a program that writes UTF-8 bytes without going through
    // `WriteConsoleW` — Bun and the native TUI renderers do — is shown there as
    // mojibake. Turn the pane's console into UTF-8 before the first prompt.
    if cfg!(windows) && kind(shell) == Some(Kind::CommandPrompt) {
        return &["/k", "chcp 65001>nul"];
    }
    &[]
}

/// What to add to a shell's command line and environment.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Integration {
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
}

/// A path inside a PowerShell single-quoted string.
fn ps_quote(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', "''"))
}

/// Per-user directory the shim is written to. Prefers `$XDG_RUNTIME_DIR` and
/// otherwise `$TMPDIR`, falling back to the system temp dir.
fn runtime_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR") {
        let path = PathBuf::from(dir);
        if path.is_absolute() {
            return path;
        }
    }
    std::env::temp_dir()
}

/// Create (or validate) a directory only the current user can access.
///
/// The shim is `source`d by every new shell, so a tamperable directory would
/// mean arbitrary code execution. We refuse anything that is not a real
/// directory or that grants group/other permissions.
#[cfg(unix)]
fn ensure_private_dir(dir: &Path) -> io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    match dir.symlink_metadata() {
        Ok(md) => {
            if !md.is_dir() || md.file_type().is_symlink() {
                return Err(io::Error::other("shim path is not a directory"));
            }
            if md.permissions().mode() & 0o077 != 0 {
                return Err(io::Error::other("shim directory is group/other accessible"));
            }
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            let mut builder = std::fs::DirBuilder::new();
            builder.recursive(true).mode(0o700);
            builder.create(dir)?;
        }
        Err(e) => return Err(e),
    }
    Ok(())
}

#[cfg(not(unix))]
fn ensure_private_dir(dir: &Path) -> io::Result<()> {
    std::fs::create_dir_all(dir)
}

fn shim_dir() -> Option<PathBuf> {
    let dir = runtime_dir().join("mtty-zdotdir");
    ensure_private_dir(&dir).ok()?;
    Some(dir)
}

/// A shell can source the shim while another pane is starting. Publishing a
/// complete replacement avoids truncating the shared file underneath a reader.
fn publish_shim(path: &Path, text: &str) -> io::Result<()> {
    use std::io::Write;
    use std::sync::atomic::{AtomicU64, Ordering};
    if std::fs::read(path).is_ok_and(|bytes| bytes == text.as_bytes()) {
        return Ok(());
    }
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let temp = path.with_extension(format!(
        "tmp-{}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos(),
        NEXT.fetch_add(1, Ordering::Relaxed),
    ));
    let result = (|| {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)?;
        file.write_all(text.as_bytes())?;
        drop(file);
        std::fs::rename(&temp, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result
}

/// Arguments and environment that turn on the integration for `shell`.
/// Unknown shells, or a shim that cannot be written safely, get nothing.
/// `env` is the environment the shell will see (for values to restore).
pub fn integration(shell: &str, env: &dyn Fn(&str) -> Option<String>) -> Integration {
    let Some(kind) = kind(shell) else {
        return Integration::default();
    };
    let Some(dir) = shim_dir() else {
        return Integration::default();
    };
    let write = |rel: &str, text: &str| -> Option<PathBuf> {
        let path = dir.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).ok()?;
        }
        publish_shim(&path, text).ok()?;
        Some(path)
    };
    let some = |name: &str| env(name).filter(|v| !v.is_empty());
    match kind {
        Kind::Zsh => {
            if write(".zshenv", ZSHENV).is_none() {
                return Integration::default();
            }
            let orig = some("ZDOTDIR").unwrap_or_else(|| env("HOME").unwrap_or_default());
            Integration {
                args: Vec::new(),
                env: vec![
                    ("ZDOTDIR".into(), dir.to_string_lossy().into_owned()),
                    ("MTTY_ZDOTDIR_ORIG".into(), orig),
                ],
            }
        }
        Kind::Bash => match write("bash/rc.bash", BASH_RC) {
            Some(rc) => Integration {
                args: vec!["--rcfile".into(), rc.to_string_lossy().into_owned()],
                env: Vec::new(),
            },
            None => Integration::default(),
        },
        Kind::Fish => {
            if write("fish/vendor_conf.d/mtty.fish", FISH_CONF).is_none() {
                return Integration::default();
            }
            let orig = some("XDG_DATA_DIRS");
            // fish falls back to these when XDG_DATA_DIRS is unset; keep
            // them so system vendor files still load.
            let rest = orig
                .clone()
                .unwrap_or_else(|| "/usr/local/share:/usr/share".into());
            Integration {
                args: Vec::new(),
                env: vec![
                    ("XDG_DATA_DIRS".into(), format!("{}:{rest}", dir.display())),
                    ("MTTY_FISH_XDG_DATA_DIRS".into(), orig.unwrap_or_default()),
                ],
            }
        }
        Kind::PowerShell => match write("pwsh/mtty.ps1", PWSH_SCRIPT) {
            Some(script) => Integration {
                args: vec![
                    "-NoLogo".into(),
                    "-NoExit".into(),
                    "-Command".into(),
                    format!(". {}", ps_quote(&script)),
                ],
                env: Vec::new(),
            },
            None => Integration::default(),
        },
        // cmd.exe has no integration, but it does get the UTF-8 console code
        // page through `startup_args`.
        Kind::CommandPrompt => Integration::default(),
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn temp_path() -> PathBuf {
        static N: AtomicU32 = AtomicU32::new(0);
        std::env::temp_dir().join(format!(
            "mtty-shim-test-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ))
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn login_zsh_loads_profile_path_with_integration() {
        let base = temp_path();
        let home = base.join("home");
        let shim = base.join("shim");
        std::fs::create_dir_all(home.join("bin")).unwrap();
        ensure_private_dir(&shim).unwrap();
        std::fs::write(shim.join(".zshenv"), ZSHENV).unwrap();
        std::fs::write(home.join(".zprofile"), "export PATH=\"$HOME/bin:$PATH\"\n").unwrap();
        let executable = home.join("bin/mtty-test-node");
        std::fs::write(&executable, "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        let output = std::process::Command::new("/bin/zsh")
            .args(startup_args("/bin/zsh"))
            .args(["-ic", "command -v mtty-test-node"])
            .env_clear()
            .env("HOME", &home)
            .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
            .env("ZDOTDIR", &shim)
            .env("MTTY_ZDOTDIR_ORIG", &home)
            .output()
            .unwrap();
        let _ = std::fs::remove_dir_all(&base);
        assert!(output.status.success(), "{:?}", output);
        assert!(
            String::from_utf8_lossy(&output.stdout).contains(executable.to_string_lossy().as_ref())
        );
    }

    #[test]
    fn recognises_cmd_for_the_code_page() {
        assert_eq!(
            kind(r"C:\Windows\System32\cmd.exe"),
            Some(Kind::CommandPrompt)
        );
        // A non-cmd shell keeps an empty argument line on every platform.
        assert!(startup_args("/bin/fish").is_empty());
    }

    #[test]
    fn concurrent_shim_publication_never_exposes_partial_scripts() {
        use std::sync::{Arc, Barrier};
        let dir = temp_path();
        ensure_private_dir(&dir).unwrap();
        let path = dir.join("script.sh");
        let first = Arc::new(format!("# first\n{}\n# end\n", "x".repeat(16_384)));
        let second = Arc::new(format!("# second\n{}\n# end\n", "y".repeat(16_384)));
        publish_shim(&path, &first).unwrap();
        let barrier = Arc::new(Barrier::new(5));
        let mut writers = Vec::new();
        for i in 0..4 {
            let path = path.clone();
            let text = if i % 2 == 0 {
                first.clone()
            } else {
                second.clone()
            };
            let barrier = barrier.clone();
            writers.push(std::thread::spawn(move || {
                barrier.wait();
                for _ in 0..128 {
                    publish_shim(&path, &text).unwrap();
                }
            }));
        }
        barrier.wait();
        for _ in 0..1024 {
            let actual = std::fs::read_to_string(&path).unwrap();
            assert!(
                actual == *first || actual == *second,
                "reader saw an incomplete shim"
            );
        }
        for writer in writers {
            writer.join().unwrap();
        }
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn creates_a_private_dir() {
        let base = temp_path();
        let dir = base.join("nested/shim");
        ensure_private_dir(&dir).unwrap();
        let mode = dir.symlink_metadata().unwrap().permissions().mode();
        assert_eq!(
            mode & 0o077,
            0,
            "shim dir must not be group/other accessible"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn rejects_a_world_accessible_dir() {
        let dir = temp_path();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(ensure_private_dir(&dir).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// Checks on the generated shim that need no shell installed, so they run on
/// every platform, Windows included.
#[cfg(test)]
mod script_tests {
    use super::*;

    #[test]
    fn powershell_records_history_without_a_version_gate() {
        // PowerShell 7+ has `ProcessStartInfo.ArgumentList`; Windows PowerShell
        // 5.1 runs on .NET Framework, which does not. Both paths must exist and
        // the history call must not be gated on the PowerShell version.
        assert!(PWSH_SCRIPT.contains("$psi.ArgumentList.Add($a)"));
        assert!(PWSH_SCRIPT.contains("$psi.Arguments"));
        assert!(PWSH_SCRIPT.contains("__mttyQuoteArg"));
        assert!(PWSH_SCRIPT.contains("'history', 'add', '--command', $line"));
        assert!(!PWSH_SCRIPT.contains("PSVersionTable.PSVersion.Major -ge 7"));
    }

    #[test]
    fn powershell_quotes_native_args_with_command_line_to_argv_rules() {
        // Backslashes before a quote and at the end are doubled, so a typed
        // command containing quotes survives Windows' command-line parsing.
        assert!(PWSH_SCRIPT.contains("$slashes * 2 + 1"));
        assert!(PWSH_SCRIPT.contains("$slashes * 2))"));
        assert!(PWSH_SCRIPT.contains("__mttyQuoteArg $_"));
    }

    #[test]
    fn powershell_keeps_the_semantic_prompt_marks() {
        assert!(PWSH_SCRIPT.contains("]133;C"));
        assert!(PWSH_SCRIPT.contains("]133;D;"));
    }

    #[test]
    fn ps_quote_escapes_single_quotes() {
        assert_eq!(ps_quote(Path::new("/tmp/plain")), "'/tmp/plain'");
        assert_eq!(ps_quote(Path::new("/tmp/a b")), "'/tmp/a b'");
        assert_eq!(ps_quote(Path::new("/tmp/it's")), "'/tmp/it''s'");
    }
}
