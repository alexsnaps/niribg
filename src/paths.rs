//! XDG path resolution and `~` / `${VAR}` expansion, hand-rolled to avoid a
//! `dirs` / `shellexpand` dependency.
//!
//! The public functions read the real process environment. Their `*_from`
//! inner variants take an environment lookup closure so tests don't race on
//! process-global env mutation.

use std::path::PathBuf;

use anyhow::{Result, bail};

/// An environment lookup: `name -> value`, non-empty values only.
type EnvFn<'a> = &'a dyn Fn(&str) -> Option<String>;

fn real_env(name: &str) -> Option<String> {
    match std::env::var(name) {
        Ok(v) if !v.is_empty() => Some(v),
        _ => None,
    }
}

/// `$XDG_CONFIG_HOME/niribg/config.toml`, else `~/.config/niribg/config.toml`.
pub fn config_file() -> Result<PathBuf> {
    config_file_from(&real_env)
}

/// `$XDG_STATE_HOME/niribg/state.json`, else
/// `~/.local/state/niribg/state.json`.
pub fn state_file() -> Result<PathBuf> {
    state_file_from(&real_env)
}

/// `$XDG_RUNTIME_DIR/niribg-$WAYLAND_DISPLAY.sock`. `$WAYLAND_DISPLAY`
/// defaults to `wayland-0`; a missing `$XDG_RUNTIME_DIR` is a hard error.
pub fn socket_path() -> Result<PathBuf> {
    socket_path_from(&real_env)
}

/// Expand a leading `~` and any `${VAR}` / `$VAR` references.
pub fn expand(s: &str) -> Result<PathBuf> {
    expand_from(s, &real_env)
}

fn home(env: EnvFn) -> Result<PathBuf> {
    match env("HOME") {
        Some(h) => Ok(PathBuf::from(h)),
        None => bail!("$HOME is not set"),
    }
}

fn config_file_from(env: EnvFn) -> Result<PathBuf> {
    let dir = match env("XDG_CONFIG_HOME") {
        Some(d) => PathBuf::from(d),
        None => home(env)?.join(".config"),
    };
    Ok(dir.join("niribg").join("config.toml"))
}

fn state_file_from(env: EnvFn) -> Result<PathBuf> {
    let dir = match env("XDG_STATE_HOME") {
        Some(d) => PathBuf::from(d),
        None => home(env)?.join(".local").join("state"),
    };
    Ok(dir.join("niribg").join("state.json"))
}

fn socket_path_from(env: EnvFn) -> Result<PathBuf> {
    let Some(run) = env("XDG_RUNTIME_DIR") else {
        bail!("$XDG_RUNTIME_DIR is not set; cannot locate the control socket");
    };
    let display = env("WAYLAND_DISPLAY").unwrap_or_else(|| "wayland-0".to_string());
    // A socket-address WAYLAND_DISPLAY can be an absolute path; keep just the
    // final component so the socket name stays a plain filename.
    let display = display.rsplit('/').next().unwrap_or("wayland-0");
    Ok(PathBuf::from(run).join(format!("niribg-{display}.sock")))
}

