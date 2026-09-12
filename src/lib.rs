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
use anki::notetype::CardTemplate;
use anki::notetype::NoteField;
use anki::notetype::Notetype;
use anki::notetype::NotetypeConfig;
use anki::notetype::NotetypeId;
use anki::notetype::NotetypeKind;
use anki::prelude::TimestampSecs;
use anki::prelude::Usn;
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
    /// `<work_dir>/collection.media`. Stored alongside as a CString so
    /// `flashify_media_dir` can hand back a stable pointer.
    pub(crate) media_dir_cstring: CString,
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
    /// Anki's sort field for this note (the notetype's `sort_field_idx`-th
    /// field, HTML-stripped) — the deck author's designated identifying text,
    /// i.e. what Anki's own Browser shows. Lets a caller render a
    /// structure-agnostic list-row preview instead of parsing the card's
    /// rendered template HTML (which can lead with tag/CSS/JS chrome).
    pub(crate) sort_field: CString,
}

/// Snapshot of notes inside a deck (or all notes if the caller passed
/// `deck_id = 0`). Materialised eagerly: enumerate_notes locks the
/// Collection once, fetches every note, and converts to row-oriented
/// `CString`s so the FFI accessors never re-lock or re-allocate.
pub struct NoteList {
    pub(crate) rows: Vec<NoteRow>,
}

/// `(card_id, ord)` pairs for a single note. Anki notetypes can produce
/// multiple cards per note: Basic+Reverse has 2 templates, Cloze has one
/// template but generates one card per `{{c1}}/{{c2}}/...` deletion in
/// the source field. Returned by `flashify_enumerate_cards_for_note` so
/// the caller can iterate every card without going through the notetype
/// machinery.
pub struct CardOrdList {
    pub(crate) rows: Vec<(u64, u32)>,
}

pub struct RenderedCard {
    pub _question_html: CString,
    pub _answer_html: CString,
    /// Notetype CSS as authored in Anki's card-template editor. Required
    /// for visually-correct rendering — without it cards lose all
    /// notetype branding (Anking's logo styling, AnKing-clone hidden
    /// state markers, the dot-leader spacing on cloze cards, etc.).
    pub _css: CString,
}

