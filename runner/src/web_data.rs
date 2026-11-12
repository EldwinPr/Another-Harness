//! The Data page of the web view (issue 57): export, spaces, import and merge, and wipe, the same
//! things `--export`, `--import` and `--wipe` do, for the database this server shows.
//!
//! The page never shows chat text or a provider key. What it can do is more than the other pages
//! (they only touch prompts, the glossary and a few settings), so each action says what it will
//! do, goes through the usual `Host`, `Origin` and token checks, and works on files on this
//! computer: an export is written into the data folder's `exports/`, an import names a file by its
//! path (there is no upload), and a wipe asks for the database's name. One server shows one
//! database; to see another, start it with `--space NAME`.

use std::path::{Path, PathBuf};

use crate::transfer::{self, WipePlan};
use crate::web::{Ctx, Form, Reply};
use crate::web_pages::{self, View, banner, escape, hidden, page, text_field};

/// The folder inside the data folder that holds the exports the page makes.
pub const EXPORTS_DIR: &str = "exports";

/// The data folder that holds `db`, and the name that confirms a wipe: the space's name for a
/// file in `spaces/`, else `harness`.
pub fn place(db: &Path) -> (PathBuf, String) {
    let parent = db.parent().unwrap_or(Path::new("."));
    if parent
        .file_name()
        .is_some_and(|n| n == transfer::SPACES_DIR)
    {
        let name = db
            .file_stem()
            .map_or_else(String::new, |s| s.to_string_lossy().into_owned());
        (parent.parent().unwrap_or(parent).to_path_buf(), name)
    } else {
        (parent.to_path_buf(), transfer::MAIN_NAME.to_owned())
    }
}

/// Files of `dir` whose names pass `keep`, with their sizes, newest name last.
fn files(dir: &Path, keep: impl Fn(&str) -> bool) -> Vec<(String, u64)> {
    let mut found: Vec<(String, u64)> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            let size = entry.metadata().ok()?.len();
            (keep(&name) && entry.path().is_file()).then_some((name, size))
        })
        .collect();
    found.sort();
    found
}

fn size(bytes: u64) -> String {
    if bytes >= 1_000_000 {
        format!("{:.1} MB", bytes as f64 / 1_000_000.0)
    } else {
        format!("{} KB", bytes.div_ceil(1000))
    }
}

fn file_table(rows: &[(String, u64)]) -> String {
    if rows.is_empty() {
        return "<p class=\"dim\">None yet.</p>".to_owned();
    }
    let rows: String = rows
        .iter()
        .map(|(name, bytes)| {
            format!(
                "<tr><td><code>{}</code></td><td>{}</td></tr>",
                escape(name),
                size(*bytes)
            )
        })
        .collect();
    format!("<table>{rows}</table>")
}

