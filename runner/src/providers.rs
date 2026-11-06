//! Providers you can switch to from inside the chat (issue 10).
//!
//! A provider is a base URL, a model, an optional API key and optional provider pinning. They
//! come from two places: profile files (`src/profiles/<name>.profile`, read by this program
//! only) and the `providers` table of the database (added with `/provider add`).
//! Both are listed by `/provider`; a name belongs to one of them only.
//!
//! No type here derives `Debug`: a provider holds its API key, and `{:?}` would print it.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use kernel::RouterOptions;

use crate::config::{ENV_BASE_URL, ENV_MODEL, Settings};
use crate::provider::Provider;

/// Longest provider name accepted.
pub const MAX_NAME_CHARS: usize = 40;

/// What a profile file is called: `<name>.profile`.
const PROFILE_SUFFIX: &str = ".profile";

/// One provider to switch to.
#[derive(Clone)]
pub struct ProviderDef {
    pub name: String,
    pub base_url: String,
    pub model: String,
    /// Never printed or logged.
    pub api_key: Option<String>,
    pub router: RouterOptions,
}

/// Where a provider is kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Profile,
    Saved,
}

impl Source {
    pub fn label(self) -> &'static str {
        match self {
            Source::Profile => "profile",
            Source::Saved => "saved",
        }
    }
}

/// One line of the provider list: a usable provider, or why a profile file could not be read.
pub struct Listed {
    pub name: String,
    pub source: Source,
    pub def: Result<ProviderDef, String>,
}

/// A name that is safe as a file name and as a command word: letters, digits, `-`, `_`, `.`;
/// not starting with `.` or `-`.
pub fn check_name(name: &str) -> Result<(), String> {
    check_name_among(name, &["add", "remove"], "/provider")
}

/// The same rules for a name that must not be one of `reserved` (the words of `command`).
pub fn check_name_among(name: &str, reserved: &[&str], command: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("the name is empty".to_owned());
    }
    if name.chars().count() > MAX_NAME_CHARS {
        return Err(format!(
            "the name is longer than {MAX_NAME_CHARS} characters"
        ));
    }
    if reserved.contains(&name) {
        return Err(format!(
            "{name} is a {command} command, so it cannot be a name"
        ));
    }
    if name.starts_with(['.', '-']) {
        return Err("the name must not start with '.' or '-'".to_owned());
    }
    if let Some(bad) = name
        .chars()
        .find(|c| !(c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')))
    {
        return Err(format!(
            "the name may only use letters, digits, '-', '_' and '.', not '{bad}'"
        ));
    }
    Ok(())
}

/// A base URL must name a web address the program can post to.
pub fn check_base_url(base_url: &str) -> Result<(), String> {
    let rest = base_url
        .strip_prefix("https://")
        .or_else(|| base_url.strip_prefix("http://"))
        .ok_or("the base URL must start with http:// or https://")?;
    if rest.is_empty() || rest.starts_with('/') || base_url.chars().any(char::is_whitespace) {
        return Err("the base URL is not a web address".to_owned());
    }
    Ok(())
}

/// A model id is one word: no spaces or control characters.
pub fn check_model(model: &str) -> Result<(), String> {
    if model.is_empty() {
        return Err("the model is empty".to_owned());
    }
    if model.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err("a model id has no spaces or control characters".to_owned());
    }
    Ok(())
}

