use anyhow::{Context, Result, bail};
use std::ops::Range;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

// =====================================================================
// Wrapper / completion content
// =====================================================================

const MARKER_BEGIN: &str = "# >>> ji shell integration >>>";
const MARKER_END: &str = "# <<< ji shell integration <<<";
const MANAGED_HEADER: &str = "# ji-managed: do not edit";

const ZSH_WRAPPER: &str = r#"if command -v ji >/dev/null 2>&1; then
    ji() {
        local directive_file exit_code=0
        directive_file="$(mktemp)"

        JI_DIRECTIVE_FILE="$directive_file" command ji "$@" || exit_code=$?

        if [[ -s "$directive_file" ]]; then
            source "$directive_file"
            local directive_status=$?
            if [[ $exit_code -eq 0 ]]; then
                exit_code=$directive_status
            fi
        fi

        rm -f "$directive_file"
        return "$exit_code"
    }
fi
"#;

const BASH_WRAPPER: &str = r#"if command -v ji >/dev/null 2>&1; then
    ji() {
        local directive_file exit_code=0
        directive_file="$(mktemp)"

        JI_DIRECTIVE_FILE="$directive_file" command ji "$@" || exit_code=$?

        if [[ -s "$directive_file" ]]; then
            source "$directive_file"
            local directive_status=$?
            if [[ $exit_code -eq 0 ]]; then
                exit_code=$directive_status
            fi
        fi

        rm -f "$directive_file"
        return "$exit_code"
    }
fi
"#;

const FISH_WRAPPER: &str = r#"function ji --description 'ji workspace switcher (with cd directive support)'
    set -l directive_file (mktemp)

    JI_DIRECTIVE_FILE=$directive_file command ji $argv
    set -l exit_code $status

    if test -s $directive_file
        eval (string collect <$directive_file)
        set -l directive_status $status
        if test $exit_code -eq 0
            set exit_code $directive_status
        end
    end

    command rm -f $directive_file
    return $exit_code
end
"#;

const SUPPORTED_SHELLS: &str = "zsh, bash, fish";

fn shell_kind(shell: &str) -> Result<clap_complete::Shell> {
    match shell {
        "zsh" => Ok(clap_complete::Shell::Zsh),
        "bash" => Ok(clap_complete::Shell::Bash),
        "fish" => Ok(clap_complete::Shell::Fish),
        other => bail!("unsupported shell: {other} (supported: {SUPPORTED_SHELLS})"),
    }
}

fn wrapper_script(shell: &str) -> Result<&'static str> {
    match shell {
        "zsh" => Ok(ZSH_WRAPPER),
        "bash" => Ok(BASH_WRAPPER),
        "fish" => Ok(FISH_WRAPPER),
        other => bail!("unsupported shell: {other} (supported: {SUPPORTED_SHELLS})"),
    }
}

fn completion_script(shell: &str, cmd: &mut clap::Command) -> Result<String> {
    let kind = shell_kind(shell)?;
    let mut buf: Vec<u8> = Vec::new();
    clap_complete::generate(kind, cmd, "ji", &mut buf);
    let script = String::from_utf8(buf).context("clap_complete generated invalid UTF-8")?;
    // zsh `compdef` only exists after `autoload -U compinit && compinit` has run.
    // Guard so users without compinit get the wrapper but silently skip completion
    // instead of an error at shell startup.
    if shell == "zsh" {
        Ok(format!(
            "if (( ${{+functions[compdef]}} )); then\n{script}\nfi\n"
        ))
    } else {
        Ok(script)
    }
}

/// What `ji config shell init <shell>` streams to stdout.
pub fn print_init(shell: &str, cmd: &mut clap::Command) -> Result<()> {
    let body = render_managed_body(shell, cmd)?;
    print!("{body}");
    Ok(())
}

/// Detect the current shell from the SHELL environment variable.
pub fn detect_shell() -> Result<String> {
    let shell_path = std::env::var("SHELL").context("SHELL not set — pass the shell explicitly")?;
    let shell_name = Path::new(&shell_path)
        .file_name()
        .and_then(|s| s.to_str())
        .context("could not parse SHELL")?
        .to_string();
    Ok(shell_name)
}

pub fn write_directive_cd(path: &Path) -> Result<()> {
    let directive_file = std::env::var("JI_DIRECTIVE_FILE")
        .context("JI_DIRECTIVE_FILE not set — did you run `ji config shell install`?")?;
    if directive_file.is_empty() {
        return Ok(());
    }
    let escaped = path.display().to_string().replace('\'', "'\\''");
    let cmd = format!("cd '{escaped}'\n");
    std::fs::write(&directive_file, cmd)
        .with_context(|| format!("failed to write directive to {directive_file}"))?;
    Ok(())
}

// =====================================================================
// Environment
// =====================================================================

#[derive(Debug, Clone)]
pub struct ShellEnv {
    pub home: PathBuf,
    pub xdg_config_home: PathBuf,
    pub zdotdir: PathBuf,
    pub zsh_custom: Option<PathBuf>,
    pub omz_root: Option<PathBuf>,
}

impl ShellEnv {
    pub fn from_process_env() -> Result<Self> {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .context("HOME not set")?;
        let xdg_config_home = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".config"));
        let zdotdir = std::env::var_os("ZDOTDIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.clone());
        let omz_root = std::env::var_os("ZSH").map(PathBuf::from).or_else(|| {
            let p = home.join(".oh-my-zsh");
            p.is_dir().then_some(p)
        });
        let zsh_custom = std::env::var_os("ZSH_CUSTOM")
            .map(PathBuf::from)
            .or_else(|| omz_root.as_ref().map(|r| r.join("custom")));
        Ok(Self {
            home,
            xdg_config_home,
            zdotdir,
            zsh_custom,
            omz_root,
        })
    }

    fn managed_file(&self, shell: &str) -> Result<PathBuf> {
        let name = match shell {
            "zsh" => "init.zsh",
            "bash" => "init.bash",
            other => bail!("managed_file is only defined for zsh and bash, got: {other}"),
        };
        Ok(self.xdg_config_home.join("ji").join(name))
    }
}

