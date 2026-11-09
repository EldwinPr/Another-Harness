//! The web view (issues 28 and 29): `./chat.sh --serve [port]`, or `/web` inside a chat.
//!
//! A small HTTP server on the standard library, for looking at and editing the prompts, skills,
//! personalities and glossary in a browser:
//!
//! - it listens on `127.0.0.1` only, with no option to change that;
//! - the `Host` header must name this server (a page on another site cannot reach it through a
//!   name that points here);
//! - `GET` and `HEAD` only show pages. Changes are `POST`s of a form, and each one must carry the
//!   run's random token (put in every form of its pages) and an `Origin` that is this server;
//!   a write is answered with a redirect, so a reload never repeats it;
//! - the request is read for at most 5 seconds, 8 KB of head and 1 MB of body;
//! - the database is opened for each request, without creating or migrating anything, and only
//!   prompts and glossary entries are read or written ([`load`], `web_write`), so provider keys
//!   and chats are out of reach of the pages;
//! - the pages are plain HTML with every stored text escaped, a restrictive
//!   `Content-Security-Policy` and no JavaScript.
//!
//! The server shares nothing with the chat but the database file (the chat is single-threaded):
//! [`Web`] runs it on a thread of its own.

use std::io::{Read, Write};
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::store::Store;
use crate::store_context::ContextDraft;
use crate::store_prompts::PromptType;
use crate::web_pages::{self, Data, GlossaryDraft, ProfileDraft, PromptDraft, View};

/// The most bytes of request line and headers that are read.
pub const MAX_REQUEST: usize = 8 * 1024;
/// The most bytes of a form that are read (a prompt of 200,000 bytes can grow to about 600 KB encoded).
pub const MAX_BODY: usize = 1024 * 1024;
/// How long a client has to send its request.
const READ_TIME: Duration = Duration::from_secs(5);
/// The longest search text used.
const MAX_QUERY_CHARS: usize = 200;
/// The most fields a form may have (a profile form has one per prompt and skill).
const MAX_FIELDS: usize = 400;
/// How often the thread looks at its stop flag when nobody is connecting.
const POLL: Duration = Duration::from_millis(50);

const READ_ROUTES: [&str; 20] = [
    "/",
    "/profiles",
    "/profile/edit",
    "/profile/delete",
    "/usage",
    "/usage/chat",
    "/database",
    "/glossary",
    "/prompts",
    "/skills",
    "/personalities",
    "/glossary/edit",
    "/glossary/delete",
    "/context",
    "/context/edit",
    "/context/delete",
    "/tools",
    "/help",
    "/prompt/edit",
    "/prompt/delete",
];
const WRITE_ROUTES: [&str; 16] = [
    "/profile/add",
    "/profile/edit",
    "/profile/delete",
    "/profile/use",
    "/profile/copy",
    "/glossary/add",
    "/glossary/edit",
    "/glossary/delete",
    "/context/add",
    "/context/edit",
    "/context/delete",
    "/prompt/add",
    "/prompt/edit",
    "/prompt/delete",
    "/prompt/enable",
    "/prompt/move",
];

/// What answering a request needs: the server's port and token, and how to read and to write the
/// database (each call opens its own connection).
pub struct Ctx<'a> {
    pub port: u16,
    /// The project folder the web view was started for: what the Glossary and Context pages show
    /// first (with the shared entries).
    pub project: &'a str,
    pub token: &'a str,
    pub read: &'a dyn Fn() -> Result<Data, String>,
    pub write: &'a dyn Fn() -> Result<Store, String>,
    /// A read-only connection, for the pages that look at the whole database file (Database, Usage).
    pub open: &'a dyn Fn() -> Result<Store, String>,
}

/// One answer, before it is turned into bytes.
pub struct Reply {
    pub status: u16,
    pub body: String,
    pub location: Option<String>,
    pub allow: Option<&'static str>,
}

impl Reply {
    pub fn page(status: u16, body: String) -> Self {
        Self {
            status,
            body,
            location: None,
            allow: None,
        }
    }

    pub fn simple(status: u16, title: &str, text: &str) -> Self {
        Self::page(status, web_pages::simple(title, text))
    }

    /// `303 See Other`: the browser then fetches `to` with a GET.
    pub fn redirect(to: &str) -> Self {
        Self {
            status: 303,
            body: web_pages::simple("Done", "Done."),
            location: Some(to.to_owned()),
            allow: None,
        }
    }

    fn allowing(mut self, allow: &'static str) -> Self {
        self.allow = Some(allow);
        self
    }

    fn bytes(&self, head_only: bool) -> Vec<u8> {
        let allow = self
            .allow
            .map(|a| format!("Allow: {a}\r\n"))
            .unwrap_or_default();
        let location = self
            .location
            .as_ref()
            .map(|l| format!("Location: {l}\r\n"))
            .unwrap_or_default();
        let mut out = format!(
            "HTTP/1.1 {} {}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\n\
             {allow}{location}Cache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\nReferrer-Policy: same-origin\r\n\
             Content-Security-Policy: default-src 'none'; style-src 'unsafe-inline'; form-action 'self'; frame-ancestors 'none'\r\n\
             Connection: close\r\n\r\n",
            self.status,
            reason(self.status),
            self.body.len()
        )
        .into_bytes();
        if !head_only {
            out.extend_from_slice(self.body.as_bytes());
        }
        out
    }
}

/// The fields of a submitted form, decoded.
pub struct Form(Vec<(String, String)>);

impl Form {
    /// The fields of `application/x-www-form-urlencoded` text; `None` when there are too many.
    pub fn parse(body: &[u8]) -> Option<Self> {
        let text = String::from_utf8_lossy(body);
        let mut fields = Vec::new();
        for pair in text.split('&').filter(|p| !p.is_empty()) {
            if fields.len() >= MAX_FIELDS {
                return None;
            }
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            fields.push((decode(key), decode(value)));
        }
        Some(Self(fields))
    }

    /// The first field called `name`, or empty text.
    /// Every value sent under `name` (a group of checkboxes), in the order sent.
    pub fn all(&self, name: &str) -> Vec<&str> {
        self.0
            .iter()
            .filter(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
            .collect()
    }

    pub fn get(&self, name: &str) -> &str {
        self.0
            .iter()
            .find(|(k, _)| k == name)
            .map_or("", |(_, v)| v.as_str())
    }
}

/// What most pages show, read from the database now: profiles, prompts and glossary entries. The
/// Database and Usage pages read their own, through [`Ctx::open`].
pub fn load(db: &Path) -> Result<Data, String> {
    let store = Store::open_read_only(db).map_err(|e| e.to_string())?;
    Ok(Data {
        profile: store.selected_profile().map_err(|e| e.to_string())?,
        profiles: store.list_profiles().map_err(|e| e.to_string())?,
        prompts: store.list_prompts().map_err(|e| e.to_string())?,
        glossary: store.list_all_glossary().map_err(|e| e.to_string())?,
    })
}

/// A random token for one run of the server, from the operating system.
fn new_token() -> Result<String, String> {
    let mut bytes = [0u8; 32];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut bytes))
        .map_err(|e| format!("cannot make a token for the web page: {e}"))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// One running server: where its database is, its port and its token.
pub struct Server {
    db: PathBuf,
    port: u16,
    project: String,
    token: String,
}

impl Server {
    fn new(db: &Path, port: u16, project: &str) -> Result<Self, String> {
        Ok(Self {
            db: db.to_path_buf(),
            port,
            project: project.to_owned(),
            token: new_token()?,
        })
    }

    fn answer(&self, request: &[u8]) -> Vec<u8> {
        let read = || load(&self.db);
        let write = || Store::open_for_writing(&self.db).map_err(|e| e.to_string());
        let open = || Store::open_read_only(&self.db).map_err(|e| e.to_string());
        respond(
            request,
            &Ctx {
                port: self.port,
                project: &self.project,
                token: &self.token,
                read: &read,
                write: &write,
                open: &open,
            },
        )
    }
}

/// Runs the server until the process is stopped. Port in use, or a database that cannot be read,
/// is an error before anything is served.
pub fn serve(db: &Path, port: u16, project: &str) -> Result<(), String> {
    load(db)?;
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, port))
        .map_err(|e| format!("cannot listen on 127.0.0.1:{port}: {e}"))?;
    let server = Server::new(db, port, project)?;
    println!("http://127.0.0.1:{port}/  (Ctrl-C stops it)");
    serve_on(&listener, &server, None);
    Ok(())
}

/// Answers requests on `listener`, one at a time, `limit` of them or forever.
pub fn serve_on(listener: &TcpListener, server: &Server, limit: Option<usize>) {
    for (served, stream) in listener.incoming().enumerate() {
        if let Ok(stream) = stream {
            let _ = handle(stream, server);
        }
        if limit.is_some_and(|limit| served + 1 >= limit) {
            break;
        }
    }
}

/// The server running on a thread of its own, inside a chat (`/web`). It shares nothing with the
/// chat but the database file, prints nothing, and stops when asked or when this is dropped.
pub struct Web {
    port: u16,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Web {
    /// Starts the server on `port` (0 lets the system choose). Fails, before anything runs, when
    /// the database cannot be read or the port is taken.
    pub fn start(db: &Path, port: u16, project: &str) -> Result<Self, String> {
        load(db)?;
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, port))
            .map_err(|e| format!("cannot listen on 127.0.0.1:{port}: {e}"))?;
        let port = listener.local_addr().map_err(|e| e.to_string())?.port();
        listener.set_nonblocking(true).map_err(|e| e.to_string())?;
        let server = Server::new(db, port, project)?;
        let stop = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stop);
        let thread = std::thread::Builder::new()
            .name("web".to_owned())
            .spawn(move || {
                while !flag.load(Ordering::Relaxed) {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            let _ = stream.set_nonblocking(false);
                            // One bad request must not end the server.
                            let _ = std::panic::catch_unwind(AssertUnwindSafe(|| {
                                let _ = handle(stream, &server);
                            }));
                        }
                        Err(_) => std::thread::sleep(POLL),
                    }
                }
            })
            .map_err(|e| format!("cannot start the web thread: {e}"))?;
        Ok(Self {
            port,
            stop,
            thread: Some(thread),
        })
    }

    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}/", self.port)
    }

    /// Whether the thread is still serving.
    pub fn running(&self) -> bool {
        self.thread.as_ref().is_some_and(|t| !t.is_finished())
    }

    /// Stops the server and waits for the thread (a request being answered finishes first, at
    /// most a few seconds). Safe to call again.
    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for Web {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Reads one request (within the time and size limits), answers it, and closes the connection.
fn handle(mut stream: TcpStream, server: &Server) -> std::io::Result<()> {
    let started = Instant::now();
    let mut request = Vec::new();
    let mut chunk = [0u8; 8192];
    // Once the head is in, how many bytes the whole request is (head and declared body).
    let mut wanted: Option<usize> = None;
    loop {
        match head_end(&request) {
            Some(end) => {
                let total = *wanted.get_or_insert_with(|| {
                    let body = declared_length(&request[..end]).filter(|n| *n <= MAX_BODY);
                    end + 4 + body.unwrap_or(0)
                });
                if request.len() >= total {
                    break;
                }
            }
            None if request.len() >= MAX_REQUEST => break,
            None => {}
        }
        let left = READ_TIME.saturating_sub(started.elapsed());
        if left.is_zero() {
            return Ok(()); // too slow: no answer
        }
        stream.set_read_timeout(Some(left))?;
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => request.extend_from_slice(&chunk[..n]),
        }
    }
    stream.set_write_timeout(Some(READ_TIME))?;
    let answer = server.answer(&request);
    stream.write_all(&answer)?;
    stream.flush()?;
    let _ = stream.shutdown(std::net::Shutdown::Write);
    Ok(())
}