/// The body of the Data page.
pub fn body(view: &View<'_>, db: &Path, plan: &WipePlan) -> String {
    let (data, name) = place(db);
    let exports = files(&data.join(EXPORTS_DIR), |n| n.ends_with(".db"));
    let db_name = db
        .file_name()
        .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
    let backups = files(db.parent().unwrap_or(Path::new(".")), |n| {
        n.starts_with(&format!("{db_name}.before-"))
    });
    let spaces = transfer::list_spaces(&data);
    let spaces = if spaces.is_empty() {
        "<p class=\"dim\">None yet.</p>".to_owned()
    } else {
        let items: String = spaces
            .iter()
            .map(|s| format!("<li><code>{}</code></li>", escape(s)))
            .collect();
        format!("<ul>{items}</ul>")
    };
    let counts: String = plan
        .lines()
        .iter()
        .map(|l| format!("<li>{}</li>", escape(l.trim())))
        .collect();
    let token = hidden("token", view.token);
    format!(
        "{banner}\
         <p>This page is about the whole database file <code>{db}</code> (<strong>{name}</strong>). \
         It never shows chat text or API keys.</p>\
         <h2>Export</h2>\
         <p class=\"dim\">Writes a copy into <code>{exports_dir}</code>, readable by you only. Provider API keys and \
         MCP environment values are left out unless you tick the box. Chat text is copied as it is, and the turn logs are not included.</p>\
         <form method=\"post\" action=\"/data/export\">{token}\
         <label class=\"check\"><input type=\"checkbox\" name=\"with_keys\" value=\"1\"> Keep API keys and MCP environment values in the copy</label>\
         <div class=\"actions\"><button>Export</button></div></form>\
         <h3>Exports</h3>{exports}\
         <h3>Backups</h3><p class=\"dim\">Copies saved next to the database before an upgrade, a merge or a wipe.</p>{backups}\
         <h2>Spaces</h2>\
         <p class=\"dim\">Other databases kept beside this one. Open one by starting the program with <code>--space NAME</code>.</p>{spaces}\
         <h2>Import</h2>\
         <p class=\"dim\">Name a database file on this computer (its full path). It is read from a copy and never changed. \
         <em>Keep as a space</em> makes a separate database; <em>Merge</em> adds what it has and this database lacks, \
         keeps everything already here, and saves a copy of this database first. The inbox and settings are not merged.</p>\
         <form method=\"post\" action=\"/data/import\">{token}\
         {file}\
         <label for=\"f-how\">What to do</label>\
         <select id=\"f-how\" name=\"how\"><option value=\"space\">Keep as a space</option><option value=\"merge\">Merge into this database</option></select>\
         {space_name}\
         <div class=\"actions\"><button>Import</button></div></form>\
         <h2>Wipe</h2>\
         <p>This empties the database. It holds now:</p><ul>{counts}</ul>\
         <p class=\"dim\">A copy is saved first (see Backups), and the defaults of a new database are put back. A chat open elsewhere finds its saved chat gone.</p>\
         <form method=\"post\" action=\"/data/wipe\">{token}\
         {word}\
         <label class=\"check\"><input type=\"checkbox\" name=\"with_logs\" value=\"1\"> Also delete the turn logs in the data folder (they hold every request as sent)</label>\
         <div class=\"actions\"><button class=\"danger\">Wipe</button></div></form>",
        banner = banner(view),
        db = escape(&db.display().to_string()),
        name = escape(&name),
        exports_dir = escape(&data.join(EXPORTS_DIR).display().to_string()),
        exports = file_table(&exports),
        backups = file_table(&backups),
        file = text_field("Path of the database file", "file", "", 500, true),
        space_name = text_field(
            "Name for the space (when keeping it as a space)",
            "name",
            "",
            40,
            false
        ),
        word = text_field(&format!("Type {name} to confirm"), "word", "", 40, true),
    )
}

/// The Data page, with a notice or an error above the forms.
fn render(ctx: &Ctx<'_>, status: u16, ok: Option<&str>, error: Option<&str>) -> Reply {
    let view = View {
        token: ctx.token,
        ok,
        error,
    };
    match (ctx.open)() {
        Ok(store) => {
            let plan = transfer::plan_of(&store.conn);
            Reply::page(status, page("/data", "Data", &body(&view, ctx.db, &plan)))
        }
        Err(why) => Reply::page(500, web_pages::error(&why)),
    }
}

/// `GET /data`.
pub fn get(ctx: &Ctx<'_>, ok: &str) -> Reply {
    render(ctx, 200, Some(ok), None)
}

