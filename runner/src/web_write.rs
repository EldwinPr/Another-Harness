//! What the web page's forms do (issue 29): add, change and delete glossary words and prompts.
//!
//! Called from `web.rs` only after the request passed the `Host`, `Origin`, content-type, size
//! and token checks. Every rule about what a word or a prompt may be lives in the store (the same
//! functions the terminal uses); this file only reads the form, calls the store and turns the
//! result into a redirect (done), the form again with what was typed (refused), or a page saying
//! the row changed or is gone. Nothing outside the prompts, glossary and profile tables (and the
//! `profile` setting) is touched.

use crate::store::{Edit, Store};
use crate::store_context::{ContextDraft, check_scope};
use crate::store_prompts::{Prompt, PromptType};
use crate::web::{Ctx, Form, Reply};
use crate::web_pages::{self, GlossaryDraft, ProfileDraft, PromptDraft, View, list_path};

/// The longest description of a prompt, in characters (the terminal takes one line).
const MAX_DESCRIPTION_CHARS: usize = 200;

pub(crate) fn post(path: &str, form: &Form, ctx: &Ctx<'_>) -> Reply {
    let mut store = match (ctx.write)() {
        Ok(store) => store,
        Err(why) => return failed(&why),
    };
    match path {
        "/glossary/add" => glossary_add(form, &mut store, ctx),
        "/glossary/edit" => glossary_edit(form, &mut store, ctx),
        "/glossary/delete" => glossary_delete(form, &mut store),
        "/context/add" => context_add(form, &mut store, ctx),
        "/context/edit" => context_edit(form, &mut store, ctx),
        "/context/delete" => context_delete(form, &mut store),
        "/prompt/add" => prompt_add(form, &mut store, ctx),
        "/prompt/edit" => prompt_edit(form, &mut store, ctx),
        "/prompt/delete" => prompt_delete(form, &mut store),
        "/prompt/enable" => prompt_enable(form, &mut store),
        "/prompt/move" => prompt_move(form, &mut store),
        "/profile/add" => profile_add(form, &mut store, ctx),
        "/profile/edit" => profile_edit(form, &mut store, ctx),
        "/profile/delete" => profile_delete(form, &mut store),
        "/profile/use" => profile_use(form, &mut store),
        "/profile/copy" => profile_copy(form, &mut store, ctx),
        _ => Reply::page(404, web_pages::not_found()),
    }
}

/// A refusal's text, and the status that goes with it: a busy database is not the person's mistake.
fn explain(why: &str) -> (u16, String) {
    if why.contains("locked") || why.contains("busy") {
        (
            503,
            "The database is busy (the chat is writing). Try again in a moment.".to_owned(),
        )
    } else {
        (400, why.to_owned())
    }
}

/// The database could not be opened or answered.
fn failed(why: &str) -> Reply {
    let (status, text) = explain(why);
    Reply::page(
        if status == 400 { 500 } else { status },
        web_pages::error(&text),
    )
}

fn view_with<'a>(ctx: &'a Ctx<'_>, error: &'a str) -> View<'a> {
    View {
        token: ctx.token,
        ok: None,
        error: Some(error),
    }
}

/// Text from a textarea: the browser sends line ends as CR LF; the database keeps LF.
fn unix(text: &str) -> String {
    text.replace("\r\n", "\n")
}

fn id_of(form: &Form) -> Option<i64> {
    form.get("id").parse().ok()
}

fn bad_form() -> Reply {
    Reply::simple(400, "Bad request", "The form is missing something.")
}

fn changed(back: &str, what: &str, typed: &str) -> Reply {
    Reply::page(409, web_pages::stale(back, what, typed))
}

fn gone() -> Reply {
    Reply::simple(404, "Gone", "That entry no longer exists.")
}

fn glossary_draft(form: &Form) -> GlossaryDraft {
    GlossaryDraft {
        term: form.get("term").to_owned(),
        area: form.get("area").to_owned(),
        definition: unix(form.get("definition")),
        project: form.get("project").to_owned(),
    }
}

fn prompt_draft(form: &Form) -> PromptDraft {
    PromptDraft {
        name: form.get("name").trim().to_owned(),
        description: form.get("description").to_owned(),
        text: unix(form.get("text")),
    }
}

