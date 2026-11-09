//! Which `bash` commands need no question, and what "always allow" covers.
//!
//! Both are decided from the command text alone, before the shell sees it, so both lean towards
//! asking: anything this code does not fully understand is not free and has no "always".

/// Commands that run without asking when they are used alone, chained with `;`, `&&` or `||`, or
/// piped into each other. `find`, `sed`, `node`, `python` and `grep` only in the narrow forms
/// `find_ok`, `sed_ok`, `free_stage` and `grep_ok` accept.
/// They only look, or only make a folder in the working directory; every path they are given
/// must be relative and stay below the working directory (see [`is_free`]).
pub const FREE_COMMANDS: &[&str] = &[
    "ls", "cd", "pwd", "echo", "mkdir", "sleep", "pgrep", "cat", "head", "tail", "wc", "du",
    "grep", "find", "sed", "sort", "uniq", "printf", "node", "python", "python3", "git",
];

/// Commands whose job is to run other commands (or code). "Always allow" is never offered for
/// them, because it would allow everything.
const RUNS_OTHER_COMMANDS: &[&str] = &[
    "sh", "bash", "zsh", "dash", "fish", "ksh", "env", "sudo", "doas", "su", "xargs", "find", ".",
    "eval", "exec", "nohup", "time", "timeout", "watch", "nice", "ionice", "setsid", "stdbuf",
    "command", "builtin", "source", "python", "python2", "python3", "node", "deno", "bun", "perl",
    "ruby", "php", "lua", "awk", "gawk", "sed", "ssh", "npx", "uv", "pipx",
];

/// Characters that give a shell more power than "run this one program": substitution, pipes,
/// redirects, subshells, background, escapes, history and braces.
const SHELL_CONTROL: &[char] = &[
    ';', '&', '|', '<', '>', '$', '`', '(', ')', '{', '}', '\\', '\n', '\r', '!',
];

/// Whether the whole command line may run without asking: every command in it, joined by `;`,
/// `&&`, `||` or a pipe `|`, must be one of the looking commands below, with arguments that stay
/// inside the working directory.
pub fn is_free(command: &str) -> bool {
    match stages(command) {
        Some(stages) => !stages.is_empty() && stages.iter().all(|words| free_stage(words)),
        None => false,
    }
}

/// Whether the command may run in a subagent: [`is_free`], and nothing that makes a folder.
pub fn is_looking(command: &str) -> bool {
    is_free(command)
        && stages(command).is_some_and(|stages| {
            stages
                .iter()
                .all(|words| words.first().map(String::as_str) != Some("mkdir"))
        })
}

