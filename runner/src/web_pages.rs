//! The HTML of the web view (issues 28 and 29): plain pages and forms, one inline stylesheet that
//! follows the system light or dark setting, no JavaScript.
//!
//! Every stored text, and every value typed into a form, goes through [`escape`] before it is put in
//! a page. The pages are built from [`Data`], which holds only prompts and glossary entries:
//! nothing else of the database is reachable from here. Every form carries the run's token
//! ([`View::token`]); what a form does is decided in `web.rs`.

use crate::store_context::{ContextDraft, ContextEntry, KINDS};
use crate::store_glossary::Entry;
use crate::store_profiles::Profile;
use crate::store_prompts::{Prompt, PromptType, size_label};
use crate::turn_log::utc_label;

/// What the pages show; loaded from the database for each request.
pub struct Data {
    /// The profile new chats start under; "on" for a system prompt means "in this profile".
    pub profile: String,
    pub profiles: Vec<Profile>,
    pub prompts: Vec<Prompt>,
    pub glossary: Vec<Entry>,
}

/// Which projects the Glossary and Context pages show. The web view reads the whole database, so
/// without this every project's words would be listed together. The default is the project the
/// web view was started in plus what is shared; the choice is the `for` word in the address.
#[derive(Debug, Clone, Copy)]
pub struct Filter<'a> {
    /// `""` (the default: this project and the shared ones), `all`, `shared`, or a project folder
    /// (that project and the shared ones).
    pub choice: &'a str,
    /// The folder the web view was started in.
    pub here: &'a str,
}

impl<'a> Filter<'a> {
    pub fn new(choice: &'a str, here: &'a str) -> Self {
        Self { choice, here }
    }

    /// Everything, for a page shown again after a failed change.
    pub fn all() -> Self {
        Self {
            choice: "all",
            here: "",
        }
    }

    pub fn shows(&self, project: &str) -> bool {
        match self.choice {
            "all" => true,
            "shared" => project.is_empty(),
            "" => project.is_empty() || project == self.here,
            other => project.is_empty() || project == other,
        }
    }

    /// The drop-down, with the other projects that have anything saved.
    fn select(&self, projects: impl Iterator<Item = &'a str>) -> String {
        let mut others: Vec<&str> = projects
            .filter(|p| !p.is_empty() && *p != self.here)
            .collect();
        others.sort_unstable();
        others.dedup();
        let option = |value: &str, label: &str| {
            let selected = if value == self.choice {
                " selected"
            } else {
                ""
            };
            format!(
                "<option value=\"{}\"{selected}>{}</option>",
                escape(value),
                escape(label)
            )
        };
        let mut html = String::from("<select name=\"for\">");
        html.push_str(&option("", &format!("this project: {}", self.here)));
        html.push_str(&option("all", "all projects"));
        html.push_str(&option("shared", "shared only"));
        for other in others {
            html.push_str(&option(other, other));
        }
        html.push_str("</select>");
        html
    }
}

/// What a page needs besides the data: the token its forms carry, a notice for what was just
/// done (one of the fixed words in [`NOTICES`]), and an error to show above the form.
#[derive(Debug, Clone, Copy)]
pub struct View<'a> {
    pub token: &'a str,
    pub ok: Option<&'a str>,
    pub error: Option<&'a str>,
}

impl<'a> View<'a> {
    pub fn new(token: &'a str) -> Self {
        Self {
            token,
            ok: None,
            error: None,
        }
    }
}

/// What a glossary form holds: typed values that are put back after a refusal.
#[derive(Debug, Clone, Default)]
pub struct GlossaryDraft {
    pub term: String,
    pub area: String,
    pub definition: String,
    /// The project folder a new word is for; empty means every project.
    pub project: String,
}

/// What a prompt form holds.
#[derive(Debug, Clone, Default)]
pub struct PromptDraft {
    pub name: String,
    pub description: String,
    pub text: String,
}

/// What a profile form holds.
#[derive(Debug, Clone, Default)]
pub struct ProfileDraft {
    pub name: String,
    pub description: String,
    pub prompts: Vec<String>,
    pub skills: Vec<String>,
    /// The personality's name, empty for none.
    pub personality: String,
}

impl ProfileDraft {
    pub fn of(profile: &Profile) -> Self {
        Self {
            name: profile.name.clone(),
            description: profile.description.clone(),
            prompts: profile.prompts.clone(),
            skills: profile.skills.clone(),
            personality: profile.personality.clone().unwrap_or_default(),
        }
    }
}

/// The words a `?ok=` can carry, and the notice each one shows. Anything else shows nothing.
const NOTICES: [(&str, &str); 12] = [
    ("added", "Added."),
    ("saved", "Saved."),
    ("deleted", "Deleted."),
    ("on", "Switched on."),
    ("off", "Switched off."),
    ("moved", "Moved."),
    ("used", "New chats now start under it."),
    ("copied", "Copied."),
    ("rejected", "Rejected."),
    ("exported", "Exported: the copy is listed under Exports."),
    (
        "imported",
        "Kept as a space: open it by starting the program with --space NAME.",
    ),
    (
        "wiped",
        "Wiped. The database as it was is saved: see Backups.",
    ),
];

/// The text of a notice word, if it is one.
pub fn notice(word: &str) -> Option<&'static str> {
    NOTICES.iter().find(|(w, _)| *w == word).map(|(_, t)| *t)
}

/// `<`, `>`, `&`, `"` and `'` made harmless for a page.
pub fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

const STYLE: &str = "\
:root{--bg:#fbfbfa;--fg:#1d1d1b;--dim:#6b6b66;--line:#dcdcd6;--card:#fff;--accent:#0a6c74}\
@media (prefers-color-scheme:dark){:root{--bg:#161615;--fg:#ececea;--dim:#9a9a94;--line:#33332f;--card:#1e1e1c;--accent:#5cc8d0}}\
body{margin:0;background:var(--bg);color:var(--fg);font:16px/1.5 system-ui,sans-serif}\
main{max-width:56rem;margin:0 auto;padding:1rem 16px 3rem}\
nav{display:flex;flex-wrap:wrap;gap:.25rem 1rem;padding:.75rem 0;border-bottom:1px solid var(--line);margin-bottom:1rem}\
nav a{color:var(--dim);text-decoration:none}nav a[aria-current]{color:var(--accent);font-weight:600}\
h1{font-size:1.4rem;margin:.5rem 0 1rem}a{color:var(--accent)}\
details{background:var(--card);border:1px solid var(--line);border-radius:6px;margin:.5rem 0;padding:.5rem .75rem}\
summary{cursor:pointer}.dim{color:var(--dim)}\
pre{white-space:pre-wrap;word-break:break-word;font:14px/1.45 ui-monospace,monospace;margin:.5rem 0 0}\
table{border-collapse:collapse;width:100%}th,td{text-align:left;vertical-align:top;padding:.4rem .5rem;border-bottom:1px solid var(--line)}\
td.def{white-space:pre-wrap;word-break:break-word}input[type=search]{font:inherit;padding:.3rem .5rem;width:min(24rem,100%)}\
button{font:inherit;padding:.3rem .8rem;cursor:pointer}input,button,textarea{background:var(--card);color:var(--fg);border:1px solid var(--line);border-radius:4px}\
label{display:block;margin:.6rem 0 .15rem;color:var(--dim)}input[type=text],textarea{font:inherit;width:100%;box-sizing:border-box;padding:.35rem .5rem}\
textarea{font:14px/1.45 ui-monospace,monospace;min-height:12rem}form.inline{display:inline}\
.actions{display:flex;flex-wrap:wrap;gap:.4rem .8rem;align-items:center;margin:.5rem 0}.actions button{padding:.1rem .6rem}\
.notice{background:var(--card);border:1px solid var(--accent);border-radius:4px;padding:.4rem .75rem}\
.error{border:1px solid #c0392b;border-radius:4px;padding:.4rem .75rem;color:#c0392b}\
@media (prefers-color-scheme:dark){.error{color:#ff8a7a;border-color:#ff8a7a}}button.danger{border-color:#c0392b}\
h2{font-size:1.1rem;margin:1.5rem 0 .5rem}code{font:13px ui-monospace,monospace}.relation{margin:.5rem 0 1.25rem}.relation p{margin:.1rem 0}\
.check{display:flex;gap:.5rem;align-items:baseline;color:var(--fg);margin:.2rem 0}fieldset{border:1px solid var(--line);border-radius:6px;margin:.75rem 0;padding:.25rem .75rem .5rem}\
legend{color:var(--dim);padding:0 .3rem}.badge{border:1px solid var(--accent);color:var(--accent);border-radius:4px;padding:0 .4rem;font-size:.85rem}\
.erd{overflow:auto;border:1px solid var(--line);border-radius:6px;background:var(--card);margin:.75rem 0;position:relative;left:50%;transform:translateX(-50%);width:min(1500px,calc(100vw - 32px))}\
.diff .add{color:#1a7f37}.diff .del{color:#c0392b}\
@media (prefers-color-scheme:dark){.diff .add{color:#7ee2a0}.diff .del{color:#ff8a7a}}\
.erd .edge:hover .line{stroke-width:3;stroke:var(--accent);opacity:1}.card{background:var(--card);border:1px solid var(--line);border-radius:6px;margin:.6rem 0;padding:.5rem .75rem}.card p{margin:.2rem 0}";