fn check_description(description: &str) -> Result<String, String> {
    let description = description.trim();
    if description.chars().any(char::is_control) {
        return Err("a description is one line of plain text".to_owned());
    }
    if description.chars().count() > MAX_DESCRIPTION_CHARS {
        return Err(format!(
            "a description is at most {MAX_DESCRIPTION_CHARS} characters"
        ));
    }
    Ok(description.to_owned())
}

fn glossary_add(form: &Form, store: &mut Store, ctx: &Ctx<'_>) -> Reply {
    let draft = glossary_draft(form);
    match store.add_glossary_in(&draft.project, &draft.term, &draft.area, &draft.definition) {
        Ok(()) => Reply::redirect("/glossary?ok=added"),
        Err(e) => {
            let (status, why) = explain(&e.to_string());
            match (ctx.read)() {
                Ok(data) => Reply::page(
                    status,
                    web_pages::glossary(
                        &data,
                        "",
                        &web_pages::Filter::all(),
                        &view_with(ctx, &why),
                        &draft,
                    ),
                ),
                Err(e) => failed(&e),
            }
        }
    }
}

fn glossary_edit(form: &Form, store: &mut Store, ctx: &Ctx<'_>) -> Reply {
    let Some(id) = id_of(form) else {
        return bad_form();
    };
    let v = form.get("v");
    let draft = glossary_draft(form);
    match store.edit_glossary(id, v, &draft.term, &draft.area, &draft.definition) {
        Ok(Edit::Done) => Reply::redirect("/glossary?ok=saved"),
        Ok(Edit::Stale) => changed(
            &format!("/glossary/edit?id={id}"),
            "word",
            &draft.definition,
        ),
        Ok(Edit::Gone) => gone(),
        Err(e) => {
            let (status, why) = explain(&e.to_string());
            let data = match (ctx.read)() {
                Ok(data) => data,
                Err(e) => return failed(&e),
            };
            match data.glossary.iter().find(|e| e.id == id) {
                Some(entry) => Reply::page(
                    status,
                    web_pages::glossary_edit(entry, v, &view_with(ctx, &why), &draft),
                ),
                None => gone(),
            }
        }
    }
}

fn glossary_delete(form: &Form, store: &mut Store) -> Reply {
    let Some(id) = id_of(form) else {
        return bad_form();
    };
    match store.delete_glossary_checked(id, form.get("v")) {
        Ok(Edit::Done) => Reply::redirect("/glossary?ok=deleted"),
        Ok(Edit::Stale) => changed(&format!("/glossary/delete?id={id}"), "word", ""),
        Ok(Edit::Gone) => gone(),
        Err(e) => failed(&e.to_string()),
    }
}

fn context_draft(form: &Form) -> ContextDraft {
    ContextDraft {
        name: form.get("name").trim().to_owned(),
        kind: form.get("kind").to_owned(),
        text: unix(form.get("text")),
        quote: form.get("quote").to_owned(),
        area: form.get("area").to_owned(),
        preload: form.get("preload") == "1",
    }
}

fn context_add(form: &Form, store: &mut Store, ctx: &Ctx<'_>) -> Reply {
    let draft = context_draft(form);
    let scope = form.get("project");
    let saved = check_scope(scope)
        .map_err(|e| e.to_string())
        .and_then(|scope| {
            store
                .add_context_entry(&scope, &draft)
                .map_err(|e| e.to_string())
        });
    match saved {
        Ok(()) => Reply::redirect("/context?ok=added"),
        Err(why) => {
            let (status, why) = explain(&why);
            match store.list_all_context() {
                Ok(entries) => Reply::page(
                    status,
                    web_pages::context(
                        &entries,
                        "",
                        &web_pages::Filter::all(),
                        &view_with(ctx, &why),
                        &draft,
                        scope,
                    ),
                ),
                Err(e) => failed(&e.to_string()),
            }
        }
    }
}

fn context_edit(form: &Form, store: &mut Store, ctx: &Ctx<'_>) -> Reply {
    let Some(id) = id_of(form) else {
        return bad_form();
    };
    let v = form.get("v");
    let draft = context_draft(form);
    match store.edit_context_entry(id, v, &draft) {
        Ok(Edit::Done) => Reply::redirect("/context?ok=saved"),
        Ok(Edit::Stale) => changed(&format!("/context/edit?id={id}"), "entry", &draft.text),
        Ok(Edit::Gone) => gone(),
        Err(e) => {
            let (status, why) = explain(&e.to_string());
            let entries = match store.list_all_context() {
                Ok(entries) => entries,
                Err(e) => return failed(&e.to_string()),
            };
            match entries.iter().find(|e| e.id == id) {
                Some(entry) => Reply::page(
                    status,
                    web_pages::context_edit(entry, v, &view_with(ctx, &why), &draft),
                ),
                None => gone(),
            }
        }
    }
}

