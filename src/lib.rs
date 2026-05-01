//! RustImportCore — Anki package parsing FFI shim.
//!
//! PR 3b: real `flashify_open_package` backed by rslib. The remaining four
//! enumeration / rendering FFIs are still stubs but now sit on top of a
//! living `anki::collection::Collection`, so each can be filled in
//! independently in follow-up commits without re-touching the FFI surface.
//!
//! Memory ownership rules (frozen now so they don't drift later):
//! - All pointers returned by `flashify_*` allocator functions are heap-
//!   allocated by Rust. Swift MUST call the matching `flashify_free_*`
//!   function to release them; never `free()` from Swift.
//! - `*const c_char` returned from accessors is borrowed from the parent
//!   handle; Swift must copy the bytes before the parent handle is freed.
//! - Errors are signalled by NULL returns or non-zero status codes; the
//!   caller can fetch the last error string with `flashify_last_error_message`.

#![allow(clippy::missing_safety_doc)]

use anki::card::CardId;
use anki::collection::Collection;
use anki::collection::CollectionBuilder;
use anki::decks::DeckId;
use anki::notes::NoteId;
use anki::search::SearchNode;
use anki::version;
use anki_proto::import_export::ImportAnkiPackageOptions;
use fsrs::FSRS;
use serde::Deserialize;

use std::ffi::{c_char, CStr, CString};
use std::os::raw::c_int;
use std::path::{Path, PathBuf};
use std::ptr;
use std::sync::Mutex;

// ---------------------------------------------------------------------------
// Error storage. Uses a global mutex; the bridge actor on the Swift side
// already serialises calls per-package, so contention is essentially zero.
// ---------------------------------------------------------------------------

static LAST_ERROR: Mutex<Option<CString>> = Mutex::new(None);

fn set_last_error(msg: impl AsRef<str>) {
    if let Ok(mut slot) = LAST_ERROR.lock() {
        *slot = CString::new(msg.as_ref()).ok();
    }
}

fn set_last_error_from<E: std::fmt::Display>(label: &str, err: E) {
    set_last_error(format!("{label}: {err}"));
}

#[no_mangle]
pub unsafe extern "C" fn flashify_last_error_message() -> *const c_char {
    match LAST_ERROR.lock() {
        Ok(slot) => slot.as_ref().map_or(ptr::null(), |s| s.as_ptr()),
        Err(_) => ptr::null(),
    }
}

// ---------------------------------------------------------------------------
// Opaque handles.
// ---------------------------------------------------------------------------

/// An opened Anki package backed by a temporary `Collection` populated
/// via `Collection::import_apkg`. The `Mutex` guards `&mut Collection`
/// access; rslib's APIs require `&mut self` even for many reads.
pub struct OpenedPackage {
    /// Collection populated from the imported apkg.
    pub(crate) collection: Mutex<Collection>,
    /// Working directory created under workspace_path; deleted by
    /// `flashify_free_package` if `owns_workspace` is set.
    pub(crate) work_dir: PathBuf,
    /// Whether `work_dir` was created by us (and should be deleted on close).
    pub(crate) owns_work_dir: bool,
}

impl Drop for OpenedPackage {
    fn drop(&mut self) {
        if self.owns_work_dir {
            let _ = std::fs::remove_dir_all(&self.work_dir);
        }
    }
}

/// Snapshot of the decks inside an `OpenedPackage`. Built up-front by
/// `flashify_enumerate_decks` so the indexed accessors (`flashify_decks_*_at`)
/// can serve every read without re-locking the Collection. Names are the
/// fully-qualified hierarchical strings rslib returns (e.g. `Parent::Child`);
/// Swift can split on `::` if it wants a tree.
pub struct DeckList {
    pub(crate) ids: Vec<u64>,
    pub(crate) names: Vec<CString>,
}

