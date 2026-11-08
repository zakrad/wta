//! `wta clone` / dashboard `R` — clone a repo, then start an agent in it.
//!
//! Accepts a full git URL (`https://…`, `git@host:owner/repo.git`, `ssh://…`) or the
//! GitHub shorthand `owner/repo`. The folder defaults to `<clone_dir>/<repo>` —
//! `wta config clone_dir ~/code` — else next to the repo you are in.

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// `owner/repo` → its GitHub https URL; anything else is used as given.
pub fn expand_url(input: &str) -> String {
    let s = input.trim();
    let shorthand = !s.contains(':')
        && !s.starts_with('.')
        && !s.starts_with('/')
        && !s.starts_with('~')
        && s.split('/').count() == 2
        && s.split('/').all(|p| {
            !p.is_empty() && p.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        });
    if shorthand {
        format!("https://github.com/{s}.git")
    } else {
        s.to_string()
    }
}

/// The folder name git would pick: the last path part, without `.git`.
pub fn repo_name(url: &str) -> Option<String> {
    let s = url.trim().trim_end_matches('/');
    let last = s.rsplit(['/', ':']).next()?;
    let name = last.strip_suffix(".git").unwrap_or(last);
    (!name.is_empty() && name != "." && name != "..").then(|| name.to_string())
}

/// `~` and `~/…` → the home dir.
pub fn expand_home(p: &str) -> PathBuf {
    let p = p.trim();
    match (p.strip_prefix('~'), dirs::home_dir()) {
        (Some(rest), Some(h)) if rest.is_empty() || rest.starts_with('/') => {
            h.join(rest.trim_start_matches('/'))
        }
        _ => PathBuf::from(p),
    }
}

/// `/Users/me/code/x` → `~/code/x`, for a shorter prefilled field.
pub fn tilde(p: &Path) -> String {
    match dirs::home_dir().and_then(|h| p.strip_prefix(&h).ok().map(Path::to_path_buf)) {
        Some(rest) if rest.as_os_str().is_empty() => "~".into(),
        Some(rest) => format!("~/{}", rest.display()),
        None => p.display().to_string(),
    }
}

/// Where new clones go: the `clone_dir` setting, else the folder that holds `near`
/// (the repo you are in), else `fallback`.
pub fn parent_dir(near: Option<&Path>, fallback: PathBuf) -> PathBuf {
    if let Some(d) = crate::config::load().clone_dir.filter(|d| !d.trim().is_empty()) {
        return expand_home(&d);
    }
    near.and_then(Path::parent).map(Path::to_path_buf).unwrap_or(fallback)
}

/// Is `dir` a git checkout (its own `.git`, dir or file)?
fn is_repo(dir: &Path) -> bool {
    dir.join(".git").exists()
}

/// What cloning into `dest` would do: `Ok(true)` = clone, `Ok(false)` = it is already
/// a repo (use it as is), `Err` = a non-empty folder that is not a repo.
pub fn check_dest(dest: &Path) -> Result<bool> {
    if is_repo(dest) {
        return Ok(false);
    }
    if dest.exists() && std::fs::read_dir(dest).map(|mut d| d.next().is_some()).unwrap_or(true) {
        bail!("that folder exists and is not a git repo — pick another one");
    }
    Ok(true)
}

/// Clone `url` into `dest`. `quiet` (the dashboard): no terminal — git must not ask for
/// a password, so it fails fast with git's own message; credential helpers and
/// ssh-agent still work. Not quiet (the CLI): git's progress shows as usual.
pub fn clone(url: &str, dest: &Path, quiet: bool) -> Result<()> {
    if let Some(p) = dest.parent() {
        std::fs::create_dir_all(p).with_context(|| format!("create {}", p.display()))?;
    }
    let mut cmd = Command::new("git");
    cmd.arg("clone").arg("--").arg(url).arg(dest);
    if quiet {
        cmd.env("GIT_TERMINAL_PROMPT", "0").stdin(Stdio::null()).stdout(Stdio::null());
        let out = cmd.stderr(Stdio::piped()).output().context("could not run git")?;
        if !out.status.success() {
            let err = String::from_utf8_lossy(&out.stderr);
            let line = err
                .lines()
                .rev()
                .find(|l| l.starts_with("fatal:") || l.starts_with("error:"))
                .or_else(|| err.lines().rev().find(|l| !l.trim().is_empty()))
                .unwrap_or("git clone failed");
            bail!("{}", line.trim());
        }
    } else if !cmd.status().context("could not run git")?.success() {
        bail!("git clone failed");
    }
    Ok(())
}