fn expand_from(s: &str, env: EnvFn) -> Result<PathBuf> {
    // Leading `~` or `~/`.
    let (mut out, rest) = if s == "~" {
        (home(env)?.to_string_lossy().into_owned(), "")
    } else if let Some(tail) = s.strip_prefix("~/") {
        (format!("{}/", home(env)?.to_string_lossy()), tail)
    } else {
        (String::new(), s)
    };

    // `${VAR}` and `$VAR` anywhere in the remainder.
    let bytes = rest.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'$' {
            if rest[i + 1..].starts_with('{') {
                let Some(end) = rest[i + 2..].find('}') else {
                    bail!("unterminated ${{...}} in path {s:?}");
                };
                let name = &rest[i + 2..i + 2 + end];
                push_var(&mut out, name, env, s)?;
                i += 2 + end + 1;
            } else {
                let name_len = rest[i + 1..]
                    .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                    .unwrap_or(rest.len() - (i + 1));
                if name_len == 0 {
                    out.push('$');
                    i += 1;
                } else {
                    let name = &rest[i + 1..i + 1 + name_len];
                    push_var(&mut out, name, env, s)?;
                    i += 1 + name_len;
                }
            }
        } else {
            // Copy one UTF-8 char.
            let ch = rest[i..].chars().next().unwrap();
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    Ok(PathBuf::from(out))
}

fn push_var(out: &mut String, name: &str, env: EnvFn, whole: &str) -> Result<()> {
    match env(name) {
        Some(v) => {
            out.push_str(&v);
            Ok(())
        }
        None => bail!("environment variable ${name} (referenced in {whole:?}) is not set"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
        move |k: &str| map.get(k).cloned()
    }

    #[test]
    fn config_file_prefers_xdg() {
        let e = env_of(&[("XDG_CONFIG_HOME", "/x/cfg"), ("HOME", "/home/u")]);
        assert_eq!(
            config_file_from(&e).unwrap(),
            PathBuf::from("/x/cfg/niribg/config.toml")
        );
    }

    #[test]
    fn config_file_falls_back_to_home() {
        let e = env_of(&[("HOME", "/home/u")]);
        assert_eq!(
            config_file_from(&e).unwrap(),
            PathBuf::from("/home/u/.config/niribg/config.toml")
        );
    }

    #[test]
    fn state_file_locations() {
        let e = env_of(&[("XDG_STATE_HOME", "/x/state")]);
        assert_eq!(
            state_file_from(&e).unwrap(),
            PathBuf::from("/x/state/niribg/state.json")
        );
        let e = env_of(&[("HOME", "/home/u")]);
        assert_eq!(
            state_file_from(&e).unwrap(),
            PathBuf::from("/home/u/.local/state/niribg/state.json")
        );
    }

    #[test]
    fn socket_path_uses_display_and_runtime_dir() {
        let e = env_of(&[
            ("XDG_RUNTIME_DIR", "/run/user/1000"),
            ("WAYLAND_DISPLAY", "wayland-1"),
        ]);
        assert_eq!(
            socket_path_from(&e).unwrap(),
            PathBuf::from("/run/user/1000/niribg-wayland-1.sock")
        );
    }

    #[test]
    fn socket_path_defaults_display_and_strips_path() {
        let e = env_of(&[("XDG_RUNTIME_DIR", "/run/user/1000")]);
        assert_eq!(
            socket_path_from(&e).unwrap(),
            PathBuf::from("/run/user/1000/niribg-wayland-0.sock")
        );
        let e = env_of(&[
            ("XDG_RUNTIME_DIR", "/run/user/1000"),
            ("WAYLAND_DISPLAY", "/run/user/1000/wayland-9"),
        ]);
        assert_eq!(
            socket_path_from(&e).unwrap(),
            PathBuf::from("/run/user/1000/niribg-wayland-9.sock")
        );
    }

    #[test]
    fn socket_path_needs_runtime_dir() {
        let e = env_of(&[("WAYLAND_DISPLAY", "wayland-0")]);
        assert!(socket_path_from(&e).is_err());
    }

    #[test]
    fn expand_tilde() {
        let e = env_of(&[("HOME", "/home/u")]);
        assert_eq!(expand_from("~", &e).unwrap(), PathBuf::from("/home/u"));
        assert_eq!(
            expand_from("~/Pictures/w.jpg", &e).unwrap(),
            PathBuf::from("/home/u/Pictures/w.jpg")
        );
        // `~` only expands at the start.
        assert_eq!(expand_from("/a/~/b", &e).unwrap(), PathBuf::from("/a/~/b"));
    }

    #[test]
    fn expand_vars() {
        let e = env_of(&[("HOME", "/home/u"), ("XDG_PICTURES_DIR", "/home/u/Pics")]);
        assert_eq!(
            expand_from("${XDG_PICTURES_DIR}/w.jpg", &e).unwrap(),
            PathBuf::from("/home/u/Pics/w.jpg")
        );
        assert_eq!(
            expand_from("$HOME/w.jpg", &e).unwrap(),
            PathBuf::from("/home/u/w.jpg")
        );
        assert_eq!(
            expand_from("~/x/${XDG_PICTURES_DIR}", &e).unwrap(),
            PathBuf::from("/home/u/x//home/u/Pics")
        );
    }

    #[test]
    fn expand_missing_var_errors() {
        let e = env_of(&[("HOME", "/home/u")]);
        assert!(expand_from("${NOPE}/x", &e).is_err());
        assert!(expand_from("$NOPE", &e).is_err());
        assert!(expand_from("~/x", &env_of(&[])).is_err());
    }

    #[test]
    fn expand_leaves_plain_paths_and_lone_dollar() {
        let e = env_of(&[]);
        assert_eq!(
            expand_from("/etc/niribg/w.png", &e).unwrap(),
            PathBuf::from("/etc/niribg/w.png")
        );
        assert_eq!(expand_from("a $ b", &e).unwrap(), PathBuf::from("a $ b"));
    }
}
