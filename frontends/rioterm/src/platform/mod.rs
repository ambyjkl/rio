#[cfg(target_os = "macos")]
pub mod macos;

/// Escape POSIX shell metacharacters by prefixing each with a backslash.
/// This does not validate control characters; file drops must go through
/// `dropped_file_text` before insertion into a live terminal.
#[inline]
pub fn shell_escape(s: &str) -> String {
    let mut escaped = String::with_capacity(s.len());
    for c in s.chars() {
        if matches!(
            c,
            '\\' | ' '
                | '('
                | ')'
                | '['
                | ']'
                | '{'
                | '}'
                | '<'
                | '>'
                | '"'
                | '\''
                | '`'
                | '!'
                | '#'
                | '$'
                | '&'
                | ';'
                | '|'
                | '*'
                | '?'
                | '\t'
        ) {
            escaped.push('\\');
        }
        escaped.push(c);
    }
    escaped
}

/// Text to insert for a dropped file, without executing the pending command.
/// Native file drops supply absolute paths. Refuse lossy path conversion and
/// control characters: without bracketed paste, tabs/newlines/escape sequences
/// are interpreted as keys by the foreground application rather than filename
/// data. A trailing space separates paths in a multi-file drop.
pub fn dropped_file_text(path: &std::path::Path) -> Option<String> {
    let text = path.to_str()?;
    if !path.is_absolute() || text.chars().any(char::is_control) {
        return None;
    }
    let mut text = shell_escape(text);
    text.push(' ');
    Some(text)
}

#[cfg(test)]
mod tests {
    use super::{dropped_file_text, shell_escape};
    use std::path::Path;

    #[test]
    fn plain_path_is_unchanged() {
        assert_eq!(shell_escape("/usr/local/bin/rio"), "/usr/local/bin/rio");
    }

    #[test]
    fn spaces_and_parens_are_escaped() {
        assert_eq!(
            shell_escape("/Users/me/My File (1).txt"),
            "/Users/me/My\\ File\\ \\(1\\).txt"
        );
    }

    #[test]
    fn shell_metacharacters_are_escaped() {
        assert_eq!(
            shell_escape("a$b`c\"d'e;f&g|h*i?j!k#l"),
            "a\\$b\\`c\\\"d\\'e\\;f\\&g\\|h\\*i\\?j\\!k\\#l"
        );
    }

    #[test]
    fn backslash_is_escaped_without_double_escaping() {
        assert_eq!(shell_escape("a\\ b"), "a\\\\\\ b");
    }

    #[test]
    fn brackets_braces_and_redirects_are_escaped() {
        assert_eq!(
            shell_escape("x[1]{2}<3>4\t5"),
            "x\\[1\\]\\{2\\}\\<3\\>4\\\t5"
        );
    }

    #[test]
    fn unicode_passes_through() {
        assert_eq!(shell_escape("/tmp/café 図.png"), "/tmp/café\\ 図.png");
    }

    #[cfg(unix)]
    #[test]
    fn dropped_files_insert_escaped_paths_separated_by_spaces() {
        let text: String = ["/home/me/My File (1).txt", "/home/me/café 図.png"]
            .into_iter()
            .map(|path| dropped_file_text(Path::new(path)).unwrap())
            .collect();
        assert_eq!(
            text,
            "/home/me/My\\ File\\ \\(1\\).txt /home/me/café\\ 図.png "
        );
        assert!(!text.contains(['\n', '\r', '\x1b']));
    }

    #[cfg(unix)]
    #[test]
    fn dropped_file_metacharacters_stay_literal() {
        assert_eq!(
            dropped_file_text(Path::new("/home/me/$(cmd);'quoted'&*.txt")),
            Some("/home/me/\\$\\(cmd\\)\\;\\'quoted\\'\\&\\*.txt ".into())
        );
    }

    #[cfg(unix)]
    #[test]
    fn dropped_file_control_characters_cannot_inject_keys() {
        for ch in [
            '\0', '\n', '\r', '\t', '\x03', '\x08', '\x15', '\x1b', '\x7f', '\u{009b}',
        ] {
            let path = format!("/home/me/file{ch}name");
            assert_eq!(dropped_file_text(Path::new(&path)), None);
        }
    }

    #[test]
    fn dropped_files_require_absolute_paths() {
        assert_eq!(dropped_file_text(Path::new("")), None);
        assert_eq!(dropped_file_text(Path::new("FILE=command")), None);
        assert_eq!(dropped_file_text(Path::new("~/file")), None);
    }

    #[cfg(unix)]
    #[test]
    fn dropped_files_do_not_silently_change_non_unicode_names() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;
        let path =
            std::path::PathBuf::from(OsString::from_vec(b"/home/me/file\xff".to_vec()));
        assert_eq!(dropped_file_text(&path), None);
    }
}