/// Splits a command line into its commands, each a list of words with the quotes taken off.
/// `None` when anything is in it that this code does not fully understand: substitution,
/// variables, escapes, redirects other than `2>&1` and `2>/dev/null`, `&`, subshells, braces, an
/// unclosed quote, control characters. Inside single quotes anything goes (the shell does not
/// look); inside double quotes `$`, backtick, `!` and a backslash that escapes are refused.
fn stages(command: &str) -> Option<Vec<Vec<String>>> {
    let mut stages: Vec<Vec<String>> = Vec::new();
    let mut words: Vec<String> = Vec::new();
    let mut word = String::new();
    // A word has started even if it is empty (`""`).
    let mut started = false;
    // The word had a quote in it, or a `<`/`>` outside quotes.
    let mut quoted = false;
    let mut redirect_chars = false;
    // The last thing was `|`, `&&` or `||`, which still needs a command after it.
    let mut dangling = false;
    let mut chars = command.trim().chars().peekable();

    fn end_word(
        words: &mut Vec<String>,
        word: &mut String,
        started: &mut bool,
        quoted: &mut bool,
        redirect_chars: &mut bool,
    ) -> bool {
        if *started {
            let finished = std::mem::take(word);
            if *redirect_chars {
                // Only these two redirects are allowed, and only written bare.
                let bare = !*quoted && matches!(finished.as_str(), "2>&1" | "2>/dev/null");
                if !bare {
                    return false;
                }
            } else {
                words.push(finished);
            }
        }
        *started = false;
        *quoted = false;
        *redirect_chars = false;
        true
    }

    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                started = true;
                dangling = false;
                quoted = true;
                loop {
                    match chars.next()? {
                        '\'' => break,
                        other => word.push(other),
                    }
                }
            }
            '"' => {
                started = true;
                dangling = false;
                quoted = true;
                loop {
                    match chars.next()? {
                        '"' => break,
                        // A backslash before an ordinary character stays as it is (`\|` in a
                        // pattern); before `$`, a backtick, `"` or another backslash it escapes.
                        '\\' => match chars.peek() {
                            Some('$' | '`' | '"' | '\\') | None => return None,
                            Some(next) if next.is_control() => return None,
                            Some(_) => word.push('\\'),
                        },
                        '$' | '`' | '!' => return None,
                        other if other.is_control() => return None,
                        other => word.push(other),
                    }
                }
            }
            c if c.is_whitespace() && !c.is_control() => {
                if !end_word(
                    &mut words,
                    &mut word,
                    &mut started,
                    &mut quoted,
                    &mut redirect_chars,
                ) {
                    return None;
                }
            }
            '&' if redirect_chars && word.ends_with('>') => word.push('&'),
            ';' | '|' | '&' => {
                if !end_word(
                    &mut words,
                    &mut word,
                    &mut started,
                    &mut quoted,
                    &mut redirect_chars,
                ) {
                    return None;
                }
                // `&&`, `||` are joiners like `;` and a lone `|` is a pipe; a lone `&` is not allowed.
                match (c, chars.peek()) {
                    ('&', Some('&')) | ('|', Some('|')) => {
                        chars.next();
                    }
                    ('&', _) => return None,
                    _ => {}
                }
                dangling = c != ';';
                if !words.is_empty() {
                    stages.push(std::mem::take(&mut words));
                } else if c != ';' || stages.is_empty() {
                    // `ls ;; x`, a leading joiner or `ls | | x` is not a command line we understand.
                    return None;
                }
            }
            '<' | '>' => {
                started = true;
                dangling = false;
                redirect_chars = true;
                word.push(c);
            }
            '$' | '`' | '(' | ')' | '{' | '}' | '\\' | '!' => return None,
            c if c.is_control() => return None,
            other => {
                started = true;
                dangling = false;
                word.push(other);
            }
        }
    }
    if !end_word(
        &mut words,
        &mut word,
        &mut started,
        &mut quoted,
        &mut redirect_chars,
    ) {
        return None;
    }
    if !words.is_empty() {
        stages.push(words);
    } else if dangling {
        return None;
    }
    Some(stages)
}

/// One command with its words: is it a looking command used in a looking way?
fn free_stage(words: &[String]) -> bool {
    let Some((name, args)) = words.split_first() else {
        return false;
    };
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    match name.as_str() {
        "ls" | "cat" | "head" | "tail" | "wc" | "mkdir" | "du" | "diff" | "cmp" | "stat"
        | "basename" | "dirname" | "cut" | "nl" => args.iter().all(|a| path_ok(a)),
        // Reads standard input only: its words are characters, not paths.
        "tr" => args.iter().all(|a| !a.starts_with('/') || a.len() == 1),
        // A bare `cd` goes to $HOME and `cd -` to the old directory: both leave the folder.
        "cd" => matches!(args.as_slice(), [dir] if !dir.starts_with('-') && path_ok(dir)),
        "pwd" => args.iter().all(|a| matches!(*a, "-L" | "-P")),
        "echo" | "pgrep" | "printf" => true,
        "sort" => sort_ok(&args),
        "uniq" => {
            // A second path would be the file it writes to.
            args.iter()
                .filter(|a| **a == "-" || !a.starts_with('-'))
                .count()
                <= 1
                && args.iter().all(|a| path_ok(a))
        }
        "sleep" => {
            args.len() == 1
                && !args[0].is_empty()
                && args[0].chars().all(|c| c.is_ascii_digit() || c == '.')
        }
        "python" | "python3" => args.len() == 1 && matches!(args[0], "--version" | "-V"),
        "node" => match args.as_slice() {
            ["--version" | "-v"] => true,
            ["--check" | "-c", file] => !file.starts_with('-') && path_ok(file),
            _ => false,
        },
        "grep" => grep_ok(&args),
        "git" => git_ok(&args),
        "find" => find_ok(&args),
        "sed" => sed_ok(&args),
        _ => false,
    }
}

/// `grep` reads: the first argument that is not a flag is the pattern (anything), the rest are
/// paths. Flags that read patterns or exclusions from a file are refused.
fn grep_ok(args: &[&str]) -> bool {
    let mut pattern_seen = false;
    for arg in args {
        if let Some(long) = arg.strip_prefix("--") {
            // `--reg` and `--exclude-f` are accepted by grep as short forms of these.
            if long.starts_with("file") || long.starts_with("exclude-f") || long.starts_with("reg")
            {
                return false;
            }
            if !path_ok(arg) {
                return false;
            }
        } else if let Some(cluster) = arg.strip_prefix('-')
            && !cluster.is_empty()
        {
            // With -e the pattern is given, so the first word after it would be a file.
            if cluster.contains('f') || cluster.contains('e') {
                return false;
            }
        } else if !pattern_seen {
            pattern_seen = true;
        } else if !path_ok(arg) {
            return false;
        }
    }
    true
}

