//! The menus behind `/provider`, `/model` and `/provider config` (issue 11).
//!
//! Each function is one screen of the owner's layout:
//!
//! ```text
//! /provider config   providers, "+ add new"
//!   a provider       refresh | models | edit | delete
//!     models         models, "+ add another"
//!       a model      test | edit | delete
//! /provider          providers -> that provider's models -> switch
//! /model             the current provider's models -> switch
//! ```
//!
//! Esc goes back one screen. Every function needs menus switched on
//! ([`Terminal::menus_enabled`]); the callers fall back to plain output otherwise.

use std::io::{BufRead, Write};

use crate::agent::Agent;
use crate::config::ApiStyle;
use crate::menu::MenuItem;
use crate::provider::{CHOOSE_A_PROVIDER, router_host};
use crate::providers::{self, Listed, ProviderDef, Source};
use crate::repl::{Shared, report_model_switch, report_switch, say_line};
use crate::store_models::{Deleted, StoredModel};

fn host_of(def: &ProviderDef) -> String {
    router_host(&def.base_url).unwrap_or_else(|| def.base_url.clone())
}

/// What a provider row shows after its name: where it points and its model.
fn provider_detail(agent: &Agent, listed: &Listed) -> String {
    let mut detail = match &listed.def {
        Ok(def) if def.api == ApiStyle::OpenAi => format!("{}  {}", host_of(def), def.model),
        Ok(def) => format!(
            "{}  {}  ({} API)",
            host_of(def),
            def.model,
            def.api.as_str()
        ),
        Err(why) => format!("cannot be used: {why}"),
    };
    if listed.name == agent.target().name {
        detail.push_str("  (in use)");
    }
    detail
}

fn provider_rows(agent: &Agent, listed: &[Listed]) -> Vec<MenuItem> {
    listed
        .iter()
        .map(|l| MenuItem::new(&l.name).with_detail(provider_detail(agent, l)))
        .collect()
}

fn model_rows(
    models: &[StoredModel],
    in_use: Option<&str>,
    default: Option<&str>,
) -> Vec<MenuItem> {
    models
        .iter()
        .map(|m| {
            let mut notes = Vec::new();
            if m.display_name != m.model_id {
                notes.push(m.model_id.clone());
            }
            if default == Some(m.model_id.as_str()) {
                notes.push("default".to_owned());
            }
            if in_use == Some(m.model_id.as_str()) {
                notes.push("in use".to_owned());
            }
            MenuItem::new(&m.display_name).with_detail(notes.join(", "))
        })
        .collect()
}

fn pick<R: BufRead, W: Write>(
    term: &Shared<R, W>,
    title: &str,
    items: &[MenuItem],
) -> Option<usize> {
    term.borrow_mut().pick(title, items)
}

fn ask<R: BufRead, W: Write>(
    term: &Shared<R, W>,
    question: &str,
    current: Option<&str>,
) -> Option<String> {
    term.borrow_mut().ask(question, current)
}

fn ask_secret<R: BufRead, W: Write>(term: &Shared<R, W>, question: &str) -> Option<String> {
    term.borrow_mut().ask_secret(question)
}

pub(crate) fn error<R: BufRead, W: Write>(term: &Shared<R, W>, why: &str) {
    let style = term.borrow().style();
    say_line(term, &style.red(&format!("[error: {why}]")));
}

pub(crate) fn done<R: BufRead, W: Write>(term: &Shared<R, W>, what: &str) {
    let style = term.borrow().style();
    say_line(term, &style.green(what));
}

/// The stored models of a saved provider, or `None` after telling why not.
fn stored_models<R: BufRead, W: Write>(
    agent: &mut Agent,
    term: &Shared<R, W>,
    provider: &str,
) -> Option<Vec<StoredModel>> {
    match agent
        .store_mut()
        .and_then(|s| s.models_of(provider).map_err(|e| e.to_string()))
    {
        Ok(models) => Some(models),
        Err(why) => {
            error(term, &why);
            None
        }
    }
}

/// `/provider`: pick a provider, then one of its models, and switch to them.
pub fn provider_menu<R: BufRead, W: Write>(agent: &mut Agent, term: &Shared<R, W>) {
    loop {
        let listed = match agent.providers() {
            Ok(listed) => listed,
            Err(why) => return error(term, &why),
        };
        if listed.is_empty() {
            let style = term.borrow().style();
            return say_line(
                term,
                &style.yellow("no providers yet: use /provider config, then + add new"),
            );
        }
        let Some(chosen) = pick(term, "providers", &provider_rows(agent, &listed)) else {
            return;
        };
        let entry = &listed[chosen];
        let name = entry.name.clone();
        if entry.def.is_err() {
            error(
                term,
                &format!("{name} cannot be used until its provider file is fixed"),
            );
            continue;
        }
        // A saved provider's stored models; a provider file or an empty list switches at once.
        let models = if entry.source == Source::Saved {
            stored_models(agent, term, &name).unwrap_or_default()
        } else {
            Vec::new()
        };
        if models.is_empty() {
            let switched = agent.switch_provider(&name);
            return report_switch(agent, term, switched);
        }
        let default = entry.def.as_ref().ok().map(|d| d.model.clone());
        let in_use = (name == agent.target().name).then(|| agent.model().to_owned());
        let rows = model_rows(&models, in_use.as_deref(), default.as_deref());
        match pick(term, &format!("models of {name}"), &rows) {
            None => {} // back to the provider list
            Some(model) => {
                let switched = agent.switch_provider_with(&name, Some(&models[model].model_id));
                return report_switch(agent, term, switched);
            }
        }
    }
}