const PAGES: [(&str, &str); 13] = [
    ("/", "Home"),
    ("/profiles", "Profiles"),
    ("/prompts", "Prompts"),
    ("/skills", "Skills"),
    ("/personalities", "Personalities"),
    ("/glossary", "Glossary"),
    ("/context", "Context"),
    ("/inbox", "Inbox"),
    ("/tools", "Tools"),
    ("/usage", "Usage"),
    ("/database", "Database"),
    ("/data", "Data"),
    ("/help", "Help"),
];

pub(crate) fn page(active: &str, title: &str, body: &str) -> String {
    let nav: String = PAGES
        .iter()
        .map(|(path, name)| {
            let current = if *path == active {
                " aria-current=\"page\""
            } else {
                ""
            };
            format!("<a href=\"{path}\"{current}>{name}</a>")
        })
        .collect::<Vec<_>>()
        .join("");
    format!(
        "<!doctype html>\n<html lang=\"en\"><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
         <title>{} - Harness</title><style>{STYLE}</style></head>\
         <body><main><nav>{nav}</nav><h1>{}</h1>{body}</main></body></html>\n",
        escape(title),
        escape(title)
    )
}

pub fn not_found() -> String {
    page(
        "",
        "Not found",
        "<p>There is no such page. <a href=\"/\">Home</a></p>",
    )
}

pub fn error(why: &str) -> String {
    page("", "Cannot show this", &format!("<p>{}</p>", escape(why)))
}

/// The page for an edit or delete that found the row changed since the form was shown. `typed`
/// is what the person had typed, so it is not lost.
pub fn stale(back: &str, what: &str, typed: &str) -> String {
    let typed = if typed.trim().is_empty() {
        String::new()
    } else {
        format!(
            "<p class=\"dim\">What you had typed:</p><pre>{}</pre>",
            escape(typed)
        )
    };
    page(
        "",
        "Changed meanwhile",
        &format!(
            "<p class=\"error\" role=\"alert\">This {} was changed after you opened it, so nothing was saved.</p><p><a href=\"{}\">Open the current version</a></p>{typed}",
            escape(what),
            escape(back)
        ),
    )
}

pub fn simple(title: &str, text: &str) -> String {
    page("", title, &format!("<p>{}</p>", escape(text)))
}

pub(crate) fn banner(view: &View<'_>) -> String {
    let mut out = String::new();
    if let Some(text) = view.ok.and_then(notice) {
        out.push_str(&format!(
            "<p class=\"notice\" role=\"status\">{}</p>",
            escape(text)
        ));
    }
    if let Some(why) = view.error {
        out.push_str(&format!(
            "<p class=\"error\" role=\"alert\">{}</p>",
            escape(why)
        ));
    }
    out
}

pub(crate) fn hidden(name: &str, value: &str) -> String {
    format!(
        "<input type=\"hidden\" name=\"{}\" value=\"{}\">",
        escape(name),
        escape(value)
    )
}

/// A small form of one button (and hidden fields) that posts to `action`.
fn button_form(view: &View<'_>, action: &str, fields: &[(&str, &str)], label: &str) -> String {
    let fields: String = fields.iter().map(|(n, v)| hidden(n, v)).collect();
    format!(
        "<form class=\"inline\" method=\"post\" action=\"{action}\">{}{fields}<button>{}</button></form>",
        hidden("token", view.token),
        escape(label)
    )
}

pub(crate) fn text_field(
    label: &str,
    name: &str,
    value: &str,
    max_chars: usize,
    required: bool,
) -> String {
    format!(
        "<label for=\"f-{name}\">{}</label><input type=\"text\" id=\"f-{name}\" name=\"{name}\" value=\"{}\" maxlength=\"{max_chars}\"{}>",
        escape(label),
        escape(value),
        if required { " required" } else { "" }
    )
}

fn text_area(label: &str, name: &str, value: &str) -> String {
    // The first newline of a textarea is dropped by the browser, so one is put in front.
    format!(
        "<label for=\"f-{name}\">{}</label><textarea id=\"f-{name}\" name=\"{name}\" required>\n{}</textarea>",
        escape(label),
        escape(value)
    )
}

fn count(data: &Data, kind: PromptType) -> usize {
    data.prompts
        .iter()
        .filter(|p| p.prompt_type == kind)
        .count()
}

pub fn index(data: &Data, view: &View<'_>) -> String {
    index_with(data, view, None)
}

/// The home page; `today` is today's token use, if the database could be asked.
pub fn index_with(
    data: &Data,
    view: &View<'_>,
    today: Option<&crate::web_usage::Totals>,
) -> String {
    let rows = [
        ("/profiles", "Profiles", data.profiles.len()),
        ("/prompts", "Prompts", count(data, PromptType::System)),
        ("/skills", "Skills", count(data, PromptType::Skill)),
        (
            "/personalities",
            "Personalities",
            count(data, PromptType::Personality),
        ),
        ("/glossary", "Glossary", data.glossary.len()),
    ];
    let list: String = rows
        .iter()
        .map(|(path, name, n)| {
            format!("<li><a href=\"{path}\">{name}</a> <span class=\"dim\">({n})</span></li>")
        })
        .collect();
    let today = match today {
        Some(t) => format!(
            "<li><a href=\"/usage\">Usage</a> <span class=\"dim\">(today: {} calls, {} prompt and {} output tokens)</span></li>",
            t.calls,
            crate::web_usage::compact(t.prompt),
            crate::web_usage::compact(t.completion)
        ),
        None => "<li><a href=\"/usage\">Usage</a></li>".to_owned(),
    };
    page(
        "/",
        "Harness",
        &format!(
            "{}<p>New chats start under the profile <a href=\"/profiles\"><strong>{}</strong></a>.</p>\
             <p class=\"dim\">What the chat keeps. You can add, change and delete here; a chat that is already open keeps the system prompt it started with.</p><ul>{list}{today}\
             <li><a href=\"/database\">Database</a> <span class=\"dim\">(tables and how they relate)</span></li></ul>",
            banner(view),
            escape(&data.profile)
        ),
    )
}

