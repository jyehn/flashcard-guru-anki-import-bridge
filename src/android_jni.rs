//! JNI layer for Android (cargo feature `android`; never compiled for iOS).
//!
//! Java side: `com.guruowl.android.core.ankibridge.NativeBridge`, a Kotlin
//! `object` whose `external` functions are `@JvmStatic`. The JNI symbol names
//! below are tied to that class name (the Kotlin package, not the Android
//! application id, so renaming the app does not touch this file). Errors are
//! thrown as `com.guruowl.android.core.ankibridge.AnkiBridgeException(String
//! message, boolean panic)`.
//!
//! Shape of the API
//! - Coarse calls: one JNI call per package open, deck list, page of notes,
//!   batch of notes to render, notetype, or render_fields. Bulk results are
//!   UTF-8 JSON returned as `byte[]` (no modified-UTF-8 round trip, and Kotlin
//!   decodes straight from bytes). iOS makes about six FFI calls per card; here a
//!   batch of notes is one call.
//! - Same behaviour as the C ABI in lib.rs: the same rslib calls, the same SQL,
//!   the same NUL stripping and the same error texts (prefixed with the C function
//!   name, so the app can show iOS-identical messages). The private helpers of
//!   lib.rs are shared (`prepare_workspace`, `read_import_history`,
//!   `parse_card_data`, `sm2_to_fsrs`, `render_fields_inner`). The bodies of the
//!   C functions are repeated as `Result`-returning Rust because a panic cannot be
//!   caught once it reaches an `extern "C"` frame (Rust aborts the process there).
//!   `nativeCompareWithCAbi` runs both paths over one package and reports any
//!   difference, so drift between the two is testable on a device.
//!
//! Safety rules
//! - Panics: every entry point runs its body inside `catch_unwind`; a panic
//!   becomes an `AnkiBridgeException` with `panic = true`. A panic while the
//!   collection mutex is held poisons it, and later calls on that package fail
//!   with "collection mutex poisoned" instead of touching a half-updated
//!   collection.
//! - Handles: a `jlong` is a `Box` raw pointer. Package handles point at the same
//!   `OpenedPackage` the C ABI uses; note-list handles point at `NoteListJni`,
//!   which owns its data and does not borrow the package. Each handle must be
//!   closed exactly once, and never used after or during its close; the Kotlin
//!   wrapper enforces that with a lock and a closed flag. Nothing else is kept
//!   between calls.
//! - Threads: the collection sits behind a mutex inside `OpenedPackage`, so calls
//!   on one package from several threads serialise; different packages and
//!   `nativeRenderFieldsJson` (its own throwaway collection) can run in parallel.
//!   No entry point touches the C ABI's global last-error slot.
//! - Strings: Java strings come in through `GetStringUTFChars` and are decoded
//!   from modified UTF-8 by the `jni` crate; JSON goes out as plain UTF-8 bytes;
//!   the few small strings returned directly are encoded by the `jni` crate.
//! - Temp files: rslib copies the package's collection into a `NamedTempFile` in
//!   `std::env::temp_dir()`. Android sets no TMPDIR and the fallback
//!   `/data/local/tmp` is not writable for apps, so Kotlin must call `nativeInit`
//!   with the app's cache directory once, before the first `nativeOpenPackage`.

use std::any::Any;
use std::ffi::{c_char, CStr, CString};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use anki::card::CardId;
use anki::collection::{Collection, CollectionBuilder};
use anki::decks::DeckId;
use anki::notes::NoteId;
use anki::notetype::{NotetypeId, NotetypeKind};
use anki::search::SearchNode;
use anki_proto::import_export::ImportAnkiPackageOptions;
use jni::objects::{JClass, JLongArray, JString, JThrowable, JValue};
use jni::sys::{jbyteArray, jint, jlong, jstring};
use jni::JNIEnv;
use serde::Serialize;
use serde_json::Value;

use super::{
    parse_card_data, prepare_workspace, read_import_history, render_fields_inner, sm2_to_fsrs,
    FsrsState, NotetypeDef, OpenedPackage, ABI_VERSION,
};

/// Version of this JNI contract (function set + JSON shapes). Bump on any change
/// the Kotlin side must know about; the Kotlin loader refuses a mismatch.
const JNI_API_VERSION: i32 = 1;

const EXCEPTION_CLASS: &str = "com/guruowl/android/core/ankibridge/AnkiBridgeException";

type BridgeResult<T> = Result<T, String>;

// ---------------------------------------------------------------------------
// Entry-point plumbing: panics, exceptions, handles, marshalling.
// ---------------------------------------------------------------------------

/// Runs `body`, turning an `Err` or a panic into a Java exception and returning
/// `fallback` (ignored by the JVM while an exception is pending).
fn entry<'local, T>(
    env: &mut JNIEnv<'local>,
    name: &str,
    fallback: T,
    body: impl FnOnce(&mut JNIEnv<'local>) -> BridgeResult<T>,
) -> T {
    match catch_unwind(AssertUnwindSafe(|| body(&mut *env))) {
        Ok(Ok(value)) => value,
        Ok(Err(message)) => {
            throw(env, &message, false);
            fallback
        }
        Err(payload) => {
            let message = format!("{name}: panic: {}", panic_message(payload.as_ref()));
            throw(env, &message, true);
            fallback
        }
    }
}

fn panic_message(payload: &(dyn Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "unknown panic payload".to_string()
    }
}