/// Snapshot of a notetype's *editing* schema: the ordered field names plus
/// every card template's `(ord, name, qfmt, afmt)` and the shared CSS.
/// `flashify_render_template` already returns *rendered* HTML for review; this
/// exposes the *source* schema so the iOS importer can persist a
/// `CardModelObject` and re-render a note after the user edits its structured
/// fields — through the exact templates Anki authored, keyed by real field
/// names instead of positional `field_0` placeholders.
///
/// Strings are owned `CString`s so the indexed accessor pointers stay valid
/// until `flashify_free_notetype_schema`. The four per-template vectors are
/// parallel (index `i` describes one template); kept column-oriented to mirror
/// the existing list accessors rather than introduce a nested handle.
pub struct NotetypeSchema {
    pub(crate) field_names: Vec<CString>,
    pub(crate) css: CString,
    /// Whether this is a cloze notetype — the iOS side stores it so
    /// `flashify_render_fields` re-creates the notetype with the right kind
    /// (cloze = one card per `{{c1::}}` deletion, single template).
    pub(crate) is_cloze: bool,
    /// The notetype's sort-field index (which field Anki titles the row by).
    pub(crate) sort_field_idx: u32,
    pub(crate) template_ords: Vec<u32>,
    pub(crate) template_names: Vec<CString>,
    pub(crate) template_qfmts: Vec<CString>,
    pub(crate) template_afmts: Vec<CString>,
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

// v2: added flashify_notes_sort_field_at (Anki sort field per note).
// v3: added the flashify_notetype_schema family (notetype field names +
//     per-template qfmt/afmt + css) so the iOS importer can persist a
//     re-renderable CardModelObject and edit a note's structured fields
//     instead of being stuck with positional `field_0` keys.
// v4: schema also exposes is_cloze + sort_field_idx; added the
//     flashify_render_fields family — render a note from a notetype definition
//     + field values through the real Anki engine (a throwaway in-memory
//     collection), so an edited card re-renders with full template support
//     (conditionals, filters, cloze) instead of the iOS half-renderer.
// v5: owned per-card source history JSON, including queue and raw revlog.
const ABI_VERSION: u32 = 5;

#[no_mangle]
pub extern "C" fn flashify_abi_version() -> u32 {
    ABI_VERSION
}

/// ABI v5: source scheduling and every revlog field, scoped to ONE card.
/// Query under the collection mutex: rslib uses exclusive SQLite locking,
/// so a separate Swift SQLite connection cannot safely read this collection.
/// Owns the returned CString; caller must copy then free it with
/// flashify_free_import_history_json (never libc free). NULL means failure,
/// distinct from a valid card with an empty reviews array.
#[no_mangle]
pub unsafe extern "C" fn flashify_card_import_history_json(
    pkg: *mut OpenedPackage,
    card_id: u64,
) -> *mut c_char {
    if pkg.is_null() || card_id > i64::MAX as u64 {
        set_last_error("flashify_card_import_history_json: invalid package or card id");
        return ptr::null_mut();
    }
    let col = match (*pkg).collection.lock() {
        Ok(col) => col,
        Err(_) => {
            set_last_error("flashify_card_import_history_json: poisoned mutex");
            return ptr::null_mut();
        }
    };
    match read_import_history(col.storage.db(), card_id as i64) {
        Ok(value) => match CString::new(value.to_string()) {
            Ok(json) => json.into_raw(),
            Err(e) => {
                set_last_error_from("flashify_card_import_history_json: encode", e);
                ptr::null_mut()
            }
        },
        Err(e) => {
            set_last_error_from("flashify_card_import_history_json: query", e);
            ptr::null_mut()
        }
    }
}

#[no_mangle]
pub unsafe extern "C" fn flashify_free_import_history_json(json: *mut c_char) {
    if !json.is_null() {
        drop(CString::from_raw(json));
    }
}

fn read_import_history(
    db: &rusqlite::Connection,
    card_id: i64,
) -> rusqlite::Result<serde_json::Value> {
    let card = db.query_row(
        "SELECT nid,ord,type,queue,due,ivl,reps,lapses FROM cards WHERE id=?",
        [card_id],
        |row| {
            (0..8)
                .map(|i| row.get::<_, i64>(i))
                .collect::<rusqlite::Result<Vec<_>>>()
        },
    )?;
    let created: i64 = db.query_row("SELECT crt FROM col LIMIT 1", [], |row| row.get(0))?;
    let mut query = db.prepare_cached(
        "SELECT id,cid,usn,ease,ivl,lastIvl,factor,time,type FROM revlog WHERE cid=? ORDER BY id",
    )?;
    let reviews = query
        .query_map([card_id], |row| {
            let v = (0..9)
                .map(|i| row.get::<_, i64>(i))
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(serde_json::json!({
                "id": v[0], "cardId": v[1], "usn": v[2], "ease": v[3],
                "interval": v[4], "lastInterval": v[5], "factor": v[6], "time": v[7], "type": v[8]
            }))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64();
    Ok(serde_json::json!({
        "cardId": card_id, "noteId": card[0], "ord": card[1], "type": card[2],
        "queue": card[3], "due": card[4], "interval": card[5], "reps": card[6], "lapses": card[7],
        "collectionCreatedAt": created, "importedAt": now, "reviews": reviews
    }))
}

#[cfg(test)]
mod import_history_tests {
    use super::*;

    #[test]
    fn history_is_scoped_by_card_and_retains_suspension_and_raw_manual_rows() {
        let db = rusqlite::Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE col(crt); INSERT INTO col VALUES(1700000000);
            CREATE TABLE cards(id,nid,ord,type,queue,due,ivl,reps,lapses);
            INSERT INTO cards VALUES(101,100,0,2,-1,3,7,1,0),(102,100,1,1,1,1800000060,0,2,1),(103,100,2,0,0,1,0,0,0);
            CREATE TABLE revlog(id,cid,usn,ease,ivl,lastIvl,factor,time,type);
            INSERT INTO revlog VALUES(1700000000001,101,-1,3,-60,0,2500,2345,0),
              (1700000000002,101,-1,0,7,-60,0,0,4),(1700000000003,102,-1,4,7,-60,2500,2000,1);").unwrap();
        let first = read_import_history(&db, 101).unwrap();
        assert_eq!(first["queue"], -1);
        assert_eq!(first["ord"], 0);
        assert_eq!(first["reviews"].as_array().unwrap().len(), 2);
        assert_eq!(first["reviews"][0]["id"], 1700000000001_i64);
        assert_eq!(first["reviews"][0]["interval"], -60);
        assert_eq!(first["reviews"][0]["time"], 2345);
        assert_eq!(first["reviews"][1]["type"], 4);
        let second = read_import_history(&db, 102).unwrap();
        assert_eq!(second["queue"], 1);
        assert_eq!(second["due"], 1800000060);
        assert_eq!(second["reviews"].as_array().unwrap().len(), 1);
        assert_eq!(second["reviews"][0]["cardId"], 102);
        assert_eq!(
            read_import_history(&db, 103).unwrap()["reviews"],
            serde_json::json!([])
        );
        assert!(read_import_history(&db, 999).is_err());
    }

    #[test]
    fn null_handle_and_null_free_are_safe() {
        unsafe {
            assert!(flashify_card_import_history_json(ptr::null_mut(), 1).is_null());
            flashify_free_import_history_json(ptr::null_mut());
        }
    }
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

    let media_folder_for_handle = media_folder.clone();
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

    let media_dir_cstring = CString::new(media_folder_for_handle.to_string_lossy().as_bytes())
        .unwrap_or_default();
    Box::into_raw(Box::new(OpenedPackage {
        collection: Mutex::new(col),
        work_dir,
        owns_work_dir: true,
        media_dir_cstring,
    }))
}

/// Absolute path to the directory rslib extracted media files into.
/// Pointer is owned by the package and stays valid until
/// `flashify_free_package`. Returns NULL if `pkg` is null.
#[no_mangle]
pub unsafe extern "C" fn flashify_media_dir(pkg: *const OpenedPackage) -> *const c_char {
    if pkg.is_null() {
        return ptr::null();
    }
    let pkg = &*pkg;
    pkg.media_dir_cstring.as_ptr()
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
/// check `flashify_last_error_message`.
///
/// Filtering: we keep every deck that has at least one card, and drop
/// the rest. We can't simply pass `skip_default: true` to rslib because
/// .apkg packages frequently store their content under deck id 1 (the
/// default) — particularly anything exported from Anki's "Export ->
/// All Decks" or from a single-deck collection. The "deck has cards"
/// check is the right invariant: empty decks aren't useful to surface
/// to the persister, regardless of id.
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
    let pairs = match col.get_all_deck_names(/* skip_default */ false) {
        Ok(p) => p,
        Err(e) => {
            set_last_error_from("flashify_enumerate_decks", e);
            return ptr::null_mut();
        }
    };

    let mut stmt = match col
        .storage
        .db()
        .prepare_cached("SELECT 1 FROM cards WHERE did = ? LIMIT 1")
    {
        Ok(s) => s,
        Err(e) => {
            set_last_error_from("flashify_enumerate_decks: prepare", e);
            return ptr::null_mut();
        }
    };

    let mut ids: Vec<u64> = Vec::with_capacity(pairs.len());
    let mut names: Vec<CString> = Vec::with_capacity(pairs.len());
    for (DeckId(raw_id), name) in pairs {
        let has_cards = stmt
            .query_row(rusqlite::params![raw_id], |_| Ok(()))
            .is_ok();
        if !has_cards {
            continue;
        }
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
        // Anki's sort field = the notetype's `sort_field_idx`-th field, HTML
        // stripped. It is the Browser-facing identifier; capturing it lets the
        // iOS list row show the author-designated text rather than parsing the
        // rendered template (which can lead with tag/CSS/JS chrome). `note` is
        // owned here, so the `&mut col` borrow in `get_notetype` is fine.
        let sort_field = {
            let idx = col
                .get_notetype(note.notetype_id)
                .ok()
                .flatten()
                .map(|nt| nt.config.sort_field_idx as usize)
                .unwrap_or(0);
            let raw = note.fields().get(idx).map(String::as_str).unwrap_or("");
            CString::new(anki::text::strip_html(raw).replace('\0', "")).unwrap_or_default()
        };
        rows.push(NoteRow {
            id: note.id.0 as u64,
            notetype_id: note.notetype_id.0 as u64,
            fields,
            tags,
            sort_field,
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

/// Anki sort-field text at the given note index (HTML-stripped). Pointer is
/// valid until `flashify_free_notes`; NULL on null/oob. This is the value
/// Anki's own Browser shows for the note, so callers should prefer it for
/// list-row previews over parsing the rendered template HTML.
#[no_mangle]
pub unsafe extern "C" fn flashify_notes_sort_field_at(
    list: *const NoteList,
    idx: u64,
) -> *const c_char {
    if list.is_null() {
        return ptr::null();
    }
    let list = &*list;
    list.rows
        .get(idx as usize)
        .map_or(ptr::null(), |r| r.sort_field.as_ptr())
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
///
/// Note: `FSRS::new(None)` constructs an instance with **no** parameters
/// configured, which then panics when memory_state_from_sm2 is called
/// (`fsrs/src/model.rs:281: "command requires parameters to be set on
/// creation"`). Passing `Some(&[])` is the documented way to use
/// `DEFAULT_PARAMETERS` — confirmed in fsrs's own `memory_from_sm2`
/// test in `inference.rs`.
fn sm2_to_fsrs(ease_factor: f32, interval_days: f32) -> Option<(f32, f32)> {
    static INSTANCE: Mutex<Option<FSRS>> = Mutex::new(None);
    let mut slot = INSTANCE.lock().ok()?;
    if slot.is_none() {
        *slot = FSRS::new(Some(&[])).ok();
    }
    let fsrs = slot.as_ref()?;
    let state = fsrs
        .memory_state_from_sm2(ease_factor, interval_days, /* sm2_retention */ 0.9)
        .ok()?;
    Some((state.stability, state.difficulty))
}

/// Enumerate every card belonging to `note_id`, sorted by ord ascending.
/// One card per template ordinal — covers Basic+Reverse (ord 0+1), Cloze
/// (one card per cloze deletion), and any other multi-template notetype.
/// Returns NULL on failure.
#[no_mangle]
pub unsafe extern "C" fn flashify_enumerate_cards_for_note(
    pkg: *mut OpenedPackage,
    note_id: u64,
) -> *mut CardOrdList {
    if pkg.is_null() {
        set_last_error("flashify_enumerate_cards_for_note: null pkg");
        return ptr::null_mut();
    }
    let pkg_ref = &*pkg;
    let col = match pkg_ref.collection.lock() {
        Ok(g) => g,
        Err(_) => {
            set_last_error("flashify_enumerate_cards_for_note: collection mutex poisoned");
            return ptr::null_mut();
        }
    };
    let mut stmt = match col
        .storage
        .db()
        .prepare_cached("SELECT id, ord FROM cards WHERE nid = ? ORDER BY ord")
    {
        Ok(s) => s,
        Err(e) => {
            set_last_error_from("flashify_enumerate_cards_for_note: prepare", e);
            return ptr::null_mut();
        }
    };
    let mapped = match stmt.query_map(rusqlite::params![note_id as i64], |row| {
        let cid: i64 = row.get(0)?;
        let ord: i64 = row.get(1)?;
        Ok((cid as u64, ord as u32))
    }) {
        Ok(iter) => iter,
        Err(e) => {
            set_last_error_from("flashify_enumerate_cards_for_note: query", e);
            return ptr::null_mut();
        }
    };
    let rows: Result<Vec<(u64, u32)>, rusqlite::Error> = mapped.collect();
    let rows = match rows {
        Ok(r) => r,
        Err(e) => {
            set_last_error_from("flashify_enumerate_cards_for_note: collect", e);
            return ptr::null_mut();
        }
    };
    Box::into_raw(Box::new(CardOrdList { rows }))
}

#[no_mangle]
pub unsafe extern "C" fn flashify_free_card_ords(list: *mut CardOrdList) {
    if !list.is_null() {
        drop(Box::from_raw(list));
    }
}

#[no_mangle]
pub unsafe extern "C" fn flashify_card_ords_count(list: *const CardOrdList) -> i64 {
    if list.is_null() {
        return -1;
    }
    let list = &*list;
    list.rows.len() as i64
}

#[no_mangle]
pub unsafe extern "C" fn flashify_card_ords_id_at(list: *const CardOrdList, idx: u64) -> u64 {
    if list.is_null() {
        return 0;
    }
    let list = &*list;
    list.rows.get(idx as usize).map_or(0, |(id, _)| *id)
}

#[no_mangle]
pub unsafe extern "C" fn flashify_card_ords_ord_at(list: *const CardOrdList, idx: u64) -> u32 {
    if list.is_null() {
        return 0;
    }
    let list = &*list;
    list.rows.get(idx as usize).map_or(0, |(_, ord)| *ord)
}

/// Resolve the card id for a given (note_id, ord) pair so the caller
/// can look up scheduler state. Returns 0 if no such card exists.
#[no_mangle]
pub unsafe extern "C" fn flashify_card_id_for(
    pkg: *mut OpenedPackage,
    note_id: u64,
    ord: u32,
) -> u64 {
    if pkg.is_null() {
        return 0;
    }
    let pkg_ref = &*pkg;
    let col = match pkg_ref.collection.lock() {
        Ok(g) => g,
        Err(_) => return 0,
    };
    let id: i64 = col.storage.db().query_row(
        "SELECT id FROM cards WHERE nid = ? AND ord = ?",
        rusqlite::params![note_id as i64, ord as i64],
        |row| row.get(0),
    ).unwrap_or(0);
    id as u64
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
    let css = CString::new(output.css.replace('\0', "")).unwrap_or_default();
    Box::into_raw(Box::new(RenderedCard {
        _question_html: question_html,
        _answer_html: answer_html,
        _css: css,
    }))
}

/// Notetype CSS for the rendered card. Pointer is borrowed from the
/// parent handle and is valid until `flashify_free_rendered`.
#[no_mangle]
pub unsafe extern "C" fn flashify_rendered_css(card: *const RenderedCard) -> *const c_char {
    if card.is_null() {
        return ptr::null();
    }
    let card = &*card;
    card._css.as_ptr()
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

// ---------------------------------------------------------------------------
// Notetype schema (ABI v3). Exposes a notetype's field names + card templates
// so the iOS importer can persist a re-renderable CardModelObject and let the
// user edit a note's structured fields. Built eagerly by
// `flashify_notetype_schema`; the column-oriented accessors below never
// re-lock the Collection.
// ---------------------------------------------------------------------------

/// Fetch the editing schema for a notetype (by id). Returns NULL on failure
/// (unknown id / poisoned mutex); check `flashify_last_error_message`. The
/// returned handle owns its strings and must be released with
/// `flashify_free_notetype_schema`.
#[no_mangle]
pub unsafe extern "C" fn flashify_notetype_schema(
    pkg: *mut OpenedPackage,
    notetype_id: u64,
) -> *mut NotetypeSchema {
    if pkg.is_null() {
        set_last_error("flashify_notetype_schema: null pkg");
        return ptr::null_mut();
    }
    let pkg_ref = &*pkg;
    // get_notetype takes &mut self (it lazily caches), so lock mutably.
    let mut col = match pkg_ref.collection.lock() {
        Ok(g) => g,
        Err(_) => {
            set_last_error("flashify_notetype_schema: collection mutex poisoned");
            return ptr::null_mut();
        }
    };

    let nt = match col.get_notetype(NotetypeId(notetype_id as i64)) {
        Ok(Some(nt)) => nt,
        Ok(None) => {
            set_last_error(format!(
                "flashify_notetype_schema: no notetype with id {notetype_id}"
            ));
            return ptr::null_mut();
        }
        Err(e) => {
            set_last_error_from("flashify_notetype_schema: get_notetype", e);
            return ptr::null_mut();
        }
    };

    // Interior-NUL defence mirrors the note/deck accessors: replace rather than
    // skip, so field/template indices stay aligned with the source order.
    let to_cstring = |s: &str| CString::new(s.replace('\0', "")).unwrap_or_default();

    let field_names: Vec<CString> = nt.fields.iter().map(|f| to_cstring(&f.name)).collect();
    let css = to_cstring(&nt.config.css);

    let mut template_ords = Vec::with_capacity(nt.templates.len());
    let mut template_names = Vec::with_capacity(nt.templates.len());
    let mut template_qfmts = Vec::with_capacity(nt.templates.len());
    let mut template_afmts = Vec::with_capacity(nt.templates.len());
    for (i, t) in nt.templates.iter().enumerate() {
        // `ord` is Option in rslib but always set for stored templates; fall
        // back to the positional index defensively so the vectors stay aligned.
        template_ords.push(t.ord.unwrap_or(i as u32));
        template_names.push(to_cstring(&t.name));
        template_qfmts.push(to_cstring(&t.config.q_format));
        template_afmts.push(to_cstring(&t.config.a_format));
    }

    let is_cloze = nt.config.kind() == NotetypeKind::Cloze;
    let sort_field_idx = nt.config.sort_field_idx;

    Box::into_raw(Box::new(NotetypeSchema {
        field_names,
        css,
        is_cloze,
        sort_field_idx,
        template_ords,
        template_names,
        template_qfmts,
        template_afmts,
    }))
}

#[no_mangle]
pub unsafe extern "C" fn flashify_free_notetype_schema(schema: *mut NotetypeSchema) {
    if !schema.is_null() {
        drop(Box::from_raw(schema));
    }
}

/// Number of fields, or -1 if `schema` is null. The order is the notetype's
/// own field order (index 0 = first field) — the order the `{{FieldName}}`
/// placeholders in the templates resolve against.
#[no_mangle]
pub unsafe extern "C" fn flashify_notetype_field_count(schema: *const NotetypeSchema) -> i64 {
    if schema.is_null() {
        return -1;
    }
    let schema = &*schema;
    schema.field_names.len() as i64
}

/// Field name at `idx`. Pointer valid until `flashify_free_notetype_schema`.
/// NULL on null/oob.
#[no_mangle]
pub unsafe extern "C" fn flashify_notetype_field_name_at(
    schema: *const NotetypeSchema,
    idx: u64,
) -> *const c_char {
    if schema.is_null() {
        return ptr::null();
    }
    let schema = &*schema;
    schema
        .field_names
        .get(idx as usize)
        .map_or(ptr::null(), |s| s.as_ptr())
}

/// Notetype CSS. Pointer valid until `flashify_free_notetype_schema`; never
/// NULL for a non-null schema (empty string when the notetype has no CSS).
#[no_mangle]
pub unsafe extern "C" fn flashify_notetype_css(schema: *const NotetypeSchema) -> *const c_char {
    if schema.is_null() {
        return ptr::null();
    }
    let schema = &*schema;
    schema.css.as_ptr()
}

/// Number of card templates, or -1 if `schema` is null. Basic = 1,
/// Basic+Reverse = 2, Cloze = 1 (every cloze card shares one template).
#[no_mangle]
pub unsafe extern "C" fn flashify_notetype_template_count(schema: *const NotetypeSchema) -> i64 {
    if schema.is_null() {
        return -1;
    }
    let schema = &*schema;
    schema.template_ords.len() as i64
}

/// Template ordinal (Anki's `ord`) at `idx`, or 0 on null/oob.
#[no_mangle]
pub unsafe extern "C" fn flashify_notetype_template_ord_at(
    schema: *const NotetypeSchema,
    idx: u64,
) -> u32 {
    if schema.is_null() {
        return 0;
    }
    let schema = &*schema;
    schema
        .template_ords
        .get(idx as usize)
        .copied()
        .unwrap_or(0)
}

/// Template name at `idx`. Pointer valid until free. NULL on null/oob.
#[no_mangle]
pub unsafe extern "C" fn flashify_notetype_template_name_at(
    schema: *const NotetypeSchema,
    idx: u64,
) -> *const c_char {
    if schema.is_null() {
        return ptr::null();
    }
    let schema = &*schema;
    schema
        .template_names
        .get(idx as usize)
        .map_or(ptr::null(), |s| s.as_ptr())
}

/// Template question format (`qfmt` — the front-side template, with
/// `{{FieldName}}` placeholders) at `idx`. Pointer valid until free. NULL on
/// null/oob.
#[no_mangle]
pub unsafe extern "C" fn flashify_notetype_template_qfmt_at(
    schema: *const NotetypeSchema,
    idx: u64,
) -> *const c_char {
    if schema.is_null() {
        return ptr::null();
    }
    let schema = &*schema;
    schema
        .template_qfmts
        .get(idx as usize)
        .map_or(ptr::null(), |s| s.as_ptr())
}

/// Template answer format (`afmt` — the back-side template) at `idx`. Pointer
/// valid until free. NULL on null/oob.
#[no_mangle]
pub unsafe extern "C" fn flashify_notetype_template_afmt_at(
    schema: *const NotetypeSchema,
    idx: u64,
) -> *const c_char {
    if schema.is_null() {
        return ptr::null();
    }
    let schema = &*schema;
    schema
        .template_afmts
        .get(idx as usize)
        .map_or(ptr::null(), |s| s.as_ptr())
}

/// 1 if the notetype is cloze, 0 if standard, -1 on null.
#[no_mangle]
pub unsafe extern "C" fn flashify_notetype_is_cloze(schema: *const NotetypeSchema) -> i32 {
    if schema.is_null() {
        return -1;
    }
    let schema = &*schema;
    if schema.is_cloze {
        1
    } else {
        0
    }
}

/// The notetype's sort-field index, or 0 on null.
#[no_mangle]
pub unsafe extern "C" fn flashify_notetype_sort_field_idx(schema: *const NotetypeSchema) -> u32 {
    if schema.is_null() {
        return 0;
    }
    let schema = &*schema;
    schema.sort_field_idx
}

// ---------------------------------------------------------------------------
// render_fields (ABI v4) — re-render a note from its notetype definition +
// field values through the real Anki engine. When the user edits a note's
// fields, the iOS half-renderer can't reproduce conditionals / filters / cloze;
// instead we rebuild the notetype in a throwaway in-memory collection, add a
// note with the edited values, and render its card(s) with rslib's own engine.
// ---------------------------------------------------------------------------

#[derive(Deserialize, Clone)]
struct TemplateDef {
    name: String,
    qfmt: String,
    afmt: String,
}

#[derive(Deserialize, Clone)]
struct NotetypeDef {
    name: String,
    is_cloze: bool,
    sort_field_idx: u32,
    css: String,
    fields: Vec<String>,
    templates: Vec<TemplateDef>,
}

/// Rendered cards from `flashify_render_fields`: per-card ord + question/answer
/// HTML, plus the shared notetype CSS. Column-oriented to match the other list
/// accessors.
pub struct RenderedFieldsList {
    pub(crate) ords: Vec<u32>,
    pub(crate) questions: Vec<CString>,
    pub(crate) answers: Vec<CString>,
    pub(crate) css: CString,
}

fn render_fields_inner(
    def: NotetypeDef,
    field_values: Vec<String>,
) -> anki::error::Result<RenderedFieldsList> {
    // Throwaway in-memory collection. Stock notetypes get auto-added but are
    // harmless — we add and render our own notetype.
    let mut col = CollectionBuilder::default().build()?;

    // Rebuild the notetype from the stored schema.
    let mut nt = Notetype {
        id: NotetypeId(0),
        name: def.name,
        mtime_secs: TimestampSecs(0),
        usn: Usn(0),
        fields: def.fields.iter().map(NoteField::new).collect(),
        templates: def
            .templates
            .iter()
            .map(|t| CardTemplate::new(&t.name, &t.qfmt, &t.afmt))
            .collect(),
        config: NotetypeConfig {
            css: def.css,
            sort_field_idx: def.sort_field_idx,
            kind: if def.is_cloze {
                NotetypeKind::Cloze as i32
            } else {
                NotetypeKind::Normal as i32
            },
            ..Default::default()
        },
    };
    col.add_notetype(&mut nt, true)?;

    // Add a note with the edited field values (pad/truncate to field count).
    let mut note = nt.new_note();
    let field_count = note.fields().len();
    for (idx, val) in field_values.into_iter().enumerate() {
        if idx < field_count {
            note.set_field(idx, val)?;
        }
    }
    col.add_note(&mut note, DeckId(1))?;

    // Render every generated card in template/cloze order.
    let card_ids = col
        .storage
        .all_card_ids_of_note_in_template_order(note.id)?;
    let to_cstring = |s: &str| CString::new(s.replace('\0', "")).unwrap_or_default();
    let mut ords = Vec::with_capacity(card_ids.len());
    let mut questions = Vec::with_capacity(card_ids.len());
    let mut answers = Vec::with_capacity(card_ids.len());
    let mut css = String::new();
    // `all_card_ids_of_note_in_template_order` returns cards in ord order
    // (template_idx for standard, cloze-deletion order for cloze), so the
    // positional index IS the card ord — and `Card::template_idx` is private.
    for (ord, cid) in card_ids.into_iter().enumerate() {
        let out = col.render_existing_card(cid, false, false)?;
        if css.is_empty() {
            css = out.css.clone();
        }
        ords.push(ord as u32);
        questions.push(to_cstring(&out.question()));
        answers.push(to_cstring(&out.answer()));
    }

    Ok(RenderedFieldsList {
        ords,
        questions,
        answers,
        css: to_cstring(&css),
    })
}

/// Render a note from a notetype definition + field values. `notetype_json` is
/// a `NotetypeDef` (`{name, is_cloze, sort_field_idx, css, fields:[name],
/// templates:[{name,qfmt,afmt}]}`); `field_values_json` is a JSON array of field
/// values in notetype field order. Returns NULL on failure (check
/// `flashify_last_error_message`); free with `flashify_free_rendered_fields`.
#[no_mangle]
pub unsafe extern "C" fn flashify_render_fields(
    notetype_json: *const c_char,
    field_values_json: *const c_char,
) -> *mut RenderedFieldsList {
    let nt_str = match cstr_to_str(notetype_json) {
        Some(s) => s,
        None => {
            set_last_error("flashify_render_fields: invalid notetype_json");
            return ptr::null_mut();
        }
    };
    let fv_str = match cstr_to_str(field_values_json) {
        Some(s) => s,
        None => {
            set_last_error("flashify_render_fields: invalid field_values_json");
            return ptr::null_mut();
        }
    };
    let def: NotetypeDef = match serde_json::from_str(nt_str) {
        Ok(d) => d,
        Err(e) => {
            set_last_error_from("flashify_render_fields: parse notetype_json", e);
            return ptr::null_mut();
        }
    };
    let field_values: Vec<String> = match serde_json::from_str(fv_str) {
        Ok(v) => v,
        Err(e) => {
            set_last_error_from("flashify_render_fields: parse field_values_json", e);
            return ptr::null_mut();
        }
    };

    match render_fields_inner(def, field_values) {
        Ok(list) => Box::into_raw(Box::new(list)),
        Err(e) => {
            set_last_error_from("flashify_render_fields", e);
            ptr::null_mut()
        }
    }
}

#[no_mangle]
pub unsafe extern "C" fn flashify_free_rendered_fields(list: *mut RenderedFieldsList) {
    if !list.is_null() {
        drop(Box::from_raw(list));
    }
}

/// Number of rendered cards, or -1 if `list` is null.
#[no_mangle]
pub unsafe extern "C" fn flashify_rendered_fields_count(list: *const RenderedFieldsList) -> i64 {
    if list.is_null() {
        return -1;
    }
    let list = &*list;
    list.ords.len() as i64
}

/// Card ordinal at `idx` (Anki `template_idx`; cloze = deletion number - 1).
#[no_mangle]
pub unsafe extern "C" fn flashify_rendered_fields_ord_at(
    list: *const RenderedFieldsList,
    idx: u64,
) -> u32 {
    if list.is_null() {
        return 0;
    }
    let list = &*list;
    list.ords.get(idx as usize).copied().unwrap_or(0)
}

/// Question HTML at `idx`. Pointer valid until `flashify_free_rendered_fields`.
#[no_mangle]
pub unsafe extern "C" fn flashify_rendered_fields_question_at(
    list: *const RenderedFieldsList,
    idx: u64,
) -> *const c_char {
    if list.is_null() {
        return ptr::null();
    }
    let list = &*list;
    list.questions
        .get(idx as usize)
        .map_or(ptr::null(), |s| s.as_ptr())
}

/// Answer HTML at `idx`. Pointer valid until `flashify_free_rendered_fields`.
#[no_mangle]
pub unsafe extern "C" fn flashify_rendered_fields_answer_at(
    list: *const RenderedFieldsList,
    idx: u64,
) -> *const c_char {
    if list.is_null() {
        return ptr::null();
    }
    let list = &*list;
    list.answers
        .get(idx as usize)
        .map_or(ptr::null(), |s| s.as_ptr())
}

/// Shared notetype CSS for the rendered cards. Never NULL for a non-null list.
#[no_mangle]
pub unsafe extern "C" fn flashify_rendered_fields_css(
    list: *const RenderedFieldsList,
) -> *const c_char {
    if list.is_null() {
        return ptr::null();
    }
    let list = &*list;
    list.css.as_ptr()
}

/// Status code for void operations. Zero = OK, non-zero = error (check
/// `flashify_last_error_message`).
#[no_mangle]
pub extern "C" fn flashify_self_test() -> c_int {
    0
}

#[cfg(test)]
mod render_fields_tests {
    use super::*;

    fn def(fields: &[&str], templates: &[(&str, &str, &str)], is_cloze: bool) -> NotetypeDef {
        NotetypeDef {
            name: "Test".into(),
            is_cloze,
            sort_field_idx: 0,
            css: ".card{}".into(),
            fields: fields.iter().map(|s| s.to_string()).collect(),
            templates: templates
                .iter()
                .map(|(n, q, a)| TemplateDef {
                    name: n.to_string(),
                    qfmt: q.to_string(),
                    afmt: a.to_string(),
                })
                .collect(),
        }
    }

    /// Baseline: a Basic notetype renders front + back from field values.
    #[test]
    fn basic_renders_front_back() {
        let d = def(
            &["Front", "Back"],
            &[("Card 1", "{{Front}}", "{{FrontSide}}<hr>{{Back}}")],
            false,
        );
        let out = render_fields_inner(d, vec!["Q text".into(), "A text".into()]).unwrap();
        assert_eq!(out.questions.len(), 1);
        assert!(out.questions[0].to_str().unwrap().contains("Q text"));
        assert!(out.answers[0].to_str().unwrap().contains("A text"));
    }

    /// THE point of going through rslib: a `{{#Field}}` conditional shows its
    /// body only when the field is non-empty — the iOS half-renderer can't.
    #[test]
    fn conditional_field_renders() {
        let d = def(
            &["Front", "Extra"],
            &[(
                "Card 1",
                "{{Front}}",
                "{{Front}}{{#Extra}}<div id=ex>{{Extra}}</div>{{/Extra}}",
            )],
            false,
        );
        let with_extra = render_fields_inner(d.clone(), vec!["F".into(), "hint".into()]).unwrap();
        assert!(with_extra.answers[0].to_str().unwrap().contains("hint"));
        assert!(with_extra.answers[0].to_str().unwrap().contains("id=ex"));
        let without = render_fields_inner(d, vec!["F".into(), "".into()]).unwrap();
        assert!(
            !without.answers[0].to_str().unwrap().contains("id=ex"),
            "empty Extra must hide the conditional block"
        );
    }

    /// Cloze: one card per `{{c1::}}/{{c2::}}` deletion, the active one hidden.
    #[test]
    fn cloze_generates_one_card_per_deletion() {
        let d = def(
            &["Text", "Extra"],
            &[("Cloze", "{{cloze:Text}}", "{{cloze:Text}}<br>{{Extra}}")],
            true,
        );
        let out = render_fields_inner(
            d,
            vec!["The {{c1::heart}} pumps {{c2::blood}}.".into(), "".into()],
        )
        .unwrap();
        assert_eq!(out.questions.len(), 2, "two deletions → two cards");
        let q0 = out.questions[0].to_str().unwrap();
        let q1 = out.questions[1].to_str().unwrap();
        // Each card masks its own active deletion as `[...]` and reveals the
        // other inline as `cloze-inactive` — the exact Anki cloze behaviour the
        // iOS half-renderer can't reproduce.
        assert!(
            q0.contains("[...]") && q0.contains("cloze-inactive"),
            "c1 card masks its deletion, reveals the other"
        );
        assert!(
            q1.contains("[...]") && q1.contains("cloze-inactive"),
            "c2 card masks its deletion, reveals the other"
        );
        assert!(q0.contains(">blood<"), "c1 card reveals c2 = blood");
        assert!(q1.contains(">heart<"), "c2 card reveals c1 = heart");
    }
}