fn context_delete(form: &Form, store: &mut Store) -> Reply {
    let Some(id) = id_of(form) else {
        return bad_form();
    };
    match store.delete_context_checked(id, form.get("v")) {
        Ok(Edit::Done) => Reply::redirect("/context?ok=deleted"),
        Ok(Edit::Stale) => changed(&format!("/context/delete?id={id}"), "entry", ""),
        Ok(Edit::Gone) => gone(),
        Err(e) => failed(&e.to_string()),
    }
}

/// The prompt with this id, as the store has it now.
fn prompt_by_id(store: &Store, id: i64) -> Option<Prompt> {
    store.list_prompts().ok()?.into_iter().find(|p| p.id == id)
}

fn prompt_add(form: &Form, store: &mut Store, ctx: &Ctx<'_>) -> Reply {
    let Some(kind) = PromptType::parse(form.get("type")) else {
        return bad_form();
    };
    let draft = prompt_draft(form);
    let saved = check_description(&draft.description)
        .and_then(|description| {
            store
                .add_prompt_of_type(&draft.name, &description, &draft.text, None, kind)
                .map_err(|e| e.to_string())
        })
        .map(|()| format!("{}?ok=added", list_path(kind)));
    match saved {
        Ok(to) => Reply::redirect(&to),
        Err(why) => {
            let (status, why) = explain(&why);
            match (ctx.read)() {
                Ok(data) => Reply::page(
                    status,
                    web_pages::prompts(&data, kind, &view_with(ctx, &why), &draft),
                ),
                Err(e) => failed(&e),
            }
        }
    }
}

fn prompt_edit(form: &Form, store: &mut Store, ctx: &Ctx<'_>) -> Reply {
    let Some(id) = id_of(form) else {
        return bad_form();
    };
    let v = form.get("v");
    let draft = prompt_draft(form);
    let result = check_description(&draft.description).and_then(|description| {
        store
            .edit_prompt(id, v, &draft.name, &description, &draft.text)
            .map_err(|e| e.to_string())
    });
    match result {
        Ok(Edit::Done) => match prompt_by_id(store, id) {
            Some(p) => Reply::redirect(&format!("{}?ok=saved", list_path(p.prompt_type))),
            None => gone(),
        },
        Ok(Edit::Stale) => changed(&format!("/prompt/edit?id={id}"), "prompt", &draft.text),
        Ok(Edit::Gone) => gone(),
        Err(why) => {
            let (status, why) = explain(&why);
            match prompt_by_id(store, id) {
                Some(p) => Reply::page(
                    status,
                    web_pages::prompt_edit(&p, v, &view_with(ctx, &why), &draft),
                ),
                None => gone(),
            }
        }
    }
}

fn prompt_delete(form: &Form, store: &mut Store) -> Reply {
    let Some(id) = id_of(form) else {
        return bad_form();
    };
    let Some(prompt) = prompt_by_id(store, id) else {
        return gone();
    };
    match store.delete_prompt_checked(id, form.get("v")) {
        Ok(Edit::Done) => Reply::redirect(&format!("{}?ok=deleted", list_path(prompt.prompt_type))),
        Ok(Edit::Stale) => changed(&format!("/prompt/delete?id={id}"), "prompt", ""),
        Ok(Edit::Gone) => gone(),
        Err(e) => failed(&e.to_string()),
    }
}

fn prompt_enable(form: &Form, store: &mut Store) -> Reply {
    let name = form.get("name");
    let on = match form.get("on") {
        "1" => true,
        "0" => false,
        _ => return bad_form(),
    };
    match store.set_prompt_enabled(name, on) {
        Ok(()) => Reply::redirect(&format!(
            "{}?ok={}",
            list_path(PromptType::System),
            if on { "on" } else { "off" }
        )),
        Err(e) => {
            let (status, why) = explain(&e.to_string());
            Reply::page(status, web_pages::error(&why))
        }
    }
}