fn throw(env: &mut JNIEnv, message: &str, panic: bool) {
    // A JNI call that failed may already have left a Java exception pending
    // (OutOfMemoryError from a large byte[]). Keep that one: throwing over a
    // pending exception is not allowed.
    if env.exception_check().unwrap_or(true) {
        return;
    }
    if throw_bridge_exception(env, message, panic).is_err() {
        let _ = env.exception_clear();
        let _ = env.throw_new("java/lang/RuntimeException", message);
    }
}

fn throw_bridge_exception(env: &mut JNIEnv, message: &str, panic: bool) -> jni::errors::Result<()> {
    let class = env.find_class(EXCEPTION_CLASS)?;
    let text = env.new_string(message)?;
    let exception = env.new_object(
        class,
        "(Ljava/lang/String;Z)V",
        &[JValue::Object(&text), JValue::Bool(u8::from(panic))],
    )?;
    env.throw(JThrowable::from(exception))
}

fn package<'a>(handle: jlong) -> BridgeResult<&'a OpenedPackage> {
    if handle == 0 {
        return Err("null package handle".to_string());
    }
    // SAFETY: non-zero handles come from `nativeOpenPackage` (Box::into_raw) and
    // the Kotlin wrapper never uses one after `nativeClosePackage`.
    Ok(unsafe { &*(handle as *const OpenedPackage) })
}

fn note_list<'a>(handle: jlong) -> BridgeResult<&'a NoteListJni> {
    if handle == 0 {
        return Err("null note list handle".to_string());
    }
    // SAFETY: as for `package`, with `nativeEnumerateNotes` / `nativeFreeNoteList`.
    Ok(unsafe { &*(handle as *const NoteListJni) })
}

fn lock<'a>(pkg: &'a OpenedPackage, label: &str) -> BridgeResult<MutexGuard<'a, Collection>> {
    pkg.collection
        .lock()
        .map_err(|_| format!("{label}: collection mutex poisoned"))
}

fn read_string(env: &mut JNIEnv, value: &JString, what: &str) -> BridgeResult<String> {
    if value.is_null() {
        return Err(format!("{what} is null"));
    }
    env.get_string(value)
        .map(Into::into)
        .map_err(|e| format!("{what}: {e}"))
}

fn read_longs(env: &mut JNIEnv, array: &JLongArray, what: &str) -> BridgeResult<Vec<i64>> {
    if array.is_null() {
        return Err(format!("{what} is null"));
    }
    let len = env
        .get_array_length(array)
        .map_err(|e| format!("{what}: {e}"))? as usize;
    let mut values = vec![0i64; len];
    env.get_long_array_region(array, 0, &mut values)
        .map_err(|e| format!("{what}: {e}"))?;
    Ok(values)
}

fn json_bytes(env: &mut JNIEnv, value: &impl Serialize) -> BridgeResult<jbyteArray> {
    let bytes = serde_json::to_vec(value).map_err(|e| format!("encode json: {e}"))?;
    env.byte_array_from_slice(&bytes)
        .map(|array| array.into_raw())
        .map_err(|e| format!("allocate byte[{}]: {e}", bytes.len()))
}

fn java_string(env: &mut JNIEnv, value: &str) -> BridgeResult<jstring> {
    env.new_string(value)
        .map(|s| s.into_raw())
        .map_err(|e| format!("new string: {e}"))
}

/// Same NUL stripping as the C ABI (`CString::new(s.replace('\0', ""))`).
fn clean(s: &str) -> String {
    s.replace('\0', "")
}

/// Removes a work directory on drop unless disarmed, so a panic inside
/// `import_apkg` does not leave the temp collection behind.
struct WorkDirGuard(Option<PathBuf>);

impl Drop for WorkDirGuard {
    fn drop(&mut self) {
        if let Some(dir) = self.0.take() {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}

// ---------------------------------------------------------------------------
// JSON shapes (the JNI contract; mirrored by Kotlin data classes).
// ---------------------------------------------------------------------------

#[derive(Serialize, PartialEq, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DeckJson {
    id: u64,
    name: String,
}

#[derive(Serialize, PartialEq, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub(crate) struct NoteJson {
    id: u64,
    notetype_id: u64,
    fields: Vec<String>,
    tags: Vec<String>,
    sort_field: String,
}