/// `git` only as `git status|log|diff|show|ls-files|blame|rev-parse ...` (and `remote -v`, `branch -a`) straight after the name (no `-c` or `-C` before
/// the subcommand), which read the repository. Flags that write a file or run a program are
/// refused, and every path must stay below the working directory.
fn git_ok(args: &[&str]) -> bool {
    const REFUSED: &[&str] = &[
        "output",
        "ext-diff",
        "textconv",
        "open-files-in-pager",
        "exec",
        "no-index",
        "pager",
        "git-dir",
        "work-tree",
    ];
    let [sub, rest @ ..] = args else {
        return false;
    };
    // Listing remotes and branches: nothing that adds, renames or deletes one.
    match *sub {
        "remote" => return rest.is_empty() || rest == ["-v"],
        "branch" => {
            return rest
                .iter()
                .all(|a| matches!(*a, "-a" | "-r" | "-v" | "-vv" | "--list" | "--all"));
        }
        _ => {}
    }
    if !matches!(
        *sub,
        "status" | "log" | "diff" | "show" | "ls-files" | "blame" | "rev-parse"
    ) {
        return false;
    }
    rest.iter().all(|arg| {
        if let Some(long) = arg.strip_prefix("--") {
            let name = long.split_once('=').map_or(long, |(name, _)| name);
            !REFUSED.contains(&name) && path_ok(arg)
        } else if let Some(cluster) = arg.strip_prefix('-')
            && !cluster.is_empty()
        {
            // `-O<file>` reads an order file; `-c` before the subcommand is not reached here.
            !cluster.starts_with('O')
        } else {
            path_ok(arg)
        }
    })
}

/// `sort` reads and prints; the flags that write a file or run a program are refused.
fn sort_ok(args: &[&str]) -> bool {
    args.iter().all(|arg| {
        if let Some(long) = arg.strip_prefix("--") {
            // GNU sort takes any unique prefix of a long option, so `--o` is `--output`.
            let name = long.split_once('=').map_or(long, |(name, _)| name);
            let writes = [
                "output",
                "compress-program",
                "files0-from",
                "temporary-directory",
            ]
            .iter()
            .any(|full| !name.is_empty() && full.starts_with(name));
            !writes && path_ok(arg)
        } else if let Some(cluster) = arg.strip_prefix('-')
            && !cluster.is_empty()
        {
            !cluster.contains('o') && !cluster.contains('T')
        } else {
            path_ok(arg)
        }
    })
}

/// `find` lists: the actions that run, delete or write are refused, and every word must stay
/// below the working directory.
fn find_ok(args: &[&str]) -> bool {
    const ACTIONS: &[&str] = &[
        "-exec", "-execdir", "-ok", "-okdir", "-delete", "-fprint", "-fprint0", "-fprintf", "-fls",
    ];
    args.iter().all(|a| !ACTIONS.contains(a) && path_ok(a))
}

/// `sed` only as `sed -n '<lines>p' [files]`, which prints lines and edits nothing.
fn sed_ok(args: &[&str]) -> bool {
    let ["-n", script, files @ ..] = args else {
        return false;
    };
    let Some(range) = script.strip_suffix('p') else {
        return false;
    };
    let number = |s: &str| s == "$" || (!s.is_empty() && s.chars().all(|c| c.is_ascii_digit()));
    let range_ok = match range.split_once(',') {
        None => number(range),
        Some((from, to)) => number(from) && number(to),
    };
    // A word starting with `-` is a flag (`-e`, `-i`, `-f`), not a file.
    range_ok && files.iter().all(|f| !f.starts_with('-') && path_ok(f))
}

/// A flag, or a path that is relative and does not climb out of the working directory.
fn path_ok(token: &str) -> bool {
    if token.starts_with('-') {
        // `--dir=/etc` names a path too.
        return token
            .split_once('=')
            .is_none_or(|(_, value)| path_ok(value));
    }
    let bare = token.trim_matches(|c| c == '"' || c == '\'');
    !bare.starts_with('/') && !bare.starts_with('~') && !bare.split('/').any(|piece| piece == "..")
}