// =====================================================================
// Options
// =====================================================================

#[derive(Debug, Default, Clone, Copy)]
pub struct InstallOpts {
    pub dry_run: bool,
    pub force: bool,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct UninstallOpts {
    pub dry_run: bool,
    pub force: bool,
}

// =====================================================================
// Line classification
// =====================================================================

fn strip_inline_comment(s: &str) -> &str {
    let bytes = s.as_bytes();
    let mut i = 0;
    let mut in_single = false;
    let mut in_double = false;
    while i < bytes.len() {
        let b = bytes[i];
        if !in_double && b == b'\'' {
            in_single = !in_single;
        } else if !in_single && b == b'"' {
            in_double = !in_double;
        } else if !in_single
            && !in_double
            && b == b'#'
            && (i == 0 || bytes[i - 1].is_ascii_whitespace())
        {
            return s[..i].trim_end();
        }
        i += 1;
    }
    s.trim_end()
}

/// Heuristic detector for "this line installs ji shell integration". Used only
/// to refuse on cross-file hits. Conservative — requires the line to start with
/// one of a small set of invocation tokens, ruling out comments, aliases, echo
/// statements, etc.
fn is_legacy_line(line: &str) -> bool {
    let line = strip_inline_comment(line.trim_start());
    if line.is_empty() || line.starts_with('#') {
        return false;
    }
    let mentions_init = line.contains("ji config shell init")
        || line.contains("/ji/init.zsh")
        || line.contains("/ji/init.bash")
        || line.contains("/ji/init.fish");
    if !mentions_init {
        return false;
    }
    let first = line.split_whitespace().next().unwrap_or("");
    matches!(
        first,
        "eval" | "source" | "." | "if" | "command" | "type" | "hash" | "ji" | "[" | "[["
    )
}

// =====================================================================
// Sourced-file follow-up
// =====================================================================

#[derive(Debug, Clone)]
enum SourceArg {
    Resolved(Vec<PathBuf>),
    Unresolved(String),
}

fn extract_source_args(env: &ShellEnv, base_dir: &Path, line: &str) -> Vec<SourceArg> {
    let line = strip_inline_comment(line);
    let trimmed = line.trim_start();
    if trimmed.is_empty() || trimmed.starts_with('#') {
        return Vec::new();
    }

    if let Some(arg) = extract_for_loop_source(trimmed) {
        return vec![match resolve_path(env, base_dir, &arg) {
            Some(p) => SourceArg::Resolved(expand_glob(&p)),
            None => SourceArg::Unresolved(arg),
        }];
    }

    let work = strip_test_guard(trimmed);
    let work = strip_if_then(work);
    let work = strip_logical_guard(work);

    let arg = match extract_source_directive_arg(work.trim_start()) {
        Some(a) => a,
        None => return Vec::new(),
    };

    vec![match resolve_path(env, base_dir, &arg) {
        Some(p) => SourceArg::Resolved(vec![p]),
        None => SourceArg::Unresolved(arg),
    }]
}

fn strip_if_then(line: &str) -> &str {
    let rest = match line.strip_prefix("if ") {
        Some(r) => r,
        None => return line,
    };
    let (_, body) = match rest.split_once(';') {
        Some(p) => p,
        None => return line,
    };
    let body = body.trim_start();
    let body = body
        .strip_prefix("then ")
        .or_else(|| body.strip_prefix("then\t"))
        .unwrap_or(body);
    let body = body.trim_start();
    body.rsplit_once(';').map(|(s, _)| s.trim()).unwrap_or(body)
}

fn strip_test_guard(line: &str) -> &str {
    if let Some(idx) = line.find("&&") {
        let head = line[..idx].trim();
        if head.starts_with('[') && head.ends_with(']') {
            return line[idx + 2..].trim_start();
        }
    }
    line
}

fn strip_logical_guard(line: &str) -> &str {
    if let Some(idx) = line.find("&&") {
        let head = line[..idx].trim();
        if (head.starts_with("command -v")
            || head.starts_with("type ")
            || head.starts_with("hash "))
            && head.contains("ji")
        {
            return line[idx + 2..].trim_start();
        }
    }
    line
}

fn extract_source_directive_arg(s: &str) -> Option<String> {
    let rest = if let Some(r) = s.strip_prefix(". ").or_else(|| s.strip_prefix(".\t")) {
        r.trim_start()
    } else if let Some(r) = s
        .strip_prefix("source ")
        .or_else(|| s.strip_prefix("source\t"))
    {
        r.trim_start()
    } else {
        return None;
    };
    let arg = match rest.chars().next()? {
        '"' => {
            let end = rest[1..].find('"')?;
            rest[1..1 + end].to_string()
        }
        '\'' => {
            let end = rest[1..].find('\'')?;
            rest[1..1 + end].to_string()
        }
        _ => rest
            .split_whitespace()
            .next()?
            .trim_end_matches(';')
            .to_string(),
    };
    Some(arg)
}

fn extract_for_loop_source(line: &str) -> Option<String> {
    let rest = line.strip_prefix("for ")?;
    let (_var, rest) = rest.split_once(" in ")?;
    let (glob_part, do_part) = rest.split_once(';')?;
    let glob = glob_part.trim();
    if !do_part.contains("source")
        && !do_part.contains(". \"")
        && !do_part.trim_start().starts_with(". ")
    {
        return None;
    }
    Some(glob.to_string())
}

fn resolve_path(env: &ShellEnv, base_dir: &Path, raw: &str) -> Option<PathBuf> {
    let mut s = raw.to_string();
    if let Some(rest) = s.strip_prefix("~/") {
        s = env.home.join(rest).to_string_lossy().into_owned();
    } else if s == "~" {
        s = env.home.to_string_lossy().into_owned();
    }

    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '$' {
            out.push(c);
            continue;
        }
        let var = if chars.peek() == Some(&'{') {
            chars.next();
            let mut buf = String::new();
            for c in chars.by_ref() {
                if c == '}' {
                    break;
                }
                buf.push(c);
            }
            buf
        } else {
            let mut buf = String::new();
            while let Some(&c) = chars.peek() {
                if c.is_alphanumeric() || c == '_' {
                    buf.push(c);
                    chars.next();
                } else {
                    break;
                }
            }
            buf
        };
        let value = lookup_var(env, &var)?;
        out.push_str(&value);
    }