/// Owned snapshot of a deck's notes; paged out by `nativeNotesJson`.
pub(crate) struct NoteListJni {
    rows: Vec<NoteJson>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SchedulerJson {
    /// f32 exactly as the C ABI's `FsrsState`; `-1` = no memory state. A
    /// non-finite value serialises as JSON null (Kotlin reads it back as NaN).
    stability: f32,
    difficulty: f32,
    due_days_from_today: i32,
    elapsed_days: i32,
    reps: u32,
    lapses: u32,
}

impl From<&FsrsState> for SchedulerJson {
    fn from(s: &FsrsState) -> Self {
        Self {
            stability: s.stability,
            difficulty: s.difficulty,
            due_days_from_today: s.due_days_from_today,
            elapsed_days: s.elapsed_days,
            reps: s.reps,
            lapses: s.lapses,
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct RenderedCardJson {
    card_id: u64,
    ord: u32,
    /// Present when `flashify_render_template` would have succeeded.
    #[serde(skip_serializing_if = "Option::is_none")]
    question: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    answer: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    css: Option<String>,
    /// Present instead of question/answer/css when rendering failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    render_error: Option<String>,
    scheduler: SchedulerJson,
    /// `flashify_card_import_history_json`, embedded as a JSON object.
    #[serde(skip_serializing_if = "Option::is_none")]
    history: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    history_error: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct RenderedNoteJson {
    note_id: u64,
    cards: Vec<RenderedCardJson>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct HistoryResultJson {
    card_id: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    history: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Serialize, PartialEq, Debug)]
#[serde(rename_all = "camelCase")]
struct TemplateJson {
    ord: u32,
    name: String,
    qfmt: String,
    afmt: String,
}

#[derive(Serialize, PartialEq, Debug)]
#[serde(rename_all = "camelCase")]
struct NotetypeJson {
    id: u64,
    field_names: Vec<String>,
    css: String,
    is_cloze: bool,
    sort_field_idx: u32,
    templates: Vec<TemplateJson>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct RenderedFieldsCardJson {
    ord: u32,
    question: String,
    answer: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct RenderedFieldsJson {
    cards: Vec<RenderedFieldsCardJson>,
    css: String,
}

// ---------------------------------------------------------------------------
// Core operations. Each mirrors one C ABI function in lib.rs line for line.
// ---------------------------------------------------------------------------

fn init(temp_dir: &str) -> BridgeResult<String> {
    if temp_dir.is_empty() {
        return Err("nativeInit: empty temp dir".to_string());
    }
    let dir = PathBuf::from(temp_dir);
    std::fs::create_dir_all(&dir).map_err(|e| format!("nativeInit: create {temp_dir}: {e}"))?;
    let probe = dir.join(format!(".rust-import-probe-{}", std::process::id()));
    std::fs::write(&probe, b"").map_err(|e| format!("nativeInit: {temp_dir} not writable: {e}"))?;
    let _ = std::fs::remove_file(&probe);
    // Process-global. Kotlin calls this once, right after loading the library and
    // before any import thread exists.
    std::env::set_var("TMPDIR", &dir);
    Ok(std::env::temp_dir().to_string_lossy().into_owned())
}

/// `flashify_open_package`.
fn open_package(package_path: &str, workspace_path: &str) -> BridgeResult<OpenedPackage> {
    let (work_dir, col_path, media_folder, media_db) = prepare_workspace(Path::new(workspace_path))
        .map_err(|e| format!("flashify_open_package: prepare workspace: {e}"))?;
    let mut guard = WorkDirGuard(Some(work_dir.clone()));

    let media_folder_for_handle = media_folder.clone();
    let mut col = CollectionBuilder::new(&col_path)
        .set_media_paths(media_folder, media_db)
        .build()
        .map_err(|e| format!("flashify_open_package: build collection: {e}"))?;

    let options = ImportAnkiPackageOptions {
        with_scheduling: true,
        with_deck_configs: true,
        merge_notetypes: true,
        ..Default::default()
    };
    if let Err(e) = col.import_apkg(package_path, options) {
        drop(col);
        return Err(format!("flashify_open_package: import_apkg: {e}"));
    }

    guard.0 = None;
    let media_dir_cstring =
        CString::new(media_folder_for_handle.to_string_lossy().as_bytes()).unwrap_or_default();
    Ok(OpenedPackage {
        collection: Mutex::new(col),
        work_dir,
        owns_work_dir: true,
        media_dir_cstring,
    })
}

/// `flashify_enumerate_decks`: every deck (default included) owning a card.
fn decks(col: &Collection) -> BridgeResult<Vec<DeckJson>> {
    let pairs = col
        .get_all_deck_names(false)
        .map_err(|e| format!("flashify_enumerate_decks: {e}"))?;
    let mut stmt = col
        .storage
        .db()
        .prepare_cached("SELECT 1 FROM cards WHERE did = ? LIMIT 1")
        .map_err(|e| format!("flashify_enumerate_decks: prepare: {e}"))?;
    let mut out = Vec::with_capacity(pairs.len());
    for (DeckId(raw_id), name) in pairs {
        let has_cards = stmt
            .query_row(rusqlite::params![raw_id], |_| Ok(()))
            .is_ok();
        if has_cards {
            out.push(DeckJson {
                id: raw_id as u64,
                name: clean(&name),
            });
        }
    }
    Ok(out)
}

/// `flashify_enumerate_notes`: `deck_id == 0` = all notes, else notes with a card
/// in the deck or any child deck; sorted by note id.
fn notes(col: &mut Collection, deck_id: u64) -> BridgeResult<Vec<NoteJson>> {
    let mut note_ids: Vec<NoteId> = if deck_id == 0 {
        col.storage
            .get_all_note_ids()
            .map_err(|e| format!("flashify_enumerate_notes: get_all_note_ids: {e}"))?
            .into_iter()
            .collect()
    } else {
        col.search_notes_unordered(SearchNode::DeckIdWithChildren(DeckId(deck_id as i64)))
            .map_err(|e| format!("flashify_enumerate_notes: search_notes_unordered: {e}"))?
    };
    note_ids.sort_unstable_by_key(|nid| nid.0);

    let mut rows = Vec::with_capacity(note_ids.len());
    for nid in note_ids {
        let note = match col.storage.get_note(nid) {
            Ok(Some(n)) => n,
            Ok(None) => continue,
            Err(e) => return Err(format!("flashify_enumerate_notes: get_note: {e}")),
        };
        let sort_field = {
            let idx = col
                .get_notetype(note.notetype_id)
                .ok()
                .flatten()
                .map(|nt| nt.config.sort_field_idx as usize)
                .unwrap_or(0);
            let raw = note.fields().get(idx).map(String::as_str).unwrap_or("");
            clean(&anki::text::strip_html(raw))
        };
        rows.push(NoteJson {
            id: note.id.0 as u64,
            notetype_id: note.notetype_id.0 as u64,
            fields: note.fields().iter().map(|f| clean(f)).collect(),
            tags: note.tags.iter().map(|t| clean(t)).collect(),
            sort_field,
        });
    }
    Ok(rows)
}

/// `flashify_enumerate_cards_for_note`.
fn cards_for_note(col: &Collection, note_id: u64) -> BridgeResult<Vec<(u64, u32)>> {
    let mut stmt = col
        .storage
        .db()
        .prepare_cached("SELECT id, ord FROM cards WHERE nid = ? ORDER BY ord")
        .map_err(|e| format!("flashify_enumerate_cards_for_note: prepare: {e}"))?;
    let rows = stmt
        .query_map(rusqlite::params![note_id as i64], |row| {
            let cid: i64 = row.get(0)?;
            let ord: i64 = row.get(1)?;
            Ok((cid as u64, ord as u32))
        })
        .map_err(|e| format!("flashify_enumerate_cards_for_note: query: {e}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("flashify_enumerate_cards_for_note: collect: {e}"))?;
    Ok(rows)
}

/// `flashify_card_state_as_fsrs`, including its "empty state on any error" rule.
fn card_state(col: &mut Collection, card_id: u64) -> FsrsState {
    let row = col
        .storage
        .db()
        .query_row::<(i64, i64, i32, i64, i64, i64, i64, String), _, _>(
            "SELECT queue, type, due, ivl, factor, reps, lapses, COALESCE(data, '') \
             FROM cards WHERE id = ?",
            [card_id as i64],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i32>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, i64>(5)?,
                    row.get::<_, i64>(6)?,
                    row.get::<_, String>(7)?,
                ))
            },
        );
    let (queue, ctype, due, ivl, factor, reps, lapses, data_json) = match row {
        Ok(r) => r,
        Err(_) => return FsrsState::empty(),
    };
    let blob = parse_card_data(&data_json);
    let timing = match col.timing_today() {
        Ok(t) => t,
        Err(_) => return FsrsState::empty(),
    };
    let today: i32 = timing.days_elapsed as i32;
    let now_secs: i64 = timing.now.0;

    let due_days_from_today: i32 = match queue {
        2 | 3 => due - today,
        1 => {
            let delta = (due as i64) - now_secs;
            (delta / 86_400) as i32
        }
        _ => 0,
    };

    let (stability, difficulty) =
        if let (Some(s), Some(d)) = (blob.fsrs_stability, blob.fsrs_difficulty) {
            (s, d)
        } else if ctype != 0 && ivl > 0 {
            let ease = (factor as f32) / 1000.0;
            match sm2_to_fsrs(ease, ivl as f32) {
                Some(pair) => pair,
                None => return FsrsState::empty(),
            }
        } else {
            return FsrsState {
                stability: -1.0,
                difficulty: -1.0,
                due_days_from_today,
                elapsed_days: 0,
                reps: reps as u32,
                lapses: lapses as u32,
            };
        };

    let elapsed_days: i32 = match blob.last_review_time_secs {
        Some(lrt) => ((now_secs - lrt).max(0) / 86_400) as i32,
        None => 0,
    };

    FsrsState {
        stability,
        difficulty,
        due_days_from_today,
        elapsed_days,
        reps: reps as u32,
        lapses: lapses as u32,
    }
}

/// `flashify_render_template`: (question, answer, css).
fn render(col: &mut Collection, note_id: u64, ord: u32) -> BridgeResult<(String, String, String)> {
    let card_id_raw: i64 = match col.storage.db().query_row::<i64, _, _>(
        "SELECT id FROM cards WHERE nid = ? AND ord = ?",
        rusqlite::params![note_id as i64, ord as i64],
        |row| row.get(0),
    ) {
        Ok(v) => v,
        Err(rusqlite::Error::QueryReturnedNoRows) => {
            return Err(format!(
                "flashify_render_template: no card for note {note_id} ord {ord}"
            ))
        }
        Err(e) => return Err(format!("flashify_render_template: lookup card id: {e}")),
    };
    let output = col
        .render_existing_card(CardId(card_id_raw), false, false)
        .map_err(|e| format!("flashify_render_template: render_existing_card: {e}"))?;
    Ok((
        clean(&output.question()),
        clean(&output.answer()),
        clean(&output.css),
    ))
}

/// `flashify_card_import_history_json`, as a JSON value.
fn history(col: &Collection, card_id: u64) -> BridgeResult<Value> {
    if card_id > i64::MAX as u64 {
        return Err("flashify_card_import_history_json: invalid package or card id".to_string());
    }
    read_import_history(col.storage.db(), card_id as i64)
        .map_err(|e| format!("flashify_card_import_history_json: query: {e}"))
}

/// `flashify_notetype_schema`.
fn notetype(col: &mut Collection, notetype_id: u64) -> BridgeResult<NotetypeJson> {
    let nt = match col.get_notetype(NotetypeId(notetype_id as i64)) {
        Ok(Some(nt)) => nt,
        Ok(None) => {
            return Err(format!(
                "flashify_notetype_schema: no notetype with id {notetype_id}"
            ))
        }
        Err(e) => return Err(format!("flashify_notetype_schema: get_notetype: {e}")),
    };
    let templates = nt
        .templates
        .iter()
        .enumerate()
        .map(|(i, t)| TemplateJson {
            ord: t.ord.unwrap_or(i as u32),
            name: clean(&t.name),
            qfmt: clean(&t.config.q_format),
            afmt: clean(&t.config.a_format),
        })
        .collect();
    Ok(NotetypeJson {
        id: notetype_id,
        field_names: nt.fields.iter().map(|f| clean(&f.name)).collect(),
        css: clean(&nt.config.css),
        is_cloze: nt.config.kind() == NotetypeKind::Cloze,
        sort_field_idx: nt.config.sort_field_idx,
        templates,
    })
}

/// `flashify_render_fields` (no package; throwaway in-memory collection).
fn render_fields(notetype_json: &str, field_values_json: &str) -> BridgeResult<RenderedFieldsJson> {
    let def: NotetypeDef = serde_json::from_str(notetype_json)
        .map_err(|e| format!("flashify_render_fields: parse notetype_json: {e}"))?;
    let values: Vec<String> = serde_json::from_str(field_values_json)
        .map_err(|e| format!("flashify_render_fields: parse field_values_json: {e}"))?;
    let list = render_fields_inner(def, values).map_err(|e| format!("flashify_render_fields: {e}"))?;
    let text = |s: &CString| s.to_string_lossy().into_owned();
    Ok(RenderedFieldsJson {
        cards: list
            .ords
            .iter()
            .zip(list.questions.iter().zip(list.answers.iter()))
            .map(|(ord, (q, a))| RenderedFieldsCardJson {
                ord: *ord,
                question: text(q),
                answer: text(a),
            })
            .collect(),
        css: text(&list.css),
    })
}

/// Everything the iOS persister asks per note (cardsForNote, then per card
/// renderTemplate, cardStateAsFsrs, importHistory), in one pass under one lock.
/// A cardsForNote failure fails the whole call (iOS fails the import); render and
/// history failures are reported per card so Kotlin applies the iOS policy.
fn render_notes(col: &mut Collection, note_ids: &[i64]) -> BridgeResult<Vec<RenderedNoteJson>> {
    let mut out = Vec::with_capacity(note_ids.len());
    for &raw in note_ids {
        let note_id = raw as u64;
        let ordinals = cards_for_note(col, note_id)?;
        let mut cards = Vec::with_capacity(ordinals.len());
        for (card_id, ord) in ordinals {
            let (question, answer, css, render_error) = match render(col, note_id, ord) {
                Ok((q, a, c)) => (Some(q), Some(a), Some(c), None),
                Err(e) => (None, None, None, Some(e)),
            };
            let scheduler = SchedulerJson::from(&card_state(col, card_id));
            let (history, history_error) = match history(col, card_id) {
                Ok(v) => (Some(v), None),
                Err(e) => (None, Some(e)),
            };
            cards.push(RenderedCardJson {
                card_id,
                ord,
                question,
                answer,
                css,
                render_error,
                scheduler,
                history,
                history_error,
            });
        }
        out.push(RenderedNoteJson { note_id, cards });
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Drift check: the same package read through the C ABI (what iOS calls).
// ---------------------------------------------------------------------------

unsafe fn c_text(ptr: *const c_char) -> String {
    if ptr.is_null() {
        String::new()
    } else {
        CStr::from_ptr(ptr).to_string_lossy().into_owned()
    }
}

unsafe fn c_last_error() -> String {
    c_text(super::flashify_last_error_message())
}

unsafe fn c_decks(pkg: *mut OpenedPackage) -> BridgeResult<Vec<DeckJson>> {
    let list = super::flashify_enumerate_decks(pkg);
    if list.is_null() {
        return Err(c_last_error());
    }
    let n = super::flashify_decks_count(list).max(0) as u64;
    let decks = (0..n)
        .map(|i| DeckJson {
            id: super::flashify_decks_id_at(list, i),
            name: c_text(super::flashify_decks_name_at(list, i)),
        })
        .collect();
    super::flashify_free_decks(list);
    Ok(decks)
}

unsafe fn c_notes(pkg: *mut OpenedPackage, deck_id: u64) -> BridgeResult<Vec<NoteJson>> {
    let list = super::flashify_enumerate_notes(pkg, deck_id);
    if list.is_null() {
        return Err(c_last_error());
    }
    let n = super::flashify_notes_count(list).max(0) as u64;
    let mut rows = Vec::with_capacity(n as usize);
    for i in 0..n {
        let field_count = super::flashify_notes_field_count_at(list, i).max(0) as u64;
        let tag_count = super::flashify_notes_tag_count_at(list, i).max(0) as u64;
        rows.push(NoteJson {
            id: super::flashify_notes_id_at(list, i),
            notetype_id: super::flashify_notes_notetype_id_at(list, i),
            fields: (0..field_count)
                .map(|f| c_text(super::flashify_notes_field_at(list, i, f)))
                .collect(),
            tags: (0..tag_count)
                .map(|t| c_text(super::flashify_notes_tag_at(list, i, t)))
                .collect(),
            sort_field: c_text(super::flashify_notes_sort_field_at(list, i)),
        });
    }
    super::flashify_free_notes(list);
    Ok(rows)
}

unsafe fn c_cards_for_note(pkg: *mut OpenedPackage, note_id: u64) -> BridgeResult<Vec<(u64, u32)>> {
    let list = super::flashify_enumerate_cards_for_note(pkg, note_id);
    if list.is_null() {
        return Err(c_last_error());
    }
    let n = super::flashify_card_ords_count(list).max(0) as u64;
    let rows = (0..n)
        .map(|i| {
            (
                super::flashify_card_ords_id_at(list, i),
                super::flashify_card_ords_ord_at(list, i),
            )
        })
        .collect();
    super::flashify_free_card_ords(list);
    Ok(rows)
}

unsafe fn c_render(pkg: *mut OpenedPackage, note_id: u64, ord: u32) -> BridgeResult<(String, String, String)> {
    let card = super::flashify_render_template(pkg, note_id, ord);
    if card.is_null() {
        return Err(c_last_error());
    }
    let out = (
        c_text(super::flashify_rendered_question(card)),
        c_text(super::flashify_rendered_answer(card)),
        c_text(super::flashify_rendered_css(card)),
    );
    super::flashify_free_rendered(card);
    Ok(out)
}

unsafe fn c_history(pkg: *mut OpenedPackage, card_id: u64) -> BridgeResult<Value> {
    let json = super::flashify_card_import_history_json(pkg, card_id);
    if json.is_null() {
        return Err(c_last_error());
    }
    let text = c_text(json);
    super::flashify_free_import_history_json(json);
    serde_json::from_str(&text).map_err(|e| e.to_string())
}

unsafe fn c_notetype(pkg: *mut OpenedPackage, id: u64) -> BridgeResult<NotetypeJson> {
    let s = super::flashify_notetype_schema(pkg, id);
    if s.is_null() {
        return Err(c_last_error());
    }
    let fields = super::flashify_notetype_field_count(s).max(0) as u64;
    let templates = super::flashify_notetype_template_count(s).max(0) as u64;
    let out = NotetypeJson {
        id,
        field_names: (0..fields)
            .map(|i| c_text(super::flashify_notetype_field_name_at(s, i)))
            .collect(),
        css: c_text(super::flashify_notetype_css(s)),
        is_cloze: super::flashify_notetype_is_cloze(s) == 1,
        sort_field_idx: super::flashify_notetype_sort_field_idx(s),
        templates: (0..templates)
            .map(|i| TemplateJson {
                ord: super::flashify_notetype_template_ord_at(s, i),
                name: c_text(super::flashify_notetype_template_name_at(s, i)),
                qfmt: c_text(super::flashify_notetype_template_qfmt_at(s, i)),
                afmt: c_text(super::flashify_notetype_template_afmt_at(s, i)),
            })
            .collect(),
    };
    super::flashify_free_notetype_schema(s);
    Ok(out)
}

fn same_state(a: &FsrsState, b: &FsrsState) -> bool {
    a.stability.to_bits() == b.stability.to_bits()
        && a.difficulty.to_bits() == b.difficulty.to_bits()
        && a.due_days_from_today == b.due_days_from_today
        && a.elapsed_days == b.elapsed_days
        && a.reps == b.reps
        && a.lapses == b.lapses
}

fn without_imported_at(mut v: Value) -> Value {
    if let Some(map) = v.as_object_mut() {
        map.remove("importedAt");
    }
    v
}

/// Reads one package through both paths and lists every difference. The C side
/// cannot catch panics; this is a diagnostic for known-good packages (tests).
fn compare_with_c_abi(handle: jlong) -> BridgeResult<Value> {
    let pkg = package(handle)?;
    let ptr = handle as *mut OpenedPackage;
    let mut mismatches: Vec<String> = Vec::new();
    let mut note_count = 0usize;
    let mut card_count = 0usize;
    let mut notetype_ids: Vec<u64> = Vec::new();

    let jni_decks = decks(&*lock(pkg, "compare")?)?;
    let c_deck_list = unsafe { c_decks(ptr)? };
    if jni_decks != c_deck_list {
        mismatches.push(format!("decks differ: jni={jni_decks:?} c={c_deck_list:?}"));
    }

    let mut deck_ids: Vec<u64> = jni_decks.iter().map(|d| d.id).collect();
    deck_ids.push(0);
    for deck_id in deck_ids {
        let jni_notes = notes(&mut *lock(pkg, "compare")?, deck_id)?;
        let c_note_list = unsafe { c_notes(ptr, deck_id)? };
        if jni_notes != c_note_list {
            mismatches.push(format!("notes of deck {deck_id} differ"));
        }
        if deck_id != 0 {
            continue;
        }
        note_count = jni_notes.len();
        for note in &jni_notes {
            if !notetype_ids.contains(&note.notetype_id) {
                notetype_ids.push(note.notetype_id);
            }
            let jni_cards = cards_for_note(&*lock(pkg, "compare")?, note.id)?;
            let c_cards = unsafe { c_cards_for_note(ptr, note.id)? };
            if jni_cards != c_cards {
                mismatches.push(format!("cards of note {} differ", note.id));
            }
            for &(card_id, ord) in &jni_cards {
                card_count += 1;
                let jni_render = render(&mut *lock(pkg, "compare")?, note.id, ord);
                let c_rendered = unsafe { c_render(ptr, note.id, ord) };
                if jni_render != c_rendered {
                    mismatches.push(format!("render of card {card_id} differs"));
                }
                let jni_state = card_state(&mut *lock(pkg, "compare")?, card_id);
                let c_state = unsafe { super::flashify_card_state_as_fsrs(ptr, card_id) };
                if !same_state(&jni_state, &c_state) {
                    mismatches.push(format!("scheduler of card {card_id} differs"));
                }
                let jni_history = history(&*lock(pkg, "compare")?, card_id).map(without_imported_at);
                let c_hist = unsafe { c_history(ptr, card_id) }.map(without_imported_at);
                if jni_history != c_hist {
                    mismatches.push(format!("history of card {card_id} differs"));
                }
            }
        }
    }

    for &id in &notetype_ids {
        let jni_nt = notetype(&mut *lock(pkg, "compare")?, id);
        let c_nt = unsafe { c_notetype(ptr, id) };
        if jni_nt != c_nt {
            mismatches.push(format!("notetype {id} differs"));
        }
    }

    Ok(serde_json::json!({
        "decks": jni_decks.len(),
        "notes": note_count,
        "cards": card_count,
        "notetypes": notetype_ids.len(),
        "mismatches": mismatches,
    }))
}

// ---------------------------------------------------------------------------
// JNI exports: com.guruowl.android.core.ankibridge.NativeBridge
// ---------------------------------------------------------------------------

/// The C ABI version this library implements (5, as `flashify_abi_version`).
#[no_mangle]
pub extern "system" fn Java_com_guruowl_android_core_ankibridge_NativeBridge_nativeAbiVersion(
    _env: JNIEnv,
    _class: JClass,
) -> jint {
    ABI_VERSION as jint
}

/// The JNI contract version (`JNI_API_VERSION`).
#[no_mangle]
pub extern "system" fn Java_com_guruowl_android_core_ankibridge_NativeBridge_nativeJniVersion(
    _env: JNIEnv,
    _class: JClass,
) -> jint {
    JNI_API_VERSION
}

/// rslib's version string.
#[no_mangle]
pub extern "system" fn Java_com_guruowl_android_core_ankibridge_NativeBridge_nativeAnkiVersion<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
) -> jstring {
    entry(&mut env, "nativeAnkiVersion", std::ptr::null_mut(), |env| {
        java_string(env, &anki::version::version())
    })
}

/// Sets TMPDIR for rslib's temp files; returns `std::env::temp_dir()` after.
#[no_mangle]
pub extern "system" fn Java_com_guruowl_android_core_ankibridge_NativeBridge_nativeInit<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    temp_dir: JString<'local>,
) -> jstring {
    entry(&mut env, "nativeInit", std::ptr::null_mut(), |env| {
        let dir = read_string(env, &temp_dir, "tempDir")?;
        let resolved = init(&dir)?;
        java_string(env, &resolved)
    })
}

/// Imports the package into a temp collection under `workspacePath`; returns a
/// package handle for `nativeClosePackage`.
#[no_mangle]
pub extern "system" fn Java_com_guruowl_android_core_ankibridge_NativeBridge_nativeOpenPackage<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    package_path: JString<'local>,
    workspace_path: JString<'local>,
) -> jlong {
    entry(&mut env, "nativeOpenPackage", 0, |env| {
        let package_path = read_string(env, &package_path, "packagePath")?;
        let workspace_path = read_string(env, &workspace_path, "workspacePath")?;
        let pkg = open_package(&package_path, &workspace_path)?;
        Ok(Box::into_raw(Box::new(pkg)) as jlong)
    })
}

/// Drops the collection and deletes its work directory. 0 is ignored.
#[no_mangle]
pub extern "system" fn Java_com_guruowl_android_core_ankibridge_NativeBridge_nativeClosePackage<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
) {
    entry(&mut env, "nativeClosePackage", (), |_| {
        if handle != 0 {
            // SAFETY: see `package`; Kotlin calls this once per handle.
            drop(unsafe { Box::from_raw(handle as *mut OpenedPackage) });
        }
        Ok(())
    })
}

/// Absolute path of rslib's `collection.media` directory for this package.
#[no_mangle]
pub extern "system" fn Java_com_guruowl_android_core_ankibridge_NativeBridge_nativeMediaDir<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
) -> jstring {
    entry(&mut env, "nativeMediaDir", std::ptr::null_mut(), |env| {
        let pkg = package(handle)?;
        let path = pkg.media_dir_cstring.to_string_lossy().into_owned();
        java_string(env, &path)
    })
}

/// `[{"id","name"}]` for every deck that owns a card.
#[no_mangle]
pub extern "system" fn Java_com_guruowl_android_core_ankibridge_NativeBridge_nativeDecksJson<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
) -> jbyteArray {
    entry(&mut env, "nativeDecksJson", std::ptr::null_mut(), |env| {
        let pkg = package(handle)?;
        let list = decks(&*lock(pkg, "flashify_enumerate_decks")?)?;
        json_bytes(env, &list)
    })
}

/// Snapshots the notes of a deck (0 = all notes); returns a note-list handle.
#[no_mangle]
pub extern "system" fn Java_com_guruowl_android_core_ankibridge_NativeBridge_nativeEnumerateNotes<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
    deck_id: jlong,
) -> jlong {
    entry(&mut env, "nativeEnumerateNotes", 0, |_| {
        let pkg = package(handle)?;
        let rows = notes(&mut *lock(pkg, "flashify_enumerate_notes")?, deck_id as u64)?;
        Ok(Box::into_raw(Box::new(NoteListJni { rows })) as jlong)
    })
}

/// Number of notes in a note list.
#[no_mangle]
pub extern "system" fn Java_com_guruowl_android_core_ankibridge_NativeBridge_nativeNoteListSize<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    list: jlong,
) -> jint {
    entry(&mut env, "nativeNoteListSize", 0, |_| {
        let rows = &note_list(list)?.rows;
        i32::try_from(rows.len()).map_err(|_| "note list too large".to_string())
    })
}