/// One row inside a `NoteList`. Strings are owned `CString`s so the
/// indexed accessor pointers stay valid until the list is freed.
pub(crate) struct NoteRow {
    pub(crate) id: u64,
    pub(crate) notetype_id: u64,
    pub(crate) fields: Vec<CString>,
    pub(crate) tags: Vec<CString>,
}

/// Snapshot of notes inside a deck (or all notes if the caller passed
/// `deck_id = 0`). Materialised eagerly: enumerate_notes locks the
/// Collection once, fetches every note, and converts to row-oriented
/// `CString`s so the FFI accessors never re-lock or re-allocate.
pub struct NoteList {
    pub(crate) rows: Vec<NoteRow>,
}

pub struct RenderedCard {
    pub _question_html: CString,
    pub _answer_html: CString,
}

#[repr(C)]
pub struct FsrsState {
    /// Card scheduler state mapped onto FSRS. -1 means "no scheduling info"
    /// (caller should treat the card as new).
    pub stability: f32,
    pub difficulty: f32,
    pub due_days_from_today: i32,
    pub elapsed_days: i32,
    pub reps: u32,
    pub lapses: u32,
}

impl FsrsState {
    fn empty() -> Self {
        Self {
            stability: -1.0,
            difficulty: -1.0,
            due_days_from_today: 0,
            elapsed_days: 0,
            reps: 0,
            lapses: 0,
        }
    }
}

// ---------------------------------------------------------------------------
// Versioning so Swift can refuse a mismatched xcframework at runtime.
// ---------------------------------------------------------------------------

const ABI_VERSION: u32 = 1;

#[no_mangle]
pub extern "C" fn flashify_abi_version() -> u32 {
    ABI_VERSION
}

/// Smoke-test helper: returns rslib's own version string. The Swift bridge
/// can call this to confirm the static library actually contains rslib code
/// (rather than just the FFI shell).
#[no_mangle]
pub extern "C" fn flashify_anki_version() -> *const c_char {
    static ANKI_VERSION: Mutex<Option<CString>> = Mutex::new(None);
    let mut slot = match ANKI_VERSION.lock() {
        Ok(slot) => slot,
        Err(_) => return ptr::null(),
    };
    if slot.is_none() {
        *slot = CString::new(version::version()).ok();
    }
    slot.as_ref().map_or(ptr::null(), |s| s.as_ptr())
}

// ---------------------------------------------------------------------------
// Internal helpers.
// ---------------------------------------------------------------------------

unsafe fn cstr_to_str<'a>(ptr: *const c_char) -> Option<&'a str> {
    if ptr.is_null() {
        return None;
    }
    CStr::from_ptr(ptr).to_str().ok()
}

/// Set up a fresh writable working directory under `workspace_path` for
/// the import. Returns `(work_dir, collection_path, media_folder, media_db)`.
fn prepare_workspace(workspace_root: &Path) -> std::io::Result<(PathBuf, PathBuf, PathBuf, PathBuf)> {
    // Subdir keyed by PID + timestamp so concurrent imports don't collide.
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    let work_dir = workspace_root.join(format!("rust-import-{}-{stamp}", std::process::id()));
    std::fs::create_dir_all(&work_dir)?;

    let media_folder = work_dir.join("collection.media");
    std::fs::create_dir_all(&media_folder)?;

    let collection_path = work_dir.join("collection.anki2");
    let media_db = work_dir.join("collection.media.db");
    Ok((work_dir, collection_path, media_folder, media_db))
}

// ---------------------------------------------------------------------------
// Core API.
// ---------------------------------------------------------------------------