/// `/model`: pick one of the current provider's stored models.
pub fn model_menu<R: BufRead, W: Write>(agent: &mut Agent, term: &Shared<R, W>) {
    let style = term.borrow().style();
    if !agent.ready() {
        return say_line(term, &style.yellow(CHOOSE_A_PROVIDER));
    }
    let provider = agent.target().name.clone();
    let models = match agent
        .store_mut()
        .and_then(|s| s.models_of(&provider).map_err(|e| e.to_string()))
    {
        Ok(models) => models,
        Err(_) => {
            return say_line(
                term,
                &style.yellow(&format!(
                    "{provider} is not a saved provider, so it has no stored models; use /model <name>"
                )),
            );
        }
    };
    if models.is_empty() {
        return say_line(
            term,
            &style.yellow(&format!(
                "no models stored for {provider}: use /provider config, then refresh, or /model <name>"
            )),
        );
    }
    let rows = model_rows(&models, Some(agent.model()), None);
    if let Some(chosen) = pick(term, &format!("models of {provider}"), &rows) {
        let switched = agent.switch_model(&models[chosen].model_id);
        report_model_switch(agent, term, switched);
    }
}

/// `/provider config`: the provider manager.
pub fn config_menu<R: BufRead, W: Write>(agent: &mut Agent, term: &Shared<R, W>) {
    // Copy provider files into the database first, so everything listed can be edited.
    match agent.import_provider_files() {
        Ok(0) => {}
        Ok(n) => done(
            term,
            &format!(
                "copied {n} provider file{} into the database",
                if n == 1 { "" } else { "s" }
            ),
        ),
        Err(why) => error(term, &format!("could not import the provider files: {why}")),
    }
    loop {
        let listed = match agent.providers() {
            Ok(listed) => listed,
            Err(why) => return error(term, &why),
        };
        let mut rows = provider_rows(agent, &listed);
        rows.push(MenuItem::action("+ add new"));
        match pick(term, "providers", &rows) {
            None => return,
            Some(chosen) if chosen == listed.len() => add_provider(agent, term),
            Some(chosen) => provider_screen(agent, term, &listed[chosen]),
        }
    }
}

/// Asks which API a provider speaks. `None` when the menu is left.
fn pick_api<R: BufRead, W: Write>(
    term: &Shared<R, W>,
    title: &str,
    current: Option<ApiStyle>,
) -> Option<ApiStyle> {
    let rows: Vec<MenuItem> = ApiStyle::ALL
        .iter()
        .map(|api| {
            let mut detail = api.describe().to_owned();
            if current == Some(*api) {
                detail.push_str("  (now)");
            }
            MenuItem::new(api.as_str()).with_detail(detail)
        })
        .collect();
    pick(term, title, &rows).map(|chosen| ApiStyle::ALL[chosen])
}

fn add_provider<R: BufRead, W: Write>(agent: &mut Agent, term: &Shared<R, W>) {
    let Some(name) = ask(term, "name (empty cancels)", None).filter(|n| !n.is_empty()) else {
        return;
    };
    if let Err(why) = providers::check_name(&name) {
        return error(term, &why);
    }
    let Some(base_url) = ask(term, "base URL, such as https://openrouter.ai/api/v1", None) else {
        return;
    };
    if let Err(why) = providers::check_base_url(&base_url) {
        return error(term, &why);
    }
    let Some(api) = pick_api(term, "which API does it speak?", None) else {
        return;
    };
    let Some(key) = ask_secret(term, "API key, Enter for none") else {
        return;
    };
    let mut def = ProviderDef {
        name: name.clone(),
        base_url,
        model: String::new(),
        api_key: (!key.trim().is_empty()).then(|| key.trim().to_owned()),
        router: kernel::RouterOptions::default(),
        api,
    };
    // The key is known now, so the provider can be asked what it offers before a default model
    // is chosen: pick one (typing filters the list) instead of typing an id from memory.
    let Some(model) = choose_default_model(agent, term, &def) else {
        return;
    };
    def.model = model;
    match agent.add_provider(def) {
        Ok(()) => done(term, &format!("saved provider {name}")),
        Err(why) => error(term, &why),
    }
}

/// The default model of a provider being added: picked from the provider's own list when it has
/// one, typed when it has none or the list could not be fetched. `None` when cancelled.
fn choose_default_model<R: BufRead, W: Write>(
    agent: &Agent,
    term: &Shared<R, W>,
    def: &ProviderDef,
) -> Option<String> {
    let offered = match agent.models_offered_by(def) {
        Ok(ids) => ids,
        Err(why) => {
            let style = term.borrow().style();
            say_line(
                term,
                &style.yellow(&format!("could not list its models ({why}); type one")),
            );
            Vec::new()
        }
    };
    if !offered.is_empty() {
        let mut rows: Vec<MenuItem> = offered.iter().map(MenuItem::new).collect();
        rows.push(MenuItem::action("+ type a model id"));
        let chosen = pick(
            term,
            &format!("default model ({} offered; type to filter)", offered.len()),
            &rows,
        )?;
        if let Some(id) = offered.get(chosen) {
            return Some(id.clone());
        }
    }
    let typed = ask(term, "default model", None)?;
    match providers::check_model(&typed) {
        Ok(()) => Some(typed),
        Err(why) => {
            error(term, &why);
            None
        }
    }
}