fn head_end(bytes: &[u8]) -> Option<usize> {
    bytes.windows(4).position(|w| w == b"\r\n\r\n")
}

/// The `Content-Length` a request head declares, if it is a number.
fn declared_length(head: &[u8]) -> Option<usize> {
    let head = String::from_utf8_lossy(head);
    head.split("\r\n").skip(1).find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.eq_ignore_ascii_case("content-length")
            .then(|| value.trim().parse().ok())?
    })
}

/// The parts of a request head the server looks at.
struct Head<'a> {
    path: &'a str,
    query: &'a str,
    host: Option<&'a str>,
    origin: Option<&'a str>,
    content_type: Option<&'a str>,
    length: Option<&'a str>,
}

fn bad(text: &str) -> (Reply, bool) {
    (Reply::simple(400, "Bad request", text), false)
}

/// The whole answer (status line, headers and page) to the bytes of a request.
pub fn respond(request: &[u8], ctx: &Ctx<'_>) -> Vec<u8> {
    let (reply, head_only) = route(request, ctx);
    reply.bytes(head_only)
}

fn route(request: &[u8], ctx: &Ctx<'_>) -> (Reply, bool) {
    let Some(end) = head_end(request) else {
        return if request.len() >= MAX_REQUEST {
            (
                Reply::simple(431, "Too large", "The request is too large."),
                false,
            )
        } else {
            bad("The request is incomplete.")
        };
    };
    if end + 4 > MAX_REQUEST {
        return (
            Reply::simple(431, "Too large", "The request is too large."),
            false,
        );
    }
    let Ok(head) = std::str::from_utf8(&request[..end]) else {
        return bad("The request is not text.");
    };
    let body = &request[end + 4..];
    let mut lines = head.split("\r\n");
    let first = lines.next().unwrap_or("");
    let parts: Vec<&str> = first.split(' ').collect();
    let [method, target, version] = parts[..] else {
        return bad("The request line is malformed.");
    };
    if !version.starts_with("HTTP/1.") || !target.starts_with('/') {
        return bad("The request line is malformed.");
    }
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let mut h = Head {
        path,
        query,
        host: None,
        origin: None,
        content_type: None,
        length: None,
    };
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            return bad("A header is malformed.");
        };
        let value = value.trim();
        let slot = match name.to_ascii_lowercase().as_str() {
            "host" => &mut h.host,
            "origin" => &mut h.origin,
            "content-type" => &mut h.content_type,
            "content-length" => &mut h.length,
            "transfer-encoding" => return bad("Chunked bodies are not accepted."),
            _ => continue,
        };
        if slot.replace(value).is_some() {
            return bad("A header is given twice.");
        }
    }
    let allowed = [
        format!("localhost:{}", ctx.port),
        format!("127.0.0.1:{}", ctx.port),
    ];
    if !h
        .host
        .is_some_and(|v| allowed.iter().any(|a| a.eq_ignore_ascii_case(v)))
    {
        return (
            Reply::simple(
                421,
                "Wrong host",
                "This server answers only as localhost or 127.0.0.1 on its port.",
            ),
            false,
        );
    }
    let read_route = READ_ROUTES.contains(&path);
    let write_route = WRITE_ROUTES.contains(&path);
    let allow = match (read_route, write_route) {
        (false, false) => return (Reply::page(404, web_pages::not_found()), method == "HEAD"),
        (true, false) => "GET, HEAD",
        (false, true) => "POST",
        (true, true) => "GET, HEAD, POST",
    };
    let not_allowed = || {
        (
            Reply::simple(405, "Not allowed", "That is not allowed on this page.").allowing(allow),
            false,
        )
    };
    match method {
        "GET" | "HEAD" if read_route => (get(&h, ctx), method == "HEAD"),
        "POST" if write_route => (post(&h, body, ctx), false),
        _ => not_allowed(),
    }
}

/// The view a page gets: the token, and the notice word of `?ok=` (shown only if it is a known word).
fn view<'a>(ctx: &'a Ctx<'_>, ok: &'a str) -> View<'a> {
    View {
        token: ctx.token,
        ok: Some(ok),
        error: None,
    }
}

fn id_of(query: &str) -> Option<i64> {
    query_value(query, "id").parse().ok()
}

/// Runs a page that reads the whole database file through the read-only connection.
fn with_db(ctx: &Ctx<'_>, page: impl FnOnce(&Store) -> Result<Reply, String>) -> Reply {
    match (ctx.open)().and_then(|store| page(&store)) {
        Ok(reply) => reply,
        Err(why) => Reply::page(500, web_pages::error(&why)),
    }
}

fn get(h: &Head<'_>, ctx: &Ctx<'_>) -> Reply {
    let data = match (ctx.read)() {
        Ok(data) => data,
        Err(why) => return Reply::page(500, web_pages::error(&why)),
    };
    let ok = query_value(h.query, "ok");
    let view = view(ctx, &ok);
    let glossary_draft = GlossaryDraft::default();
    let prompt_draft = PromptDraft::default();
    let kind = match h.path {
        "/prompts" => Some(PromptType::System),
        "/skills" => Some(PromptType::Skill),
        "/personalities" => Some(PromptType::Personality),
        _ => None,
    };
    if let Some(kind) = kind {
        return Reply::page(200, web_pages::prompts(&data, kind, &view, &prompt_draft));
    }
    let word = || data.glossary.iter().find(|e| Some(e.id) == id_of(h.query));
    let prompt = || data.prompts.iter().find(|p| Some(p.id) == id_of(h.query));
    let profile = || data.profiles.iter().find(|p| Some(p.id) == id_of(h.query));
    let missing = || Reply::page(404, web_pages::not_found());
    let here = ctx.project.to_owned();
    match h.path {
        "/" => {
            let today = (ctx.open)().ok().and_then(|store| {
                let day = crate::web_usage::today(&store.conn).ok()?;
                crate::web_usage::on_day(&store.conn, &day).ok()
            });
            Reply::page(200, web_pages::index_with(&data, &view, today.as_ref()))
        }
        "/profiles" => Reply::page(
            200,
            web_pages::profiles(&data, &view, &ProfileDraft::default()),
        ),
        "/profile/edit" => match profile() {
            Some(p) => Reply::page(
                200,
                web_pages::profile_edit(p, &data, &p.fingerprint(), &view, &ProfileDraft::of(p)),
            ),
            None => missing(),
        },
        "/profile/delete" => match profile() {
            Some(p) => Reply::page(200, web_pages::profile_delete(p, &view)),
            None => missing(),
        },
        "/database" => with_db(ctx, |store| {
            let schema = crate::web_schema::load(&store.conn).map_err(|e| e.to_string())?;
            Ok(Reply::page(200, web_pages::database(&schema)))
        }),
        "/usage" => with_db(ctx, |store| {
            let usage = crate::web_usage::load(&store.conn).map_err(|e| e.to_string())?;
            Ok(Reply::page(200, web_pages::usage(&usage)))
        }),
        "/usage/chat" => with_db(ctx, |store| {
            let Some(id) = id_of(h.query) else {
                return Ok(Reply::page(404, web_pages::not_found()));
            };
            let Some(chat) = crate::web_usage::chat(&store.conn, id).map_err(|e| e.to_string())?
            else {
                return Ok(Reply::page(404, web_pages::not_found()));
            };
            let calls = crate::web_usage::calls_of(&store.conn, id).map_err(|e| e.to_string())?;
            Ok(Reply::page(200, web_pages::usage_chat(&chat, &calls)))
        }),
        "/glossary" => Reply::page(
            200,
            web_pages::glossary(
                &data,
                &query_value(h.query, "q"),
                &web_pages::Filter::new(&query_value(h.query, "for"), &here),
                &view,
                &glossary_draft,
            ),
        ),
        "/glossary/edit" => match word() {
            Some(entry) => {
                let draft = GlossaryDraft {
                    term: entry.term.clone(),
                    area: entry.area.clone(),
                    definition: entry.definition.clone(),
                    project: entry.project.clone(),
                };
                Reply::page(
                    200,
                    web_pages::glossary_edit(entry, &entry.fingerprint(), &view, &draft),
                )
            }
            None => missing(),
        },
        "/glossary/delete" => match word() {
            Some(entry) => Reply::page(200, web_pages::glossary_delete(entry, &view)),
            None => missing(),
        },
        "/help" => Reply::page(200, web_pages::help(crate::repl::COMMANDS)),
        "/tools" => with_db(ctx, |store| {
            let db = store.path().ok_or("the database has no file")?;
            let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
            Ok(Reply::page(
                200,
                web_pages::tools(&crate::tool_list::catalogue(&cwd, &db)),
            ))
        }),
        "/context" => with_db(ctx, |store| {
            let entries = store.list_all_context().map_err(|e| e.to_string())?;
            Ok(Reply::page(
                200,
                web_pages::context(
                    &entries,
                    &query_value(h.query, "q"),
                    &web_pages::Filter::new(&query_value(h.query, "for"), &here),
                    &view,
                    &ContextDraft::default(),
                    "",
                ),
            ))
        }),
        "/context/edit" | "/context/delete" => with_db(ctx, |store| {
            let entries = store.list_all_context().map_err(|e| e.to_string())?;
            let Some(e) = entries.iter().find(|e| Some(e.id) == id_of(h.query)) else {
                return Ok(Reply::page(404, web_pages::not_found()));
            };
            Ok(Reply::page(
                200,
                if h.path == "/context/edit" {
                    web_pages::context_edit(e, &e.fingerprint(), &view, &ContextDraft::of(e))
                } else {
                    web_pages::context_delete(e, &view)
                },
            ))
        }),
        "/prompt/edit" => match prompt() {
            Some(p) => {
                let draft = PromptDraft {
                    name: p.name.clone(),
                    description: p.description.clone(),
                    text: p.text.clone(),
                };
                Reply::page(
                    200,
                    web_pages::prompt_edit(p, &p.fingerprint(), &view, &draft),
                )
            }
            None => missing(),
        },
        "/prompt/delete" => match prompt() {
            Some(p) => Reply::page(200, web_pages::prompt_delete(p, &view)),
            None => missing(),
        },
        _ => missing(),
    }
}