    if out.contains('$') {
        return None;
    }

    let p = PathBuf::from(&out);
    if p.is_absolute() {
        Some(p)
    } else {
        Some(base_dir.join(p))
    }
}

fn lookup_var(env: &ShellEnv, raw: &str) -> Option<String> {
    let (name, default) = if let Some(idx) = raw.find(":-") {
        (&raw[..idx], Some(&raw[idx + 2..]))
    } else if let Some(idx) = raw.find('-') {
        (&raw[..idx], Some(&raw[idx + 1..]))
    } else {
        (raw, None)
    };
    let resolved = match name {
        "HOME" => Some(env.home.to_string_lossy().into_owned()),
        "ZDOTDIR" => Some(env.zdotdir.to_string_lossy().into_owned()),
        "XDG_CONFIG_HOME" => Some(env.xdg_config_home.to_string_lossy().into_owned()),
        "ZSH" => env
            .omz_root
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned()),
        "ZSH_CUSTOM" => env
            .zsh_custom
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned()),
        _ => None,
    };
    if let Some(v) = resolved {
        return Some(v);
    }
    if let Some(d) = default {
        let mut expanded = String::new();
        let mut chars = d.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '$' {
                let var = if chars.peek() == Some(&'{') {
                    chars.next();
                    let mut buf = String::new();
                    for c in chars.by_ref() {
                        if c == '}' {
                            break;
                        }
                        buf.push(c);
                    }
                    buf
                } else {
                    let mut buf = String::new();
                    while let Some(&c) = chars.peek() {
                        if c.is_alphanumeric() || c == '_' {
                            buf.push(c);
                            chars.next();
                        } else {
                            break;
                        }
                    }
                    buf
                };
                let v = lookup_var(env, &var)?;
                expanded.push_str(&v);
            } else {
                expanded.push(c);
            }
        }
        return Some(expanded);
    }
    None
}

fn expand_glob(pattern: &Path) -> Vec<PathBuf> {
    let s = pattern.to_string_lossy().into_owned();
    let star_idx = match s.find('*') {
        Some(i) => i,
        None => return vec![pattern.to_path_buf()],
    };
    let dir_end = s[..star_idx].rfind('/').unwrap_or(0);
    let dir = if dir_end == 0 {
        PathBuf::from("/")
    } else {
        PathBuf::from(&s[..dir_end])
    };
    let seg_end = s[star_idx..]
        .find('/')
        .map(|i| star_idx + i)
        .unwrap_or(s.len());
    let segment = &s[dir_end + 1..seg_end];
    let trailing = &s[seg_end..];

    let (prefix, suffix) = match segment.split_once('*') {
        Some(p) => p,
        None => return Vec::new(),
    };

    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(_) => return Vec::new(),
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if name_str.starts_with(prefix) && name_str.ends_with(suffix) {
            let mut p = dir.join(name_str.as_ref());
            if !trailing.is_empty() {
                let tail = trailing.strip_prefix('/').unwrap_or(trailing);
                if !tail.is_empty() {
                    p = p.join(tail);
                }
            }
            out.push(p);
        }
    }
    out
}

// =====================================================================
// Scan + hit detection
// =====================================================================