/// Reads the text of a profile file. Only plain `KEY=VALUE` lines are understood (the same
/// lines; the shell never sources these files): comments and blank lines are skipped, a trailing ` # comment` on an
/// unquoted value is dropped, and one pair of surrounding quotes is removed. A value that the
/// shell would expand (`$` or a backtick) is refused rather than guessed at.
pub fn parse_profile(name: &str, text: &str) -> Result<ProviderDef, String> {
    let mut values: HashMap<String, String> = HashMap::new();
    for (index, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line);
        let Some((key, value)) = line.split_once('=') else {
            return Err(format!("line {} is not KEY=VALUE", index + 1));
        };
        let key = key.trim();
        if key.is_empty() || key.contains(char::is_whitespace) {
            return Err(format!("line {} is not KEY=VALUE", index + 1));
        }
        let value = unquote(value.trim());
        if value.contains('$') || value.contains('`') {
            return Err(format!(
                "line {} uses shell expansion, which is not understood here",
                index + 1
            ));
        }
        values.insert(key.to_owned(), value);
    }
    for required in [ENV_BASE_URL, ENV_MODEL] {
        if values.get(required).is_none_or(|v| v.trim().is_empty()) {
            return Err(format!("{required} is missing"));
        }
    }
    // The pinning rules (true/false, comma lists) are the ones the launch settings use.
    let settings = Settings::from_lookup(|key| values.get(key).cloned())?;
    check_base_url(&settings.base_url)?;
    check_model(&settings.model)?;
    Ok(ProviderDef {
        name: name.to_owned(),
        base_url: settings.base_url,
        model: settings.model,
        api_key: settings.api_key,
        router: settings.router,
    })
}

/// Drops a trailing ` # comment` from an unquoted value, and one pair of matching quotes.
fn unquote(value: &str) -> String {
    for quote in ['"', '\''] {
        if let Some(inner) = value
            .strip_prefix(quote)
            .and_then(|rest| rest.split_once(quote))
        {
            return inner.0.to_owned();
        }
    }
    match value.find(" #") {
        Some(at) => value[..at].trim_end().to_owned(),
        None => value.to_owned(),
    }
}

/// The profile files in `dir`, by name. A file that cannot be read is listed with the reason.
/// A missing directory gives an empty list.
pub fn read_profiles(dir: &Path) -> Vec<Listed> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut found: Vec<Listed> = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let file = entry.file_name().to_string_lossy().into_owned();
            let name = file.strip_suffix(PROFILE_SUFFIX)?.to_owned();
            check_name(&name).ok()?;
            let def = std::fs::read_to_string(entry.path())
                .map_err(|e| format!("cannot read the file: {e}"))
                .and_then(|text| parse_profile(&name, &text));
            Some(Listed {
                name,
                source: Source::Profile,
                def,
            })
        })
        .collect();
    found.sort_by(|a, b| a.name.cmp(&b.name));
    found
}

/// Turns a provider definition into something that answers requests.
pub type BuildProvider = Box<dyn Fn(&ProviderDef) -> Box<dyn Provider>>;

/// Where the program can find providers, and how to turn one into something that answers
/// requests. The second part is a function so tests can answer with a scripted provider.
pub struct Catalog {
    pub profile_dir: Option<PathBuf>,
    pub build: BuildProvider,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plain_profile_is_read() {
        let def = parse_profile(
            "or",
            "# a comment\n\nHARNESS_BASE_URL=https://openrouter.ai/api/v1\nHARNESS_MODEL=deepseek/x\nHARNESS_API_KEY=sk-test\n",
        )
        .unwrap();
        assert_eq!(def.name, "or");
        assert_eq!(def.base_url, "https://openrouter.ai/api/v1");
        assert_eq!(def.model, "deepseek/x");
        assert_eq!(def.api_key.as_deref(), Some("sk-test"));
        assert_eq!(def.router, RouterOptions::default());
    }

    #[test]
    fn quotes_export_and_trailing_comments_are_understood() {
        let def = parse_profile(
            "q",
            "export HARNESS_BASE_URL=\"http://localhost:1/v1\"\nHARNESS_MODEL='m' \nHARNESS_API_KEY=abc # not part of it\n",
        )
        .unwrap();
        assert_eq!(def.base_url, "http://localhost:1/v1");
        assert_eq!(def.model, "m");
        assert_eq!(def.api_key.as_deref(), Some("abc"));
    }