/// A `POST`: the origin, the kind and size of the body and the token are all checked before
/// anything is written.
fn post(h: &Head<'_>, body: &[u8], ctx: &Ctx<'_>) -> Reply {
    let origin_ok = match (h.origin, h.host) {
        (Some(origin), Some(host)) => origin.eq_ignore_ascii_case(&format!("http://{host}")),
        _ => false,
    };
    if !origin_ok {
        return Reply::simple(
            403,
            "Refused",
            "This form was not sent from a page of this server.",
        );
    }
    let form_type = h.content_type.is_some_and(|c| {
        c.split(';')
            .next()
            .unwrap_or("")
            .trim()
            .eq_ignore_ascii_case("application/x-www-form-urlencoded")
    });
    if !form_type {
        return Reply::simple(415, "Refused", "Only a form can be sent here.");
    }
    let Some(length) = h.length.and_then(|l| l.parse::<usize>().ok()) else {
        return Reply::simple(411, "Refused", "The length of the form is missing.");
    };
    if length > MAX_BODY {
        return Reply::simple(413, "Too large", "The form is too large.");
    }
    if body.len() < length {
        return Reply::simple(400, "Bad request", "The form is incomplete.");
    }
    let Some(form) = Form::parse(&body[..length]) else {
        return Reply::simple(400, "Bad request", "The form has too many fields.");
    };
    if !same(form.get("token"), ctx.token) {
        return Reply::simple(
            403,
            "Refused",
            "This form is out of date, or not from this server. Open the page again and retry.",
        );
    }
    crate::web_write::post(h.path, &form, ctx)
}

/// Equal texts, compared in a time that does not depend on where they differ.
fn same(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    let mut diff = a.len() ^ b.len();
    for i in 0..a.len().max(b.len()) {
        diff |= usize::from(a.get(i).copied().unwrap_or(0) ^ b.get(i).copied().unwrap_or(1));
    }
    diff == 0
}

/// The value of `name` in a query string like `q=a%20b&x=1`, decoded and cut.
fn query_value(query: &str, name: &str) -> String {
    for pair in query.split('&') {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        if key == name {
            return decode(value).chars().take(MAX_QUERY_CHARS).collect();
        }
    }
    String::new()
}