#[derive(Debug, Clone)]
struct Hit {
    path: PathBuf,
    line_no: usize,
    line_text: String,
    kind: HitKind,
    via: Option<(PathBuf, usize)>,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
enum HitKind {
    /// A complete `# >>> … <<<` marker block; byte range covers both marker lines.
    MarkerBlock {
        byte_range: Range<usize>,
    },
    /// A non-marker line that mentions ji integration via eval/source/etc.
    Legacy,
    MalformedMarker,
    UnresolvedSource(String),
}

#[derive(Debug, Clone)]
struct ScanLocation {
    path: PathBuf,
    via: Option<(PathBuf, usize)>,
}

fn scan_locations(env: &ShellEnv, shell: &str, primary: &Path) -> Vec<ScanLocation> {
    let mut out = Vec::new();
    let mut push = |p: PathBuf, via: Option<(PathBuf, usize)>| {
        if !out.iter().any(|l: &ScanLocation| l.path == p) {
            out.push(ScanLocation { path: p, via });
        }
    };

    push(primary.to_path_buf(), None);

    match shell {
        "zsh" => {
            for name in [
                ".zshenv",
                ".zprofile",
                ".zlogin",
                ".zshrc.local",
                ".zshrc.custom",
                ".zshrc.custom.zsh",
            ] {
                push(env.zdotdir.join(name), None);
            }
            if let Some(zc) = &env.zsh_custom
                && let Ok(entries) = std::fs::read_dir(zc)
            {
                for e in entries.flatten() {
                    let p = e.path();
                    if p.extension().and_then(|s| s.to_str()) == Some("zsh") {
                        push(p, None);
                    }
                }
            }
        }
        "bash" => {
            for name in [
                ".bashrc",
                ".bash_profile",
                ".bash_login",
                ".profile",
                ".bashrc.local",
            ] {
                push(env.home.join(name), None);
            }
            let d = env.home.join(".bashrc.d");
            if let Ok(entries) = std::fs::read_dir(&d) {
                for e in entries.flatten() {
                    let p = e.path();
                    if p.extension().and_then(|s| s.to_str()) == Some("sh") {
                        push(p, None);
                    }
                }
            }
        }
        "fish" => {
            push(env.xdg_config_home.join("fish/config.fish"), None);
            let conf_d = env.xdg_config_home.join("fish/conf.d");
            if let Ok(entries) = std::fs::read_dir(&conf_d) {
                for e in entries.flatten() {
                    let p = e.path();
                    if p.extension().and_then(|s| s.to_str()) == Some("fish") {
                        push(p, None);
                    }
                }
            }
        }
        _ => {}
    }

    if matches!(shell, "zsh" | "bash") && primary.is_file() {
        let contents = std::fs::read_to_string(primary).unwrap_or_default();
        let base_dir = primary
            .parent()
            .unwrap_or_else(|| Path::new("/"))
            .to_path_buf();
        for (i, line) in contents.lines().enumerate() {
            for arg in extract_source_args(env, &base_dir, line) {
                if let SourceArg::Resolved(paths) = arg {
                    for p in paths {
                        if p.is_file() {
                            push(p, Some((primary.to_path_buf(), i + 1)));
                        }
                    }
                }
            }
        }
    }

    out
}

fn find_hits(locations: &[ScanLocation], env: &ShellEnv) -> Result<Vec<Hit>> {
    let mut out = Vec::new();
    for loc in locations {
        if !loc.path.is_file() {
            continue;
        }
        let contents = std::fs::read_to_string(&loc.path)
            .with_context(|| format!("failed to read {}", loc.path.display()))?;
        let marker_ranges = find_marker_blocks(&loc.path, &contents, &mut out, loc.via.clone());
        let primary_dir = loc
            .path
            .parent()
            .unwrap_or_else(|| Path::new("/"))
            .to_path_buf();
        scan_for_legacy(
            env,
            &loc.path,
            &contents,
            &marker_ranges,
            &primary_dir,
            loc.via.clone(),
            &mut out,
        );
    }
    Ok(out)
}

fn find_marker_blocks(
    path: &Path,
    contents: &str,
    out: &mut Vec<Hit>,
    via: Option<(PathBuf, usize)>,
) -> Vec<Range<usize>> {
    let mut ranges = Vec::new();
    let mut begin: Option<(usize, usize)> = None;
    let mut byte = 0usize;
    let mut malformed_double_begin = false;
    for (i, line) in contents.split_inclusive('\n').enumerate() {
        let line_trim = line.trim_end_matches('\n');
        if line_trim == MARKER_BEGIN {
            if let Some((_, prev_line)) = begin {
                out.push(Hit {
                    path: path.to_path_buf(),
                    line_no: prev_line,
                    line_text: line_trim.to_string(),
                    kind: HitKind::MalformedMarker,
                    via: via.clone(),
                });
                malformed_double_begin = true;
            }
            begin = Some((byte, i + 1));
        } else if line_trim == MARKER_END {
            match begin.take() {
                Some((start, start_line)) => {
                    let end = byte + line.len();
                    if !malformed_double_begin {
                        ranges.push(start..end);
                        out.push(Hit {
                            path: path.to_path_buf(),
                            line_no: start_line,
                            line_text: MARKER_BEGIN.to_string(),
                            kind: HitKind::MarkerBlock {
                                byte_range: start..end,
                            },
                            via: via.clone(),
                        });
                    }
                    malformed_double_begin = false;
                }
                None => {
                    out.push(Hit {
                        path: path.to_path_buf(),
                        line_no: i + 1,
                        line_text: line_trim.to_string(),
                        kind: HitKind::MalformedMarker,
                        via: via.clone(),
                    });
                }
            }
        }
        byte += line.len();
    }
    if let Some((_, start_line)) = begin {
        out.push(Hit {
            path: path.to_path_buf(),
            line_no: start_line,
            line_text: MARKER_BEGIN.to_string(),
            kind: HitKind::MalformedMarker,
            via,
        });
    }
    ranges
}

fn scan_for_legacy(
    env: &ShellEnv,
    path: &Path,
    contents: &str,
    marker_ranges: &[Range<usize>],
    base_dir: &Path,
    via: Option<(PathBuf, usize)>,
    out: &mut Vec<Hit>,
) {
    let mut byte = 0usize;
    for (i, line) in contents.split_inclusive('\n').enumerate() {
        let in_marker = marker_ranges
            .iter()
            .any(|r| byte >= r.start && byte < r.end);
        if !in_marker {
            let line_trim = line.trim_end_matches('\n');
            if is_legacy_line(line_trim) {
                out.push(Hit {
                    path: path.to_path_buf(),
                    line_no: i + 1,
                    line_text: line_trim.to_string(),
                    kind: HitKind::Legacy,
                    via: via.clone(),
                });
            } else {
                for arg in extract_source_args(env, base_dir, line_trim) {
                    if let SourceArg::Unresolved(raw) = arg {
                        out.push(Hit {
                            path: path.to_path_buf(),
                            line_no: i + 1,
                            line_text: line_trim.to_string(),
                            kind: HitKind::UnresolvedSource(raw),
                            via: via.clone(),
                        });
                    }
                }
            }
        }
        byte += line.len();
    }
}

// =====================================================================
// Managed-body rendering
// =====================================================================

fn render_managed_body(shell: &str, cmd: &mut clap::Command) -> Result<String> {
    let wrapper = wrapper_script(shell)?;
    let completion = completion_script(shell, cmd)?;
    Ok(format!("{MANAGED_HEADER}\n{wrapper}\n{completion}"))
}

fn render_fish_wrapper_body() -> String {
    format!("{MANAGED_HEADER}\n{FISH_WRAPPER}")
}

fn render_fish_completion_body(cmd: &mut clap::Command) -> Result<String> {
    let completion = completion_script("fish", cmd)?;
    Ok(format!("{MANAGED_HEADER}\n{completion}"))
}

fn render_rc_stanza(shell: &str) -> Result<String> {
    let init = match shell {
        "zsh" => "init.zsh",
        "bash" => "init.bash",
        other => bail!("render_rc_stanza is only defined for zsh and bash, got: {other}"),
    };
    Ok(format!(
        "{MARKER_BEGIN}\n{MANAGED_HEADER}. Run `ji config shell uninstall` to remove.\n[ -f \"${{XDG_CONFIG_HOME:-$HOME/.config}}/ji/{init}\" ] \\\n    && . \"${{XDG_CONFIG_HOME:-$HOME/.config}}/ji/{init}\"\n{MARKER_END}\n",
    ))
}

fn is_managed(s: &str) -> bool {
    s.contains(MANAGED_HEADER)
}

// =====================================================================
// Atomic write
// =====================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WriteOutcome {
    Created,
    Updated,
    Unchanged,
}