/// The forms of the Data page, after the request passed the origin, type, size and token checks.
pub fn post(path: &str, form: &Form, ctx: &Ctx<'_>) -> Reply {
    let (data, name) = place(ctx.db);
    let refuse = |why: &str| render(ctx, 400, None, Some(why));
    match path {
        "/data/export" => {
            let stamp =
                crate::turn_log::utc_stamp(u64::try_from(crate::store::now_secs()).unwrap_or(0));
            let to = data.join(EXPORTS_DIR).join(format!("{name}-{stamp}.db"));
            match transfer::export(ctx.db, &to, form.get("with_keys") == "1") {
                Ok(_) => Reply::redirect("/data?ok=exported"),
                Err(why) => refuse(&why),
            }
        }
        "/data/import" => {
            let file = form.get("file").trim();
            if file.is_empty() || !Path::new(file).is_absolute() {
                return refuse("Give the full path of the database file, starting with /.");
            }
            let file = Path::new(file);
            if file.canonicalize().ok() == ctx.db.canonicalize().ok() {
                return refuse("That is this database itself.");
            }
            match form.get("how") {
                "space" => match transfer::import_as_space(file, &data, form.get("name").trim()) {
                    Ok(_) => Reply::redirect("/data?ok=imported"),
                    Err(why) => refuse(&why),
                },
                "merge" => match transfer::merge(ctx.db, file) {
                    Ok(report) => Reply::page(200, merged(&report)),
                    Err(why) => refuse(&why),
                },
                _ => refuse("Choose what to do with the file."),
            }
        }
        "/data/wipe" => {
            if form.get("word").trim() != name {
                return refuse(&format!("Not wiped: type {name} exactly to confirm."));
            }
            match transfer::wipe(ctx.db) {
                Ok(_) => {
                    if form.get("with_logs") == "1" {
                        transfer::remove_turn_logs(&data);
                    }
                    Reply::redirect("/data?ok=wiped")
                }
                Err(why) => refuse(&why),
            }
        }
        _ => Reply::page(404, web_pages::not_found()),
    }
}