/// The list page and path of a kind of prompt.
pub fn list_path(kind: PromptType) -> &'static str {
    match kind {
        PromptType::System => "/prompts",
        PromptType::Skill => "/skills",
        PromptType::Personality => "/personalities",
    }
}

fn kind_title(kind: PromptType) -> &'static str {
    match kind {
        PromptType::System => "Prompts",
        PromptType::Skill => "Skills",
        PromptType::Personality => "Personalities",
    }
}

/// The prompts of one kind: an add form, then a fold per prompt with its text and what can be done.
pub fn prompts(data: &Data, kind: PromptType, view: &View<'_>, draft: &PromptDraft) -> String {
    let (path, title) = (list_path(kind), kind_title(kind));
    let mut list: Vec<&Prompt> = data
        .prompts
        .iter()
        .filter(|p| p.prompt_type == kind)
        .collect();
    list.sort_by_key(|p| (p.position, p.id));
    let mut body = banner(view);
    body.push_str(&format!(
        "<details{}><summary>Add</summary><form method=\"post\" action=\"/prompt/add\">{}{}{}{}{}<p><button>Add</button></p></form></details>",
        if view.error.is_some() { " open" } else { "" },
        hidden("token", view.token),
        hidden("type", kind.as_str()),
        text_field("Name", "name", &draft.name, 60, true),
        text_field("Description", "description", &draft.description, 200, false),
        text_area("Text", "text", &draft.text),
    ));
    if list.is_empty() {
        body.push_str("<p class=\"dim\">None yet.</p>");
        return page(path, title, &body);
    }
    if kind == PromptType::System {
        body.push_str(&format!(
            "<p class=\"dim\">The prompts in the profile <strong>{}</strong>, in this order, make the system prompt of a new chat. Profiles are chosen in the chat with /profile.</p>",
            escape(&data.profile)
        ));
    } else {
        body.push_str(
            "<p class=\"dim\">Which ones are on is set inside each chat, so it is not shown here.</p>",
        );
    }
    for prompt in list {
        let state = match (kind, prompt.enabled) {
            (PromptType::System, true) => " &middot; <strong>in the profile</strong>",
            (PromptType::System, false) => " &middot; not in it",
            _ => "",
        };
        let description = if prompt.description.is_empty() {
            String::new()
        } else {
            format!(" &mdash; {}", escape(&prompt.description))
        };
        let source = match &prompt.source_path {
            Some(file) => format!("<div class=\"dim\">copied from {}</div>", escape(file)),
            None => String::new(),
        };
        let name = [("name", prompt.name.as_str())];
        let mut actions = format!(
            "<a href=\"/prompt/edit?id={id}\">Edit</a><a href=\"/prompt/delete?id={id}\">Delete</a>",
            id = prompt.id
        );
        if kind == PromptType::System {
            let (to, label) = if prompt.enabled {
                ("0", "Take out of the profile")
            } else {
                ("1", "Put in the profile")
            };
            actions.push_str(&button_form(
                view,
                "/prompt/enable",
                &[name[0], ("on", to)],
                label,
            ));
        }
        actions.push_str(&button_form(
            view,
            "/prompt/move",
            &[name[0], ("by", "-1")],
            "Up",
        ));
        actions.push_str(&button_form(
            view,
            "/prompt/move",
            &[name[0], ("by", "1")],
            "Down",
        ));
        body.push_str(&format!(
            "<details><summary><strong>{}</strong>{description} <span class=\"dim\">({}{state})</span></summary>{source}<div class=\"actions\">{actions}</div><pre>{}</pre></details>",
            escape(&prompt.name),
            size_label(prompt.chars()),
            escape(&prompt.text)
        ));
    }
    page(path, title, &body)
}

/// The edit form of one prompt. `v` is the stamp of the row as it was when the form was first shown.
pub fn prompt_edit(prompt: &Prompt, v: &str, view: &View<'_>, draft: &PromptDraft) -> String {
    let body = format!(
        "{}<form method=\"post\" action=\"/prompt/edit\">{}{}{}{}{}{}<div class=\"actions\"><button>Save</button><a href=\"{}\">Cancel</a></div></form>",
        banner(view),
        hidden("token", view.token),
        hidden("id", &prompt.id.to_string()),
        hidden("v", v),
        text_field("Name", "name", &draft.name, 60, true),
        text_field("Description", "description", &draft.description, 200, false),
        text_area("Text", "text", &draft.text),
        list_path(prompt.prompt_type),
    );
    page(
        list_path(prompt.prompt_type),
        &format!("Edit {}", prompt.name),
        &body,
    )
}

/// The question before a prompt is deleted.
pub fn prompt_delete(prompt: &Prompt, view: &View<'_>) -> String {
    let body = format!(
        "{}<p>Delete <strong>{}</strong> ({})? This cannot be undone. Chats that had chosen it will no longer have it.</p><pre>{}</pre>\
         <form method=\"post\" action=\"/prompt/delete\">{}{}{}<div class=\"actions\"><button class=\"danger\">Delete</button><a href=\"{}\">Cancel</a></div></form>",
        banner(view),
        escape(&prompt.name),
        prompt.prompt_type.as_str(),
        escape(&prompt.text),
        hidden("token", view.token),
        hidden("id", &prompt.id.to_string()),
        hidden("v", &prompt.fingerprint()),
        list_path(prompt.prompt_type),
    );
    page(
        list_path(prompt.prompt_type),
        &format!("Delete {}?", prompt.name),
        &body,
    )
}