fn write_atomic(path: &Path, contents: &str) -> Result<WriteOutcome> {
    let target = if path.exists() {
        path.canonicalize()
            .with_context(|| format!("failed to canonicalize {}", path.display()))?
    } else {
        path.to_path_buf()
    };

    if let Ok(existing) = std::fs::read_to_string(&target)
        && existing == contents
    {
        return Ok(WriteOutcome::Unchanged);
    }

    let mode = std::fs::metadata(&target).ok().map(|m| {
        use std::os::unix::fs::PermissionsExt;
        m.permissions().mode()
    });
    let mode = mode.unwrap_or(0o644) & 0o7777;

    let parent = target
        .parent()
        .with_context(|| format!("{} has no parent", target.display()))?;
    if !parent.exists() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let pid = std::process::id();
    let file_name = target
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let tmp = parent.join(format!(".{file_name}.ji.tmp.{pid}"));

    if tmp.exists() {
        let _ = std::fs::remove_file(&tmp);
    }

    let outcome = if target.exists() {
        WriteOutcome::Updated
    } else {
        WriteOutcome::Created
    };

    {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .open(&tmp)
            .with_context(|| format!("failed to create {}", tmp.display()))?;
        f.write_all(contents.as_bytes())
            .with_context(|| format!("failed to write {}", tmp.display()))?;
        f.sync_all().ok();
    }

    std::fs::rename(&tmp, &target)
        .with_context(|| format!("failed to rename {} -> {}", tmp.display(), target.display()))?;

    if let Ok(dir_f) = std::fs::File::open(parent) {
        let _ = dir_f.sync_all();
    }

    Ok(outcome)
}

// =====================================================================
// Primary targets
// =====================================================================

fn zsh_primary(env: &ShellEnv) -> PathBuf {
    env.zdotdir.join(".zshrc")
}

fn bash_primary(env: &ShellEnv) -> PathBuf {
    for name in [".bash_profile", ".bash_login", ".profile"] {
        let p = env.home.join(name);
        if p.exists() {
            return p;
        }
    }
    env.home.join(".bash_profile")
}

fn fish_primary(env: &ShellEnv) -> PathBuf {
    env.xdg_config_home.join("fish/functions/ji.fish")
}

fn fish_completions_path(env: &ShellEnv) -> PathBuf {
    env.xdg_config_home.join("fish/completions/ji.fish")
}

/// Any file at this path would be auto-sourced by fish at shell start and
/// would define `ji` before our lazy `functions/ji.fish` ever loads —
/// silently shadowing us. Treated as a hard conflict at install time.
fn fish_shadow_path(env: &ShellEnv) -> PathBuf {
    env.xdg_config_home.join("fish/conf.d/ji.fish")
}

// =====================================================================
// chezmoi / dotfile-manager detection
// =====================================================================

fn chezmoi_strong_signal(canonical: &Path) -> bool {
    let parents = canonical.ancestors().skip(1).take(2);
    for p in parents {
        for marker in [
            "chezmoi.toml",
            ".chezmoiroot",
            ".chezmoiignore",
            ".chezmoiversion",
        ] {
            if p.join(marker).exists() {
                return true;
            }
        }
    }
    false
}

fn dotfile_manager_soft_signal(env: &ShellEnv, canonical: &Path) -> bool {
    if !canonical.starts_with(&env.home) {
        return true;
    }
    if let Some(parent) = canonical.parent()
        && let Ok(entries) = std::fs::read_dir(parent)
    {
        for e in entries.flatten() {
            let n = e.file_name();
            let s = n.to_string_lossy();
            if s.starts_with("dot_") || s.starts_with("private_") || s.starts_with("symlink_") {
                return true;
            }
        }
    }
    false
}

// =====================================================================
// Block manipulation
// =====================================================================

fn append_block(rc_contents: &str, stanza: &str) -> String {
    let mut out = rc_contents.to_string();
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    if !out.is_empty() && !out.ends_with("\n\n") {
        out.push('\n');
    }
    out.push_str(stanza);
    out
}

fn replace_block(rc_contents: &str, byte_range: &Range<usize>, stanza: &str) -> String {
    let mut out = String::with_capacity(rc_contents.len());
    out.push_str(&rc_contents[..byte_range.start]);
    out.push_str(stanza);
    out.push_str(&rc_contents[byte_range.end..]);
    out
}

fn remove_block(rc_contents: &str, byte_range: &Range<usize>) -> String {
    let mut out = String::with_capacity(rc_contents.len());
    let start = byte_range.start;
    let mut real_start = start;
    let mut end = byte_range.end;
    let before = &rc_contents[..start];
    if before.ends_with("\n\n") {
        real_start -= 1;
    }
    let after = &rc_contents[end..];
    if after.starts_with('\n') && before.ends_with("\n\n") {
        end += 1;
    }
    out.push_str(&rc_contents[..real_start]);
    out.push_str(&rc_contents[end..]);
    out
}

// =====================================================================
// Install
// =====================================================================

pub fn install(
    env: &ShellEnv,
    shell: &str,
    cmd: &mut clap::Command,
    opts: InstallOpts,
) -> Result<()> {
    match shell {
        "zsh" | "bash" => install_posix(env, shell, cmd, opts),
        "fish" => install_fish(env, cmd, opts),
        other => bail!("unsupported shell: {other} (supported: {SUPPORTED_SHELLS})"),
    }
}