/// Open an Anki package (.apkg or .colpkg) and prepare it for enumeration.
///
/// `workspace_path` is a writable directory under which we create a
/// per-import subdirectory containing a fresh SQLite collection that
/// rslib populates from the package. The subdirectory is deleted when
/// `flashify_free_package` is called.
///
/// Returns NULL on failure; check `flashify_last_error_message`.
#[no_mangle]
pub unsafe extern "C" fn flashify_open_package(
    package_path: *const c_char,
    workspace_path: *const c_char,
) -> *mut OpenedPackage {
    let package_str = match cstr_to_str(package_path) {
        Some(s) => s,
        None => {
            set_last_error("flashify_open_package: invalid package_path");
            return ptr::null_mut();
        }
    };
    let workspace_str = match cstr_to_str(workspace_path) {
        Some(s) => s,
        None => {
            set_last_error("flashify_open_package: invalid workspace_path");
            return ptr::null_mut();
        }
    };

    let (work_dir, col_path, media_folder, media_db) =
        match prepare_workspace(Path::new(workspace_str)) {
            Ok(paths) => paths,
            Err(e) => {
                set_last_error_from("flashify_open_package: prepare workspace", e);
                return ptr::null_mut();
            }
        };

    let mut col = match CollectionBuilder::new(&col_path)
        .set_media_paths(media_folder, media_db)
        .build()
    {
        Ok(col) => col,
        Err(e) => {
            set_last_error_from("flashify_open_package: build collection", e);
            let _ = std::fs::remove_dir_all(&work_dir);
            return ptr::null_mut();
        }
    };

    // Default options preserve scheduling and merge notetypes by id.
    let options = ImportAnkiPackageOptions {
        with_scheduling: true,
        with_deck_configs: true,
        merge_notetypes: true,
        ..Default::default()
    };

    if let Err(e) = col.import_apkg(package_str, options) {
        set_last_error_from("flashify_open_package: import_apkg", e);
        let _ = std::fs::remove_dir_all(&work_dir);
        return ptr::null_mut();
    }

    Box::into_raw(Box::new(OpenedPackage {
        collection: Mutex::new(col),
        work_dir,
        owns_work_dir: true,
    }))
}

#[no_mangle]
pub unsafe extern "C" fn flashify_free_package(pkg: *mut OpenedPackage) {
    if !pkg.is_null() {
        // Drop runs cleanup of work_dir.
        drop(Box::from_raw(pkg));
    }
}

// ---------------------------------------------------------------------------
// Stub enumerators / renderer. Each will be implemented in a follow-up
// commit using the now-living `Collection` inside OpenedPackage.
// ---------------------------------------------------------------------------

/// Enumerate decks inside the opened package. Returns NULL on failure;
/// check `flashify_last_error_message`. The default deck (id=1) is omitted
/// since import_apkg leaves it empty.
#[no_mangle]
pub unsafe extern "C" fn flashify_enumerate_decks(pkg: *mut OpenedPackage) -> *mut DeckList {
    if pkg.is_null() {
        set_last_error("flashify_enumerate_decks: null pkg");
        return ptr::null_mut();
    }
    let pkg_ref = &*pkg;
    let col = match pkg_ref.collection.lock() {
        Ok(g) => g,
        Err(_) => {
            set_last_error("flashify_enumerate_decks: collection mutex poisoned");
            return ptr::null_mut();
        }
    };
    let pairs = match col.get_all_deck_names(/* skip_default */ true) {
        Ok(p) => p,
        Err(e) => {
            set_last_error_from("flashify_enumerate_decks", e);
            return ptr::null_mut();
        }
    };

    let mut ids: Vec<u64> = Vec::with_capacity(pairs.len());
    let mut names: Vec<CString> = Vec::with_capacity(pairs.len());
    for (DeckId(raw_id), name) in pairs {
        ids.push(raw_id as u64);
        // CString::new fails if the name contains an interior NUL. Decks
        // can't really have those but be defensive — replace rather than
        // skip the row, since indices are tied to ids.
        let cstring = CString::new(name.replace('\0', "")).unwrap_or_default();
        names.push(cstring);
    }
    Box::into_raw(Box::new(DeckList { ids, names }))
}

#[no_mangle]
pub unsafe extern "C" fn flashify_free_decks(decks: *mut DeckList) {
    if !decks.is_null() {
        drop(Box::from_raw(decks));
    }
}