fn provider_screen<R: BufRead, W: Write>(agent: &mut Agent, term: &Shared<R, W>, entry: &Listed) {
    let name = entry.name.clone();
    if entry.source == Source::File {
        let why = entry.def.as_ref().err().cloned().unwrap_or_default();
        return error(
            term,
            &format!(
                "{name} is a provider file that could not be copied into the database ({why}); fix the file"
            ),
        );
    }
    let mut name = name;
    loop {
        let items = [
            MenuItem::new("refresh").with_detail("ask the router for its model list"),
            MenuItem::new("models").with_detail("the stored models: test, edit, delete, add"),
            MenuItem::new("edit").with_detail("name, URL, key, default model, pinning"),
            MenuItem::new("delete").with_detail("remove this provider and its models"),
        ];
        match pick(term, &name, &items) {
            None => return,
            Some(0) => match agent.refresh_models(&name) {
                Ok(report) => done(
                    term,
                    &format!(
                        "{} new model{}, {} stored",
                        report.added,
                        if report.added == 1 { "" } else { "s" },
                        report.shown
                    ),
                ),
                Err(why) => error(term, &why),
            },
            Some(1) => models_screen(agent, term, &name),
            Some(2) => name = edit_screen(agent, term, &name),
            Some(_) => {
                let sure = ask(
                    term,
                    &format!("delete provider {name} and its models? [y/N]"),
                    None,
                );
                if matches!(
                    sure.as_deref().map(str::to_ascii_lowercase).as_deref(),
                    Some("y" | "yes")
                ) {
                    match agent.remove_provider(&name) {
                        Ok(()) => done(term, &format!("removed provider {name}")),
                        Err(why) => error(term, &why),
                    }
                    return;
                }
                let style = term.borrow().style();
                say_line(term, &style.yellow("[not deleted]"));
            }
        }
    }
}

fn models_screen<R: BufRead, W: Write>(agent: &mut Agent, term: &Shared<R, W>, provider: &str) {
    loop {
        let Some(models) = stored_models(agent, term, provider) else {
            return;
        };
        let default = agent.provider_def(provider).ok().map(|d| d.model);
        let mut rows = model_rows(&models, None, default.as_deref());
        rows.push(MenuItem::action("+ add another"));
        match pick(term, &format!("models of {provider}"), &rows) {
            None => return,
            Some(chosen) if chosen == models.len() => add_model(agent, term, provider),
            Some(chosen) => model_screen(agent, term, provider, &models[chosen]),
        }
    }
}

fn add_model<R: BufRead, W: Write>(agent: &mut Agent, term: &Shared<R, W>, provider: &str) {
    let Some(id) = ask(term, "model id (empty cancels)", None).filter(|id| !id.is_empty()) else {
        return;
    };
    if let Err(why) = providers::check_model(&id) {
        return error(term, &why);
    }
    let Some(display) = ask(term, "display name (Enter uses the id)", None) else {
        return;
    };
    let added = agent.store_mut().and_then(|s| {
        s.add_model(provider, &id, &display)
            .map_err(|e| e.to_string())
    });
    match added {
        Ok(()) => done(term, &format!("added model {id}")),
        Err(why) => error(term, &why),
    }
}

fn model_screen<R: BufRead, W: Write>(
    agent: &mut Agent,
    term: &Shared<R, W>,
    provider: &str,
    model: &StoredModel,
) {
    let style = term.borrow().style();
    let items = [
        MenuItem::new("test").with_detail("send one tiny request"),
        MenuItem::new("edit").with_detail("display name and id"),
        MenuItem::new("delete").with_detail("remove it from the list"),
    ];
    match pick(term, &model.display_name, &items) {
        None => {}
        Some(0) => {
            say_line(
                term,
                &style.dim(&format!("testing {} on {provider}...", model.model_id)),
            );
            match agent.test_model(provider, &model.model_id) {
                Ok(line) => done(term, &line),
                Err(why) => error(term, &why),
            }
        }
        Some(1) => {
            let Some(display) = ask(term, "display name", Some(&model.display_name)) else {
                return;
            };
            let Some(id) = ask(term, "model id", Some(&model.model_id)) else {
                return;
            };
            let display = if display.is_empty() {
                model.display_name.clone()
            } else {
                display
            };
            let id = if id.is_empty() {
                model.model_id.clone()
            } else {
                id
            };
            if let Err(why) = providers::check_model(&id) {
                return error(term, &why);
            }
            let edited = agent.store_mut().and_then(|s| {
                s.edit_model(provider, model.row_id, &id, &display)
                    .map_err(|e| e.to_string())
            });
            match edited {
                Ok(()) => done(term, &format!("saved model {id}")),
                Err(why) => error(term, &why),
            }
        }
        Some(_) => {
            let deleted = agent.store_mut().and_then(|s| {
                s.delete_model(provider, model.row_id)
                    .map_err(|e| e.to_string())
            });
            match deleted {
                Ok(Deleted::Removed) => done(term, &format!("removed {}", model.model_id)),
                Ok(Deleted::Hidden) => done(
                    term,
                    &format!(
                        "hidden {} (a refresh will not bring it back)",
                        model.model_id
                    ),
                ),
                Err(why) => error(term, &why),
            }
        }
    }
}

fn list_text(list: &[String]) -> String {
    if list.is_empty() {
        "(none)".to_owned()
    } else {
        list.join(", ")
    }
}