/// The glossary as a table, filtered by `query` (words and definitions containing it, ignoring case),
/// with the add form above it.
pub fn glossary(
    data: &Data,
    query: &str,
    filter: &Filter<'_>,
    view: &View<'_>,
    draft: &GlossaryDraft,
) -> String {
    let needle = query.trim().to_lowercase();
    let in_scope: Vec<&Entry> = data
        .glossary
        .iter()
        .filter(|e| filter.shows(&e.project))
        .collect();
    let shown: Vec<&Entry> = in_scope
        .iter()
        .copied()
        .filter(|e| {
            needle.is_empty()
                || e.term.to_lowercase().contains(&needle)
                || e.area.to_lowercase().contains(&needle)
                || e.definition.to_lowercase().contains(&needle)
        })
        .collect();
    let mut body = banner(view);
    body.push_str(&format!(
        "<details{}><summary>Add a word</summary><form method=\"post\" action=\"/glossary/add\">{}{}{}{}{}<p><button>Add</button></p></form></details>",
        if view.error.is_some() { " open" } else { "" },
        hidden("token", view.token),
        text_field("Word", "term", &draft.term, 60, true),
        text_field("Area", "area", &draft.area, 40, false),
        text_field(
            "Project folder (empty: every project)",
            "project",
            &draft.project,
            500,
            false
        ),
        text_area("Definition", "definition", &draft.definition),
    ));
    body.push_str(&format!(
        "<form method=\"get\" action=\"/glossary\"><input type=\"search\" name=\"q\" value=\"{}\" placeholder=\"search words and definitions\"> {} <button>Show</button></form>",
        escape(query.trim()),
        filter.select(data.glossary.iter().map(|e| e.project.as_str()))
    ));
    let total = in_scope.len();
    if needle.is_empty() {
        body.push_str(&format!(
            "<p class=\"dim\">{total} words{}.</p>",
            hidden_note(data.glossary.len(), total)
        ));
    } else {
        body.push_str(&format!(
            "<p class=\"dim\">{} of {total} words match.</p>",
            shown.len()
        ));
    }
    if shown.is_empty() {
        return page("/glossary", "Glossary", &body);
    }
    body.push_str(
        "<table><tr><th>Word</th><th>Area</th><th>For</th><th>Definition</th><th>Changed</th><th></th></tr>",
    );
    for entry in shown {
        body.push_str(&format!(
            "<tr><td><strong>{}</strong></td><td>{}</td><td class=\"dim\">{}</td><td class=\"def\">{}</td><td class=\"dim\">{}</td><td><a href=\"/glossary/edit?id={id}\">Edit</a> <a href=\"/glossary/delete?id={id}\">Delete</a></td></tr>",
            escape(&entry.term),
            escape(&entry.area),
            escape(if entry.project.is_empty() { "every project" } else { &entry.project }),
            escape(&entry.definition),
            utc_label(u64::try_from(entry.updated_at).unwrap_or(0)),
            id = entry.id
        ));
    }
    body.push_str("</table>");
    page("/glossary", "Glossary", &body)
}

/// The edit form of one word. `v` is the stamp of the entry as it was when the form was first shown.
pub fn glossary_edit(entry: &Entry, v: &str, view: &View<'_>, draft: &GlossaryDraft) -> String {
    let body = format!(
        "{}<p class=\"dim\">For: {}</p><form method=\"post\" action=\"/glossary/edit\">{}{}{}{}{}{}<div class=\"actions\"><button>Save</button><a href=\"/glossary\">Cancel</a></div></form>",
        banner(view),
        escape(if entry.project.is_empty() {
            "every project"
        } else {
            &entry.project
        }),
        hidden("token", view.token),
        hidden("id", &entry.id.to_string()),
        hidden("v", v),
        text_field("Word", "term", &draft.term, 60, true),
        text_field("Area", "area", &draft.area, 40, false),
        text_area("Definition", "definition", &draft.definition),
    );
    page("/glossary", &format!("Edit {}", entry.term), &body)
}

/// The question before a word is deleted.
pub fn glossary_delete(entry: &Entry, view: &View<'_>) -> String {
    let body = format!(
        "{}<p>Delete <strong>{}</strong> from the glossary? This cannot be undone.</p><p class=\"def\" style=\"white-space:pre-wrap\">{}</p>\
         <form method=\"post\" action=\"/glossary/delete\">{}{}{}<div class=\"actions\"><button class=\"danger\">Delete</button><a href=\"/glossary\">Cancel</a></div></form>",
        banner(view),
        escape(&entry.term),
        escape(&entry.definition),
        hidden("token", view.token),
        hidden("id", &entry.id.to_string()),
        hidden("v", &entry.fingerprint()),
    );
    page("/glossary", &format!("Delete {}?", entry.term), &body)
}

/// The kind chooser of a context form.
fn kind_select(chosen: &str) -> String {
    let options: String = KINDS
        .iter()
        .map(|k| {
            format!(
                "<option value=\"{k}\"{}>{k}</option>",
                if *k == chosen { " selected" } else { "" }
            )
        })
        .collect();
    format!(
        "<label for=\"f-kind\">Kind</label><select id=\"f-kind\" name=\"kind\">{options}</select>"
    )
}

fn preload_box(on: bool) -> String {
    format!(
        "<label class=\"check\"><input type=\"checkbox\" name=\"preload\" value=\"1\"{}> <span>Put it in a subagent's starting prompt <span class=\"dim\">(small, owner's words only)</span></span></label>",
        if on { " checked" } else { "" }
    )
}

/// " (n more in other projects)" when the choice leaves some out.
fn hidden_note(all: usize, shown: usize) -> String {
    if all > shown {
        format!(" ({} more in other projects)", all - shown)
    } else {
        String::new()
    }
}

fn scope_label(entry: &ContextEntry) -> String {
    if entry.project.is_empty() {
        "every project".to_owned()
    } else {
        entry.project.clone()
    }
}

/// The context entries (rules, decisions, shapes, guides) of every project as a table, filtered by
/// `query`, with the add form above it.
pub fn context(
    all_entries: &[ContextEntry],
    query: &str,
    filter: &Filter<'_>,
    view: &View<'_>,
    draft: &ContextDraft,
    scope: &str,
) -> String {
    let needle = query.trim().to_lowercase();
    let entries: Vec<&ContextEntry> = all_entries
        .iter()
        .filter(|e| filter.shows(&e.project))
        .collect();
    let shown: Vec<&ContextEntry> = entries
        .iter()
        .copied()
        .filter(|e| {
            needle.is_empty()
                || e.name.to_lowercase().contains(&needle)
                || e.kind.contains(&needle)
                || e.area.to_lowercase().contains(&needle)
                || e.text.to_lowercase().contains(&needle)
        })
        .collect();
    let mut body = banner(view);
    body.push_str(&format!(
        "<details{}><summary>Add an entry</summary><form method=\"post\" action=\"/context/add\">{}{}{}{}{}{}{}{}<p><button>Add</button></p></form></details>",
        if view.error.is_some() { " open" } else { "" },
        hidden("token", view.token),
        text_field("Name", "name", &draft.name, 60, true),
        kind_select(&draft.kind),
        text_field("Area", "area", &draft.area, 40, false),
        text_area("Text", "text", &draft.text),
        text_field("Your words (the quote)", "quote", &draft.quote, 1000, false),
        text_field(
            "Project folder (empty: every project)",
            "project",
            scope,
            500,
            false
        ),
        preload_box(draft.preload),
    ));
    body.push_str(&format!(
        "<form method=\"get\" action=\"/context\"><input type=\"search\" name=\"q\" value=\"{}\" placeholder=\"search names, kinds, areas and text\"> {} <button>Show</button></form>",
        escape(query.trim()),
        filter.select(all_entries.iter().map(|e| e.project.as_str()))
    ));
    let total = entries.len();
    if needle.is_empty() {
        body.push_str(&format!(
            "<p class=\"dim\">{total} entries{}.</p>",
            hidden_note(all_entries.len(), total)
        ));
    } else {
        body.push_str(&format!(
            "<p class=\"dim\">{} of {total} entries match.</p>",
            shown.len()
        ));
    }
    if shown.is_empty() {
        return page("/context", "Context", &body);
    }
    body.push_str(
        "<table><tr><th>Name</th><th>Kind</th><th>For</th><th>Text</th><th>Changed</th><th></th></tr>",
    );
    for e in shown {
        body.push_str(&format!(
            "<tr><td><strong>{}</strong>{}</td><td>{}</td><td class=\"dim\">{}</td><td class=\"def\">{}</td><td class=\"dim\">{}</td><td><a href=\"/context/edit?id={id}\">Edit</a> <a href=\"/context/delete?id={id}\">Delete</a></td></tr>",
            escape(&e.name),
            if e.preload { " <span class=\"badge\">preload</span>" } else { "" },
            escape(&e.kind),
            escape(&scope_label(e)),
            escape(&e.text),
            utc_label(u64::try_from(e.updated_at).unwrap_or(0)),
            id = e.id
        ));
    }
    body.push_str("</table>");
    page("/context", "Context", &body)
}