/// Number of decks in the list, or -1 if `list` is null.
#[no_mangle]
pub unsafe extern "C" fn flashify_decks_count(list: *const DeckList) -> i64 {
    if list.is_null() {
        return -1;
    }
    (*list).ids.len() as i64
}

/// Deck id at the given index, or 0 if out of range / null.
#[no_mangle]
pub unsafe extern "C" fn flashify_decks_id_at(list: *const DeckList, idx: u64) -> u64 {
    if list.is_null() {
        return 0;
    }
    let list = &*list;
    list.ids.get(idx as usize).copied().unwrap_or(0)
}

/// Deck name at the given index. Pointer is valid until `flashify_free_decks`
/// is called on the parent list. Returns NULL if out of range / null.
#[no_mangle]
pub unsafe extern "C" fn flashify_decks_name_at(list: *const DeckList, idx: u64) -> *const c_char {
    if list.is_null() {
        return ptr::null();
    }
    let list = &*list;
    list.names.get(idx as usize).map_or(ptr::null(), |s| s.as_ptr())
}

/// Enumerate notes for a specific deck. Pass `deck_id = 0` for all notes
/// regardless of deck. Returns NULL on failure; check
/// `flashify_last_error_message`.
///
/// Order: by note id ascending. rslib's underlying calls return either
/// a HashSet (no order) or a search result (search-mode order); we
/// sort here so callers get a stable iteration order across runs.
#[no_mangle]
pub unsafe extern "C" fn flashify_enumerate_notes(
    pkg: *mut OpenedPackage,
    deck_id: u64,
) -> *mut NoteList {
    if pkg.is_null() {
        set_last_error("flashify_enumerate_notes: null pkg");
        return ptr::null_mut();
    }
    let pkg_ref = &*pkg;
    let mut col = match pkg_ref.collection.lock() {
        Ok(g) => g,
        Err(_) => {
            set_last_error("flashify_enumerate_notes: collection mutex poisoned");
            return ptr::null_mut();
        }
    };

    let mut note_ids: Vec<NoteId> = if deck_id == 0 {
        match col.storage.get_all_note_ids() {
            Ok(set) => set.into_iter().collect(),
            Err(e) => {
                set_last_error_from("flashify_enumerate_notes: get_all_note_ids", e);
                return ptr::null_mut();
            }
        }
    } else {
        let did = DeckId(deck_id as i64);
        match col.search_notes_unordered(SearchNode::DeckIdWithChildren(did)) {
            Ok(ids) => ids,
            Err(e) => {
                set_last_error_from("flashify_enumerate_notes: search_notes_unordered", e);
                return ptr::null_mut();
            }
        }
    };
    note_ids.sort_unstable_by_key(|nid| nid.0);

    let mut rows: Vec<NoteRow> = Vec::with_capacity(note_ids.len());
    for nid in note_ids {
        let note = match col.storage.get_note(nid) {
            Ok(Some(n)) => n,
            Ok(None) => continue, // gone between the search and the fetch — silently skip
            Err(e) => {
                set_last_error_from("flashify_enumerate_notes: get_note", e);
                return ptr::null_mut();
            }
        };
        let fields: Vec<CString> = note
            .fields()
            .iter()
            .map(|f| CString::new(f.replace('\0', "")).unwrap_or_default())
            .collect();
        let tags: Vec<CString> = note
            .tags
            .iter()
            .map(|t| CString::new(t.replace('\0', "")).unwrap_or_default())
            .collect();
        rows.push(NoteRow {
            id: note.id.0 as u64,
            notetype_id: note.notetype_id.0 as u64,
            fields,
            tags,
        });
    }

    Box::into_raw(Box::new(NoteList { rows }))
}

#[no_mangle]
pub unsafe extern "C" fn flashify_free_notes(notes: *mut NoteList) {
    if !notes.is_null() {
        drop(Box::from_raw(notes));
    }
}