/// `[{"id","notetypeId","fields","tags","sortField"}]` for `[offset, offset+limit)`.
#[no_mangle]
pub extern "system" fn Java_com_guruowl_android_core_ankibridge_NativeBridge_nativeNotesJson<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    list: jlong,
    offset: jint,
    limit: jint,
) -> jbyteArray {
    entry(&mut env, "nativeNotesJson", std::ptr::null_mut(), |env| {
        let rows = &note_list(list)?.rows;
        if offset < 0 || limit < 0 {
            return Err(format!("nativeNotesJson: bad page offset={offset} limit={limit}"));
        }
        let start = (offset as usize).min(rows.len());
        let end = start.saturating_add(limit as usize).min(rows.len());
        json_bytes(env, &&rows[start..end])
    })
}

/// Frees a note list. 0 is ignored.
#[no_mangle]
pub extern "system" fn Java_com_guruowl_android_core_ankibridge_NativeBridge_nativeFreeNoteList<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    list: jlong,
) {
    entry(&mut env, "nativeFreeNoteList", (), |_| {
        if list != 0 {
            // SAFETY: see `note_list`; Kotlin frees each list once.
            drop(unsafe { Box::from_raw(list as *mut NoteListJni) });
        }
        Ok(())
    })
}

/// For each note id: its cards with rendered q/a/css (or `renderError`), the
/// scheduler state and the source history (or `historyError`).
#[no_mangle]
pub extern "system" fn Java_com_guruowl_android_core_ankibridge_NativeBridge_nativeRenderNotesJson<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
    note_ids: JLongArray<'local>,
) -> jbyteArray {
    entry(&mut env, "nativeRenderNotesJson", std::ptr::null_mut(), |env| {
        let pkg = package(handle)?;
        let ids = read_longs(env, &note_ids, "noteIds")?;
        let rendered = render_notes(&mut *lock(pkg, "nativeRenderNotesJson")?, &ids)?;
        json_bytes(env, &rendered)
    })
}