fn install_posix(
    env: &ShellEnv,
    shell: &str,
    cmd: &mut clap::Command,
    opts: InstallOpts,
) -> Result<()> {
    let primary = if shell == "zsh" {
        zsh_primary(env)
    } else {
        bash_primary(env)
    };

    if primary.exists() {
        let canonical = primary
            .canonicalize()
            .with_context(|| format!("failed to canonicalize {}", primary.display()))?;
        if chezmoi_strong_signal(&canonical) && !opts.force {
            bail!(
                "(ji)::shell: {} resolves into a chezmoi source directory ({}); edits will be reverted by `chezmoi apply`. Pass --force to install anyway.",
                primary.display(),
                canonical.display(),
            );
        }
        if dotfile_manager_soft_signal(env, &canonical) {
            eprintln!(
                "(ji)::shell: note: {} resolves to {} (outside $HOME or dotfile-manager-shaped layout); edits may be reverted by your dotfile tool.",
                primary.display(),
                canonical.display(),
            );
        }
        let md = std::fs::metadata(&primary)
            .with_context(|| format!("failed to stat {}", primary.display()))?;
        if md.permissions().readonly() {
            bail!("(ji)::shell: {} is not writable", primary.display());
        }
    }

    let locations = scan_locations(env, shell, &primary);
    let hits = find_hits(&locations, env)?;

    let malformed: Vec<&Hit> = hits
        .iter()
        .filter(|h| matches!(h.kind, HitKind::MalformedMarker))
        .collect();
    if !malformed.is_empty() && !opts.force {
        let first = malformed[0];
        bail!(
            "(ji)::shell: malformed marker block in {}:{} — edit manually or pass --force to overwrite",
            first.path.display(),
            first.line_no,
        );
    }

    let primary_marker = hits
        .iter()
        .find(|h| h.path == primary && matches!(h.kind, HitKind::MarkerBlock { .. }));
    let primary_legacy = hits
        .iter()
        .find(|h| h.path == primary && matches!(h.kind, HitKind::Legacy));
    let nonprimary_hits: Vec<&Hit> = hits
        .iter()
        .filter(|h| {
            h.path != primary && matches!(h.kind, HitKind::MarkerBlock { .. } | HitKind::Legacy)
        })
        .collect();
    let unresolved: Vec<&Hit> = hits
        .iter()
        .filter(|h| matches!(h.kind, HitKind::UnresolvedSource(_)))
        .collect();

    for u in &unresolved {
        if let HitKind::UnresolvedSource(raw) = &u.kind {
            eprintln!(
                "(ji)::shell: note: unresolved source-like line in {}:{} (`{}`)",
                u.path.display(),
                u.line_no,
                raw,
            );
        }
    }

    if !nonprimary_hits.is_empty() && !opts.force {
        let first = nonprimary_hits[0];
        let via = first
            .via
            .as_ref()
            .map(|(p, n)| format!(" (via source line {} in {})", n, p.display()))
            .unwrap_or_default();
        bail!(
            "(ji)::shell: existing integration found in {}:{}{} — remove it manually, or pass --force to install anyway.",
            first.path.display(),
            first.line_no,
            via,
        );
    }

    if let Some(h) = primary_legacy
        && primary_marker.is_none()
        && !opts.force
    {
        bail!(
            "(ji)::shell: existing integration line found in {}:{} — remove it manually, or pass --force to install alongside it.",
            h.path.display(),
            h.line_no,
        );
    }

    let managed_path = env.managed_file(shell)?;
    let managed_body = render_managed_body(shell, cmd)?;
    let stanza = render_rc_stanza(shell)?;

    let primary_contents = if primary.exists() {
        std::fs::read_to_string(&primary)
            .with_context(|| format!("failed to read {}", primary.display()))?
    } else {
        String::new()
    };

    let new_primary_contents = if let Some(h) = primary_marker {
        let HitKind::MarkerBlock { byte_range } = &h.kind else {
            bail!(
                "internal: primary_marker hit at {}:{} had unexpected kind (expected MarkerBlock)",
                h.path.display(),
                h.line_no,
            );
        };
        let after = &primary_contents[byte_range.end..];
        let trailing = if after.is_empty() || after.starts_with('\n') {
            ""
        } else {
            "\n"
        };
        let block = format!("{stanza}{trailing}");
        replace_block(&primary_contents, byte_range, &block)
    } else {
        append_block(&primary_contents, &stanza)
    };

    if opts.dry_run {
        print_dry_run_diff(
            &managed_path,
            &existing_or_empty(&managed_path),
            &managed_body,
        );
        print_dry_run_diff(&primary, &primary_contents, &new_primary_contents);
        return Ok(());
    }

    let managed_outcome = write_atomic(&managed_path, &managed_body)?;
    match managed_outcome {
        WriteOutcome::Created => eprintln!(
            "(ji)::shell installed managed file {}",
            managed_path.display()
        ),
        WriteOutcome::Updated => eprintln!(
            "(ji)::shell updated managed file {}",
            managed_path.display()
        ),
        WriteOutcome::Unchanged => {}
    }

    let primary_outcome = if new_primary_contents == primary_contents {
        WriteOutcome::Unchanged
    } else {
        if !primary.exists()
            && let Some(parent) = primary.parent()
            && !parent.exists()
        {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        write_atomic(&primary, &new_primary_contents)?
    };
    match primary_outcome {
        WriteOutcome::Created => eprintln!(
            "(ji)::shell created {} with managed block",
            primary.display()
        ),
        WriteOutcome::Updated => eprintln!(
            "(ji)::shell updated {} with managed block",
            primary.display()
        ),
        WriteOutcome::Unchanged => {
            eprintln!("(ji)::shell {} already up-to-date", primary.display());
        }
    }

    Ok(())
}

fn existing_or_empty(p: &Path) -> String {
    std::fs::read_to_string(p).unwrap_or_default()
}

fn print_dry_run_diff(path: &Path, before: &str, after: &str) {
    if before == after {
        return;
    }
    println!("--- {}", path.display());
    println!("+++ {} (after install)", path.display());
    for line in before.lines() {
        println!("- {line}");
    }
    for line in after.lines() {
        println!("+ {line}");
    }
    println!();
}

fn install_fish(env: &ShellEnv, cmd: &mut clap::Command, opts: InstallOpts) -> Result<()> {
    let primary = fish_primary(env);
    let completions = fish_completions_path(env);
    let shadow = fish_shadow_path(env);

    if shadow.exists() && !opts.force {
        bail!(
            "(ji)::shell: {} exists and would shadow our `functions/ji.fish` (fish auto-sources conf.d at startup, blocking the autoload) — remove it manually, or pass --force.",
            shadow.display(),
        );
    }

    let locations = scan_locations(env, "fish", &primary);
    let hits = find_hits(&locations, env)?;

    let malformed: Vec<&Hit> = hits
        .iter()
        .filter(|h| matches!(h.kind, HitKind::MalformedMarker))
        .collect();
    if !malformed.is_empty() && !opts.force {
        let first = malformed[0];
        bail!(
            "(ji)::shell: malformed marker block in {}:{} — edit manually or pass --force to overwrite",
            first.path.display(),
            first.line_no,
        );
    }

    let nonprimary_hits: Vec<&Hit> = hits
        .iter()
        .filter(|h| {
            h.path != primary && matches!(h.kind, HitKind::MarkerBlock { .. } | HitKind::Legacy)
        })
        .collect();

    if !nonprimary_hits.is_empty() && !opts.force {
        let first = nonprimary_hits[0];
        bail!(
            "(ji)::shell: existing integration found in {}:{} — remove it manually, or pass --force to install anyway.",
            first.path.display(),
            first.line_no,
        );
    }

    let wrapper_body = render_fish_wrapper_body();
    let completion_body = render_fish_completion_body(cmd)?;

    for (path, label) in [(&primary, "wrapper"), (&completions, "completions")] {
        if path.exists() {
            let existing = std::fs::read_to_string(path)
                .with_context(|| format!("failed to read {}", path.display()))?;
            if !is_managed(&existing) && !opts.force {
                bail!(
                    "(ji)::shell: {} exists and is not ji-managed — remove it manually, or pass --force to overwrite ({label}).",
                    path.display(),
                );
            }
        }
    }

    if opts.dry_run {
        print_dry_run_diff(&primary, &existing_or_empty(&primary), &wrapper_body);
        print_dry_run_diff(
            &completions,
            &existing_or_empty(&completions),
            &completion_body,
        );
        return Ok(());
    }

    let wo = write_atomic(&primary, &wrapper_body)?;
    match wo {
        WriteOutcome::Created => {
            eprintln!("(ji)::shell wrapper installed in {}", primary.display());
        }
        WriteOutcome::Updated => eprintln!("(ji)::shell wrapper updated in {}", primary.display()),
        WriteOutcome::Unchanged => eprintln!(
            "(ji)::shell wrapper already up-to-date in {}",
            primary.display()
        ),
    }
    let co = write_atomic(&completions, &completion_body)?;
    match co {
        WriteOutcome::Created => eprintln!(
            "(ji)::shell completions installed in {}",
            completions.display()
        ),
        WriteOutcome::Updated => eprintln!(
            "(ji)::shell completions updated in {}",
            completions.display()
        ),
        WriteOutcome::Unchanged => eprintln!(
            "(ji)::shell completions already up-to-date in {}",
            completions.display()
        ),
    }

    Ok(())
}

// =====================================================================
// Uninstall
// =====================================================================

pub fn uninstall(env: &ShellEnv, shell: &str, opts: UninstallOpts) -> Result<()> {
    match shell {
        "zsh" | "bash" => uninstall_posix(env, shell, opts),
        "fish" => uninstall_fish(env, opts),
        other => bail!("unsupported shell: {other} (supported: {SUPPORTED_SHELLS})"),
    }
}

fn uninstall_posix(env: &ShellEnv, shell: &str, opts: UninstallOpts) -> Result<()> {
    let primary = if shell == "zsh" {
        zsh_primary(env)
    } else {
        bash_primary(env)
    };
    let managed = env.managed_file(shell)?;

    let mut nothing_to_do = true;

    if primary.is_file() {
        let contents = std::fs::read_to_string(&primary)
            .with_context(|| format!("failed to read {}", primary.display()))?;
        let mut hits = Vec::new();
        let ranges = find_marker_blocks(&primary, &contents, &mut hits, None);
        if let Some(range) = ranges.first() {
            let after = remove_block(&contents, range);
            if opts.dry_run {
                print_dry_run_diff(&primary, &contents, &after);
            } else {
                write_atomic(&primary, &after)?;
                eprintln!(
                    "(ji)::shell removed managed block from {}",
                    primary.display()
                );
            }
            nothing_to_do = false;
        }
    }

    if managed.is_file() {
        if opts.dry_run {
            println!("- would delete {}", managed.display());
        } else {
            std::fs::remove_file(&managed)
                .with_context(|| format!("failed to remove {}", managed.display()))?;
            eprintln!("(ji)::shell removed managed file {}", managed.display());
        }
        nothing_to_do = false;
    }

    let _ = opts.force; // reserved for symmetry with install; nothing to override here

    if nothing_to_do {
        eprintln!("(ji)::shell nothing to uninstall for {shell}");
    }
    Ok(())
}

fn uninstall_fish(env: &ShellEnv, opts: UninstallOpts) -> Result<()> {
    let primary = fish_primary(env);
    let completions = fish_completions_path(env);
    let shadow = fish_shadow_path(env);

    let mut nothing_to_do = true;

    for path in [&primary, &completions, &shadow] {
        if !path.is_file() {
            continue;
        }
        let existing = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        if is_managed(&existing) || opts.force {
            if opts.dry_run {
                println!("- would delete {}", path.display());
            } else {
                std::fs::remove_file(path)
                    .with_context(|| format!("failed to remove {}", path.display()))?;
                eprintln!("(ji)::shell removed {}", path.display());
            }
            nothing_to_do = false;
        } else {
            eprintln!(
                "(ji)::shell: leaving {} (not ji-managed; pass --force to remove)",
                path.display()
            );
        }
    }

    if nothing_to_do {
        eprintln!("(ji)::shell nothing to uninstall for fish");
    }
    Ok(())
}

// =====================================================================
// Status
// =====================================================================

pub fn status(env: &ShellEnv, shell: &str, cmd: &mut clap::Command) -> Result<()> {
    match shell {
        "zsh" | "bash" => status_posix(env, shell, cmd),
        "fish" => status_fish(env, cmd),
        other => bail!("unsupported shell: {other} (supported: {SUPPORTED_SHELLS})"),
    }
}

fn status_posix(env: &ShellEnv, shell: &str, cmd: &mut clap::Command) -> Result<()> {
    let primary = if shell == "zsh" {
        zsh_primary(env)
    } else {
        bash_primary(env)
    };
    let managed = env.managed_file(shell)?;
    let body = render_managed_body(shell, cmd)?;
    let stanza = render_rc_stanza(shell)?;

    println!("ji shell integration: {shell}");
    println!("  primary rc: {}", display_canonical(&primary));
    if shell == "bash" {
        let candidates: Vec<String> = [".bash_profile", ".bash_login", ".profile"]
            .iter()
            .map(|n| {
                let p = env.home.join(n);
                format!(
                    "{} ({})",
                    p.display(),
                    if p.exists() { "exists" } else { "absent" }
                )
            })
            .collect();
        println!("  bash candidates: {}", candidates.join(", "));
    }
    println!(
        "  managed file: {} ({})",
        display_canonical(&managed),
        state_for(&managed, &body)
    );
    println!("  rc stanza: {}", rc_stanza_state(&primary, &stanza));

    let locations = scan_locations(env, shell, &primary);
    let hits = find_hits(&locations, env)?;
    let mut cross: Vec<&Hit> = hits
        .iter()
        .filter(|h| {
            h.path != primary && matches!(h.kind, HitKind::MarkerBlock { .. } | HitKind::Legacy)
        })
        .collect();
    cross.sort_by_key(|h| (h.path.clone(), h.line_no));
    if cross.is_empty() {
        println!("  cross-file hits: none");
    } else {
        println!("  cross-file hits:");
        for h in cross {
            let via = h
                .via
                .as_ref()
                .map(|(p, n)| format!(" (via source line {} in {})", n, p.display()))
                .unwrap_or_default();
            println!(
                "    {}:{}{} — {}",
                h.path.display(),
                h.line_no,
                via,
                h.line_text
            );
        }
    }
    let unresolved: Vec<&Hit> = hits
        .iter()
        .filter(|h| matches!(h.kind, HitKind::UnresolvedSource(_)))
        .collect();
    if !unresolved.is_empty() {
        println!("  unresolved source lines:");
        for h in unresolved {
            println!("    {}:{} — {}", h.path.display(), h.line_no, h.line_text);
        }
    }
    println!(
        "  detected $SHELL: {}",
        std::env::var("SHELL").unwrap_or_else(|_| "<unset>".to_string())
    );

    Ok(())
}

fn status_fish(env: &ShellEnv, cmd: &mut clap::Command) -> Result<()> {
    let primary = fish_primary(env);
    let completions = fish_completions_path(env);
    let shadow = fish_shadow_path(env);
    let wrapper_body = render_fish_wrapper_body();
    let completion_body = render_fish_completion_body(cmd)?;

    println!("ji shell integration: fish");
    println!(
        "  wrapper file: {} ({})",
        display_canonical(&primary),
        state_for(&primary, &wrapper_body)
    );
    println!(
        "  completions file: {} ({})",
        display_canonical(&completions),
        state_for(&completions, &completion_body)
    );
    if shadow.exists() {
        println!(
            "  WARNING shadow file: {} (would override the autoload)",
            display_canonical(&shadow)
        );
    }

    let locations = scan_locations(env, "fish", &primary);
    let hits = find_hits(&locations, env)?;
    let cross: Vec<&Hit> = hits
        .iter()
        .filter(|h| {
            h.path != primary && matches!(h.kind, HitKind::MarkerBlock { .. } | HitKind::Legacy)
        })
        .collect();
    if cross.is_empty() {
        println!("  cross-file hits: none");
    } else {
        println!("  cross-file hits:");
        for h in cross {
            println!("    {}:{} — {}", h.path.display(), h.line_no, h.line_text);
        }
    }
    Ok(())
}

fn state_for(path: &Path, desired: &str) -> &'static str {
    if !path.exists() {
        return "absent";
    }
    match std::fs::read_to_string(path) {
        Ok(s) if s == desired => "present",
        Ok(_) => "drift",
        Err(_) => "absent",
    }
}

fn rc_stanza_state(rc: &Path, stanza: &str) -> String {
    if !rc.is_file() {
        return format!("{} (absent)", rc.display());
    }
    let contents = match std::fs::read_to_string(rc) {
        Ok(s) => s,
        Err(_) => return format!("{} (unreadable)", rc.display()),
    };
    let mut tmp = Vec::new();
    let ranges = find_marker_blocks(rc, &contents, &mut tmp, None);
    match ranges.first() {
        None => format!("{} (absent)", rc.display()),
        Some(range) => {
            let block = &contents[range.clone()];
            let normalized_block = block.trim_end_matches('\n');
            let normalized_stanza = stanza.trim_end_matches('\n');
            if normalized_block == normalized_stanza {
                format!("{} (present)", rc.display())
            } else {
                format!("{} (drift)", rc.display())
            }
        }
    }
}

fn display_canonical(p: &Path) -> String {
    match p.canonicalize() {
        Ok(c) if c != p => format!("{} -> {}", p.display(), c.display()),
        _ => p.display().to_string(),
    }
}