    #[test]
    fn pinning_is_read_like_the_launch_settings_read_it() {
        let def = parse_profile(
            "p",
            "HARNESS_BASE_URL=http://h/v1\nHARNESS_MODEL=m\nHARNESS_PROVIDER_ORDER=relace/fp4, other\nHARNESS_ALLOW_FALLBACKS=false\nHARNESS_SESSION_ID=harness-test\n",
        )
        .unwrap();
        assert_eq!(def.router.provider_order, ["relace/fp4", "other"]);
        assert_eq!(def.router.allow_fallbacks, Some(false));
        assert_eq!(def.router.session_id.as_deref(), Some("harness-test"));
        let bad = parse_profile(
            "p",
            "HARNESS_BASE_URL=http://h/v1\nHARNESS_MODEL=m\nHARNESS_ALLOW_FALLBACKS=maybe\n",
        );
        assert!(bad.err().unwrap().contains("true or false"));
    }

    #[test]
    fn a_profile_that_cannot_be_understood_says_why() {
        let missing = parse_profile("x", "HARNESS_MODEL=m\n");
        assert_eq!(missing.err().unwrap(), "HARNESS_BASE_URL is missing");
        let shell = parse_profile(
            "x",
            "HARNESS_BASE_URL=http://h/v1\nHARNESS_MODEL=m\nHARNESS_API_KEY=$KEY\n",
        );
        assert!(shell.err().unwrap().contains("shell expansion"));
        let junk = parse_profile("x", "HARNESS_BASE_URL=http://h/v1\nnot a pair\n");
        assert_eq!(junk.err().unwrap(), "line 2 is not KEY=VALUE");
        let url = parse_profile("x", "HARNESS_BASE_URL=ftp://h\nHARNESS_MODEL=m\n");
        assert!(url.err().unwrap().contains("http://"));
    }

    #[test]
    fn names_are_checked() {
        for good in ["openrouter", "9router", "my-provider_2", "a.b"] {
            assert!(check_name(good).is_ok(), "{good}");
        }
        for bad in [
            "",
            ".hidden",
            "-x",
            "has space",
            "a/b",
            "../x",
            "ünï",
            "add",
            "remove",
            &"a".repeat(41),
        ] {
            assert!(check_name(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn urls_and_models_are_checked() {
        assert!(check_base_url("http://localhost:20128/v1").is_ok());
        assert!(check_base_url("https://openrouter.ai/api/v1").is_ok());
        for bad in [
            "",
            "localhost:1",
            "http://",
            "http:///x",
            "https://a b/v1",
            "file:///x",
        ] {
            assert!(check_base_url(bad).is_err(), "{bad}");
        }
        assert!(check_model("deepseek/deepseek-v4:free").is_ok());
        assert!(check_model("").is_err());
        assert!(check_model("two words").is_err());
    }

    #[test]
    fn profile_files_are_listed_by_name_with_unreadable_ones_marked() {
        let dir = crate::tools::testutil::Scratch::new("profiles");
        let write = |file: &str, text: &str| std::fs::write(dir.path().join(file), text).unwrap();
        write(
            "b.profile",
            "HARNESS_BASE_URL=http://b/v1\nHARNESS_MODEL=mb\n",
        );
        write(
            "a.profile",
            "HARNESS_BASE_URL=http://a/v1\nHARNESS_MODEL=ma\n",
        );
        write("broken.profile", "HARNESS_MODEL=m\n");
        write(
            "a.profile.example",
            "HARNESS_BASE_URL=http://x/v1\nHARNESS_MODEL=mx\n",
        );
        write("notes.txt", "ignored");
        let listed = read_profiles(dir.path());
        let names: Vec<&str> = listed.iter().map(|l| l.name.as_str()).collect();
        assert_eq!(names, ["a", "b", "broken"]);
        assert!(listed[0].def.is_ok() && listed[1].def.is_ok());
        assert_eq!(
            listed[2].def.as_ref().err().map(String::as_str),
            Some("HARNESS_BASE_URL is missing")
        );
        assert!(listed.iter().all(|l| l.source == Source::Profile));
        assert!(read_profiles(&dir.path().join("missing")).is_empty());
    }
}