/// For a command that may be remembered with "always": the key the permission is stored under and
/// what to call it on screen. `None` for anything but one plain program with plain arguments, and
/// for programs that run other commands.
pub fn always_for(command: &str) -> Option<(String, String)> {
    let command = command.trim();
    if command
        .chars()
        .any(|c| SHELL_CONTROL.contains(&c) || c.is_control())
    {
        return None;
    }
    let name = command.split_whitespace().next()?;
    let plain = name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '+'));
    if !plain || RUNS_OTHER_COMMANDS.contains(&name) {
        return None;
    }
    Some((format!("bash:{name}"), format!("`{name}` commands")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn more_reading_commands_from_the_subagent_logs_are_free_and_the_writing_forms_are_not() {
        for command in [
            "diff -u a.txt b.txt",
            "diff -r old new | head",
            "cmp a b",
            "stat a.txt",
            "basename src/a.rs",
            "cut -d: -f1 a.txt | sort | uniq",
            "tr a-z A-Z",
            "nl a.txt",
            "git remote -v",
            "git remote",
            "git branch -a",
            "git ls-files src",
            "git blame a.txt",
            "git rev-parse --show-toplevel",
        ] {
            assert!(is_free(command), "{command}");
        }
        for command in [
            "diff /etc/passwd a",
            "cmp ../x a",
            "git remote add x http://h",
            "git remote remove origin",
            "git branch -D old",
            "git branch new",
            "git rev-parse --git-dir",
            "git -C . remote -v",
            "cut -d: -f1 /etc/passwd",
        ] {
            assert!(!is_free(command), "{command}");
        }
    }

    #[test]
    fn the_commands_from_the_logs_that_only_look_are_free() {
        for command in [
            "ls",
            "ls -a",
            "ls -la",
            "ls -R a b",
            "cd src",
            "cd sub",
            "pwd",
            "mkdir -p counter",
            "mkdir -p src/demo",
            "python3 --version 2>&1; python --version 2>&1",
            "pgrep -af \"server.py\" || echo \"NOT RUNNING\"",
            "ls -la; echo \"---\"; cat count.txt",
            "ls && echo hi",
            "sleep 1.5",
            "ls;",
            "  ls  ",
            "cat a.txt 2>/dev/null",
            "ls -la camera-store 2>/dev/null || find . -maxdepth 3 -iname \"*camera*\" | head",
            "ls -la; echo \"---\"; find . -not -path \"*/node_modules/*\" -not -path '*/.git/*' | head -50",
            "grep -rn \"color-scheme\\|prefers\" camera-store/style.css | head",
            "grep -n \"a;b\" index.html",
            "grep -n 'x|y' a.txt b.txt | wc -l",
            "cd camera-store && node --check script.js && echo \"JS OK\"",
            "node --version 2>/dev/null; python3 --version 2>/dev/null",
            "sed -n '185,215p' index.html",
            "sed -n 5p index.html",
            "sed -n '10,$p' a.txt",
            "du -sh .",
            "du -sh src/* | tail -3",
            "cat a.txt 2>&1 | head -5",
            "grep -o 'id=\"[^\"]*\"' index.html | sort -u | uniq -d",
            "printf \"%s\\n\" hi",
            "git status",
            "git status --short",
            "git log --oneline -5",
            "git diff HEAD~1 -- src/main.rs",
            "git show HEAD:src/main.rs | head -20",
            "git diff --stat 2>&1 | tail -3",
            "sort -u a.txt | uniq -",
            "cat a.txt | uniq -c",
            "grep -n pat a.txt",
            "sort --reverse a.txt",
        ] {
            assert!(is_free(command), "should be free: {command:?}");
        }
    }

    #[test]
    fn anything_that_changes_or_runs_more_asks() {
        for command in [
            "git",
            "git commit -m x",
            "git push",
            "git checkout main",
            "git branch -D x",
            "git stash",
            "git -c core.pager=sh log",
            "git -C /etc status",
            "git diff --output=out.txt",
            "git log --output out",
            "git diff --ext-diff",
            "git diff --no-index /etc/passwd x",
            "git diff -Oorder.txt",
            "git show /etc/passwd",
            "git diff ../other",
            "",
            "   ",
            "rm -rf x",
            "rm -rf __pycache__ && echo \"removed\"; ls",
            "python3 server.py",
            "python3 -c 'print(1)'",
            "python3 --version --help",
            "ls > out.txt",
            "ls >> out.txt",
            "cat < in.txt",
            "ls &",
            "ls & rm x",
            "ls $(rm x)",
            "ls `rm x`",
            "echo $HOME",
            "cd \"$(pwd)\" && ls",
            "ls; rm x",
            "ls && curl http://x",
            "ls\nrm x",
            "ls \"unclosed",
            "ls (",
            "ls ${x}",
            "ls 2>out",
            "sleep 1; sleep",
            "sleep abc",
            "pwd extra",
            "LS",
            "ls\\ x",
            "ls | | cat",
            "| ls",
            "ls |",
            "cat a | rm x",
            "echo hi | sh",
            "sort -o out.txt in.txt",
            "sort -uo out.txt in.txt",
            "sort --output=x y",
            "sort -T /tmp x",
            "sort --compress-program=sh x",
            "uniq in.txt out.txt",
            "sort /etc/passwd",
            "ls 2>/dev/nullx",
            "ls 2>&1x",
            "ls >&2",
            "find . -name x -delete",
            "find . -exec rm {} ;",
            "find . -name x -fprint out.txt",
            "find / -name x",
            "find . -path /etc/x",
            "sed -i 's/a/b/' x",
            "sed -n '1p;w out' x",
            "sed -n 'w out' x",
            "sed 's/a/b/' x",
            "sed -n 1,2p /etc/passwd",
            "grep -f patterns x",
            "grep -rnf patterns x",
            "grep --file=patterns x",
            "grep --exclude-from=/etc/x pat .",
            "grep x /etc/passwd",
            "grep x ../secret",
            "grep \"a\\\"b\" x",
            "grep \"$HOME\" x",
            "node script.js",
            "node -e 'x'",
            "node --check /etc/x.js",
            "node --check --require=./pre.js",
            "node --check --import=./x.mjs",
            "sed -n 1p -e 'e touch pwned' g",
            "sed -n 1p -i f",
            "sed -n 1p --in-place f",
            "sed -n 1p --expression=w\\ x g",
            "cd",
            "cd -",
            "cd && cat .ssh/id_rsa",
            "cd; ls -a",
            "cd -P src",
            "grep -e. /etc/hostname",
            "grep -ex /etc/passwd",
            "grep -r -e. ~/.ssh",
            "grep --regexp=x /etc/passwd",
            "grep --reg=x /etc/passwd",
            "grep --exclude-f=/etc/x pat .",
            "sort --o=s2 s",
            "sort --ou=x s",
            "sort --co=sh x",
            "sort --t=/tmp x",
            "echo x | uniq - out.txt",
            ". ./x.sh",
            "du /",
            "ls --color=/etc/x",
            "grep 'unclosed x",
            "echo 'a' 'b",
        ] {
            assert!(!is_free(command), "should ask: {command:?}");
        }
    }

    #[test]
    fn paths_must_stay_below_the_working_directory() {
        for command in [
            "ls /",
            "ls /etc",
            "cd /tmp",
            "cd ~",
            "cd ~/x",
            "cat ../secret",
            "cat a/../../b",
            "mkdir -p /tmp/x",
            "head -n 3 \"/etc/passwd\"",
            "cat ok.txt /etc/passwd",
            "ls ..",
        ] {
            assert!(!is_free(command), "should ask: {command:?}");
        }
        assert!(is_free("cat ./a/b.txt"));
        assert!(is_free("ls -- dir"));
        assert!(is_free("ls a..b"));
        assert!(is_free("grep -rn 'a/b' src docs"));
        assert!(is_free("echo 'quoted | ; & > < $ ` stay literal'"));
    }

    #[test]
    fn always_covers_one_plain_program() {
        assert_eq!(
            always_for("pytest -q tests/"),
            Some(("bash:pytest".to_owned(), "`pytest` commands".to_owned()))
        );
        assert_eq!(
            always_for("  rg foo src").map(|k| k.0),
            Some("bash:rg".to_owned())
        );
        assert_eq!(
            always_for("git status").map(|k| k.0),
            Some("bash:git".to_owned())
        );
    }

    #[test]
    fn always_is_not_offered_when_it_would_allow_too_much() {
        for command in [
            "bash -c 'rm x'",
            "sh script.sh",
            "env FOO=1 rm x",
            "sudo ls",
            "xargs rm",
            "find . -delete",
            "python3 x.py",
            "ls | wc",
            "ls; rm x",
            "make && make install",
            "FOO=bar ls",
            "./run.sh",
            "/bin/rm x",
            "echo $(rm x)",
            "ls > x",
            ". ./x.sh",
            "",
        ] {
            assert_eq!(always_for(command), None, "{command:?}");
        }
    }
}