/// `%XX` and `+` decoded; bytes that are not valid text become the replacement character.
fn decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < bytes.len()
                && bytes[i + 1].is_ascii_hexdigit()
                && bytes[i + 2].is_ascii_hexdigit() =>
            {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("00");
                out.push(u8::from_str_radix(hex, 16).unwrap_or(b'?'));
                i += 2;
            }
            other => out.push(other),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        303 => "See Other",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        411 => "Length Required",
        413 => "Content Too Large",
        415 => "Unsupported Media Type",
        421 => "Misdirected Request",
        431 => "Request Header Fields Too Large",
        503 => "Service Unavailable",
        _ => "Internal Server Error",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::ProviderDef;
    use crate::store_prompts::Prompt;
    use crate::tools::testutil::Scratch;
    use kernel::RouterOptions;

    const PORT: u16 = 8787;
    const TOKEN: &str = "test-token-0123456789abcdef";
    const SECRET: &str = "sk-DISTINCT-KEY-12345";

    fn no_open() -> Result<Store, String> {
        Err("no database".to_owned())
    }

    fn no_data() -> Result<Data, String> {
        Ok(Data {
            profile: "default".to_owned(),
            profiles: vec![],
            prompts: vec![],
            glossary: vec![],
        })
    }

    /// An answer from a server whose database is `db`.
    fn ask_db(db: &Path, request: &str) -> String {
        let read = || load(db);
        let write = || Store::open_for_writing(db).map_err(|e| e.to_string());
        let open = || Store::open_read_only(db).map_err(|e| e.to_string());
        let ctx = Ctx {
            project: "",
            port: PORT,
            token: TOKEN,
            read: &read,
            write: &write,
            open: &open,
        };
        String::from_utf8(respond(request.as_bytes(), &ctx)).unwrap()
    }

    /// An answer from a server with an empty database that cannot be written.
    fn ask(request: &str) -> String {
        let write = || Err("no database".to_owned());
        let ctx = Ctx {
            project: "",
            port: PORT,
            token: TOKEN,
            read: &no_data,
            write: &write,
            open: &no_open,
        };
        String::from_utf8(respond(request.as_bytes(), &ctx)).unwrap()
    }

    fn get(path: &str) -> String {
        ask(&format!(
            "GET {path} HTTP/1.1\r\nHost: localhost:{PORT}\r\n\r\n"
        ))
    }

    fn get_db(db: &Path, path: &str) -> String {
        ask_db(
            db,
            &format!("GET {path} HTTP/1.1\r\nHost: localhost:{PORT}\r\n\r\n"),
        )
    }

    fn status(answer: &str) -> &str {
        answer.lines().next().unwrap_or("")
    }

    fn location(answer: &str) -> Option<&str> {
        answer.lines().find_map(|l| l.strip_prefix("Location: "))
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

    fn form_body(fields: &[(&str, &str)]) -> String {
        fields
            .iter()
            .map(|(k, v)| format!("{}={}", encode(k), encode(v)))
            .collect::<Vec<_>>()
            .join("&")
    }

    /// A POST with exactly these extra headers.
    fn post_raw(db: &Path, path: &str, headers: &[(&str, String)], body: &str) -> String {
        let mut request = format!("POST {path} HTTP/1.1\r\nHost: localhost:{PORT}\r\n");
        for (name, value) in headers {
            request.push_str(&format!("{name}: {value}\r\n"));
        }
        request.push_str("\r\n");
        request.push_str(body);
        ask_db(db, &request)
    }

    /// A well-formed POST of a form: right origin, content type, length and token.
    fn post(db: &Path, path: &str, fields: &[(&str, &str)]) -> String {
        let mut all = vec![("token", TOKEN)];
        all.extend_from_slice(fields);
        let body = form_body(&all);
        post_raw(
            db,
            path,
            &[
                ("Origin", format!("http://localhost:{PORT}")),
                (
                    "Content-Type",
                    "application/x-www-form-urlencoded".to_owned(),
                ),
                ("Content-Length", body.len().to_string()),
            ],
            &body,
        )
    }

    /// A database with a provider holding a distinctive key, a prompt of each kind and two words.
    fn seeded(tag: &str) -> (Scratch, PathBuf) {
        let dir = Scratch::new(tag);
        let path = dir.path().join("harness.db");
        let mut store = Store::open(&path).unwrap();
        store
            .add_provider(&ProviderDef {
                api: crate::config::ApiStyle::OpenAi,
                name: "secretrouter".to_owned(),
                base_url: "https://secret.example/v1".to_owned(),
                model: "secret-model".to_owned(),
                api_key: Some(SECRET.to_owned()),
                router: RouterOptions::default(),
            })
            .unwrap();
        store
            .add_prompt_of_type(
                "tdd",
                "test first",
                "Write the test first.",
                None,
                PromptType::Skill,
            )
            .unwrap();
        store
            .add_prompt_of_type(
                "pirate",
                "arr",
                "Talk like a pirate.",
                None,
                PromptType::Personality,
            )
            .unwrap();
        store
            .set_glossary("kartu stok", "the stock ledger", Some("inventory"))
            .unwrap();
        store
            .set_glossary("claim", "a request for <payment>", None)
            .unwrap();
        drop(store);
        (dir, path)
    }

    fn words(db: &Path) -> Vec<(String, String, String)> {
        Store::open_read_only(db)
            .unwrap()
            .list_glossary()
            .unwrap()
            .into_iter()
            .map(|e| (e.term, e.area, e.definition))
            .collect()
    }

    fn prompt(db: &Path, name: &str) -> Option<Prompt> {
        Store::open_read_only(db).unwrap().prompt(name).ok()
    }

    fn word_id(db: &Path, term: &str) -> i64 {
        Store::open_read_only(db)
            .unwrap()
            .glossary_entry(term)
            .unwrap()
            .unwrap()
            .id
    }

    #[test]
    fn every_page_answers_with_the_safety_headers_and_no_script() {
        let (_dir, db) = seeded("web-headers");
        let (claim, tdd) = (word_id(&db, "claim"), prompt(&db, "tdd").unwrap().id);
        for path in [
            "/".to_owned(),
            "/prompts".to_owned(),
            "/skills".to_owned(),
            "/personalities".to_owned(),
            "/glossary".to_owned(),
            "/context".to_owned(),
            "/tools".to_owned(),
            "/help".to_owned(),
            "/profiles".to_owned(),
            "/database".to_owned(),
            "/usage".to_owned(),
            format!("/glossary/edit?id={claim}"),
            format!("/glossary/delete?id={claim}"),
            format!("/prompt/edit?id={tdd}"),
            format!("/prompt/delete?id={tdd}"),
        ] {
            let page = get_db(&db, &path);
            assert_eq!(status(&page), "HTTP/1.1 200 OK", "{path}");
            for header in [
                "Content-Security-Policy: default-src 'none'; style-src 'unsafe-inline'; form-action 'self'; frame-ancestors 'none'",
                "X-Content-Type-Options: nosniff",
                "Cache-Control: no-store",
                "Referrer-Policy: same-origin",
                "Connection: close",
            ] {
                assert!(page.contains(header), "{path}: {header}");
            }
            assert!(!page.contains("<script"), "{path}");
        }
    }

    #[test]
    fn context_entries_are_added_edited_and_deleted_on_the_web_page() {
        let (_dir, db) = seeded("web-context");
        let ids = |db: &Path| -> Vec<(i64, String, bool, String)> {
            Store::open_read_only(db)
                .unwrap()
                .list_all_context()
                .unwrap()
                .into_iter()
                .map(|e| (e.id, e.name, e.preload, e.project))
                .collect()
        };
        let page = get_db(&db, "/context");
        assert_eq!(status(&page), "HTTP/1.1 200 OK");
        assert!(page.contains("0 entries."));

        // A refused add shows the form again with what was typed.
        let bad = post(
            &db,
            "/context/add",
            &[("name", ".hidden"), ("kind", "rule"), ("text", "<b>x</b>")],
        );
        assert_eq!(status(&bad), "HTTP/1.1 400 Bad Request");
        assert!(bad.contains("&lt;b&gt;x&lt;/b&gt;") && !bad.contains("<b>x</b>"));
        assert!(ids(&db).is_empty());

        let added = post(
            &db,
            "/context/add",
            &[
                ("name", "no-input-edits"),
                ("kind", "shape"),
                ("text", "Never edit input/ <i>ever</i>"),
                ("quote", "never touch input"),
                ("project", ""),
                ("preload", "1"),
            ],
        );
        assert_eq!(location(&added), Some("/context?ok=added"));
        let (id, _, preload, project) = ids(&db).remove(0);
        assert!(preload && project.is_empty());
        let again = post(
            &db,
            "/context/add",
            &[("name", "No-Input-Edits"), ("kind", "rule"), ("text", "x")],
        );
        assert_eq!(status(&again), "HTTP/1.1 400 Bad Request");
        assert!(again.contains("exists already"));

        let list = get_db(&db, "/context");
        assert!(list.contains("no-input-edits") && list.contains("every project"));
        assert!(list.contains("&lt;i&gt;ever&lt;/i&gt;") && !list.contains("<i>ever"));
        let found = get_db(&db, "/context?q=INPUT");
        assert!(found.contains("1 of 1 entries match."));

        // Edit: the stamp is the one the form carried.
        let edit = get_db(&db, &format!("/context/edit?id={id}"));
        assert_eq!(status(&edit), "HTTP/1.1 200 OK");
        let stamp = edit
            .split("name=\"v\" value=\"")
            .nth(1)
            .and_then(|r| r.split('"').next())
            .unwrap()
            .to_owned();
        let id_text = id.to_string();
        let fields = |text: &'static str, v: String| {
            vec![
                ("id", id_text.clone()),
                ("v", v),
                ("name", "no-input-edits".to_owned()),
                ("kind", "rule".to_owned()),
                ("text", text.to_owned()),
                ("quote", String::new()),
            ]
        };
        let send = |text: &'static str, v: String| {
            let f = fields(text, v);
            let f: Vec<(&str, &str)> = f.iter().map(|(k, v)| (*k, v.as_str())).collect();
            post(&db, "/context/edit", &f)
        };
        let saved = send("Never edit input/ at all", stamp.clone());
        assert_eq!(location(&saved), Some("/context?ok=saved"));
        let stale = send("late change", stamp.clone());
        assert_eq!(status(&stale), "HTTP/1.1 409 Conflict");
        let entry = Store::open_read_only(&db)
            .unwrap()
            .list_all_context()
            .unwrap()
            .remove(0);
        assert_eq!(entry.text, "Never edit input/ at all");
        assert_eq!(entry.kind, "rule");
        assert!(!entry.preload);

        // Delete is checked against the stamp too.
        let delete_page = get_db(&db, &format!("/context/delete?id={id}"));
        assert_eq!(status(&delete_page), "HTTP/1.1 200 OK");
        let wrong = post(&db, "/context/delete", &[("id", &id_text), ("v", "nope")]);
        assert_eq!(status(&wrong), "HTTP/1.1 409 Conflict");
        let done = post(
            &db,
            "/context/delete",
            &[("id", &id_text), ("v", &entry.fingerprint())],
        );
        assert_eq!(location(&done), Some("/context?ok=deleted"));
        assert!(ids(&db).is_empty());
        let gone = post(
            &db,
            "/context/delete",
            &[("id", &id_text), ("v", &entry.fingerprint())],
        );
        assert_eq!(status(&gone), "HTTP/1.1 404 Not Found");
        assert_eq!(
            status(&get_db(&db, &format!("/context/edit?id={id}"))),
            "HTTP/1.1 404 Not Found"
        );
    }

    #[test]
    fn the_tools_page_lists_what_the_model_is_given() {
        let (_dir, db) = seeded("web-tools");
        let page = get_db(&db, "/tools");
        assert_eq!(status(&page), "HTTP/1.1 200 OK");
        for name in [
            "read_file(path)",
            "bash(background?, command)",
            "task(",
            "glossary_propose(",
            "context_propose(",
            "library(",
            "profile(",
            "delegate(",
            "write_note",
        ] {
            assert!(page.contains(name), "{name}");
        }
        assert!(page.contains("A subagent"));
        assert!(!page.contains(SECRET));
    }

    #[test]
    fn the_help_page_explains_the_program_and_lists_every_chat_command() {
        let (_dir, db) = seeded("web-help");
        let page = get_db(&db, "/help");
        assert_eq!(status(&page), "HTTP/1.1 200 OK");
        for text in [
            "Context: rules, decisions and shapes",
            "Subagents",
            "/web stop",
        ] {
            assert!(page.contains(text), "{text}");
        }
        for (command, _) in crate::repl::COMMANDS {
            let shown = web_pages::escape(command);
            assert!(page.contains(&shown), "{command}");
        }
        // Every page it links to exists.
        for link in [
            "/tools",
            "/context",
            "/glossary",
            "/profiles",
            "/usage",
            "/database",
        ] {
            assert_eq!(status(&get_db(&db, link)), "HTTP/1.1 200 OK", "{link}");
            assert!(page.contains(&format!("href=\"{link}\"")), "{link}");
        }
    }

    #[test]
    fn a_glossary_word_added_on_the_web_can_be_for_one_project() {
        let (_dir, db) = seeded("web-glossary-project");
        let done = post(
            &db,
            "/glossary/add",
            &[
                ("term", "sku"),
                ("project", "/repo/a"),
                ("definition", "stock unit"),
            ],
        );
        assert_eq!(location(&done), Some("/glossary?ok=added"));
        let all = Store::open_read_only(&db)
            .unwrap()
            .list_all_glossary()
            .unwrap();
        assert!(
            all.iter()
                .any(|e| e.term == "sku" && e.project == "/repo/a")
        );
        let page = get_db(&db, "/glossary");
        assert!(page.contains("/repo/a") && page.contains("every project"));
        // The same word for every project is a different entry.
        let both = post(
            &db,
            "/glossary/add",
            &[("term", "sku"), ("definition", "x")],
        );
        assert_eq!(location(&both), Some("/glossary?ok=added"));
        let again = post(
            &db,
            "/glossary/add",
            &[("term", "SKU"), ("definition", "y")],
        );
        assert_eq!(status(&again), "HTTP/1.1 400 Bad Request");
    }

    #[test]
    fn other_paths_are_404_and_wrong_methods_405() {
        assert_eq!(status(&get("/nope")), "HTTP/1.1 404 Not Found");
        assert_eq!(status(&get("/../etc/passwd")), "HTTP/1.1 404 Not Found");
        assert_eq!(status(&get("/prompts/1")), "HTTP/1.1 404 Not Found");
        for method in ["POST", "PUT", "DELETE", "PATCH", "OPTIONS"] {
            let page = ask(&format!(
                "{method} /prompts HTTP/1.1\r\nHost: localhost:{PORT}\r\nContent-Length: 0\r\n\r\n"
            ));
            assert_eq!(status(&page), "HTTP/1.1 405 Method Not Allowed", "{method}");
            assert!(page.contains("Allow: GET, HEAD\r\n"));
        }
        // A page that only takes a form is not shown to a GET, and takes nothing else.
        let page = get("/glossary/add");
        assert_eq!(status(&page), "HTTP/1.1 405 Method Not Allowed");
        assert!(page.contains("Allow: POST\r\n"));
        let page = ask(&format!(
            "PUT /prompt/move HTTP/1.1\r\nHost: localhost:{PORT}\r\n\r\n"
        ));
        assert_eq!(status(&page), "HTTP/1.1 405 Method Not Allowed");
        // A page that is both shown and posted to.
        let page = ask(&format!(
            "DELETE /glossary/edit HTTP/1.1\r\nHost: localhost:{PORT}\r\n\r\n"
        ));
        assert!(page.contains("Allow: GET, HEAD, POST\r\n"), "{page}");
    }

    #[test]
    fn head_has_the_headers_and_length_but_no_page() {
        let page = ask(&format!(
            "HEAD / HTTP/1.1\r\nHost: 127.0.0.1:{PORT}\r\n\r\n"
        ));
        assert_eq!(status(&page), "HTTP/1.1 200 OK");
        assert!(page.contains("Content-Length: "));
        assert!(page.ends_with("\r\n\r\n"), "no body after the headers");
    }

    #[test]
    fn the_host_header_must_name_this_server() {
        for host in [
            "evil.example",
            "localhost",
            "localhost:1",
            "127.0.0.1:8788",
            "localhost:8787.evil.com",
        ] {
            let page = ask(&format!("GET / HTTP/1.1\r\nHost: {host}\r\n\r\n"));
            assert_eq!(status(&page), "HTTP/1.1 421 Misdirected Request", "{host}");
        }
        let none = ask("GET / HTTP/1.1\r\n\r\n");
        assert_eq!(status(&none), "HTTP/1.1 421 Misdirected Request");
        let two = ask(&format!(
            "GET / HTTP/1.1\r\nHost: localhost:{PORT}\r\nHost: localhost:{PORT}\r\n\r\n"
        ));
        assert_eq!(status(&two), "HTTP/1.1 400 Bad Request");
        let upper = ask(&format!("GET / HTTP/1.1\r\nHOST: LOCALHOST:{PORT}\r\n\r\n"));
        assert_eq!(status(&upper), "HTTP/1.1 200 OK");
    }

    #[test]
    fn malformed_and_oversize_requests_are_refused() {
        assert_eq!(status(&ask("GET /\r\n\r\n")), "HTTP/1.1 400 Bad Request");
        assert_eq!(
            status(&ask("GET / FTP/1.0\r\n\r\n")),
            "HTTP/1.1 400 Bad Request"
        );
        assert_eq!(
            status(&ask("GET x HTTP/1.1\r\n\r\n")),
            "HTTP/1.1 400 Bad Request"
        );
        assert_eq!(
            status(&ask(&format!(
                "GET / HTTP/1.1\r\nHost: localhost:{PORT}\r\nbad header\r\n\r\n"
            ))),
            "HTTP/1.1 400 Bad Request"
        );
        assert_eq!(
            status(&ask("GET / HTTP/1.1\r\nHost: x")),
            "HTTP/1.1 400 Bad Request"
        );
        let huge = format!("GET / HTTP/1.1\r\nX: {}", "a".repeat(MAX_REQUEST));
        assert_eq!(
            status(&ask(&huge)),
            "HTTP/1.1 431 Request Header Fields Too Large"
        );
        let long_head = format!(
            "GET / HTTP/1.1\r\nHost: localhost:{PORT}\r\nX: {}\r\n\r\n",
            "a".repeat(MAX_REQUEST)
        );
        assert_eq!(
            status(&ask(&long_head)),
            "HTTP/1.1 431 Request Header Fields Too Large"
        );
        let bytes = [
            b'G', b'E', b'T', b' ', 0xff, 0xfe, b'\r', b'\n', b'\r', b'\n',
        ];
        let write = || Err(String::new());
        let ctx = Ctx {
            project: "",
            port: PORT,
            token: TOKEN,
            read: &no_data,
            write: &write,
            open: &no_open,
        };
        let page = String::from_utf8(respond(&bytes, &ctx)).unwrap();
        assert_eq!(status(&page), "HTTP/1.1 400 Bad Request");
        let chunked = ask(&format!(
            "POST /glossary/add HTTP/1.1\r\nHost: localhost:{PORT}\r\nTransfer-Encoding: chunked\r\n\r\n"
        ));
        assert_eq!(status(&chunked), "HTTP/1.1 400 Bad Request");
    }

    #[test]
    fn a_database_that_cannot_be_read_is_a_500_with_the_reason_escaped() {
        let read = || Err("cannot <open> it".to_owned());
        let write = || Err(String::new());
        let ctx = Ctx {
            project: "",
            port: PORT,
            token: TOKEN,
            read: &read,
            write: &write,
            open: &no_open,
        };
        let page = String::from_utf8(respond(
            format!("GET / HTTP/1.1\r\nHost: localhost:{PORT}\r\n\r\n").as_bytes(),
            &ctx,
        ))
        .unwrap();
        assert_eq!(status(&page), "HTTP/1.1 500 Internal Server Error");
        assert!(page.contains("cannot &lt;open&gt; it"));
    }

    #[test]
    fn the_search_text_is_decoded_cut_and_shown_escaped() {
        assert_eq!(decode("a%20b+c%3Cd%zz%"), "a b c<d%zz%");
        assert_eq!(query_value("x=1&q=pay%20ment", "q"), "pay ment");
        assert_eq!(query_value("x=1", "q"), "");
        assert_eq!(
            query_value(&format!("q={}", "a".repeat(500)), "q").len(),
            MAX_QUERY_CHARS
        );
        let page = get("/glossary?q=%3Cscript%3E");
        assert!(
            page.contains("&lt;script&gt;") && !page.contains("<script"),
            "{page}"
        );
    }

    #[test]
    fn the_notice_word_is_known_or_shows_nothing() {
        let shown = get("/glossary?ok=added");
        assert!(shown.contains("class=\"notice\"") && shown.contains("Added."));
        let unknown = get("/glossary?ok=%3Cb%3Ehello");
        assert!(!unknown.contains("class=\"notice\"") && !unknown.contains("hello"));
    }

    #[test]
    fn no_page_shows_a_provider_key_or_anything_but_prompts_and_words() {
        let (_dir, db) = seeded("web-no-leak");
        let (claim, tdd) = (word_id(&db, "claim"), prompt(&db, "tdd").unwrap().id);
        let mut all = String::new();
        for target in [
            "/".to_owned(),
            "/prompts".to_owned(),
            "/skills".to_owned(),
            "/personalities".to_owned(),
            "/glossary".to_owned(),
            "/glossary?q=claim".to_owned(),
            "/profiles".to_owned(),
            "/database".to_owned(),
            "/usage".to_owned(),
            format!("/glossary/edit?id={claim}"),
            format!("/prompt/delete?id={tdd}"),
        ] {
            let page = get_db(&db, &target);
            assert_eq!(status(&page), "HTTP/1.1 200 OK", "{target}");
            all.push_str(&page);
        }
        // The answers to writes and refusals too.
        all.push_str(&post(
            &db,
            "/glossary/add",
            &[("term", ""), ("definition", "x")],
        ));
        all.push_str(&post(
            &db,
            "/prompt/add",
            &[("type", "system"), ("name", "")],
        ));
        all.push_str(&post_raw(&db, "/prompt/add", &[], ""));
        for secret in [SECRET, "secretrouter", "secret.example", "secret-model"] {
            assert!(!all.contains(secret), "{secret} leaked");
        }
        for shown in [
            "Write the test first.",
            "Talk like a pirate.",
            "kartu stok",
            "the stock ledger",
            "a request for &lt;payment&gt;",
        ] {
            assert!(all.contains(shown), "{shown} missing");
        }
    }

    #[test]
    fn the_read_only_open_sees_a_chat_that_is_running_and_cannot_write() {
        let (dir, path) = seeded("web-readonly");
        // The chat holds the database open (WAL mode) and has just written.
        let mut running = Store::open(&path).unwrap();
        running
            .set_glossary("fresh", "written a moment ago", None)
            .unwrap();
        let data = load(&path).unwrap();
        assert!(data.glossary.iter().any(|e| e.term == "fresh"));
        let reader = Store::open_read_only(&path).unwrap();
        assert!(
            reader
                .conn()
                .execute("INSERT INTO glossary (term, definition, created_at, updated_at) VALUES ('x','y',0,0)", [])
                .is_err(),
            "a read-only connection cannot write"
        );
        drop(running);
        drop(dir);
    }

    #[test]
    fn a_missing_or_other_version_database_is_an_error_and_nothing_is_created() {
        let dir = Scratch::new("web-bad-db");
        let missing = dir.path().join("none.db");
        assert!(load(&missing).is_err());
        assert!(Store::open_for_writing(&missing).is_err());
        assert!(!missing.exists(), "reading must not create the file");
        let old = dir.path().join("old.db");
        {
            let conn = rusqlite::Connection::open(&old).unwrap();
            conn.pragma_update(None, "user_version", 3).unwrap();
        }
        let why = load(&old).err().unwrap();
        assert!(why.contains("version 3"), "{why}");
        let why = Store::open_for_writing(&old).err().unwrap().to_string();
        assert!(why.contains("version 3"), "{why}");
        assert!(serve(&old, 0, "").is_err());
        assert!(Web::start(&old, 0, "").is_err());
    }

    #[test]
    fn words_can_be_added_changed_and_deleted_from_the_page() {
        let (_dir, db) = seeded("web-glossary");
        // Add.
        let added = post(
            &db,
            "/glossary/add",
            &[
                ("term", " retur "),
                ("area", "sales"),
                ("definition", "goods sent back"),
            ],
        );
        assert_eq!(status(&added), "HTTP/1.1 303 See Other");
        assert_eq!(location(&added), Some("/glossary?ok=added"));
        assert!(words(&db).contains(&(
            "retur".to_owned(),
            "sales".to_owned(),
            "goods sent back".to_owned()
        )));
        // Adding it again is refused and keeps the first.
        let again = post(
            &db,
            "/glossary/add",
            &[("term", "RETUR"), ("definition", "other")],
        );
        assert_eq!(status(&again), "HTTP/1.1 400 Bad Request");
        assert!(again.contains("is in the glossary already"));
        assert_eq!(
            words(&db)
                .iter()
                .filter(|w| w.0.eq_ignore_ascii_case("retur"))
                .count(),
            1
        );
        // The form on the edit page carries the id, the stamp and the token.
        let id = word_id(&db, "retur");
        let page = get_db(&db, &format!("/glossary/edit?id={id}"));
        assert!(page.contains(&format!("name=\"id\" value=\"{id}\"")));
        assert!(page.contains("name=\"v\" value=\""));
        assert!(page.contains(&format!("name=\"token\" value=\"{TOKEN}\"")));
        let entry = Store::open_read_only(&db)
            .unwrap()
            .glossary_entry("retur")
            .unwrap()
            .unwrap();
        // Edit, with a browser's line ends.
        let saved = post(
            &db,
            "/glossary/edit",
            &[
                ("id", &id.to_string()),
                ("v", &entry.fingerprint()),
                ("term", "retur"),
                ("area", "sales"),
                ("definition", "goods\r\nsent back"),
            ],
        );
        assert_eq!(location(&saved), Some("/glossary?ok=saved"), "{saved}");
        assert!(words(&db).iter().any(|w| w.2 == "goods\nsent back"));
        // Deleting asks first: the GET page changes nothing, the POST does.
        let ask_page = get_db(&db, &format!("/glossary/delete?id={id}"));
        assert!(ask_page.contains("cannot be undone"));
        assert_eq!(words(&db).len(), 3);
        let entry = Store::open_read_only(&db)
            .unwrap()
            .glossary_entry("retur")
            .unwrap()
            .unwrap();
        let deleted = post(
            &db,
            "/glossary/delete",
            &[("id", &id.to_string()), ("v", &entry.fingerprint())],
        );
        assert_eq!(location(&deleted), Some("/glossary?ok=deleted"));
        assert_eq!(words(&db).len(), 2);
        // Deleting what is gone says so.
        let gone = post(
            &db,
            "/glossary/delete",
            &[("id", &id.to_string()), ("v", "x")],
        );
        assert_eq!(status(&gone), "HTTP/1.1 404 Not Found");
    }

    #[test]
    fn prompts_can_be_added_changed_switched_moved_and_deleted() {
        let (_dir, db) = seeded("web-prompts");
        let added = post(
            &db,
            "/prompt/add",
            &[
                ("type", "skill"),
                ("name", "review"),
                ("description", "look it over"),
                ("text", "Read\r\nthe diff."),
            ],
        );
        assert_eq!(location(&added), Some("/skills?ok=added"), "{added}");
        let review = prompt(&db, "review").unwrap();
        assert_eq!(review.text, "Read\nthe diff.");
        assert_eq!(review.prompt_type, PromptType::Skill);
        assert!(!review.enabled && review.source_path.is_none());
        // Edit and rename in one step.
        let saved = post(
            &db,
            "/prompt/edit",
            &[
                ("id", &review.id.to_string()),
                ("v", &review.fingerprint()),
                ("name", "review2"),
                ("description", "new"),
                ("text", "Read it."),
            ],
        );
        assert_eq!(location(&saved), Some("/skills?ok=saved"), "{saved}");
        assert!(prompt(&db, "review").is_none());
        let review = prompt(&db, "review2").unwrap();
        assert_eq!(
            (review.description.as_str(), review.text.as_str()),
            ("new", "Read it.")
        );
        // A name that is taken is refused and nothing changes.
        let taken = post(
            &db,
            "/prompt/edit",
            &[
                ("id", &review.id.to_string()),
                ("v", &review.fingerprint()),
                ("name", "tdd"),
                ("description", "new"),
                ("text", "Read it."),
            ],
        );
        assert_eq!(status(&taken), "HTTP/1.1 400 Bad Request");
        assert!(taken.contains("there is already a prompt named tdd"));
        assert!(prompt(&db, "review2").is_some());
        // System prompts: switch on and off.
        let base = prompt(&db, "base").unwrap();
        assert!(base.enabled);
        let off = post(&db, "/prompt/enable", &[("name", "base"), ("on", "0")]);
        assert_eq!(location(&off), Some("/prompts?ok=off"));
        assert!(!prompt(&db, "base").unwrap().enabled);
        let on = post(&db, "/prompt/enable", &[("name", "base"), ("on", "1")]);
        assert_eq!(location(&on), Some("/prompts?ok=on"));
        // A skill cannot be switched on here.
        let skill = post(&db, "/prompt/enable", &[("name", "tdd"), ("on", "1")]);
        assert_eq!(status(&skill), "HTTP/1.1 400 Bad Request");
        // Move within the kind: the only system prompt is at both ends.
        let stay = post(&db, "/prompt/move", &[("name", "base"), ("by", "-1")]);
        assert_eq!(location(&stay), Some("/prompts"));
        let down = post(&db, "/prompt/move", &[("name", "tdd"), ("by", "1")]);
        assert_eq!(location(&down), Some("/skills?ok=moved"), "{down}");
        let bad = post(&db, "/prompt/move", &[("name", "tdd"), ("by", "7")]);
        assert_eq!(status(&bad), "HTTP/1.1 400 Bad Request");
        // Delete asks first.
        let tdd = prompt(&db, "tdd").unwrap();
        let page = get_db(&db, &format!("/prompt/delete?id={}", tdd.id));
        assert!(page.contains("cannot be undone") && prompt(&db, "tdd").is_some());
        let deleted = post(
            &db,
            "/prompt/delete",
            &[("id", &tdd.id.to_string()), ("v", &tdd.fingerprint())],
        );
        assert_eq!(location(&deleted), Some("/skills?ok=deleted"));
        assert!(prompt(&db, "tdd").is_none());
    }

    #[test]
    fn a_refused_form_comes_back_with_what_was_typed_escaped() {
        let (_dir, db) = seeded("web-typed");
        let page = post(
            &db,
            "/glossary/add",
            &[
                ("term", ""),
                ("area", "<b>x</b>"),
                ("definition", "<script>alert(1)</script>\"'"),
            ],
        );
        assert_eq!(status(&page), "HTTP/1.1 400 Bad Request");
        assert!(page.contains("the term is empty"));
        assert!(page.contains("&lt;script&gt;alert(1)&lt;/script&gt;&quot;&#39;"));
        assert!(page.contains("&lt;b&gt;x&lt;/b&gt;"));
        assert!(!page.contains("<script") && !page.contains("<b>x"));
        assert!(page.contains("<details open>"), "the add form stays open");
        let page = post(
            &db,
            "/prompt/add",
            &[
                ("type", "system"),
                ("name", "bad name"),
                ("description", "<i>"),
                ("text", "<script>"),
            ],
        );
        assert_eq!(status(&page), "HTTP/1.1 400 Bad Request");
        assert!(page.contains("may only use letters"));
        assert!(page.contains("&lt;script&gt;") && !page.contains("<script"));
        let long = "x".repeat(MAX_DESCRIPTION_TEST + 1);
        let page = post(
            &db,
            "/prompt/add",
            &[
                ("type", "system"),
                ("name", "ok"),
                ("description", &long),
                ("text", "t"),
            ],
        );
        assert!(page.contains("a description is at most"), "{page}");
        assert!(prompt(&db, "ok").is_none());
        // Nothing was written by any of them.
        assert_eq!(words(&db).len(), 2);
    }

    const MAX_DESCRIPTION_TEST: usize = 200;

    #[test]
    fn an_edit_or_delete_of_a_row_that_changed_meanwhile_is_refused() {
        let (_dir, db) = seeded("web-stale");
        let id = word_id(&db, "claim");
        let before = Store::open_read_only(&db)
            .unwrap()
            .glossary_entry("claim")
            .unwrap()
            .unwrap();
        // The chat edits it while the page is open.
        Store::open(&db)
            .unwrap()
            .set_glossary("claim", "changed in the chat", None)
            .unwrap();
        let page = post(
            &db,
            "/glossary/edit",
            &[
                ("id", &id.to_string()),
                ("v", &before.fingerprint()),
                ("term", "claim"),
                ("area", ""),
                ("definition", "my <text>"),
            ],
        );
        assert_eq!(status(&page), "HTTP/1.1 409 Conflict");
        assert!(page.contains("was changed after you opened it"));
        assert!(page.contains("my &lt;text&gt;"), "what was typed is kept");
        assert!(words(&db).iter().any(|w| w.2 == "changed in the chat"));
        let page = post(
            &db,
            "/glossary/delete",
            &[("id", &id.to_string()), ("v", &before.fingerprint())],
        );
        assert_eq!(status(&page), "HTTP/1.1 409 Conflict");
        assert_eq!(words(&db).len(), 2);
        // The same for a prompt.
        let tdd = prompt(&db, "tdd").unwrap();
        Store::open(&db)
            .unwrap()
            .set_prompt_text("tdd", "new text", None)
            .unwrap();
        for (path, extra) in [("/prompt/edit", true), ("/prompt/delete", false)] {
            let mut fields = vec![("id", tdd.id.to_string()), ("v", tdd.fingerprint())];
            if extra {
                fields.push(("name", "tdd".to_owned()));
                fields.push(("description", String::new()));
                fields.push(("text", "mine".to_owned()));
            }
            let borrowed: Vec<(&str, &str)> =
                fields.iter().map(|(k, v)| (*k, v.as_str())).collect();
            let page = post(&db, path, &borrowed);
            assert_eq!(status(&page), "HTTP/1.1 409 Conflict", "{path}");
        }
        assert_eq!(prompt(&db, "tdd").unwrap().text, "new text");
    }

    /// A named request that must be refused: its extra headers and its body.
    type Case<'a> = (&'a str, Vec<(&'a str, String)>, String);

    #[test]
    fn a_post_that_fails_a_check_writes_nothing() {
        let (_dir, db) = seeded("web-refused");
        let before = (
            words(&db),
            Store::open_read_only(&db).unwrap().list_prompts().unwrap(),
        );
        let body = form_body(&[
            ("token", TOKEN),
            ("term", "intruder"),
            ("definition", "should not be stored"),
        ]);
        let with_token = |token: &str| {
            form_body(&[
                ("token", token),
                ("term", "intruder"),
                ("definition", "should not be stored"),
            ])
        };
        let good = |extra: &[(&'static str, String)]| -> Vec<(&'static str, String)> {
            let mut headers: Vec<(&'static str, String)> = vec![
                ("Origin", format!("http://localhost:{PORT}")),
                (
                    "Content-Type",
                    "application/x-www-form-urlencoded".to_owned(),
                ),
                ("Content-Length", body.len().to_string()),
            ];
            headers.retain(|(name, _)| !extra.iter().any(|(n, _)| n == name));
            headers.extend_from_slice(extra);
            headers
        };
        let origin = |value: &str| good(&[("Origin", value.to_owned())]);
        let ctype = |value: &str| good(&[("Content-Type", value.to_owned())]);
        let length = |value: String| good(&[("Content-Length", value)]);
        let cases: Vec<Case> = vec![
            (
                "foreign origin",
                origin("http://evil.example"),
                body.clone(),
            ),
            ("null origin", origin("null"), body.clone()),
            ("other port", origin("http://localhost:9999"), body.clone()),
            (
                "https origin",
                origin(&format!("https://localhost:{PORT}")),
                body.clone(),
            ),
            ("json", ctype("application/json"), body.clone()),
            ("text", ctype("text/plain"), body.clone()),
            (
                "multipart",
                ctype("multipart/form-data; boundary=x"),
                body.clone(),
            ),
            ("wrong token", good(&[]), with_token("wrong")),
            (
                "no token",
                good(&[]),
                "term=intruder&definition=x".to_owned(),
            ),
            ("empty token", good(&[]), with_token("")),
            (
                "token cut short",
                good(&[]),
                with_token(&TOKEN[..TOKEN.len() - 1]),
            ),
            (
                "token with more",
                good(&[]),
                with_token(&format!("{TOKEN}0")),
            ),
            (
                "short body",
                length((body.len() + 10).to_string()),
                body.clone(),
            ),
            (
                "huge length",
                length((MAX_BODY + 1).to_string()),
                body.clone(),
            ),
            ("bad length", length("abc".to_owned()), body.clone()),
        ];
        for (name, headers, text) in cases {
            let page = post_raw(&db, "/glossary/add", &headers, &text);
            let code = status(&page).to_owned();
            assert!(
                [
                    "HTTP/1.1 403 Forbidden",
                    "HTTP/1.1 415 Unsupported Media Type",
                    "HTTP/1.1 400 Bad Request",
                    "HTTP/1.1 413 Content Too Large",
                    "HTTP/1.1 411 Length Required"
                ]
                .contains(&code.as_str()),
                "{name}: {code}"
            );
            assert_eq!(location(&page), None, "{name}");
        }
        // No length at all, and no origin at all.
        let page = post_raw(
            &db,
            "/glossary/add",
            &[
                ("Origin", format!("http://localhost:{PORT}")),
                (
                    "Content-Type",
                    "application/x-www-form-urlencoded".to_owned(),
                ),
            ],
            &body,
        );
        assert_eq!(status(&page), "HTTP/1.1 411 Length Required");
        let page = post_raw(
            &db,
            "/glossary/add",
            &[
                (
                    "Content-Type",
                    "application/x-www-form-urlencoded".to_owned(),
                ),
                ("Content-Length", body.len().to_string()),
            ],
            &body,
        );
        assert_eq!(status(&page), "HTTP/1.1 403 Forbidden");
        // The token must not be accepted from the address line either.
        let page = ask_db(
            &db,
            &format!(
                "GET /glossary/add?token={TOKEN}&term=x&definition=y HTTP/1.1\r\nHost: localhost:{PORT}\r\n\r\n"
            ),
        );
        assert_eq!(status(&page), "HTTP/1.1 405 Method Not Allowed");
        let after = (
            words(&db),
            Store::open_read_only(&db).unwrap().list_prompts().unwrap(),
        );
        assert_eq!(before, after, "a refused request changed the database");
    }

    #[test]
    fn too_many_fields_and_the_token_comparison() {
        assert!(Form::parse(b"a=1&b=2").is_some());
        let many = (0..MAX_FIELDS + 1)
            .map(|i| format!("f{i}=1"))
            .collect::<Vec<_>>()
            .join("&");
        assert!(Form::parse(many.as_bytes()).is_none());
        let form = Form::parse(b"a=x%20y&a=second&b").unwrap();
        assert_eq!(
            (form.get("a"), form.get("b"), form.get("c")),
            ("x y", "", "")
        );
        assert!(same("abc", "abc"));
        assert!(!same("abc", "abd") && !same("abc", "ab") && !same("", "a") && !same("a", ""));
        assert!(same("", ""));
    }

    #[test]
    fn a_database_that_cannot_be_written_gives_a_page_not_a_crash() {
        let (_dir, db) = seeded("web-nowrite");
        let write = || Err("the database is locked".to_owned());
        let read = || load(&db);
        let ctx = Ctx {
            project: "",
            port: PORT,
            token: TOKEN,
            read: &read,
            write: &write,
            open: &no_open,
        };
        let body = form_body(&[("token", TOKEN), ("term", "x"), ("definition", "y")]);
        let request = format!(
            "POST /glossary/add HTTP/1.1\r\nHost: localhost:{PORT}\r\nOrigin: http://localhost:{PORT}\r\nContent-Type: application/x-www-form-urlencoded\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        let page = String::from_utf8(respond(request.as_bytes(), &ctx)).unwrap();
        assert_eq!(status(&page), "HTTP/1.1 503 Service Unavailable");
        assert!(page.contains("busy"));
    }

    #[test]
    fn a_write_waits_for_a_chat_that_holds_the_database_open() {
        let (_dir, db) = seeded("web-with-chat");
        // The chat has the database open and has just written (WAL mode).
        let mut chat = Store::open(&db).unwrap();
        chat.set_glossary("from chat", "typed in the terminal", None)
            .unwrap();
        let added = post(
            &db,
            "/glossary/add",
            &[
                ("term", "from page"),
                ("definition", "typed in the browser"),
            ],
        );
        assert_eq!(location(&added), Some("/glossary?ok=added"), "{added}");
        // The chat sees the page's word, and the page the chat's.
        assert!(chat.glossary_entry("from page").unwrap().is_some());
        assert!(words(&db).iter().any(|w| w.0 == "from chat"));
    }

    fn fetch(port: u16, request: &str) -> String {
        let mut stream = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).unwrap();
        stream.write_all(request.as_bytes()).unwrap();
        let mut text = String::new();
        stream.read_to_string(&mut text).unwrap();
        text
    }

    #[test]
    fn a_real_connection_on_the_loopback_address_gets_the_page() {
        let (_dir, path) = seeded("web-loopback");
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = Server::new(&path, port, "").unwrap();
        let thread = std::thread::spawn(move || serve_on(&listener, &server, Some(2)));
        let good = fetch(
            port,
            &format!("GET /glossary HTTP/1.1\r\nHost: localhost:{port}\r\n\r\n"),
        );
        assert_eq!(status(&good), "HTTP/1.1 200 OK");
        assert!(good.contains("kartu stok"));
        let bad = fetch(port, "GET /glossary HTTP/1.1\r\nHost: evil.example\r\n\r\n");
        assert_eq!(status(&bad), "HTTP/1.1 421 Misdirected Request");
        thread.join().unwrap();
    }

    #[test]
    fn the_server_runs_on_a_thread_takes_a_big_form_and_stops() {
        let (_dir, db) = seeded("web-thread");
        let mut web = Web::start(&db, 0, "").unwrap();
        assert!(web.running());
        let port: u16 = web
            .url()
            .trim_start_matches("http://127.0.0.1:")
            .trim_end_matches('/')
            .parse()
            .unwrap();
        let page = fetch(
            port,
            &format!("GET /glossary HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\r\n"),
        );
        assert_eq!(status(&page), "HTTP/1.1 200 OK");
        // The token is in the page's forms, like a browser would read it.
        let token = page
            .split("name=\"token\" value=\"")
            .nth(1)
            .and_then(|rest| rest.split('"').next())
            .unwrap()
            .to_owned();
        assert_eq!(token.len(), 64);
        // A form far bigger than one read, sent in pieces.
        let text = "line of a long prompt\n".repeat(9_000);
        let body = form_body(&[
            ("token", &token),
            ("type", "skill"),
            ("name", "long"),
            ("description", "big"),
            ("text", &text),
        ]);
        assert!(body.len() > 100_000);
        let head = format!(
            "POST /prompt/add HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nOrigin: http://127.0.0.1:{port}\r\nContent-Type: application/x-www-form-urlencoded\r\nContent-Length: {}\r\n\r\n",
            body.len()
        );
        let mut stream = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).unwrap();
        stream.write_all(head.as_bytes()).unwrap();
        for piece in body.as_bytes().chunks(20_000) {
            stream.write_all(piece).unwrap();
        }
        let mut answer = String::new();
        stream.read_to_string(&mut answer).unwrap();
        assert_eq!(
            location(&answer),
            Some("/skills?ok=added"),
            "{}",
            &answer[..answer.len().min(300)]
        );
        assert_eq!(
            prompt(&db, "long").unwrap().text,
            text.replace("\r\n", "\n")
        );
        // A request with the wrong token over the real socket is refused.
        let wrong = form_body(&[("token", "nope"), ("term", "x"), ("definition", "y")]);
        let refused = fetch(
            port,
            &format!(
                "POST /glossary/add HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nOrigin: http://127.0.0.1:{port}\r\nContent-Type: application/x-www-form-urlencoded\r\nContent-Length: {}\r\n\r\n{wrong}",
                wrong.len()
            ),
        );
        assert_eq!(status(&refused), "HTTP/1.1 403 Forbidden");
        // Stopping ends the thread and frees the port; stopping twice is harmless.
        web.stop();
        assert!(!web.running());
        web.stop();
        assert!(TcpListener::bind((Ipv4Addr::LOCALHOST, port)).is_ok());
    }

    #[test]
    fn a_taken_port_is_an_error_and_dropping_stops_the_thread() {
        let (_dir, db) = seeded("web-port");
        let holder = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let taken = holder.local_addr().unwrap().port();
        let why = Web::start(&db, taken, "").err().unwrap();
        assert!(why.contains("cannot listen"), "{why}");
        drop(holder);
        let web = Web::start(&db, 0, "").unwrap();
        let url = web.url();
        let port: u16 = url
            .trim_start_matches("http://127.0.0.1:")
            .trim_end_matches('/')
            .parse()
            .unwrap();
        drop(web);
        assert!(TcpListener::bind((Ipv4Addr::LOCALHOST, port)).is_ok());
    }

    #[test]
    fn tokens_are_long_and_different_each_time() {
        let (a, b) = (new_token().unwrap(), new_token().unwrap());
        assert_eq!(a.len(), 64);
        assert!(a.bytes().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
    }

    /// Like [`post`]: a field may repeat (a group of checkboxes).
    fn post_many(db: &Path, path: &str, fields: &[(&str, &str)]) -> String {
        post(db, path, fields)
    }

    fn profile_id(db: &Path, name: &str) -> i64 {
        Store::open_read_only(db).unwrap().profile(name).unwrap().id
    }

    fn profile(db: &Path, name: &str) -> Option<crate::store_profiles::Profile> {
        Store::open_read_only(db).unwrap().profile(name).ok()
    }

    #[test]
    fn profiles_are_added_edited_copied_used_and_deleted_from_the_web() {
        let (_dir, db) = seeded("web-profiles");
        {
            let mut store = Store::open_for_writing(&db).unwrap();
            store
                .add_prompt("extra", "more", "Extra rules.", None)
                .unwrap();
        }
        let page = get_db(&db, "/profiles");
        assert!(page.contains("new chats start under it"), "{page}");
        assert!(page.contains("<code>base</code>"), "{page}");
        // Add, then fill it in from the edit form.
        let added = post(
            &db,
            "/profile/add",
            &[("name", "work"), ("description", "for work")],
        );
        assert_eq!(location(&added), Some("/profiles?ok=added"), "{added}");
        let work = profile(&db, "work").unwrap();
        assert!(work.prompts.is_empty());
        let edit_page = get_db(&db, &format!("/profile/edit?id={}", work.id));
        assert!(
            edit_page.contains("name=\"prompt\" value=\"extra\""),
            "{edit_page}"
        );
        assert!(
            edit_page.contains("name=\"skill\" value=\"tdd\""),
            "{edit_page}"
        );
        assert!(
            edit_page.contains("name=\"personality\" value=\"pirate\""),
            "{edit_page}"
        );
        let saved = post_many(
            &db,
            "/profile/edit",
            &[
                ("id", &work.id.to_string()),
                ("v", &work.fingerprint()),
                ("name", "work2"),
                ("description", "for the week"),
                ("prompt", "base"),
                ("prompt", "extra"),
                ("skill", "tdd"),
                ("personality", "pirate"),
            ],
        );
        assert_eq!(location(&saved), Some("/profiles?ok=saved"), "{saved}");
        assert!(profile(&db, "work").is_none());
        let work = profile(&db, "work2").unwrap();
        assert_eq!(work.description, "for the week");
        assert_eq!(work.prompts, ["base", "extra"]);
        assert_eq!(work.skills, ["tdd"]);
        assert_eq!(work.personality.as_deref(), Some("pirate"));
        // Unticking everything and choosing no personality empties it again.
        let emptied = post_many(
            &db,
            "/profile/edit",
            &[
                ("id", &work.id.to_string()),
                ("v", &work.fingerprint()),
                ("name", "work2"),
                ("description", ""),
                ("personality", ""),
            ],
        );
        assert_eq!(location(&emptied), Some("/profiles?ok=saved"), "{emptied}");
        let work = profile(&db, "work2").unwrap();
        assert!(work.prompts.is_empty() && work.skills.is_empty() && work.personality.is_none());
        // Copy, use, delete.
        let copied = post(
            &db,
            "/profile/copy",
            &[("name", "default"), ("new", "second")],
        );
        assert_eq!(location(&copied), Some("/profiles?ok=copied"), "{copied}");
        assert_eq!(profile(&db, "second").unwrap().prompts, ["base"]);
        let used = post(&db, "/profile/use", &[("name", "second")]);
        assert_eq!(location(&used), Some("/profiles?ok=used"), "{used}");
        assert_eq!(
            Store::open_read_only(&db)
                .unwrap()
                .selected_profile()
                .unwrap(),
            "second"
        );
        let home = get_db(&db, "/");
        assert!(home.contains("<strong>second</strong>"), "{home}");
        let doomed = profile(&db, "work2").unwrap();
        let confirm = get_db(&db, &format!("/profile/delete?id={}", doomed.id));
        assert!(confirm.contains("cannot be undone"), "{confirm}");
        let deleted = post(
            &db,
            "/profile/delete",
            &[("id", &doomed.id.to_string()), ("v", &doomed.fingerprint())],
        );
        assert_eq!(
            location(&deleted),
            Some("/profiles?ok=deleted"),
            "{deleted}"
        );
        assert!(profile(&db, "work2").is_none());
    }

    #[test]
    fn profile_writes_that_are_wrong_change_nothing() {
        let (_dir, db) = seeded("web-profiles-refused");
        let default = profile(&db, "default").unwrap();
        // The selected profile cannot be deleted.
        let page = post(
            &db,
            "/profile/delete",
            &[
                ("id", &default.id.to_string()),
                ("v", &default.fingerprint()),
            ],
        );
        assert_eq!(status(&page), "HTTP/1.1 400 Bad Request", "{page}");
        assert!(page.contains("use another one first"), "{page}");
        assert!(profile(&db, "default").is_some());
        // A stale stamp is refused for edit and delete.
        let stale = post_many(
            &db,
            "/profile/edit",
            &[
                ("id", &default.id.to_string()),
                ("v", "0000000000000000"),
                ("name", "default"),
                ("description", "typed meanwhile"),
            ],
        );
        assert_eq!(status(&stale), "HTTP/1.1 409 Conflict", "{stale}");
        assert!(stale.contains("typed meanwhile"), "{stale}");
        assert_eq!(profile(&db, "default").unwrap(), default);
        // A name that is taken, a wrong kind, an unknown prompt and a long description.
        let other = post(
            &db,
            "/profile/add",
            &[("name", "other"), ("description", "")],
        );
        assert_eq!(location(&other), Some("/profiles?ok=added"));
        let other = profile(&db, "other").unwrap();
        for (fields, why) in [
            (vec![("name", "DEFAULT")], "already a profile named"),
            (vec![("name", "other"), ("skill", "base")], "not a skill"),
            (vec![("name", "other"), ("prompt", "tdd")], "not a system"),
            (
                vec![("name", "other"), ("prompt", "ghost")],
                "no prompt named ghost",
            ),
            (
                vec![("name", "other"), ("personality", "tdd")],
                "not a personality",
            ),
            (vec![("name", "list")], "cannot be a name"),
        ] {
            let mut all = vec![
                ("id", other.id.to_string()),
                ("v", other.fingerprint()),
                ("description", String::new()),
            ];
            all.extend(fields.iter().map(|(k, v)| (*k, (*v).to_owned())));
            let borrowed: Vec<(&str, &str)> = all.iter().map(|(k, v)| (*k, v.as_str())).collect();
            let page = post_many(&db, "/profile/edit", &borrowed);
            assert_eq!(status(&page), "HTTP/1.1 400 Bad Request", "{why}: {page}");
            assert!(page.contains(why), "{why}: {page}");
            assert!(
                page.contains("<form"),
                "the edit form comes back with what was typed"
            );
            assert_eq!(profile(&db, "other").unwrap(), other, "{why}");
        }
        let long = "x".repeat(201);
        let page = post(
            &db,
            "/profile/add",
            &[("name", "long"), ("description", &long)],
        );
        assert_eq!(status(&page), "HTTP/1.1 400 Bad Request", "{page}");
        assert!(profile(&db, "long").is_none());
        // Using or copying what is not there.
        assert_eq!(
            status(&post(&db, "/profile/use", &[("name", "ghost")])),
            "HTTP/1.1 400 Bad Request"
        );
        assert_eq!(
            status(&post(
                &db,
                "/profile/copy",
                &[("name", "ghost"), ("new", "x")]
            )),
            "HTTP/1.1 400 Bad Request"
        );
        // A profile that is gone.
        let gone = post(&db, "/profile/delete", &[("id", "99999"), ("v", "x")]);
        assert_eq!(status(&gone), "HTTP/1.1 404 Not Found", "{gone}");
        assert_eq!(
            status(&get_db(&db, "/profile/edit?id=99999")),
            "HTTP/1.1 404 Not Found"
        );
        // Without the token or from another origin nothing is written.
        let body = form_body(&[("name", "sneaky"), ("description", "")]);
        let headers = |origin: &str| {
            vec![
                ("Origin", origin.to_owned()),
                (
                    "Content-Type",
                    "application/x-www-form-urlencoded".to_owned(),
                ),
                ("Content-Length", body.len().to_string()),
            ]
        };
        let page = post_raw(
            &db,
            "/profile/add",
            &headers(&format!("http://localhost:{PORT}")),
            &body,
        );
        assert_eq!(status(&page), "HTTP/1.1 403 Forbidden", "{page}");
        let with_token = form_body(&[("token", TOKEN), ("name", "sneaky"), ("description", "")]);
        let foreign = vec![
            ("Origin", "http://evil.example".to_owned()),
            (
                "Content-Type",
                "application/x-www-form-urlencoded".to_owned(),
            ),
            ("Content-Length", with_token.len().to_string()),
        ];
        let page = post_raw(&db, "/profile/add", &foreign, &with_token);
        assert_eq!(status(&page), "HTTP/1.1 403 Forbidden", "{page}");
        assert!(profile(&db, "sneaky").is_none());
    }

    #[test]
    fn a_profile_form_with_many_boxes_ticked_is_accepted() {
        let (_dir, db) = seeded("web-profiles-many");
        {
            let mut store = Store::open_for_writing(&db).unwrap();
            for n in 0..60 {
                store
                    .add_prompt(&format!("p{n:02}"), "", "text", None)
                    .unwrap();
            }
            store.add_profile("big", "").unwrap();
        }
        let big = profile(&db, "big").unwrap();
        let names: Vec<String> = (0..60).map(|n| format!("p{n:02}")).collect();
        let mut fields: Vec<(&str, &str)> = vec![
            ("id", ""),
            ("v", ""),
            ("name", "big"),
            ("description", ""),
            ("personality", ""),
        ];
        let id = big.id.to_string();
        let stamp = big.fingerprint();
        fields[0].1 = &id;
        fields[1].1 = &stamp;
        fields.extend(names.iter().map(|n| ("prompt", n.as_str())));
        let saved = post_many(&db, "/profile/edit", &fields);
        assert_eq!(location(&saved), Some("/profiles?ok=saved"), "{saved}");
        assert_eq!(profile(&db, "big").unwrap().prompts.len(), 60);
    }

    #[test]
    fn profile_names_and_texts_are_escaped_on_every_new_page() {
        let (_dir, db) = seeded("web-profiles-escape");
        let nasty = "<script>alert(1)</script>";
        {
            let mut store = Store::open_for_writing(&db).unwrap();
            store.add_profile("evil", "").unwrap();
            store.set_profile_description("evil", "<b>x</b>").unwrap();
            store
                .add_prompt_of_type("a-prompt", nasty, "t", None, PromptType::Personality)
                .unwrap();
            store
                .set_profile_personality("evil", Some("a-prompt"))
                .unwrap();
        }
        let evil = profile_id(&db, "evil");
        for path in [
            "/profiles".to_owned(),
            format!("/profile/edit?id={evil}"),
            format!("/profile/delete?id={evil}"),
            "/".to_owned(),
        ] {
            let page = get_db(&db, &path);
            assert_eq!(status(&page), "HTTP/1.1 200 OK", "{path}");
            assert!(
                !page.contains("<script>alert") && !page.contains("<b>x</b>"),
                "{path}"
            );
        }
        let page = get_db(&db, "/profiles");
        assert!(page.contains("&lt;b&gt;x&lt;/b&gt;"), "{page}");
    }

    #[test]
    fn the_database_page_draws_the_relationships_and_reads_no_rows() {
        let (_dir, db) = seeded("web-database");
        let page = get_db(&db, "/database");
        assert_eq!(status(&page), "HTTP/1.1 200 OK");
        for expected in [
            "<svg",
            "Relationships",
            "id=\"t-profile_prompts\"",
            "ON DELETE CASCADE",
            "A row of profiles has zero or many rows of profile_prompts",
            "id=\"t-providers\"",
        ] {
            assert!(page.contains(expected), "{expected}");
        }
        for secret in [
            SECRET,
            "secret.example",
            "secret-model",
            "secretrouter",
            "Write the test first.",
        ] {
            assert!(!page.contains(secret), "{secret} reached the database page");
        }
        assert!(
            page.contains("api_key"),
            "the column name is shown, never its value"
        );
        // It never changes the database file.
        let before = std::fs::read(&db).unwrap();
        get_db(&db, "/database");
        assert_eq!(std::fs::read(&db).unwrap(), before);
    }

    #[test]
    fn the_usage_pages_show_numbers_and_a_chat_that_is_not_there_is_a_404() {
        let (_dir, db) = seeded("web-usage");
        {
            let store = Store::open_for_writing(&db).unwrap();
            store
                .conn
                .execute(
                    "INSERT INTO system_prompts (sha256, text, created_at) VALUES ('h', 'SECRET-SYSTEM-TEXT', 1)",
                    [],
                )
                .unwrap();
            store
                .conn
                .execute(
                    "INSERT INTO sessions (model, base_url, router_options, system_prompt_id, cwd, harness_version, created_at, updated_at, profile) \
                     VALUES ('m1', 'http://x', '{}', 1, '/', 'v', 1000, 2000, 'default')",
                    [],
                )
                .unwrap();
            store
                .conn
                .execute(
                    "INSERT INTO messages (session_id, seq, turn, role, json, text, status, created_at) \
                     VALUES (1, 1, 1, 'user', '{}', 'SECRET-CHAT-TEXT', 'ok', 1)",
                    [],
                )
                .unwrap();
            store
                .conn
                .execute(
                    "INSERT INTO calls (session_id, turn, message_count, request_sha256, request_bytes, response, prompt_tokens, cached_tokens, completion_tokens, elapsed_ms, created_at, model, http_status) \
                     VALUES (1, 1, 2, 'h', 5, 'SECRET-RESPONSE', 12345, 6000, 77, 2500, 1767225600, 'm1', 200)",
                    [],
                )
                .unwrap();
        }
        let page = get_db(&db, "/usage");
        assert_eq!(status(&page), "HTTP/1.1 200 OK");
        for expected in [
            "12.3k",
            "2026-01-01",
            "<a href=\"/usage/chat?id=1\">1</a>",
            "default",
            "m1",
        ] {
            assert!(page.contains(expected), "{expected}: {page}");
        }
        let chat = get_db(&db, "/usage/chat?id=1");
        assert_eq!(status(&chat), "HTTP/1.1 200 OK");
        assert!(chat.contains("2.5 s") && chat.contains("12.3k"), "{chat}");
        for path in ["/usage/chat?id=999", "/usage/chat?id=abc", "/usage/chat"] {
            assert_eq!(
                status(&get_db(&db, path)),
                "HTTP/1.1 404 Not Found",
                "{path}"
            );
        }
        let all = format!("{page}{chat}{}", get_db(&db, "/"));
        for secret in [
            "SECRET-SYSTEM-TEXT",
            "SECRET-CHAT-TEXT",
            "SECRET-RESPONSE",
            SECRET,
        ] {
            assert!(!all.contains(secret), "{secret} reached a page");
        }
        // The home page says how much today's use is, and works with no database to ask.
        assert!(get("/").contains("Usage"));
    }
}