fn prompt_move(form: &Form, store: &mut Store) -> Reply {
    let name = form.get("name");
    let by = match form.get("by") {
        "-1" => -1,
        "1" => 1,
        _ => return bad_form(),
    };
    let kind = store
        .list_prompts()
        .ok()
        .and_then(|all| all.into_iter().find(|p| p.name == name))
        .map(|p| p.prompt_type);
    match (store.move_prompt(name, by), kind) {
        (Ok(moved), Some(kind)) => Reply::redirect(&format!(
            "{}{}",
            list_path(kind),
            if moved { "?ok=moved" } else { "" }
        )),
        (Ok(_), None) => gone(),
        (Err(e), _) => {
            let (status, why) = explain(&e.to_string());
            Reply::page(status, web_pages::error(&why))
        }
    }
}

fn profile_draft(form: &Form) -> ProfileDraft {
    ProfileDraft {
        name: form.get("name").trim().to_owned(),
        description: form.get("description").to_owned(),
        prompts: form.all("prompt").into_iter().map(str::to_owned).collect(),
        skills: form.all("skill").into_iter().map(str::to_owned).collect(),
        personality: form.get("personality").to_owned(),
    }
}

/// The profiles page again, with an error above it and what was typed kept.
fn profiles_again(ctx: &Ctx<'_>, status: u16, why: &str, draft: &ProfileDraft) -> Reply {
    match (ctx.read)() {
        Ok(data) => Reply::page(
            status,
            web_pages::profiles(&data, &view_with(ctx, why), draft),
        ),
        Err(e) => failed(&e),
    }
}

fn profile_add(form: &Form, store: &mut Store, ctx: &Ctx<'_>) -> Reply {
    let draft = profile_draft(form);
    match store.add_profile(&draft.name, &draft.description) {
        Ok(()) => Reply::redirect("/profiles?ok=added"),
        Err(e) => {
            let (status, why) = explain(&e.to_string());
            profiles_again(ctx, status, &why, &draft)
        }
    }
}

fn profile_copy(form: &Form, store: &mut Store, ctx: &Ctx<'_>) -> Reply {
    match store.copy_profile(form.get("name"), form.get("new").trim()) {
        Ok(()) => Reply::redirect("/profiles?ok=copied"),
        Err(e) => {
            let (status, why) = explain(&e.to_string());
            profiles_again(ctx, status, &why, &ProfileDraft::default())
        }
    }
}

fn profile_use(form: &Form, store: &mut Store) -> Reply {
    match store.select_profile(form.get("name")) {
        Ok(()) => Reply::redirect("/profiles?ok=used"),
        Err(e) => {
            let (status, why) = explain(&e.to_string());
            Reply::page(status, web_pages::error(&why))
        }
    }
}

fn profile_edit(form: &Form, store: &mut Store, ctx: &Ctx<'_>) -> Reply {
    let Some(id) = id_of(form) else {
        return bad_form();
    };
    let v = form.get("v");
    let draft = profile_draft(form);
    let personality = (!draft.personality.is_empty()).then_some(draft.personality.as_str());
    let result = check_description(&draft.description).and_then(|description| {
        store
            .edit_profile(
                id,
                v,
                &draft.name,
                &description,
                &draft.prompts,
                &draft.skills,
                personality,
            )
            .map_err(|e| e.to_string())
    });
    match result {
        Ok(Edit::Done) => Reply::redirect("/profiles?ok=saved"),
        Ok(Edit::Stale) => changed(
            &format!("/profile/edit?id={id}"),
            "profile",
            &draft.description,
        ),
        Ok(Edit::Gone) => gone(),
        Err(why) => {
            let (status, why) = explain(&why);
            let data = match (ctx.read)() {
                Ok(data) => data,
                Err(e) => return failed(&e),
            };
            match data.profiles.iter().find(|p| p.id == id) {
                Some(p) => Reply::page(
                    status,
                    web_pages::profile_edit(p, &data, v, &view_with(ctx, &why), &draft),
                ),
                None => gone(),
            }
        }
    }
}

fn profile_delete(form: &Form, store: &mut Store) -> Reply {
    let Some(id) = id_of(form) else {
        return bad_form();
    };
    match store.delete_profile_checked(id, form.get("v")) {
        Ok(Edit::Done) => Reply::redirect("/profiles?ok=deleted"),
        Ok(Edit::Stale) => changed(&format!("/profile/delete?id={id}"), "profile", ""),
        Ok(Edit::Gone) => gone(),
        Err(e) => {
            let (status, why) = explain(&e.to_string());
            Reply::page(status, web_pages::error(&why))
        }
    }
}