/// The provider's variables. Returns the provider's name afterwards (it can be renamed).
fn edit_screen<R: BufRead, W: Write>(agent: &mut Agent, term: &Shared<R, W>, name: &str) -> String {
    let mut name = name.to_owned();
    loop {
        let def = match agent.provider_def(&name) {
            Ok(def) => def,
            Err(why) => {
                error(term, &why);
                return name;
            }
        };
        let items = [
            MenuItem::new("name").with_detail(&def.name),
            MenuItem::new("base URL").with_detail(&def.base_url),
            MenuItem::new("default model").with_detail(&def.model),
            MenuItem::new("API key").with_detail(if def.api_key.is_some() {
                "(set)"
            } else {
                "(none)"
            }),
            MenuItem::new("provider order").with_detail(list_text(&def.router.provider_order)),
            MenuItem::new("allow fallbacks").with_detail(
                def.router
                    .allow_fallbacks
                    .map_or("(not sent)".to_owned(), |b| b.to_string()),
            ),
            MenuItem::new("session id").with_detail(
                def.router
                    .session_id
                    .clone()
                    .unwrap_or_else(|| "(none)".to_owned()),
            ),
            MenuItem::new("API").with_detail(def.api.as_str()),
        ];
        let Some(field) = pick(term, &format!("edit {name}"), &items) else {
            return name;
        };
        let mut next = def.clone();
        let hint = "Enter keeps, - clears";
        let applied = match field {
            0 => ask(term, "name", Some(&def.name)).map(|v| {
                if !v.is_empty() {
                    next.name = v;
                }
            }),
            1 => ask(term, "base URL", Some(&def.base_url)).map(|v| {
                if !v.is_empty() {
                    next.base_url = v;
                }
            }),
            2 => ask(term, "default model", Some(&def.model)).map(|v| {
                if !v.is_empty() {
                    next.model = v;
                }
            }),
            3 => ask_secret(term, &format!("new API key ({hint})")).map(|v| match v.trim() {
                "" => {}
                "-" => next.api_key = None,
                key => next.api_key = Some(key.to_owned()),
            }),
            4 => ask(
                term,
                &format!("provider order, comma separated ({hint})"),
                Some(&def.router.provider_order.join(", ")),
            )
            .map(|v| match v.as_str() {
                "" => {}
                "-" => next.router.provider_order.clear(),
                list => {
                    next.router.provider_order = list
                        .split(',')
                        .map(|p| p.trim().to_owned())
                        .filter(|p| !p.is_empty())
                        .collect();
                }
            }),
            5 => ask(
                term,
                &format!("allow fallbacks, true or false ({hint})"),
                def.router.allow_fallbacks.map(|b| b.to_string()).as_deref(),
            )
            .and_then(|v| match v.as_str() {
                "" => Some(()),
                "-" => {
                    next.router.allow_fallbacks = None;
                    Some(())
                }
                "true" => {
                    next.router.allow_fallbacks = Some(true);
                    Some(())
                }
                "false" => {
                    next.router.allow_fallbacks = Some(false);
                    Some(())
                }
                _ => {
                    error(term, "allow fallbacks is true or false");
                    None
                }
            }),
            7 => pick_api(term, "API", Some(def.api)).map(|api| next.api = api),
            _ => ask(
                term,
                &format!("session id ({hint})"),
                def.router.session_id.as_deref(),
            )
            .map(|v| match v.as_str() {
                "" => {}
                "-" => next.router.session_id = None,
                id => next.router.session_id = Some(id.to_owned()),
            }),
        };
        if applied.is_none() {
            continue; // end of input, or a bad value that was already reported
        }
        let new_name = next.name.clone();
        match agent.edit_provider(&name, next) {
            Ok(notes) => {
                done(term, "saved");
                let style = term.borrow().style();
                crate::repl::warnings(term, style, &notes);
                name = new_name;
            }
            Err(why) => error(term, &why),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::{ScriptedProvider, StoreContext};
    use crate::providers::Catalog;
    use crate::repl::{Terminal, run};
    use crate::store::Store;
    use crate::style::Style;
    use crate::tools::testutil::Scratch;
    use crate::turn_log::TurnLog;
    use std::io::Cursor;

    const UP: &str = "\x1b[A";
    const DOWN: &str = "\x1b[B";
    const ENTER: &str = "\r";
    const ESC: &str = "\x1b";

    type TestTerm = Shared<Cursor<Vec<u8>>, Vec<u8>>;

    fn screen(term: &TestTerm) -> String {
        String::from_utf8(term.borrow().output().clone()).unwrap()
    }

    fn def(name: &str, model: &str) -> ProviderDef {
        ProviderDef {
            api: crate::config::ApiStyle::OpenAi,
            name: name.to_owned(),
            base_url: format!("http://{name}.example/v1"),
            model: model.to_owned(),
            api_key: Some(format!("KEY-{name}")),
            router: kernel::RouterOptions::default(),
        }
    }

    /// An agent saving into the scratch folder, with menus on (keys are read from the typed
    /// text) and a catalog whose providers list `models` and answer "ok".
    fn rig(dir: &Scratch, typed: &str, models: Result<Vec<String>, String>) -> (Agent, TestTerm) {
        let term = Terminal::shared_styled(
            Cursor::new(typed.as_bytes().to_vec()),
            Vec::new(),
            Style::plain(),
        );
        term.borrow_mut().enable_menus(false);
        let log = TurnLog::create(&dir.path().join("logs")).unwrap();
        let store = Store::open(&dir.path().join("harness.db")).unwrap();
        let agent = Agent::new(
            "m",
            Box::new(ScriptedProvider::new(vec![])),
            Vec::new(),
            log,
        )
        .with_target("test", "http://x/v1")
        .with_store(
            store,
            StoreContext {
                cwd: dir.path().display().to_string(),
            },
        )
        .with_catalog(Catalog {
            provider_dir: Some(dir.path().join("providers")),
            build: Box::new(move |_| {
                Box::new(
                    ScriptedProvider::new(vec![ScriptedProvider::text("ok")])
                        .with_models(models.clone()),
                )
            }),
        });
        (agent, term)
    }

    fn keys(parts: &[&str]) -> String {
        parts.concat()
    }

    fn saved(dir: &Scratch) -> Vec<crate::providers::ProviderDef> {
        Store::open(&dir.path().join("harness.db"))
            .unwrap()
            .list_providers()
            .unwrap()
    }

    fn model_ids(dir: &Scratch, provider: &str) -> Vec<String> {
        Store::open(&dir.path().join("harness.db"))
            .unwrap()
            .models_of(provider)
            .unwrap()
            .into_iter()
            .map(|m| m.model_id)
            .collect()
    }

    fn two_providers(agent: &mut Agent) {
        agent.add_provider(def("a", "am")).unwrap();
        agent.add_provider(def("b", "bm")).unwrap();
        agent
            .store_mut()
            .unwrap()
            .add_model("b", "b-two", "b-two")
            .unwrap();
    }

    #[test]
    fn provider_menu_switches_to_the_provider_and_the_model_chosen() {
        let dir = Scratch::new("ui-provider-switch");
        let typed = keys(&[DOWN, ENTER, ENTER]);
        let (mut agent, term) = rig(&dir, &typed, Ok(vec![]));
        two_providers(&mut agent);
        provider_menu(&mut agent, &term);
        assert_eq!(agent.target().name, "b");
        assert_eq!(agent.model(), "b-two");
        let out = screen(&term);
        assert!(out.contains("providers"), "{out}");
        assert!(out.contains("models of b"), "{out}");
        assert!(out.contains("now using b"), "{out}");
    }

    #[test]
    fn escape_goes_back_one_screen_at_a_time_and_changes_nothing() {
        let dir = Scratch::new("ui-escape");
        // a -> its models -> Esc -> providers -> Esc -> out.
        let typed = keys(&[ENTER, ESC, ESC]);
        let (mut agent, term) = rig(&dir, &typed, Ok(vec![]));
        two_providers(&mut agent);
        provider_menu(&mut agent, &term);
        assert_eq!(agent.target().name, "test");
        assert_eq!(agent.model(), "m");
        let out = screen(&term);
        assert!(out.contains("models of a"), "{out}");
        assert!(!out.contains("now using"), "{out}");
    }

    #[test]
    fn end_of_input_cancels_every_menu() {
        let dir = Scratch::new("ui-eof");
        let (mut agent, term) = rig(&dir, "", Ok(vec![]));
        two_providers(&mut agent);
        provider_menu(&mut agent, &term);
        config_menu(&mut agent, &term);
        model_menu(&mut agent, &term);
        assert_eq!(agent.target().name, "test");
        assert_eq!(saved(&dir).len(), 2);
    }

    #[test]
    fn an_empty_provider_list_points_to_the_config_menu() {
        let dir = Scratch::new("ui-empty");
        let (mut agent, term) = rig(&dir, "", Ok(vec![]));
        provider_menu(&mut agent, &term);
        assert!(screen(&term).contains("no providers yet: use /provider config"));
    }

    #[test]
    fn model_menu_switches_among_the_current_providers_models() {
        let dir = Scratch::new("ui-model");
        let typed = keys(&[ENTER]);
        let (mut agent, term) = rig(&dir, &typed, Ok(vec![]));
        two_providers(&mut agent);
        // The start provider is not a saved one: say so, read no keys.
        model_menu(&mut agent, &term);
        assert!(screen(&term).contains("is not a saved provider"));
        agent.switch_provider("b").unwrap();
        model_menu(&mut agent, &term);
        assert_eq!(agent.model(), "b-two");
        assert!(screen(&term).contains("model is now b-two"));
    }

    #[test]
    fn model_menu_without_a_provider_says_to_choose_one() {
        let dir = Scratch::new("ui-model-none");
        let term = Terminal::shared_styled(Cursor::new(Vec::new()), Vec::new(), Style::plain());
        term.borrow_mut().enable_menus(false);
        let log = TurnLog::create(&dir.path().join("logs")).unwrap();
        let mut agent = Agent::new("", Box::new(crate::provider::NoProvider), Vec::new(), log);
        model_menu(&mut agent, &term);
        assert!(screen(&term).contains(CHOOSE_A_PROVIDER));
    }

    #[test]
    fn add_new_saves_a_provider_and_its_key_stays_off_the_screen() {
        let dir = Scratch::new("ui-add");
        let typed = keys(&[
            ENTER,
            "newp\nhttp://n.example/v1\n",
            ENTER, // API: the first one, chat completions
            "SECRET-KEY-555\n",
            "nm\n", // the router offered no models, so the model is typed
            ESC,
        ]);
        let (mut agent, term) = rig(&dir, &typed, Ok(vec![]));
        config_menu(&mut agent, &term);
        let all = saved(&dir);
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].name, "newp");
        assert_eq!(all[0].model, "nm");
        assert_eq!(all[0].api_key.as_deref(), Some("SECRET-KEY-555"));
        assert_eq!(all[0].api, ApiStyle::OpenAi);
        let out = screen(&term);
        assert!(out.contains("saved provider newp"), "{out}");
        assert!(!out.contains("SECRET-KEY"), "{out}");
        assert!(
            !std::fs::read_to_string(agent.log_path())
                .unwrap_or_default()
                .contains("SECRET-KEY")
        );
        // It was only saved: the chat still uses what it used.
        assert_eq!(agent.target().name, "test");
    }

    #[test]
    fn a_bad_add_is_explained_and_saves_nothing() {
        let dir = Scratch::new("ui-add-bad");
        let typed = keys(&[
            ENTER,
            "x\nftp://bad\n",
            ENTER,
            "../y\n",
            ENTER,
            "ok\nhttp://h/v1\n",
            ENTER, // API
            "\n",  // no key
            "two words\n",
            ESC,
        ]);
        let (mut agent, term) = rig(&dir, &typed, Ok(vec![]));
        config_menu(&mut agent, &term);
        let out = screen(&term);
        assert!(
            out.contains("the base URL must start with http://"),
            "{out}"
        );
        assert!(out.contains("the name must not start with"), "{out}");
        assert!(out.contains("a model id has no spaces"), "{out}");
        assert!(saved(&dir).is_empty());
    }

    #[test]
    fn refresh_stores_the_routers_models_and_reports_the_count() {
        let dir = Scratch::new("ui-refresh");
        let typed = keys(&[ENTER, ENTER, ESC, ESC]);
        let (mut agent, term) = rig(
            &dir,
            &typed,
            Ok(vec!["x/one".to_owned(), "x/two".to_owned()]),
        );
        agent.add_provider(def("a", "am")).unwrap();
        config_menu(&mut agent, &term);
        let ids = model_ids(&dir, "a");
        assert!(ids.contains(&"x/one".to_owned()) && ids.contains(&"x/two".to_owned()));
        assert!(ids.contains(&"am".to_owned()));
        assert!(
            screen(&term).contains("2 new models, 3 stored"),
            "{}",
            screen(&term)
        );
    }

    #[test]
    fn a_refresh_that_fails_says_why_and_keeps_the_list() {
        let dir = Scratch::new("ui-refresh-fail");
        let typed = keys(&[ENTER, ENTER, ESC, ESC]);
        let (mut agent, term) = rig(&dir, &typed, Err("status 401".to_owned()));
        agent.add_provider(def("a", "am")).unwrap();
        config_menu(&mut agent, &term);
        assert!(screen(&term).contains("status 401"));
        assert_eq!(model_ids(&dir, "a"), ["am"]);
    }

    #[test]
    fn models_can_be_added_tested_edited_and_deleted() {
        let dir = Scratch::new("ui-models");
        let typed = keys(&[
            ENTER, // provider a
            DOWN,
            ENTER, // models
            DOWN,
            ENTER,
            "x/new\nzz new\n", // + add another
            DOWN,
            ENTER,
            ENTER, // zz new -> test
            DOWN,
            ENTER,
            DOWN,
            ENTER,
            "zz renamed\nx/renamed\n", // zz new -> edit
            DOWN,
            ENTER,
            DOWN,
            DOWN,
            ENTER, // the renamed one -> delete
            ESC,
            ESC,
            ESC,
        ]);
        let (mut agent, term) = rig(&dir, &typed, Ok(vec![]));
        agent.add_provider(def("a", "am")).unwrap();
        config_menu(&mut agent, &term);
        let out = screen(&term);
        assert!(out.contains("added model x/new"), "{out}");
        assert!(out.contains("the model said: ok"), "{out}");
        assert!(out.contains("saved model x/renamed"), "{out}");
        assert!(out.contains("removed x/renamed"), "{out}");
        assert_eq!(model_ids(&dir, "a"), ["am"]);
    }

    #[test]
    fn deleting_a_fetched_model_hides_it_and_a_refresh_does_not_bring_it_back() {
        let dir = Scratch::new("ui-hide");
        // refresh, then delete "x/one" (rows: am, x/one), then refresh again.
        let typed = keys(&[
            ENTER, ENTER, // refresh
            DOWN, ENTER, // models
            DOWN, ENTER, DOWN, DOWN, ENTER, // x/one -> delete
            ESC,   // back to the provider screen
            ENTER, // refresh again
            ESC, ESC,
        ]);
        let (mut agent, term) = rig(&dir, &typed, Ok(vec!["x/one".to_owned()]));
        agent.add_provider(def("a", "am")).unwrap();
        config_menu(&mut agent, &term);
        let out = screen(&term);
        assert!(out.contains("hidden x/one"), "{out}");
        assert!(out.contains("0 new models"), "{out}");
        assert_eq!(model_ids(&dir, "a"), ["am"]);
    }

    #[test]
    fn editing_a_provider_renames_it_and_changes_its_key_without_showing_it() {
        let dir = Scratch::new("ui-edit");
        let typed = keys(&[
            ENTER, // provider a
            DOWN,
            DOWN,
            ENTER, // edit
            ENTER,
            "renamed\n", // name
            DOWN,
            DOWN,
            DOWN,
            ENTER,
            "NEW-SECRET-9\n", // API key
            DOWN,
            DOWN,
            DOWN,
            DOWN,
            ENTER,
            "b-one, b-two\n", // provider order
            DOWN,
            DOWN,
            DOWN,
            DOWN,
            DOWN,
            ENTER,
            "false\n", // allow fallbacks
            ESC,
            ESC,
            ESC,
        ]);
        let (mut agent, term) = rig(&dir, &typed, Ok(vec![]));
        agent.add_provider(def("a", "am")).unwrap();
        config_menu(&mut agent, &term);
        let all = saved(&dir);
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].name, "renamed");
        assert_eq!(all[0].api_key.as_deref(), Some("NEW-SECRET-9"));
        assert_eq!(all[0].router.provider_order, ["b-one", "b-two"]);
        assert_eq!(all[0].router.allow_fallbacks, Some(false));
        assert_eq!(
            model_ids(&dir, "renamed"),
            ["am"],
            "its models went with it"
        );
        assert!(!screen(&term).contains("NEW-SECRET"), "{}", screen(&term));
    }

    #[test]
    fn the_default_model_is_picked_from_what_the_provider_offers() {
        let dir = Scratch::new("ui-add-pick");
        let typed = keys(&[
            ENTER,
            "pick\nhttp://p.example/v1\n",
            ENTER, // API
            "KEY-9\n",
            DOWN,
            ENTER, // the second model offered
            ESC,
        ]);
        let offered = ["x/one", "x/two", "x/three"].map(str::to_owned).to_vec();
        let (mut agent, term) = rig(&dir, &typed, Ok(offered));
        config_menu(&mut agent, &term);
        let all = saved(&dir);
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].model, "x/two");
        let out = screen(&term);
        assert!(out.contains("3 offered"), "{out}");
        assert!(!out.contains("KEY-9"), "{out}");
    }

    #[test]
    fn when_the_provider_cannot_list_models_the_model_is_typed_and_the_reason_is_shown() {
        let dir = Scratch::new("ui-add-no-list");
        let typed = keys(&[
            ENTER,
            "nolist\nhttp://n.example/v1\n",
            ENTER,
            "\n",
            "typed/model\n",
            ESC,
        ]);
        let (mut agent, term) = rig(&dir, &typed, Err("no model list here".to_owned()));
        config_menu(&mut agent, &term);
        assert_eq!(saved(&dir)[0].model, "typed/model");
        assert!(
            screen(&term).contains("could not list its models (no model list here)"),
            "{}",
            screen(&term)
        );
    }

    #[test]
    fn a_provider_can_be_added_with_the_anthropic_api() {
        let dir = Scratch::new("ui-add-anthropic");
        let typed = keys(&[
            ENTER,
            "claude\nhttps://api.anthropic.com/v1\n",
            DOWN,
            ENTER, // API: Anthropic Messages
            "\n",
            "claude-sonnet-5.5\n",
            ESC,
        ]);
        let (mut agent, term) = rig(&dir, &typed, Ok(vec![]));
        config_menu(&mut agent, &term);
        let all = saved(&dir);
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].api, ApiStyle::Anthropic);
        let listing = screen(&term);
        assert!(listing.contains("saved provider claude"), "{listing}");
    }

    #[test]
    fn the_api_of_a_saved_provider_can_be_changed_and_the_list_says_when_it_is_not_the_default() {
        let dir = Scratch::new("ui-edit-api");
        let mut typed = vec![ENTER, DOWN, DOWN, ENTER]; // provider a, edit
        typed.extend([DOWN; 7]);
        typed.extend([ENTER, DOWN, ENTER]); // API: Anthropic Messages
        typed.extend([ESC, ESC, ESC]);
        let (mut agent, term) = rig(&dir, &keys(&typed), Ok(vec![]));
        agent.add_provider(def("a", "am")).unwrap();
        config_menu(&mut agent, &term);
        assert_eq!(saved(&dir)[0].api, ApiStyle::Anthropic);
        assert!(screen(&term).contains("anthropic"), "{}", screen(&term));
    }

    #[test]
    fn a_dash_clears_the_key_and_a_bad_value_changes_nothing() {
        let dir = Scratch::new("ui-edit-clear");
        let typed = keys(&[
            ENTER,
            DOWN,
            DOWN,
            ENTER, // edit
            DOWN,
            DOWN,
            DOWN,
            ENTER,
            "-\n", // key: cleared
            DOWN,
            DOWN,
            DOWN,
            DOWN,
            DOWN,
            ENTER,
            "maybe\n", // fallbacks: refused
            DOWN,
            ENTER,
            "ftp://bad\n", // base URL: refused by the agent
            ESC,
            ESC,
            ESC,
        ]);
        let (mut agent, term) = rig(&dir, &typed, Ok(vec![]));
        agent.add_provider(def("a", "am")).unwrap();
        config_menu(&mut agent, &term);
        let all = saved(&dir);
        assert_eq!(all[0].api_key, None);
        assert_eq!(all[0].router.allow_fallbacks, None);
        assert_eq!(all[0].base_url, "http://a.example/v1");
        let out = screen(&term);
        assert!(out.contains("allow fallbacks is true or false"), "{out}");
    }

    #[test]
    fn renaming_the_provider_in_use_keeps_the_chat_on_it() {
        let dir = Scratch::new("ui-edit-in-use");
        let typed = keys(&[ENTER, DOWN, DOWN, ENTER, ENTER, "moved\n", ESC, ESC, ESC]);
        let (mut agent, term) = rig(&dir, &typed, Ok(vec![]));
        agent.add_provider(def("a", "am")).unwrap();
        agent.switch_provider("a").unwrap();
        config_menu(&mut agent, &term);
        assert_eq!(agent.target().name, "moved");
        assert_eq!(agent.model(), "am");
    }

    #[test]
    fn delete_asks_first_and_only_a_yes_deletes() {
        let dir = Scratch::new("ui-delete");
        let typed = keys(&[
            ENTER, DOWN, DOWN, DOWN, ENTER, "\n", // refused: empty answer
            DOWN, DOWN, DOWN, ENTER, "n\n", // refused: no
            ESC, ESC,
        ]);
        let (mut agent, term) = rig(&dir, &typed, Ok(vec![]));
        agent.add_provider(def("a", "am")).unwrap();
        config_menu(&mut agent, &term);
        assert_eq!(saved(&dir).len(), 1);
        assert!(screen(&term).contains("[not deleted]"));

        let dir = Scratch::new("ui-delete-yes");
        let typed = keys(&[ENTER, DOWN, DOWN, DOWN, ENTER, "y\n", ESC]);
        let (mut agent, term) = rig(&dir, &typed, Ok(vec![]));
        agent.add_provider(def("a", "am")).unwrap();
        config_menu(&mut agent, &term);
        assert!(saved(&dir).is_empty());
        assert!(model_ids_missing(&dir, "a"));
        assert!(screen(&term).contains("removed provider a"));
    }

    fn model_ids_missing(dir: &Scratch, provider: &str) -> bool {
        Store::open(&dir.path().join("harness.db"))
            .unwrap()
            .models_of(provider)
            .is_err()
    }

    fn write_provider_file(dir: &Scratch, name: &str, text: &str) -> std::path::PathBuf {
        let folder = dir.path().join("providers");
        std::fs::create_dir_all(&folder).unwrap();
        let path = folder.join(format!("{name}.provider"));
        std::fs::write(&path, text).unwrap();
        path
    }

    #[test]
    fn provider_files_are_copied_in_once_with_their_keys_and_left_alone() {
        let dir = Scratch::new("ui-import");
        let text =
            "HARNESS_BASE_URL=http://p.example/v1\nHARNESS_MODEL=pm\nHARNESS_API_KEY=FILE-KEY\n";
        let path = write_provider_file(&dir, "p", text);
        write_provider_file(&dir, "broken", "HARNESS_MODEL=m\n");
        let (mut agent, term) = rig(&dir, ESC, Ok(vec![]));
        assert_eq!(agent.import_provider_files().unwrap(), 1);
        let all = saved(&dir);
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].name, "p");
        assert_eq!(all[0].api_key.as_deref(), Some("FILE-KEY"));
        assert_eq!(model_ids(&dir, "p"), ["pm"]);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            text,
            "file untouched"
        );
        // Again: nothing new. The config menu says nothing about an import either.
        assert_eq!(agent.import_provider_files().unwrap(), 0);
        config_menu(&mut agent, &term);
        assert!(!screen(&term).contains("copied"));
    }

    #[test]
    fn a_deleted_import_is_not_imported_again() {
        let dir = Scratch::new("ui-import-tombstone");
        write_provider_file(
            &dir,
            "p",
            "HARNESS_BASE_URL=http://p.example/v1\nHARNESS_MODEL=pm\n",
        );
        let (mut agent, _term) = rig(&dir, "", Ok(vec![]));
        assert_eq!(agent.import_provider_files().unwrap(), 1);
        agent.remove_provider("p").unwrap();
        assert_eq!(agent.import_provider_files().unwrap(), 0);
        assert!(saved(&dir).is_empty());
        assert!(dir.path().join("providers/p.provider").exists());
    }

    #[test]
    fn the_config_menu_announces_an_import() {
        let dir = Scratch::new("ui-import-announced");
        write_provider_file(
            &dir,
            "p",
            "HARNESS_BASE_URL=http://p.example/v1\nHARNESS_MODEL=pm\n",
        );
        let (mut agent, term) = rig(&dir, ESC, Ok(vec![]));
        config_menu(&mut agent, &term);
        let out = screen(&term);
        assert!(
            out.contains("copied 1 provider file into the database"),
            "{out}"
        );
        assert_eq!(saved(&dir).len(), 1);
    }

    #[test]
    fn without_menus_the_commands_keep_their_plain_output() {
        let dir = Scratch::new("ui-plain");
        let (mut agent, _) = rig(&dir, "", Ok(vec![]));
        agent.add_provider(def("a", "am")).unwrap();
        let term = Terminal::shared_styled(
            Cursor::new(b"/provider\n/provider config\n/model\n/exit\n".to_vec()),
            Vec::new(),
            Style::plain(),
        );
        run(&mut agent, &term).unwrap();
        let out = screen(&term);
        assert!(
            out.contains("providers:\n  a  saved    a.example  am\n"),
            "{out}"
        );
        assert!(out.contains("/provider config needs a terminal"), "{out}");
        assert!(out.contains("model:    m\n"), "{out}");
        assert!(!out.contains('\x1b'), "no control codes: {out:?}");
    }

    #[test]
    fn the_up_key_wraps_to_the_last_row() {
        let dir = Scratch::new("ui-up");
        // Up from the first provider wraps to the last one, b.
        let typed = keys(&[UP, ENTER, ENTER]);
        // (b's models sort as b-two, bm: the first is chosen.)
        let (mut agent, term) = rig(&dir, &typed, Ok(vec![]));
        two_providers(&mut agent);
        provider_menu(&mut agent, &term);
        assert_eq!(agent.target().name, "b");
        assert_eq!(agent.model(), "b-two");
    }

    #[test]
    fn typing_searches_the_model_lists_in_model_and_provider() {
        let dir = Scratch::new("ui-search");
        let models: Vec<String> = (0..40).map(|i| format!("vendor/model-{i:02}")).collect();
        let typed = keys(&["model-37", ENTER, ENTER, "MODEL-05", ENTER]);
        let (mut agent, term) = rig(&dir, &typed, Ok(vec![]));
        agent.add_provider(def("a", "am")).unwrap();
        for id in &models {
            agent.store_mut().unwrap().add_model("a", id, id).unwrap();
        }
        agent.switch_provider("a").unwrap();
        // /model: type "model-37", Enter picks the one row left.
        model_menu(&mut agent, &term);
        assert_eq!(agent.model(), "vendor/model-37");
        assert!(
            screen(&term).contains("search: model-37"),
            "{}",
            screen(&term)
        );
        // /provider: provider, then search its models, then pick.
        provider_menu(&mut agent, &term);
        assert_eq!(agent.model(), "vendor/model-05", "search ignores case");
    }
}