/// `wta clone <url> [dir] [-t task] [-- agent args]`: clone (or reuse an existing
/// checkout), then start an agent in it like `wta new`. Without `-t`, asks for the
/// name on a terminal; an empty answer only clones.
pub fn run_cli(url: &str, dir: Option<&str>, task: Option<String>, agent_args: &[String]) -> Result<()> {
    use std::io::{BufRead, IsTerminal, Write};
    let url = expand_url(url);
    let dest = match dir {
        Some(d) => expand_home(d),
        None => {
            let name = repo_name(&url).context("cannot tell the repo name from that URL — give a folder")?;
            let here = crate::worktree::repo_root().ok();
            let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
            // no setting: like `git clone`, into the current folder — unless you are
            // inside a repo, then next to it (never a repo inside a repo)
            parent_dir(here.as_deref(), cwd).join(name)
        }
    };
    if check_dest(&dest).with_context(|| tilde(&dest))? {
        println!("cloning {url} → {}", tilde(&dest));
        clone(&url, &dest, false)?;
    } else {
        println!("{} is already a repo — using it", tilde(&dest));
    }
    let task = match task {
        Some(t) => Some(t),
        None if std::io::stdin().is_terminal() => {
            print!("agent name (Enter to skip): ");
            std::io::stdout().flush().ok();
            let mut line = String::new();
            std::io::stdin().lock().read_line(&mut line).ok();
            Some(line.trim().to_string()).filter(|t| !t.is_empty())
        }
        None => None,
    };
    let Some(task) = task else {
        println!("cloned — start an agent with:  cd {} && wta new <task>", tilde(&dest));
        return Ok(());
    };
    std::env::set_current_dir(&dest).with_context(|| format!("cd {}", dest.display()))?;
    crate::worktree::new(&task, agent_args)?;
    println!("started agent '{task}' in {} — attach with `wta attach {task}` (or `wta dash`)", tilde(&dest));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_expand_and_name() {
        assert_eq!(expand_url("zakrad/wta"), "https://github.com/zakrad/wta.git");
        assert_eq!(expand_url(" git@github.com:a/b.git "), "git@github.com:a/b.git");
        assert_eq!(expand_url("./local/repo"), "./local/repo");
        assert_eq!(expand_url("/abs/path"), "/abs/path");
        assert_eq!(repo_name("https://github.com/zakrad/wta.git").as_deref(), Some("wta"));
        assert_eq!(repo_name("git@github.com:zakrad/wta.git").as_deref(), Some("wta"));
        assert_eq!(repo_name("https://gitlab.com/g/sub/proj/").as_deref(), Some("proj"));
        assert_eq!(repo_name("host:repo").as_deref(), Some("repo"));
        assert_eq!(repo_name(""), None);
    }

    #[test]
    fn dest_checks() {
        let base = std::env::temp_dir().join(format!("wta-clone-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        assert!(check_dest(&base.join("new")).unwrap(), "missing folder → clone");
        std::fs::create_dir_all(base.join("empty")).unwrap();
        assert!(check_dest(&base.join("empty")).unwrap(), "empty folder → clone");
        std::fs::create_dir_all(base.join("repo/.git")).unwrap();
        assert!(!check_dest(&base.join("repo")).unwrap(), "existing repo → reuse");
        std::fs::create_dir_all(base.join("junk")).unwrap();
        std::fs::write(base.join("junk/f"), "x").unwrap();
        assert!(check_dest(&base.join("junk")).is_err(), "non-empty non-repo → refuse");
        let _ = std::fs::remove_dir_all(&base);
    }
}
