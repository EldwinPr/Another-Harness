//! Stored models and provider edits (issue 11): the rows behind the provider and model menus.
//!
//! Part of the store (`impl Store` continues here); the table layout is owned by `docs/erd.md`.

use rusqlite::{OptionalExtension, params};

use crate::providers::ProviderDef;
use crate::store::{LAST_PROVIDER, Store, StoreError};

/// The `settings` key holding the names of the provider files already copied into the database
/// (a JSON array). A name stays in it after its provider is deleted, so the file is not imported
/// again.
/// The stored text of the key keeps its old spelling (`imported_profiles`): changing it would make
/// the program forget which files were imported (issue 30).
pub const IMPORTED_PROVIDER_FILES: &str = "imported_profiles";

/// One stored model of a provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredModel {
    pub row_id: i64,
    /// What is sent to the router as the model.
    pub model_id: String,
    /// What the menus show.
    pub display_name: String,
    /// The id as the router listed it; `None` for a model added by hand.
    pub origin_id: Option<String>,
}

/// What a refresh did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MergeReport {
    pub added: usize,
    /// Models shown for the provider afterwards.
    pub shown: usize,
}

/// What deleting a model did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Deleted {
    /// A model added by hand is gone.
    Removed,
    /// A fetched model is hidden, so a refresh does not bring it back.
    Hidden,
}

fn err(text: impl Into<String>) -> StoreError {
    StoreError::new(text)
}

impl Store {
    fn provider_row(&self, name: &str) -> Result<i64, StoreError> {
        self.conn
            .query_row("SELECT id FROM providers WHERE name = ?1", [name], |row| {
                row.get(0)
            })
            .optional()?
            .ok_or_else(|| err(format!("there is no saved provider named {name}")))
    }