/// Number of notes in the list, or -1 if `list` is null.
#[no_mangle]
pub unsafe extern "C" fn flashify_notes_count(list: *const NoteList) -> i64 {
    if list.is_null() {
        return -1;
    }
    let list = &*list;
    list.rows.len() as i64
}

/// Note id at the given index, or 0 if out of range / null.
#[no_mangle]
pub unsafe extern "C" fn flashify_notes_id_at(list: *const NoteList, idx: u64) -> u64 {
    if list.is_null() {
        return 0;
    }
    let list = &*list;
    list.rows.get(idx as usize).map_or(0, |r| r.id)
}

/// Notetype (model) id at the given index.
#[no_mangle]
pub unsafe extern "C" fn flashify_notes_notetype_id_at(list: *const NoteList, idx: u64) -> u64 {
    if list.is_null() {
        return 0;
    }
    let list = &*list;
    list.rows.get(idx as usize).map_or(0, |r| r.notetype_id)
}

/// Number of fields on the note at the given index, -1 on null/oob.
#[no_mangle]
pub unsafe extern "C" fn flashify_notes_field_count_at(list: *const NoteList, idx: u64) -> i64 {
    if list.is_null() {
        return -1;
    }
    let list = &*list;
    list.rows.get(idx as usize).map_or(-1, |r| r.fields.len() as i64)
}

/// Field text at (note index, field index). Pointer valid until
/// `flashify_free_notes`. NULL on null/oob.
#[no_mangle]
pub unsafe extern "C" fn flashify_notes_field_at(
    list: *const NoteList,
    idx: u64,
    field_idx: u64,
) -> *const c_char {
    if list.is_null() {
        return ptr::null();
    }
    let list = &*list;
    match list.rows.get(idx as usize) {
        Some(row) => row
            .fields
            .get(field_idx as usize)
            .map_or(ptr::null(), |s| s.as_ptr()),
        None => ptr::null(),
    }
}

/// Number of tags on the note at the given index, -1 on null/oob.
#[no_mangle]
pub unsafe extern "C" fn flashify_notes_tag_count_at(list: *const NoteList, idx: u64) -> i64 {
    if list.is_null() {
        return -1;
    }
    let list = &*list;
    list.rows.get(idx as usize).map_or(-1, |r| r.tags.len() as i64)
}

/// Tag at (note index, tag index). Pointer valid until
/// `flashify_free_notes`. NULL on null/oob.
#[no_mangle]
pub unsafe extern "C" fn flashify_notes_tag_at(
    list: *const NoteList,
    idx: u64,
    tag_idx: u64,
) -> *const c_char {
    if list.is_null() {
        return ptr::null();
    }
    let list = &*list;
    match list.rows.get(idx as usize) {
        Some(row) => row
            .tags
            .get(tag_idx as usize)
            .map_or(ptr::null(), |s| s.as_ptr()),
        None => ptr::null(),
    }
}

/// Subset of the JSON serialised in the `cards.data` column. We only
/// pull out the fields we need (FSRS state + last review time);
/// `#[serde(default)]` keeps deserialisation tolerant when columns are
/// missing or the file is from an older Anki version.
#[derive(Debug, Default, Deserialize)]
struct CardDataBlob {
    #[serde(rename = "s", default)]
    fsrs_stability: Option<f32>,
    #[serde(rename = "d", default)]
    fsrs_difficulty: Option<f32>,
    #[serde(rename = "lrt", default)]
    last_review_time_secs: Option<i64>,
}

fn parse_card_data(raw: &str) -> CardDataBlob {
    serde_json::from_str(raw).unwrap_or_default()
}