/// The edit form of one entry. `v` is the stamp of the entry as it was when the form was first shown.
pub fn context_edit(
    entry: &ContextEntry,
    v: &str,
    view: &View<'_>,
    draft: &ContextDraft,
) -> String {
    let body = format!(
        "{}<p class=\"dim\">For: {}</p><form method=\"post\" action=\"/context/edit\">{}{}{}{}{}{}{}{}{}<div class=\"actions\"><button>Save</button><a href=\"/context\">Cancel</a></div></form>",
        banner(view),
        escape(&scope_label(entry)),
        hidden("token", view.token),
        hidden("id", &entry.id.to_string()),
        hidden("v", v),
        text_field("Name", "name", &draft.name, 60, true),
        kind_select(&draft.kind),
        text_field("Area", "area", &draft.area, 40, false),
        text_area("Text", "text", &draft.text),
        text_field("Your words (the quote)", "quote", &draft.quote, 1000, false),
        preload_box(draft.preload),
    );
    page("/context", &format!("Edit {}", entry.name), &body)
}

/// The question before an entry is deleted.
pub fn context_delete(entry: &ContextEntry, view: &View<'_>) -> String {
    let body = format!(
        "{}<p>Delete <strong>{}</strong> ({}, for {})? This cannot be undone.</p><p class=\"def\" style=\"white-space:pre-wrap\">{}</p>\
         <form method=\"post\" action=\"/context/delete\">{}{}{}<div class=\"actions\"><button class=\"danger\">Delete</button><a href=\"/context\">Cancel</a></div></form>",
        banner(view),
        escape(&entry.name),
        escape(&entry.kind),
        escape(&scope_label(entry)),
        escape(&entry.text),
        hidden("token", view.token),
        hidden("id", &entry.id.to_string()),
        hidden("v", &entry.fingerprint()),
    );
    page("/context", &format!("Delete {}?", entry.name), &body)
}

/// How to use the program, in plain words, with the chat's command list taken from the program
/// itself (`commands`: the command and what it does) so the list cannot go stale.
pub fn help(commands: &[(&str, &str)]) -> String {
    let mut body = String::from(HELP_TOP);
    body.push_str(
        "<h2>Every chat command</h2><table><tr><th>Command</th><th>What it does</th></tr>",
    );
    for (command, what) in commands {
        body.push_str(&format!(
            "<tr><td><code>{}</code></td><td>{}</td></tr>",
            escape(command),
            escape(what)
        ));
    }
    body.push_str("</table>");
    page("/help", "Help", &body)
}

const HELP_TOP: &str = "\
<p class=\"dim\">How to use the harness. The chat is in the terminal; this page is its settings and notebook.</p>\
<h2>Chatting</h2>\
<ul>\
<li>Start it with <code>aharness</code> (installed from the .deb, .rpm or .tar.gz) or, from a source checkout, <code>cd src &amp;&amp; ./chat.sh</code>. Pick a provider with <code>/provider</code>, then type a message. <code>/help</code> lists every command.</li>\
<li>To use it inside a repository, run <code>src/harness.sh</code> from that folder (or link it onto your PATH). Your chats, providers and entries are kept in <code>~/.local/share/aharness</code>, not in the repository; the only thing added there is <code>.Aharness/</code> (subagent notes), which ignores itself.</li>\
<li>A provider is a base URL, a model, a key and the API it speaks: <code>/provider add &lt;name&gt; &lt;base_url&gt; &lt;model&gt; [openai|anthropic]</code>. <code>openai</code> (the default) is chat completions, which OpenRouter and most routers speak; for Claude models on OpenRouter use an id such as <code>anthropic/claude-...</code> and the program asks for prompt caching by itself. <code>anthropic</code> is Anthropic\'s own Messages API (base URL <code>https://api.anthropic.com/v1</code>). The providers menu asks for the name, URL, which API and the key, then lists the models the provider offers so you pick the default (type to filter) instead of typing it; it can change the API later.</li>\
<li>Esc (or Ctrl-C) while the model works cancels the message. <code>@</code> in the box attaches a file or folder of the working directory. A line starting with <code>/</code> lists the matching commands: Tab completes one, Enter completes a part of a command and sends a finished one.</li>\
<li><code>/new</code> starts a fresh chat, <code>/resume</code> goes back to a saved one, <code>/compact</code> replaces a long chat with a summary. The model you choose with <code>/model</code> is the one the next start uses.</li>\
<li><code>/approve</code> chooses how much the model may do alone: <em>manual</em> asks before every file change, saved glossary word or context entry, and command (answer <em>always</em> to stop being asked for glossary words or context for the rest of the run); <em>auto</em> lets file changes and saved words and entries run and still asks for commands. <code>/tools</code> shows what the model has, and the <a href=\"/tools\">Tools</a> page lists the same.</li>\
</ul>\
<h2>This web view</h2>\
<ul>\
<li>Start it from the chat with <code>/web</code> (stop with <code>/web stop</code>), or on its own with <code>aharness --serve</code> (<code>./chat.sh --serve</code> from source). It answers only on this computer (<code>127.0.0.1</code>).</li>\
<li>You can add, change and delete here; the terminal sees the change at once. A chat that is already open keeps the system prompt it started with, so use <code>/new</code> to pick up an edited prompt.</li>\
<li>An edit or delete is refused if someone changed the same item meanwhile, and the page shows what you had typed.</li>\
</ul>\
<h2>Prompts, skills, personalities and profiles</h2>\
<ul>\
<li><a href=\"/prompts\">Prompts</a> make up the system prompt. <a href=\"/skills\">Skills</a> are instructions you switch on for a chat. A <a href=\"/personalities\">personality</a> is a style, one at a time.</li>\
<li>A <a href=\"/profiles\">profile</a> is the starting setup of a new chat: which prompts, skills and personality it begins with. <em>Use</em> makes new chats start under it.</li>\
</ul>\
<h2>Glossary</h2>\
<p>The <a href=\"/glossary\">glossary</a> holds your project's words and what they mean. The model looks a word up before guessing, and may propose a new one, which you confirm in the terminal. A word is for one project (the folder the chat was started in) or for every project; a project's own version of a word wins over the shared one. In the terminal: <code>/glossary</code>.</p>\
<h2>Context: rules, decisions and shapes</h2>\
<p>The <a href=\"/context\">Context</a> page keeps what you have said about how a project works, so you do not have to say it again. Nothing is loaded by itself: the model reads an entry by name only when it needs it.</p>\
<ul>\
<li><strong>Kinds</strong> are labels: <em>rule</em> (always do or never do), <em>decision</em> (we chose X, because Y), <em>shape</em> (what an area means and which way to lean), <em>guide</em> (how to do something).</li>\
<li><strong>Add one</strong> by saying it in the chat: when you state a rule or how your business works, the model may propose it, with your own words as the quote, and you confirm each one. Or add it yourself with <code>/context</code> in the terminal, or with the form on the Context page.</li>\
<li><strong>For</strong>: an entry belongs to one project (the folder the chat was started in) or to every project. A project entry hides an every-project entry of the same name.</li>\
<li><strong>Preload</strong>: an entry marked preload is put in the starting prompt of every subagent (shapes are on by default; about 2,000 tokens at most, the rest is named as left out). Use it sparingly, because it costs tokens on every subagent.</li>\
<li>Limits: a name is up to 60 letters, digits, <code>.</code>, <code>-</code> or <code>_</code>; the text up to 8,000 bytes; the quote up to 1,000 characters.</li>\
</ul>\
<h2>Inbox</h2>\
<p>Another harness (Claude Code, for example) that uses this program\'s MCP server can only <em>propose</em>: its <code>submit</code> tool puts an entry in the <a href=\"/inbox\">Inbox</a> and changes nothing else. You open each one, see it next to what is stored now, and accept or reject it; a rejection keeps your note. Who sent it and why are what the sender says, not checked, and text with control or invisible characters is refused. In the terminal: <code>/inbox</code>, <code>/inbox show N</code>, <code>/inbox accept N</code>, <code>/inbox reject N [note]</code>.</p>\
<ul>\
<li>The server is started by the other harness: <code>--mcp-serve</code> (read and propose, the default), <code>--mcp-serve --read-only</code> (read only) or <code>--mcp-serve --direct</code> (read and write with no review).</li>\
<li>With <code>--decider llm[:PROVIDER[:MODEL]]</code> or <code>--decider jev[:PROVIDER[:MODEL]]</code> a model also suggests <em>accept</em>, <em>reject</em> or <em>unsure</em> for each entry. It is only a suggestion, shown next to the entry and never to the sender, and it never applies anything. It sends the entry\'s text to that provider, so it is off unless you start the server with it.</li>\
<li><code>--eval-decider SPEC</code> tries a decider on 18 labelled cases and prints how it did.</li>\
</ul>\
<h2>Subagents</h2>\
<ul>\
<li>The model can hand a task to a subagent with <code>delegate</code>, or you can start one with <code>/subagent [profile] &lt;task&gt;</code>. It runs in the background while you keep chatting; its answer is added to the chat with your next message, and the model is woken by itself when one finishes.</li>\
<li><code>/subagents</code> lists them, <code>/subagents show N</code> shows one, <code>/subagents stop N</code> stops one. At most 3 run at once and one of them may change files.</li>\
<li>A subagent only looks (reads files, runs plain looking commands, reads the glossary and context, saves notes in <code>.Aharness/tmp/</code>), unless the approval mode is <em>auto</em>, when it may also change files.</li>\
</ul>\
<h2>Usage and Database</h2>\
<p><a href=\"/usage\">Usage</a> shows the tokens used by day, model and chat. <a href=\"/database\">Database</a> draws the tables and how they relate. Neither shows your keys or your messages.</p>";