/// The page that shows what a merge did.
fn merged(report: &transfer::MergeReport) -> String {
    let lines = report.lines().join("\n");
    let backup = report.backup.as_ref().map_or_else(String::new, |b| {
        format!(
            "<p>The database as it was is saved at <code>{}</code>.</p>",
            escape(&b.display().to_string())
        )
    });
    page(
        "/data",
        "Merged",
        &format!(
            "<pre>{}</pre>{backup}<p><a href=\"/data\">Back to Data</a></p>",
            escape(&lines)
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;
    use crate::tools::testutil::Scratch;
    use crate::web::respond;
    use rusqlite::Connection;

    const PORT: u16 = 8123;
    const TOKEN: &str = "tok";
    const SECRET: &str = "sk-live-VERY-secret";

    fn with_provider(path: &Path, name: &str, key: &str) {
        let store = Store::open(path).unwrap();
        store
            .conn
            .execute(
                "INSERT INTO providers (name, base_url, model, api_key, created_at) VALUES (?1, 'http://x/v1', 'm', ?2, 1)",
                rusqlite::params![name, key],
            )
            .unwrap();
    }

    fn ask(db: &Path, request: &str) -> String {
        let read = || crate::web::load(db);
        let write = || Store::open_for_writing(db).map_err(|e| e.to_string());
        let open = || Store::open_read_only(db).map_err(|e| e.to_string());
        let ctx = Ctx {
            project: "",
            port: PORT,
            token: TOKEN,
            db,
            read: &read,
            write: &write,
            open: &open,
        };
        String::from_utf8(respond(request.as_bytes(), &ctx)).unwrap()
    }

    fn get(db: &Path, path: &str) -> String {
        ask(
            db,
            &format!("GET {path} HTTP/1.1\r\nHost: localhost:{PORT}\r\n\r\n"),
        )
    }

    fn encode(text: &str) -> String {
        text.bytes()
            .map(|b| {
                if b.is_ascii_alphanumeric() {
                    char::from(b).to_string()
                } else {
                    format!("%{b:02X}")
                }
            })
            .collect()
    }

    fn post_with(db: &Path, path: &str, token: Option<&str>, fields: &[(&str, &str)]) -> String {
        let mut all: Vec<(&str, &str)> = token.map(|t| ("token", t)).into_iter().collect();
        all.extend_from_slice(fields);
        let body = all
            .iter()
            .map(|(k, v)| format!("{}={}", encode(k), encode(v)))
            .collect::<Vec<_>>()
            .join("&");
        ask(
            db,
            &format!(
                "POST {path} HTTP/1.1\r\nHost: localhost:{PORT}\r\nOrigin: http://localhost:{PORT}\r\n\
                 Content-Type: application/x-www-form-urlencoded\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            ),
        )
    }

    fn post(db: &Path, path: &str, fields: &[(&str, &str)]) -> String {
        post_with(db, path, Some(TOKEN), fields)
    }

    fn status(answer: &str) -> &str {
        answer.lines().next().unwrap_or("")
    }

    fn location(answer: &str) -> Option<&str> {
        answer.lines().find_map(|l| l.strip_prefix("Location: "))
    }

    fn n(path: &Path, sql: &str) -> i64 {
        Connection::open(path)
            .unwrap()
            .query_row(sql, [], |r| r.get(0))
            .unwrap()
    }

    #[test]
    fn the_place_of_a_database_is_its_data_folder_and_the_word_that_confirms_a_wipe() {
        assert_eq!(
            place(Path::new("/d/aharness/harness.db")),
            (PathBuf::from("/d/aharness"), "harness".to_owned())
        );
        assert_eq!(
            place(Path::new("/d/aharness/spaces/old.db")),
            (PathBuf::from("/d/aharness"), "old".to_owned())
        );
    }

    #[test]
    fn the_page_shows_what_the_forms_do_and_never_a_key() {
        let dir = Scratch::new("wd-page");
        let db = dir.path().join("harness.db");
        with_provider(&db, "or", SECRET);
        let page = get(&db, "/data");
        assert!(status(&page).contains("200"), "{page}");
        for want in [
            "Export",
            "Spaces",
            "Import",
            "Wipe",
            "Type harness to confirm",
            "1 providers, 1 of them with a saved API key",
            "action=\"/data/export\"",
            "name=\"token\"",
        ] {
            assert!(page.contains(want), "{want}\n{page}");
        }
        assert!(!page.contains(SECRET));
        assert!(!page.contains("<script"));
        // It is in the menu of every page.
        assert!(get(&db, "/help").contains("href=\"/data\""));
    }

    #[test]
    fn an_export_goes_into_the_exports_folder_without_the_key_unless_asked() {
        let dir = Scratch::new("wd-export");
        let db = dir.path().join("harness.db");
        with_provider(&db, "or", SECRET);
        let answer = post(&db, "/data/export", &[]);
        assert!(status(&answer).contains("303"), "{answer}");
        assert_eq!(location(&answer), Some("/data?ok=exported"));
        let made = files(&dir.path().join(EXPORTS_DIR), |n| n.ends_with(".db"));
        assert_eq!(made.len(), 1);
        let copy = dir.path().join(EXPORTS_DIR).join(&made[0].0);
        assert_eq!(
            n(
                &copy,
                "SELECT COUNT(*) FROM providers WHERE api_key IS NULL"
            ),
            1
        );
        // The database still has its key, and the page lists the export.
        assert_eq!(
            n(
                &db,
                "SELECT COUNT(*) FROM providers WHERE api_key IS NOT NULL"
            ),
            1
        );
        let page = get(&db, "/data?ok=exported");
        assert!(
            page.contains(&made[0].0) && page.contains("Exported"),
            "{page}"
        );
        assert!(!page.contains(SECRET));

        std::thread::sleep(std::time::Duration::from_millis(1100));
        post(&db, "/data/export", &[("with_keys", "1")]);
        let all = files(&dir.path().join(EXPORTS_DIR), |n| n.ends_with(".db"));
        assert_eq!(all.len(), 2);
        let with = dir.path().join(EXPORTS_DIR).join(&all[1].0);
        assert_eq!(
            n(
                &with,
                "SELECT COUNT(*) FROM providers WHERE api_key IS NOT NULL"
            ),
            1
        );
    }

    #[test]
    fn an_import_keeps_a_space_or_merges_and_refuses_what_is_wrong() {
        let dir = Scratch::new("wd-import");
        let db = dir.path().join("harness.db");
        with_provider(&db, "mine", "k1");
        let other = dir.path().join("other.db");
        with_provider(&other, "extra", "k2");
        let other_text = other.display().to_string();

        // A path must be a full path, and not this database itself.
        let answer = post(
            &db,
            "/data/import",
            &[("file", "other.db"), ("how", "merge")],
        );
        assert!(
            status(&answer).contains("400") && answer.contains("full path"),
            "{answer}"
        );
        let answer = post(
            &db,
            "/data/import",
            &[("file", &db.display().to_string()), ("how", "merge")],
        );
        assert!(answer.contains("this database itself"), "{answer}");
        let answer = post(&db, "/data/import", &[("file", &other_text), ("how", "")]);
        assert!(answer.contains("Choose what to do"), "{answer}");

        // Keep as a space.
        let answer = post(
            &db,
            "/data/import",
            &[("file", &other_text), ("how", "space"), ("name", "old")],
        );
        assert_eq!(location(&answer), Some("/data?ok=imported"), "{answer}");
        assert!(transfer::space_path(dir.path(), "old").is_file());
        assert!(get(&db, "/data").contains("<code>old</code>"));
        let answer = post(
            &db,
            "/data/import",
            &[("file", &other_text), ("how", "space"), ("name", "harness")],
        );
        assert!(status(&answer).contains("400"), "{answer}");

        // Merge: the report is shown, what was here is kept, a copy is saved.
        let answer = post(
            &db,
            "/data/import",
            &[("file", &other_text), ("how", "merge")],
        );
        assert!(
            status(&answer).contains("200") && answer.contains("1 added"),
            "{answer}"
        );
        assert_eq!(n(&db, "SELECT COUNT(*) FROM providers"), 2);
        assert_eq!(
            n(
                &db,
                "SELECT COUNT(*) FROM providers WHERE name = 'mine' AND api_key = 'k1'"
            ),
            1
        );
        assert!(get(&db, "/data").contains("harness.db.before-merge-"));
        assert!(!answer.contains("k2"));
    }

    #[test]
    fn a_wipe_needs_the_token_and_the_name_and_leaves_a_listed_backup() {
        let dir = Scratch::new("wd-wipe");
        let db = dir.path().join("harness.db");
        with_provider(&db, "or", SECRET);
        std::fs::write(dir.path().join("20261101T000000Z.jsonl"), "{}").unwrap();

        let answer = post_with(&db, "/data/wipe", None, &[("word", "harness")]);
        assert!(status(&answer).contains("403"), "{answer}");
        let answer = post(&db, "/data/wipe", &[("word", "yes")]);
        assert!(
            status(&answer).contains("400") && answer.contains("type harness exactly"),
            "{answer}"
        );
        assert_eq!(n(&db, "SELECT COUNT(*) FROM providers"), 1);

        let answer = post(&db, "/data/wipe", &[("word", "harness")]);
        assert_eq!(location(&answer), Some("/data?ok=wiped"), "{answer}");
        assert_eq!(n(&db, "SELECT COUNT(*) FROM providers"), 0);
        assert!(dir.path().join("20261101T000000Z.jsonl").exists());
        let page = get(&db, "/data?ok=wiped");
        assert!(
            page.contains("harness.db.before-wipe-") && page.contains("Wiped"),
            "{page}"
        );
        // Asked for, the turn logs go too.
        post(
            &db,
            "/data/wipe",
            &[("word", "harness"), ("with_logs", "1")],
        );
        assert!(!dir.path().join("20261101T000000Z.jsonl").exists());
    }

    #[test]
    fn a_space_confirms_a_wipe_with_its_own_name() {
        let dir = Scratch::new("wd-space-wipe");
        let src = dir.path().join("src.db");
        with_provider(&src, "or", "k");
        let space = transfer::import_as_space(&src, dir.path(), "old").unwrap();
        assert!(get(&space, "/data").contains("Type old to confirm"));
        let answer = post(&space, "/data/wipe", &[("word", "harness")]);
        assert!(status(&answer).contains("400"), "{answer}");
        let answer = post(&space, "/data/wipe", &[("word", "old")]);
        assert!(status(&answer).contains("303"), "{answer}");
        assert_eq!(n(&space, "SELECT COUNT(*) FROM providers"), 0);
        // The space's backup sits beside the space, and the file it came from is untouched.
        let beside = files(&dir.path().join(transfer::SPACES_DIR), |n| {
            n.starts_with("old.db.before-wipe-")
        });
        assert_eq!(beside.len(), 1);
        assert_eq!(n(&src, "SELECT COUNT(*) FROM providers"), 1);
    }
}