/// `[{"cardId","history"} | {"cardId","error"}]` (history repair path).
#[no_mangle]
pub extern "system" fn Java_com_guruowl_android_core_ankibridge_NativeBridge_nativeCardHistoriesJson<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
    card_ids: JLongArray<'local>,
) -> jbyteArray {
    entry(&mut env, "nativeCardHistoriesJson", std::ptr::null_mut(), |env| {
        let pkg = package(handle)?;
        let ids = read_longs(env, &card_ids, "cardIds")?;
        let col = lock(pkg, "flashify_card_import_history_json")?;
        let results: Vec<HistoryResultJson> = ids
            .iter()
            .map(|&raw| match history(&col, raw as u64) {
                Ok(v) => HistoryResultJson { card_id: raw as u64, history: Some(v), error: None },
                Err(e) => HistoryResultJson { card_id: raw as u64, history: None, error: Some(e) },
            })
            .collect();
        drop(col);
        json_bytes(env, &results)
    })
}

/// `{"id","fieldNames","css","isCloze","sortFieldIdx","templates":[{"ord","name","qfmt","afmt"}]}`.
#[no_mangle]
pub extern "system" fn Java_com_guruowl_android_core_ankibridge_NativeBridge_nativeNotetypeJson<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
    notetype_id: jlong,
) -> jbyteArray {
    entry(&mut env, "nativeNotetypeJson", std::ptr::null_mut(), |env| {
        let pkg = package(handle)?;
        let schema = notetype(&mut *lock(pkg, "flashify_notetype_schema")?, notetype_id as u64)?;
        json_bytes(env, &schema)
    })
}