    /// The models shown for a provider, by display name.
    pub fn models_of(&self, provider: &str) -> Result<Vec<StoredModel>, StoreError> {
        let provider_id = self.provider_row(provider)?;
        let mut statement = self.conn.prepare(
            "SELECT id, model_id, display_name, origin_id FROM provider_models \
             WHERE provider_id = ?1 AND hidden = 0 ORDER BY display_name, model_id",
        )?;
        let rows = statement.query_map([provider_id], |row| {
            Ok(StoredModel {
                row_id: row.get(0)?,
                model_id: row.get(1)?,
                display_name: row.get(2)?,
                origin_id: row.get(3)?,
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Stores a model added by hand. An empty display name means the id. A model of that id that
    /// was hidden comes back (with the new display name).
    pub fn add_model(
        &mut self,
        provider: &str,
        model_id: &str,
        display_name: &str,
    ) -> Result<(), StoreError> {
        let provider_id = self.provider_row(provider)?;
        let display = if display_name.trim().is_empty() {
            model_id
        } else {
            display_name.trim()
        };
        let existing: Option<(i64, bool)> = self
            .conn
            .query_row(
                "SELECT id, hidden FROM provider_models WHERE provider_id = ?1 AND model_id = ?2",
                params![provider_id, model_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        match existing {
            Some((_, false)) => Err(err(format!("{provider} already has the model {model_id}"))),
            Some((row_id, true)) => {
                self.conn.execute(
                    "UPDATE provider_models SET hidden = 0, display_name = ?2 WHERE id = ?1",
                    params![row_id, display],
                )?;
                Ok(())
            }
            None => {
                self.conn.execute(
                    "INSERT INTO provider_models (provider_id, model_id, display_name, origin_id, created_at) \
                     VALUES (?1, ?2, ?3, NULL, ?4)",
                    params![provider_id, model_id, display, now_secs()],
                )?;
                Ok(())
            }
        }
    }

    /// Stores the ids a router listed that are not stored yet. An id counts as stored when it is
    /// a model id or the original id of a fetched model, shown or hidden, so a renamed or deleted
    /// model does not come back.
    pub fn merge_fetched(
        &mut self,
        provider: &str,
        ids: &[String],
    ) -> Result<MergeReport, StoreError> {
        let provider_id = self.provider_row(provider)?;
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let mut added = 0;
        let now = now_secs();
        for id in ids {
            let known: bool = tx.query_row(
                "SELECT EXISTS (SELECT 1 FROM provider_models \
                 WHERE provider_id = ?1 AND (model_id = ?2 OR origin_id = ?2))",
                params![provider_id, id],
                |row| row.get(0),
            )?;
            if !known {
                tx.execute(
                    "INSERT INTO provider_models (provider_id, model_id, display_name, origin_id, created_at) \
                     VALUES (?1, ?2, ?2, ?2, ?3)",
                    params![provider_id, id, now],
                )?;
                added += 1;
            }
        }
        let shown: i64 = tx.query_row(
            "SELECT COUNT(*) FROM provider_models WHERE provider_id = ?1 AND hidden = 0",
            [provider_id],
            |row| row.get(0),
        )?;
        tx.commit()?;
        Ok(MergeReport {
            added,
            shown: usize::try_from(shown).unwrap_or(0),
        })
    }

    /// Changes a stored model's id and display name. Changing the id of the provider's default
    /// model changes the default too.
    pub fn edit_model(
        &mut self,
        provider: &str,
        row_id: i64,
        model_id: &str,
        display_name: &str,
    ) -> Result<(), StoreError> {
        let provider_id = self.provider_row(provider)?;
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let old: String = tx
            .query_row(
                "SELECT model_id FROM provider_models WHERE id = ?1 AND provider_id = ?2",
                params![row_id, provider_id],
                |row| row.get(0),
            )
            .optional()?
            .ok_or_else(|| err("that model is not stored any more"))?;
        let clash: bool = tx.query_row(
            "SELECT EXISTS (SELECT 1 FROM provider_models \
             WHERE provider_id = ?1 AND model_id = ?2 AND id <> ?3)",
            params![provider_id, model_id, row_id],
            |row| row.get(0),
        )?;
        if clash {
            return Err(err(format!("{provider} already has the model {model_id}")));
        }
        let display = if display_name.trim().is_empty() {
            model_id
        } else {
            display_name.trim()
        };
        tx.execute(
            "UPDATE provider_models SET model_id = ?2, display_name = ?3 WHERE id = ?1",
            params![row_id, model_id, display],
        )?;
        if old != model_id {
            tx.execute(
                "UPDATE providers SET model = ?2 WHERE id = ?1 AND model = ?3",
                params![provider_id, model_id, old],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Deletes a model added by hand, or hides a fetched one.
    pub fn delete_model(&mut self, provider: &str, row_id: i64) -> Result<Deleted, StoreError> {
        let provider_id = self.provider_row(provider)?;
        let origin: Option<Option<String>> = self
            .conn
            .query_row(
                "SELECT origin_id FROM provider_models WHERE id = ?1 AND provider_id = ?2",
                params![row_id, provider_id],
                |row| row.get(0),
            )
            .optional()?;
        match origin {
            None => Err(err("that model is not stored any more")),
            Some(None) => {
                self.conn
                    .execute("DELETE FROM provider_models WHERE id = ?1", [row_id])?;
                Ok(Deleted::Removed)
            }
            Some(Some(_)) => {
                self.conn.execute(
                    "UPDATE provider_models SET hidden = 1 WHERE id = ?1",
                    [row_id],
                )?;
                Ok(Deleted::Hidden)
            }
        }
    }

    /// Makes `model` the provider's default, storing it as a model if it is not yet (a hidden one
    /// comes back).
    pub fn set_default_model(&mut self, provider: &str, model: &str) -> Result<(), StoreError> {
        let provider_id = self.provider_row(provider)?;
        self.conn.execute(
            "UPDATE providers SET model = ?2 WHERE id = ?1",
            params![provider_id, model],
        )?;
        match self.add_model(provider, model, "") {
            Ok(()) => Ok(()),
            // Already stored and shown: nothing to add.
            Err(e) if e.to_string().contains("already has the model") => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// Replaces a saved provider's settings. The name may change; the "used last" memory follows.
    /// The default model is stored as a model if it is new.
    pub fn update_provider(&mut self, old_name: &str, def: &ProviderDef) -> Result<(), StoreError> {
        let provider_id = self.provider_row(old_name)?;
        if def.name != old_name {
            let taken: bool = self.conn.query_row(
                "SELECT EXISTS (SELECT 1 FROM providers WHERE name = ?1)",
                [&def.name],
                |row| row.get(0),
            )?;
            if taken {
                return Err(err(format!(
                    "there is already a saved provider named {}",
                    def.name
                )));
            }
        }
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        tx.execute(
            "UPDATE providers SET name = ?2, base_url = ?3, model = ?4, api_key = ?5, \
             provider_order = ?6, allow_fallbacks = ?7, session_id = ?8, api = ?9 WHERE id = ?1",
            params![
                provider_id,
                def.name,
                def.base_url,
                def.model,
                def.api_key,
                serde_json::to_string(&def.router.provider_order)
                    .map_err(|e| err(e.to_string()))?,
                def.router.allow_fallbacks,
                def.router.session_id,
                def.api.as_str()
            ],
        )?;
        if def.name != old_name {
            tx.execute(
                "UPDATE settings SET value = ?2 WHERE key = ?1 AND value = ?3",
                params![LAST_PROVIDER, def.name, old_name],
            )?;
        }
        let known: bool = tx.query_row(
            "SELECT EXISTS (SELECT 1 FROM provider_models WHERE provider_id = ?1 AND model_id = ?2)",
            params![provider_id, def.model],
            |row| row.get(0),
        )?;
        if !known {
            tx.execute(
                "INSERT INTO provider_models (provider_id, model_id, display_name, origin_id, created_at) \
                 VALUES (?1, ?2, ?2, NULL, ?3)",
                params![provider_id, def.model, now_secs()],
            )?;
        } else {
            tx.execute(
                "UPDATE provider_models SET hidden = 0 WHERE provider_id = ?1 AND model_id = ?2",
                params![provider_id, def.model],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// The provider files already copied into the database (see [`IMPORTED_PROVIDER_FILES`]).
    pub fn imported_provider_files(&self) -> Result<Vec<String>, StoreError> {
        Ok(self
            .setting(IMPORTED_PROVIDER_FILES)?
            .and_then(|json| serde_json::from_str(&json).ok())
            .unwrap_or_default())
    }

    /// Remembers that these provider files have been imported.
    pub fn mark_imported(&mut self, names: &[String]) -> Result<(), StoreError> {
        let mut all = self.imported_provider_files()?;
        for name in names {
            if !all.contains(name) {
                all.push(name.clone());
            }
        }
        all.sort();
        let json = serde_json::to_string(&all).map_err(|e| err(e.to_string()))?;
        self.set_setting(IMPORTED_PROVIDER_FILES, &json)
    }
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::testutil::Scratch;
    use kernel::RouterOptions;

    fn open(dir: &Scratch) -> Store {
        Store::open(&dir.path().join("harness.db")).unwrap()
    }

    fn provider(name: &str, model: &str) -> ProviderDef {
        ProviderDef {
            api: crate::config::ApiStyle::OpenAi,
            name: name.into(),
            base_url: format!("http://{name}/v1"),
            model: model.into(),
            api_key: Some("key".into()),
            router: RouterOptions::default(),
        }
    }

    fn ids(store: &Store, provider: &str) -> Vec<String> {
        store
            .models_of(provider)
            .unwrap()
            .into_iter()
            .map(|m| m.model_id)
            .collect()
    }

    fn strings(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn a_new_provider_starts_with_its_default_model_stored() {
        let dir = Scratch::new("models-default");
        let mut store = open(&dir);
        store.add_provider(&provider("p", "first")).unwrap();
        let models = store.models_of("p").unwrap();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].model_id, "first");
        assert_eq!(models[0].display_name, "first");
        assert_eq!(models[0].origin_id, None);
        assert!(
            store
                .models_of("nope")
                .unwrap_err()
                .to_string()
                .contains("no saved provider")
        );
    }

    #[test]
    fn a_model_is_added_by_hand_once_and_a_hidden_one_comes_back() {
        let dir = Scratch::new("models-add");
        let mut store = open(&dir);
        store.add_provider(&provider("p", "m")).unwrap();
        store.add_model("p", "a/b", "Nice name").unwrap();
        let again = store.add_model("p", "a/b", "").unwrap_err();
        assert!(again.to_string().contains("already has the model a/b"));
        let shown = store.models_of("p").unwrap();
        assert_eq!(
            shown
                .iter()
                .find(|m| m.model_id == "a/b")
                .unwrap()
                .display_name,
            "Nice name"
        );
        // Listed by display name: "Nice name" sorts before "m".
        assert_eq!(ids(&store, "p"), ["a/b", "m"]);
        // A fetched model that was hidden returns when added by hand.
        store.merge_fetched("p", &strings(&["x"])).unwrap();
        let row = store
            .models_of("p")
            .unwrap()
            .into_iter()
            .find(|m| m.model_id == "x")
            .unwrap();
        store.delete_model("p", row.row_id).unwrap();
        assert!(!ids(&store, "p").contains(&"x".to_owned()));
        store.add_model("p", "x", "X again").unwrap();
        assert!(ids(&store, "p").contains(&"x".to_owned()));
    }

    #[test]
    fn a_refresh_adds_only_ids_it_has_not_seen_and_reports_counts() {
        let dir = Scratch::new("models-merge");
        let mut store = open(&dir);
        store.add_provider(&provider("p", "m")).unwrap();
        let first = store
            .merge_fetched("p", &strings(&["m", "a", "b"]))
            .unwrap();
        assert_eq!(
            (first.added, first.shown),
            (2, 3),
            "the default model m was already there"
        );
        let second = store
            .merge_fetched("p", &strings(&["a", "b", "c"]))
            .unwrap();
        assert_eq!((second.added, second.shown), (1, 4));
        let fetched = store.models_of("p").unwrap();
        assert_eq!(
            fetched
                .iter()
                .find(|m| m.model_id == "a")
                .unwrap()
                .origin_id
                .as_deref(),
            Some("a")
        );
        assert_eq!(
            fetched
                .iter()
                .find(|m| m.model_id == "m")
                .unwrap()
                .origin_id,
            None
        );
    }

    #[test]
    fn a_renamed_or_deleted_fetched_model_is_not_brought_back_by_a_refresh() {
        let dir = Scratch::new("models-no-resurrection");
        let mut store = open(&dir);
        store.add_provider(&provider("p", "m")).unwrap();
        store
            .merge_fetched("p", &strings(&["deepseek", "gone"]))
            .unwrap();
        let rows = store.models_of("p").unwrap();
        let deepseek = rows
            .iter()
            .find(|r| r.model_id == "deepseek")
            .unwrap()
            .row_id;
        let gone = rows.iter().find(|r| r.model_id == "gone").unwrap().row_id;
        store
            .edit_model("p", deepseek, "openrouter/deepseek", "DeepSeek")
            .unwrap();
        assert_eq!(store.delete_model("p", gone).unwrap(), Deleted::Hidden);
        let report = store
            .merge_fetched("p", &strings(&["deepseek", "gone"]))
            .unwrap();
        assert_eq!(report.added, 0, "neither comes back");
        assert_eq!(
            ids(&store, "p"),
            ["openrouter/deepseek", "m"].map(String::from)
        );
        let renamed = store
            .models_of("p")
            .unwrap()
            .into_iter()
            .find(|m| m.display_name == "DeepSeek")
            .unwrap();
        assert_eq!(
            renamed.origin_id.as_deref(),
            Some("deepseek"),
            "the original id is remembered"
        );
    }

    #[test]
    fn deleting_a_model_added_by_hand_removes_it() {
        let dir = Scratch::new("models-delete-manual");
        let mut store = open(&dir);
        store.add_provider(&provider("p", "m")).unwrap();
        store.add_model("p", "mine", "").unwrap();
        let row = store
            .models_of("p")
            .unwrap()
            .into_iter()
            .find(|m| m.model_id == "mine")
            .unwrap();
        assert_eq!(
            store.delete_model("p", row.row_id).unwrap(),
            Deleted::Removed
        );
        assert_eq!(ids(&store, "p"), ["m"]);
        assert!(store.delete_model("p", row.row_id).is_err());
    }

    #[test]
    fn editing_a_model_checks_clashes_and_moves_the_default_with_the_id() {
        let dir = Scratch::new("models-edit");
        let mut store = open(&dir);
        store.add_provider(&provider("p", "m")).unwrap();
        store.add_model("p", "other", "").unwrap();
        let rows = store.models_of("p").unwrap();
        let m = rows.iter().find(|r| r.model_id == "m").unwrap().row_id;
        let clash = store.edit_model("p", m, "other", "x").unwrap_err();
        assert!(clash.to_string().contains("already has the model other"));
        store.edit_model("p", m, "m2", "").unwrap();
        let after = store
            .models_of("p")
            .unwrap()
            .into_iter()
            .find(|r| r.row_id == m)
            .unwrap();
        assert_eq!(
            (after.model_id.as_str(), after.display_name.as_str()),
            ("m2", "m2")
        );
        let default: String = store
            .conn
            .query_row("SELECT model FROM providers WHERE name = 'p'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(default, "m2", "the default followed the id");
        assert!(store.edit_model("p", 9999, "z", "z").is_err());
    }

    #[test]
    fn the_default_model_can_be_set_to_a_model_not_stored_yet() {
        let dir = Scratch::new("models-set-default");
        let mut store = open(&dir);
        store.add_provider(&provider("p", "m")).unwrap();
        store.set_default_model("p", "fresh").unwrap();
        store.set_default_model("p", "fresh").unwrap();
        assert_eq!(ids(&store, "p"), ["fresh", "m"]);
        let default: String = store
            .conn
            .query_row("SELECT model FROM providers WHERE name = 'p'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(default, "fresh");
    }

    #[test]
    fn removing_a_provider_removes_its_models_and_only_its_models() {
        let dir = Scratch::new("models-remove-provider");
        let mut store = open(&dir);
        store.add_provider(&provider("a", "ma")).unwrap();
        store.add_provider(&provider("b", "mb")).unwrap();
        store.merge_fetched("a", &strings(&["x", "y"])).unwrap();
        assert!(store.remove_provider("a").unwrap());
        let left: i64 = store
            .conn
            .query_row("SELECT COUNT(*) FROM provider_models", [], |r| r.get(0))
            .unwrap();
        assert_eq!(left, 1);
        assert_eq!(ids(&store, "b"), ["mb"]);
    }

    #[test]
    fn the_api_of_a_provider_is_saved_listed_and_changed() {
        let dir = Scratch::new("models-api");
        let mut store = open(&dir);
        store.add_provider(&provider("plain", "m")).unwrap();
        let mut claude = provider("claude", "c");
        claude.api = crate::config::ApiStyle::Anthropic;
        store.add_provider(&claude).unwrap();
        let apis = |store: &Store| {
            store
                .list_providers()
                .unwrap()
                .into_iter()
                .map(|p| (p.name, p.api))
                .collect::<Vec<_>>()
        };
        use crate::config::ApiStyle::{Anthropic, OpenAi};
        assert_eq!(
            apis(&store),
            [
                ("claude".to_owned(), Anthropic),
                ("plain".to_owned(), OpenAi)
            ]
        );
        let mut changed = provider("plain", "m");
        changed.api = Anthropic;
        store.update_provider("plain", &changed).unwrap();
        assert!(apis(&store).iter().all(|(_, api)| *api == Anthropic));
    }

    #[test]
    fn schema_18_makes_every_saved_provider_a_chat_completions_one() {
        let dir = Scratch::new("models-schema-18");
        let path = dir.path().join("harness.db");
        let mut store = Store::open(&path).unwrap();
        store.add_provider(&provider("old", "m")).unwrap();
        drop(store);
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute_batch("ALTER TABLE providers DROP COLUMN api; PRAGMA user_version = 17;")
                .unwrap();
        }
        let store = Store::open(&path).unwrap();
        let saved = store.list_providers().unwrap();
        assert_eq!(saved.len(), 1);
        assert_eq!(saved[0].api, crate::config::ApiStyle::OpenAi);
        assert!(dir.path().join("harness.db.before-20").exists());
    }

    #[test]
    fn updating_a_provider_renames_it_and_the_used_last_memory_follows() {
        let dir = Scratch::new("models-update");
        let mut store = open(&dir);
        store.add_provider(&provider("old", "m")).unwrap();
        store.add_provider(&provider("taken", "m")).unwrap();
        store.set_setting(LAST_PROVIDER, "old").unwrap();
        let mut changed = provider("new", "newer");
        changed.api_key = None;
        changed.router.provider_order = strings(&["relace/fp4"]);
        changed.router.allow_fallbacks = Some(false);
        changed.router.session_id = Some("sid".into());
        store.update_provider("old", &changed).unwrap();
        assert_eq!(
            store.setting(LAST_PROVIDER).unwrap().as_deref(),
            Some("new")
        );
        let saved = store.list_providers().unwrap();
        let new = saved.iter().find(|p| p.name == "new").unwrap();
        assert_eq!(new.api_key, None);
        assert_eq!(new.model, "newer");
        assert_eq!(new.router, changed.router);
        assert!(saved.iter().all(|p| p.name != "old"));
        // Models stayed with the provider, and the new default model was stored.
        assert_eq!(ids(&store, "new"), ["m", "newer"]);
        let clash = store
            .update_provider("new", &provider("taken", "m"))
            .unwrap_err();
        assert!(clash.to_string().contains("already a saved provider"));
        assert!(
            store
                .update_provider("missing", &provider("z", "m"))
                .is_err()
        );
    }

    #[test]
    fn imported_provider_files_are_remembered_sorted_and_without_repeats() {
        let dir = Scratch::new("models-imported");
        let mut store = open(&dir);
        assert!(store.imported_provider_files().unwrap().is_empty());
        store.mark_imported(&strings(&["b", "a"])).unwrap();
        store.mark_imported(&strings(&["a", "c"])).unwrap();
        assert_eq!(store.imported_provider_files().unwrap(), ["a", "b", "c"]);
    }

    #[test]
    fn a_version_three_database_gains_the_models_table_and_keeps_its_providers() {
        use crate::store::{SCHEMA_V1_FOR_TESTS, SCHEMA_V2_FOR_TESTS, SCHEMA_V3_FOR_TESTS};
        let dir = Scratch::new("models-v3-to-v4");
        let path = dir.path().join("harness.db");
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute_batch(&format!(
                "{SCHEMA_V1_FOR_TESTS} {SCHEMA_V2_FOR_TESTS} {SCHEMA_V3_FOR_TESTS} PRAGMA user_version = 3;
                 INSERT INTO providers (name, base_url, model, created_at)
                     VALUES ('kept', 'http://k/v1', 'm', 1);"
            ))
            .unwrap();
        }
        let store = Store::open(&path).unwrap();
        let version: i64 = store
            .conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, crate::store::SCHEMA_VERSION);
        assert_eq!(store.list_providers().unwrap().len(), 1);
        assert!(
            store.models_of("kept").unwrap().is_empty(),
            "old providers start with no stored models"
        );
    }
}