/// Convert SM-2 (ease, interval) into FSRS memory state with default
/// parameters. `FSRS` itself is `!Sync` (it owns ndarray-backed model
/// state), so we hold it behind a Mutex and lazy-construct on first
/// use; the Swift bridge actor already serialises calls per package
/// so contention is essentially zero.
fn sm2_to_fsrs(ease_factor: f32, interval_days: f32) -> Option<(f32, f32)> {
    static INSTANCE: Mutex<Option<FSRS>> = Mutex::new(None);
    let mut slot = INSTANCE.lock().ok()?;
    if slot.is_none() {
        *slot = FSRS::new(None).ok();
    }
    let fsrs = slot.as_ref()?;
    let state = fsrs
        .memory_state_from_sm2(ease_factor, interval_days, /* sm2_retention */ 0.9)
        .ok()?;
    Some((state.stability, state.difficulty))
}

/// Map an Anki card's scheduler state onto FSRS.
///
/// Three paths, in priority order:
/// 1. Card has FSRS-native state in cards.data (`s`/`d` columns set):
///    use those directly.
/// 2. Card has SM-2 state (interval > 0, ctype != New): convert via
///    `fsrs::FSRS::memory_state_from_sm2` with default parameters and
///    historical_retention = 0.9 (Anki's default before per-deck override).
/// 3. Card is new / no scheduling history: return `FsrsState::empty()`
///    (stability = -1 sentinel) so the caller treats it as a fresh card.
///
/// `due_days_from_today` is computed from the cards.queue + cards.due
/// row interpretation: Review/DayLearn store days-since-creation,
/// Learn stores a unix timestamp. `elapsed_days` is derived from
/// `last_review_time` if known; 0 otherwise.
#[no_mangle]
pub unsafe extern "C" fn flashify_card_state_as_fsrs(
    pkg: *mut OpenedPackage,
    card_id: u64,
) -> FsrsState {
    if pkg.is_null() {
        set_last_error("flashify_card_state_as_fsrs: null pkg");
        return FsrsState::empty();
    }
    let pkg_ref = &*pkg;
    let mut col = match pkg_ref.collection.lock() {
        Ok(g) => g,
        Err(_) => {
            set_last_error("flashify_card_state_as_fsrs: collection mutex poisoned");
            return FsrsState::empty();
        }
    };

    // We use direct SQL because Card's scheduling fields are
    // pub(crate) inside rslib — not reachable from our shim. The cards
    // table schema has been stable for years and this query is read-only.
    let row = col.storage.db().query_row::<(i64, i64, i32, i64, i64, i64, i64, String), _, _>(
        "SELECT queue, type, due, ivl, factor, reps, lapses, COALESCE(data, '') \
         FROM cards WHERE id = ?",
        [card_id as i64],
        |row| {
            Ok((
                row.get::<_, i64>(0)?,    // queue
                row.get::<_, i64>(1)?,    // type / ctype
                row.get::<_, i32>(2)?,    // due
                row.get::<_, i64>(3)?,    // ivl
                row.get::<_, i64>(4)?,    // factor
                row.get::<_, i64>(5)?,    // reps
                row.get::<_, i64>(6)?,    // lapses
                row.get::<_, String>(7)?, // data
            ))
        },
    );

    let (queue, ctype, due, ivl, factor, reps, lapses, data_json) = match row {
        Ok(r) => r,
        Err(e) => {
            set_last_error_from("flashify_card_state_as_fsrs: get card row", e);
            return FsrsState::empty();
        }
    };

    let blob = parse_card_data(&data_json);

    // Compute days-since-collection-creation for "today".
    let timing = match col.timing_today() {
        Ok(t) => t,
        Err(e) => {
            set_last_error_from("flashify_card_state_as_fsrs: timing_today", e);
            return FsrsState::empty();
        }
    };
    let today: i32 = timing.days_elapsed as i32;
    let now_secs: i64 = timing.now.0;

    let due_days_from_today: i32 = match queue {
        // Review (2) and DayLearn (3): due is days-since-creation
        2 | 3 => due - today,
        // Learn (1): due is unix timestamp; convert to days-from-today
        1 => {
            let delta = (due as i64) - now_secs;
            // Round towards zero — Anki shows learning steps in minutes
            (delta / 86_400) as i32
        }
        // New, suspended, buried: not really due
        _ => 0,
    };

    // Resolve memory_state via the priority order described above.
    let (stability, difficulty) = if let (Some(s), Some(d)) =
        (blob.fsrs_stability, blob.fsrs_difficulty)
    {
        (s, d)
    } else if ctype != 0 /* New */ && ivl > 0 {
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
        Some(lrt) => {
            let delta = now_secs - lrt;
            ((delta.max(0)) / 86_400) as i32
        }
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

/// Render a note template (question + answer HTML) for the card at
/// `(note_id, ord)`. Returns NULL on failure; check
/// `flashify_last_error_message`. The returned handle owns the strings
/// and must be released with `flashify_free_rendered`.
///
/// Internally we look up the card id via direct SQL (the rslib
/// equivalent `storage.get_card_by_ordinal` is `pub(crate)`), then call
/// `render_existing_card(cid, browser=false, partial_render=false)` so
/// `RenderCardOutput::question()` / `answer()` return fully-rendered
/// HTML.
#[no_mangle]
pub unsafe extern "C" fn flashify_render_template(
    pkg: *mut OpenedPackage,
    note_id: u64,
    ord: u32,
) -> *mut RenderedCard {
    if pkg.is_null() {
        set_last_error("flashify_render_template: null pkg");
        return ptr::null_mut();
    }
    let pkg_ref = &*pkg;
    let mut col = match pkg_ref.collection.lock() {
        Ok(g) => g,
        Err(_) => {
            set_last_error("flashify_render_template: collection mutex poisoned");
            return ptr::null_mut();
        }
    };

    let card_id_raw: i64 = match col.storage.db().query_row::<i64, _, _>(
        "SELECT id FROM cards WHERE nid = ? AND ord = ?",
        rusqlite::params![note_id as i64, ord as i64],
        |row| row.get(0),
    ) {
        Ok(v) => v,
        Err(rusqlite::Error::QueryReturnedNoRows) => {
            set_last_error(format!(
                "flashify_render_template: no card for note {note_id} ord {ord}"
            ));
            return ptr::null_mut();
        }
        Err(e) => {
            set_last_error_from("flashify_render_template: lookup card id", e);
            return ptr::null_mut();
        }
    };

    let output = match col.render_existing_card(
        CardId(card_id_raw),
        /* browser */ false,
        /* partial_render */ false,
    ) {
        Ok(o) => o,
        Err(e) => {
            set_last_error_from("flashify_render_template: render_existing_card", e);
            return ptr::null_mut();
        }
    };

    let question = output.question().into_owned();
    let answer = output.answer().into_owned();
    let question_html = CString::new(question.replace('\0', "")).unwrap_or_default();
    let answer_html = CString::new(answer.replace('\0', "")).unwrap_or_default();
    Box::into_raw(Box::new(RenderedCard {
        _question_html: question_html,
        _answer_html: answer_html,
    }))
}

#[no_mangle]
pub unsafe extern "C" fn flashify_free_rendered(card: *mut RenderedCard) {
    if !card.is_null() {
        drop(Box::from_raw(card));
    }
}

/// Question HTML pointer. Valid until `flashify_free_rendered`. NULL if `card` is null.
#[no_mangle]
pub unsafe extern "C" fn flashify_rendered_question(card: *const RenderedCard) -> *const c_char {
    if card.is_null() {
        return ptr::null();
    }
    let card = &*card;
    card._question_html.as_ptr()
}

/// Answer HTML pointer. Valid until `flashify_free_rendered`. NULL if `card` is null.
#[no_mangle]
pub unsafe extern "C" fn flashify_rendered_answer(card: *const RenderedCard) -> *const c_char {
    if card.is_null() {
        return ptr::null();
    }
    let card = &*card;
    card._answer_html.as_ptr()
}

/// Status code for void operations. Zero = OK, non-zero = error (check
/// `flashify_last_error_message`).
#[no_mangle]
pub extern "C" fn flashify_self_test() -> c_int {
    0
}