/// `flashify_render_fields`: `{"cards":[{"ord","question","answer"}],"css"}`.
#[no_mangle]
pub extern "system" fn Java_com_guruowl_android_core_ankibridge_NativeBridge_nativeRenderFieldsJson<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    notetype_json: JString<'local>,
    field_values_json: JString<'local>,
) -> jbyteArray {
    entry(&mut env, "nativeRenderFieldsJson", std::ptr::null_mut(), |env| {
        let nt = read_string(env, &notetype_json, "notetypeJson")?;
        let values = read_string(env, &field_values_json, "fieldValuesJson")?;
        let rendered = render_fields(&nt, &values)?;
        json_bytes(env, &rendered)
    })
}

/// Diagnostic: `{"decks","notes","cards","notetypes","mismatches":[..]}` comparing
/// this layer with the C ABI over one package. Empty `mismatches` = identical.
#[no_mangle]
pub extern "system" fn Java_com_guruowl_android_core_ankibridge_NativeBridge_nativeCompareWithCAbi<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
) -> jbyteArray {
    entry(&mut env, "nativeCompareWithCAbi", std::ptr::null_mut(), |env| {
        let report = compare_with_c_abi(handle)?;
        json_bytes(env, &report)
    })
}

/// Test hook: panics inside an entry point, to prove panics become exceptions.
#[no_mangle]
pub extern "system" fn Java_com_guruowl_android_core_ankibridge_NativeBridge_nativePanicForTest<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
) {
    entry(&mut env, "nativePanicForTest", (), |_| -> BridgeResult<()> {
        panic!("deliberate panic for the JNI panic test")
    })
}