/// The tools the model is given: the main chat's, then a reader subagent's and a coder's.
pub fn tools(
    catalogue: &crate::tool_list::Catalogue,
    servers: &[(
        crate::store_mcp::McpServer,
        Vec<(String, crate::store_mcp::Rule)>,
    )],
) -> String {
    let group = |title: &str, note: &str, list: &[crate::tool_list::ToolInfo]| -> String {
        let mut out = format!(
            "<h2>{}</h2><p class=\"dim\">{}</p>",
            escape(title),
            escape(note)
        );
        for tool in list {
            out.push_str(&format!(
                "<div class=\"card\"><p><strong><code>{}</code></strong></p><p class=\"def\" style=\"white-space:pre-wrap\">{}</p></div>",
                escape(&tool.signature()),
                escape(&tool.description)
            ));
        }
        out
    };
    let names = |list: &[crate::tool_list::ToolInfo]| {
        list.iter()
            .map(|t| t.name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    };
    let mut body = String::from(
        "<p class=\"dim\">What the model is given, read from the program itself. An argument ending in ? is optional. The context tool is offered only once an entry exists.</p>",
    );
    body.push_str(&group(
        &format!("The chat ({} tools)", catalogue.chat.len()),
        "Asks you before it changes a file or runs a command that is not a plain look.",
        &catalogue.chat,
    ));
    body.push_str(&format!(
        "<h2>A subagent</h2><p>As a reader: <code>{}</code></p><p>As a coder (when the approval mode is auto): <code>{}</code></p>",
        escape(&names(&catalogue.reader)),
        escape(&names(&catalogue.coder))
    ));
    body.push_str("<h2>MCP servers</h2>");
    if servers.is_empty() {
        body.push_str(
            "<p class=\"dim\">None. <code>/mcp add NAME COMMAND</code> in the chat adds one.</p>",
        );
    } else {
        body.push_str("<p class=\"dim\">Off in a new chat; <code>/mcp on NAME</code> starts one in the chat you are in. Their tools are named <code>mcp__server__tool</code> and appear in <code>/tools</code> once on. Rules and settings apply the next time a server is switched on. Variable values are never shown.</p>");
    }
    for (server, rules) in servers {
        let mut facts = vec![
            format!("timeout {}s", server.timeout_secs),
            format!("other tools: {}", server.default_rule.name()),
        ];
        if !server.vars.is_empty() {
            let names: Vec<&str> = server.vars.iter().map(|(n, _)| n.as_str()).collect();
            facts.push(format!("variables: {}", names.join(", ")));
        }
        for (tool, rule) in rules {
            facts.push(format!("{tool}: {}", rule.name()));
        }
        body.push_str(&format!(
            "<div class=\"card\"><p><strong><code>{}</code></strong>{} <code>{} {}</code></p><p class=\"dim\">{}</p></div>",
            escape(&server.name),
            if server.enabled { "" } else { " (off)" },
            escape(&server.command),
            escape(&server.args.join(" ")),
            escape(&facts.join("; "))
        ));
    }
    page("/tools", "Tools", &body)
}

fn prompts_of_kind(data: &Data, kind: PromptType) -> Vec<&Prompt> {
    let mut list: Vec<&Prompt> = data
        .prompts
        .iter()
        .filter(|p| p.prompt_type == kind)
        .collect();
    list.sort_by_key(|p| (p.position, p.id));
    list
}

fn names_or_none(names: &[String]) -> String {
    if names.is_empty() {
        "<span class=\"dim\">none</span>".to_owned()
    } else {
        names
            .iter()
            .map(|n| format!("<code>{}</code>", escape(n)))
            .collect::<Vec<_>>()
            .join(" ")
    }
}

/// The profiles, with the form that adds one.
pub fn profiles(data: &Data, view: &View<'_>, draft: &ProfileDraft) -> String {
    let mut body = banner(view);
    body.push_str(&format!(
        "<p class=\"dim\">A profile is the setup a chat starts with: its system prompts, the skills that are on and a personality. \
         A change applies to the next chat; a chat that is open keeps what it began with.</p>\
         <details{}><summary>Add</summary><form method=\"post\" action=\"/profile/add\">{}{}{}<p>It starts empty; Edit fills it.</p><p><button>Add</button></p></form></details>",
        if view.error.is_some() { " open" } else { "" },
        hidden("token", view.token),
        text_field("Name", "name", &draft.name, 60, true),
        text_field("Description", "description", &draft.description, 200, false),
    ));
    for profile in &data.profiles {
        let selected = profile.name.eq_ignore_ascii_case(&data.profile);
        let id = profile.id;
        let mut actions = format!("<a href=\"/profile/edit?id={id}\">Edit</a>");
        if selected {
            actions.push_str("<span class=\"dim\">in use, so it cannot be deleted</span>");
        } else {
            actions.push_str(&button_form(
                view,
                "/profile/use",
                &[("name", &profile.name)],
                "Use for new chats",
            ));
            actions.push_str(&format!("<a href=\"/profile/delete?id={id}\">Delete</a>"));
        }
        actions.push_str(&format!(
            "<form class=\"inline\" method=\"post\" action=\"/profile/copy\">{}{}<input type=\"text\" name=\"new\" placeholder=\"name of the copy\" aria-label=\"name of the copy\" maxlength=\"60\" required style=\"width:11rem\"> <button>Copy</button></form>",
            hidden("token", view.token),
            hidden("name", &profile.name)
        ));
        let description = if profile.description.is_empty() {
            String::new()
        } else {
            format!("<p class=\"dim\">{}</p>", escape(&profile.description))
        };
        body.push_str(&format!(
            "<div class=\"card\"><strong>{}</strong>{}{description}\
             <p>System prompts: {}</p><p>Skills on: {}</p><p>Personality: {}</p><div class=\"actions\">{actions}</div></div>",
            escape(&profile.name),
            if selected {
                " <span class=\"badge\">new chats start under it</span>"
            } else {
                ""
            },
            names_or_none(&profile.prompts),
            names_or_none(&profile.skills),
            profile
                .personality
                .as_deref()
                .map_or("<span class=\"dim\">none</span>".to_owned(), |p| format!(
                    "<code>{}</code>",
                    escape(p)
                )),
        ));
    }
    page("/profiles", "Profiles", &body)
}

fn checks(name: &str, list: &[&Prompt], chosen: &[String]) -> String {
    if list.is_empty() {
        return "<p class=\"dim\">None in the library yet.</p>".to_owned();
    }
    list.iter()
        .map(|p| {
            format!(
                "<label class=\"check\"><input type=\"checkbox\" name=\"{name}\" value=\"{0}\"{1}> <span><code>{0}</code> <span class=\"dim\">{2}</span></span></label>",
                escape(&p.name),
                if chosen.iter().any(|c| c == &p.name) {
                    " checked"
                } else {
                    ""
                },
                escape(&p.description)
            )
        })
        .collect()
}

/// The form that edits one profile; `stamp` is what the profile was when the form was made.
pub fn profile_edit(
    profile: &Profile,
    data: &Data,
    stamp: &str,
    view: &View<'_>,
    draft: &ProfileDraft,
) -> String {
    let personalities = prompts_of_kind(data, PromptType::Personality);
    let mut choices = format!(
        "<label class=\"check\"><input type=\"radio\" name=\"personality\" value=\"\"{}> <span>none</span></label>",
        if draft.personality.is_empty() {
            " checked"
        } else {
            ""
        }
    );
    for p in &personalities {
        choices.push_str(&format!(
            "<label class=\"check\"><input type=\"radio\" name=\"personality\" value=\"{0}\"{1}> <span><code>{0}</code> <span class=\"dim\">{2}</span></span></label>",
            escape(&p.name),
            if draft.personality == p.name { " checked" } else { "" },
            escape(&p.description)
        ));
    }
    let body = format!(
        "{}<form method=\"post\" action=\"/profile/edit\">{}{}{}{}{}\
         <fieldset><legend>System prompts (joined in the library's order)</legend>{}</fieldset>\
         <fieldset><legend>Skills that are on when a chat starts</legend>{}</fieldset>\
         <fieldset><legend>Personality</legend>{choices}</fieldset>\
         <div class=\"actions\"><button>Save</button><a href=\"/profiles\">Cancel</a></div></form>",
        banner(view),
        hidden("token", view.token),
        hidden("id", &profile.id.to_string()),
        hidden("v", stamp),
        text_field("Name", "name", &draft.name, 60, true),
        text_field("Description", "description", &draft.description, 200, false),
        checks(
            "prompt",
            &prompts_of_kind(data, PromptType::System),
            &draft.prompts
        ),
        checks(
            "skill",
            &prompts_of_kind(data, PromptType::Skill),
            &draft.skills
        ),
    );
    page("/profiles", &format!("Edit {}", profile.name), &body)
}

/// The question before a profile is deleted.
pub fn profile_delete(profile: &Profile, view: &View<'_>) -> String {
    let body = format!(
        "{}<p>Delete the profile <strong>{}</strong>? This cannot be undone. Chats that began under it keep their own copy of what they started with.</p>\
         <p>System prompts: {}</p><p>Skills on: {}</p>\
         <form method=\"post\" action=\"/profile/delete\">{}{}{}<div class=\"actions\"><button class=\"danger\">Delete</button><a href=\"/profiles\">Cancel</a></div></form>",
        banner(view),
        escape(&profile.name),
        names_or_none(&profile.prompts),
        names_or_none(&profile.skills),
        hidden("token", view.token),
        hidden("id", &profile.id.to_string()),
        hidden("v", &profile.fingerprint()),
    );
    page("/profiles", &format!("Delete {}?", profile.name), &body)
}

/// The Database page.
pub fn database(schema: &crate::web_schema::Schema) -> String {
    page(
        "/database",
        "Database",
        &crate::web_schema::page_body(schema),
    )
}

/// The Usage page.
pub fn usage(usage: &crate::web_usage::Usage) -> String {
    page("/usage", "Usage", &crate::web_usage::page_body(usage))
}

/// One chat's calls.
pub fn usage_chat(chat: &crate::web_usage::ChatRow, calls: &[crate::web_usage::CallRow]) -> String {
    page(
        "/usage",
        &format!("Chat {}", chat.id),
        &crate::web_usage::chat_body(chat, calls),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const V: View<'static> = View {
        token: "tok",
        ok: None,
        error: None,
    };

    fn prompt(name: &str, kind: PromptType, text: &str, enabled: bool, position: i64) -> Prompt {
        Prompt {
            id: position,
            name: name.to_owned(),
            description: "d".to_owned(),
            text: text.to_owned(),
            source_path: None,
            enabled,
            position,
            prompt_type: kind,
        }
    }

    fn entry(term: &str, definition: &str) -> Entry {
        Entry {
            id: 1,
            term: term.to_owned(),
            area: "a".to_owned(),
            definition: definition.to_owned(),
            project: String::new(),
            created_at: 0,
            updated_at: 0,
        }
    }

    #[test]
    fn escape_neutralises_the_five_characters() {
        assert_eq!(
            escape(r#"<script>alert("x")</script> & 'y'"#),
            "&lt;script&gt;alert(&quot;x&quot;)&lt;/script&gt; &amp; &#39;y&#39;"
        );
    }

    #[test]
    fn stored_text_never_reaches_a_page_unescaped() {
        let nasty = "<script>alert(1)</script>\"'>";
        let data = Data {
            profile: "default".to_owned(),
            profiles: vec![],
            prompts: vec![
                prompt(nasty, PromptType::System, nasty, true, 1),
                prompt(nasty, PromptType::Skill, nasty, false, 2),
                prompt(nasty, PromptType::Personality, nasty, false, 3),
            ],
            glossary: vec![entry(nasty, nasty)],
        };
        for html in [
            index(&data, &V),
            prompts(&data, PromptType::System, &V, &PromptDraft::default()),
            prompts(&data, PromptType::Skill, &V, &PromptDraft::default()),
            prompts(&data, PromptType::Personality, &V, &PromptDraft::default()),
            glossary(&data, "", &Filter::all(), &V, &GlossaryDraft::default()),
            glossary(&data, nasty, &Filter::all(), &V, &GlossaryDraft::default()),
            error(nasty),
        ] {
            assert!(!html.contains("<script"), "{html}");
            assert!(!html.contains("alert(1)</"), "{html}");
        }
        assert!(
            glossary(&data, "", &Filter::all(), &V, &GlossaryDraft::default())
                .contains("&lt;script&gt;")
        );
    }

    #[test]
    fn forms_carry_the_token_and_never_unescaped_values() {
        let nasty = "<script>alert(1)</script>\"'>";
        let p = prompt(nasty, PromptType::System, nasty, true, 1);
        let e = entry(nasty, nasty);
        let view = View {
            token: "tok\"><script>",
            ok: Some("saved"),
            error: Some(nasty),
        };
        let pdraft = PromptDraft {
            name: nasty.to_owned(),
            description: nasty.to_owned(),
            text: nasty.to_owned(),
        };
        let gdraft = GlossaryDraft {
            term: nasty.to_owned(),
            area: nasty.to_owned(),
            definition: nasty.to_owned(),
            project: nasty.to_owned(),
        };
        let data = Data {
            profile: "default".to_owned(),
            profiles: vec![],
            prompts: vec![p.clone()],
            glossary: vec![e.clone()],
        };
        let pages = [
            prompts(&data, PromptType::System, &view, &pdraft),
            prompt_edit(&p, nasty, &view, &pdraft),
            prompt_delete(&p, &view),
            glossary(&data, "", &Filter::all(), &view, &gdraft),
            glossary_edit(&e, nasty, &view, &gdraft),
            glossary_delete(&e, &view),
            stale(nasty, nasty, nasty),
        ];
        for html in &pages {
            assert!(!html.contains("<script"), "{html}");
            assert!(!html.contains("alert(1)</"), "{html}");
        }
        for html in &pages[..6] {
            assert!(
                html.contains("name=\"token\" value=\"tok&quot;&gt;&lt;script&gt;\""),
                "{html}"
            );
            assert!(html.contains("Saved."));
            assert!(html.contains("method=\"post\""));
        }
    }

    #[test]
    fn only_a_system_prompt_can_be_switched_and_every_edit_form_posts() {
        let data = Data {
            profile: "default".to_owned(),
            profiles: vec![],
            prompts: vec![
                prompt("sys", PromptType::System, "a", true, 1),
                prompt("skl", PromptType::Skill, "b", false, 2),
            ],
            glossary: vec![],
        };
        let system = prompts(&data, PromptType::System, &V, &PromptDraft::default());
        assert!(
            system.contains("Take out of the profile")
                && system.contains("action=\"/prompt/enable\"")
        );
        assert!(system.contains("action=\"/prompt/move\""));
        let skills = prompts(&data, PromptType::Skill, &V, &PromptDraft::default());
        assert!(!skills.contains("Switch o") && skills.contains("action=\"/prompt/move\""));
        assert!(skills.contains("name=\"type\" value=\"skill\""));
        assert!(skills.contains("href=\"/prompt/edit?id=2\""));
        // A new line at the start of a text is not lost by the browser.
        let draft = PromptDraft {
            text: "\nstarts with a blank line".to_owned(),
            ..PromptDraft::default()
        };
        assert!(prompts(&data, PromptType::Skill, &V, &draft).contains("required>\n\nstarts"));
    }

    #[test]
    fn prompts_show_state_only_for_system_prompts_and_in_order() {
        let data = Data {
            profile: "default".to_owned(),
            profiles: vec![],
            prompts: vec![
                prompt("second", PromptType::System, "two", false, 2),
                prompt("first", PromptType::System, "one", true, 1),
                prompt("tdd", PromptType::Skill, "test early", false, 1),
            ],
            glossary: vec![],
        };
        let system = prompts(&data, PromptType::System, &V, &PromptDraft::default());
        assert!(system.find("first").unwrap() < system.find("second").unwrap());
        assert!(system.contains("<strong>in the profile</strong>"));
        assert!(system.contains("&middot; not in it"));
        let skills = prompts(&data, PromptType::Skill, &V, &PromptDraft::default());
        assert!(skills.contains("tdd") && !skills.contains("first"));
        assert!(!skills.contains("&middot; not in it"));
        assert!(
            prompts(&data, PromptType::Personality, &V, &PromptDraft::default())
                .contains("None yet")
        );
    }

    #[test]
    fn the_context_page_filters_by_project_the_same_way() {
        let entry = |name: &str, project: &str| crate::store_context::ContextEntry {
            id: 1,
            name: name.to_owned(),
            kind: "rule".to_owned(),
            text: "t".to_owned(),
            quote: String::new(),
            area: String::new(),
            project: project.to_owned(),
            preload: false,
            created_at: 0,
            updated_at: 0,
        };
        let entries = vec![
            entry("shared-rule", ""),
            entry("here-rule", "/work/a"),
            entry("other-rule", "/work/b"),
        ];
        let page = |choice: &str| {
            context(
                &entries,
                "",
                &Filter::new(choice, "/work/a"),
                &V,
                &ContextDraft::default(),
                "",
            )
        };
        let by_default = page("");
        assert!(by_default.contains("shared-rule") && by_default.contains("here-rule"));
        assert!(!by_default.contains("other-rule"));
        assert!(by_default.contains("(1 more in other projects)"));
        assert!(page("all").contains("other-rule"));
    }

    #[test]
    fn the_glossary_shows_this_project_and_the_shared_words_unless_told_otherwise() {
        let mut here = entry("here-word", "d");
        here.project = "/work/a".to_owned();
        let mut other = entry("other-word", "d");
        other.project = "/work/b".to_owned();
        let data = Data {
            profile: "default".to_owned(),
            profiles: vec![],
            prompts: vec![],
            glossary: vec![entry("shared-word", "d"), here, other],
        };
        let page = |choice: &str| {
            glossary(
                &data,
                "",
                &Filter::new(choice, "/work/a"),
                &V,
                &GlossaryDraft::default(),
            )
        };
        let by_default = page("");
        assert!(by_default.contains("shared-word") && by_default.contains("here-word"));
        assert!(!by_default.contains("other-word"));
        assert!(by_default.contains("2 words (1 more in other projects)."));
        // The drop-down offers the other project, and the choice stays selected.
        assert!(
            by_default.contains("<option value=\"/work/b\">"),
            "{by_default}"
        );
        let everything = page("all");
        assert!(everything.contains("other-word") && everything.contains("3 words."));
        assert!(everything.contains("<option value=\"all\" selected>"));
        let shared = page("shared");
        assert!(shared.contains("shared-word") && !shared.contains("here-word"));
        let b = page("/work/b");
        assert!(b.contains("other-word") && b.contains("shared-word") && !b.contains("here-word"));
    }

    #[test]
    fn the_glossary_search_matches_words_areas_and_definitions_ignoring_case() {
        let data = Data {
            profile: "default".to_owned(),
            profiles: vec![],
            prompts: vec![],
            glossary: vec![
                entry("Claim", "a request for payment"),
                entry("order", "a purchase"),
            ],
        };
        let all = glossary(&data, "", &Filter::all(), &V, &GlossaryDraft::default());
        assert!(all.contains("2 words."));
        let found = glossary(
            &data,
            "PAYMENT",
            &Filter::all(),
            &V,
            &GlossaryDraft::default(),
        );
        assert!(found.contains("1 of 2 words match."), "{found}");
        assert!(found.contains("Claim") && !found.contains("purchase"));
        assert!(
            glossary(&data, "zzz", &Filter::all(), &V, &GlossaryDraft::default())
                .contains("0 of 2 words match.")
        );
    }
}
