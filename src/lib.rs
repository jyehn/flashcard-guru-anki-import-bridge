//! RustImportCore — Anki package parsing FFI shim.
//!
//! PR 2b: stub implementations. The functions, types, and ownership rules
//! defined here are the contract; PR 3 fills in the actual rslib-backed
//! logic behind the same C ABI so the Swift side never has to change.
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

// PR 3a smoke ref: a real call into rslib that LTO can't eliminate, so the
// linker actually drags rslib's object code into the staticlib. PR 3b will
// add real flashify_open_package etc. on top of import_export::*.
use anki::version;

use std::ffi::{c_char, CStr, CString};
use std::os::raw::c_int;
use std::ptr;
use std::sync::Mutex;

// ---------------------------------------------------------------------------
// Error storage (thread-local would be nicer; keeping it simple for the stub).
// ---------------------------------------------------------------------------

static LAST_ERROR: Mutex<Option<CString>> = Mutex::new(None);

fn set_last_error(msg: &str) {
    if let Ok(mut slot) = LAST_ERROR.lock() {
        *slot = CString::new(msg).ok();
    }
}

#[no_mangle]
pub unsafe extern "C" fn flashify_last_error_message() -> *const c_char {
    match LAST_ERROR.lock() {
        Ok(slot) => slot.as_ref().map_or(ptr::null(), |s| s.as_ptr()),
        Err(_) => ptr::null(),
    }
}

// ---------------------------------------------------------------------------
// Opaque handles. PR 3 replaces the stub fields with real rslib state.
// ---------------------------------------------------------------------------

pub struct OpenedPackage {
    /// Stub: path the caller opened. Replaced by an `anki::collection::Collection`
    /// or equivalent in PR 3.
    pub _path: CString,
}

pub struct DeckList {
    pub _names: Vec<CString>,
}

pub struct NoteList {
    pub _ids: Vec<i64>,
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
/// can call this in PR 3a to confirm the static library actually contains
/// rslib code (rather than just the FFI shell).
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
// Core API — 5 functions called out in the plan.
// ---------------------------------------------------------------------------

/// Open an Anki package (.apkg or .colpkg) and prepare it for enumeration.
/// `workspace_path` is a writable directory the bridge can use for temp files
/// (e.g. zstd-decompressed sqlite).
///
/// Returns NULL on failure; check `flashify_last_error_message`.
#[no_mangle]
pub unsafe extern "C" fn flashify_open_package(
    package_path: *const c_char,
    workspace_path: *const c_char,
) -> *mut OpenedPackage {
    if package_path.is_null() || workspace_path.is_null() {
        set_last_error("flashify_open_package: null path");
        return ptr::null_mut();
    }
    let path = match CStr::from_ptr(package_path).to_str() {
        Ok(s) => s,
        Err(_) => {
            set_last_error("flashify_open_package: package_path not utf-8");
            return ptr::null_mut();
        }
    };
    set_last_error("rust_import_core stub: open_package not implemented yet");
    Box::into_raw(Box::new(OpenedPackage {
        _path: CString::new(path).unwrap_or_default(),
    }))
}

#[no_mangle]
pub unsafe extern "C" fn flashify_free_package(pkg: *mut OpenedPackage) {
    if !pkg.is_null() {
        drop(Box::from_raw(pkg));
    }
}

/// Enumerate decks inside the opened package.
#[no_mangle]
pub unsafe extern "C" fn flashify_enumerate_decks(_pkg: *mut OpenedPackage) -> *mut DeckList {
    Box::into_raw(Box::new(DeckList { _names: Vec::new() }))
}

#[no_mangle]
pub unsafe extern "C" fn flashify_free_decks(decks: *mut DeckList) {
    if !decks.is_null() {
        drop(Box::from_raw(decks));
    }
}

/// Enumerate notes for a specific deck (or all notes if `deck_id` is 0).
#[no_mangle]
pub unsafe extern "C" fn flashify_enumerate_notes(
    _pkg: *mut OpenedPackage,
    _deck_id: u64,
) -> *mut NoteList {
    Box::into_raw(Box::new(NoteList { _ids: Vec::new() }))
}

#[no_mangle]
pub unsafe extern "C" fn flashify_free_notes(notes: *mut NoteList) {
    if !notes.is_null() {
        drop(Box::from_raw(notes));
    }
}

/// Map an Anki card's scheduler state onto FSRS. PR 3 wires this to the
/// real conversion baked into rslib.
#[no_mangle]
pub unsafe extern "C" fn flashify_card_state_as_fsrs(
    _pkg: *mut OpenedPackage,
    _card_id: u64,
) -> FsrsState {
    FsrsState::empty()
}

/// Render a note template (front/back HTML) for the given note + ordinal.
#[no_mangle]
pub unsafe extern "C" fn flashify_render_template(
    _pkg: *mut OpenedPackage,
    _note_id: u64,
    _ord: u32,
) -> *mut RenderedCard {
    Box::into_raw(Box::new(RenderedCard {
        _question_html: CString::new("").unwrap(),
        _answer_html: CString::new("").unwrap(),
    }))
}

#[no_mangle]
pub unsafe extern "C" fn flashify_free_rendered(card: *mut RenderedCard) {
    if !card.is_null() {
        drop(Box::from_raw(card));
    }
}

/// Status code for void operations. Zero = OK, non-zero = error (check
/// `flashify_last_error_message`).
#[no_mangle]
pub extern "C" fn flashify_self_test() -> c_int {
    0
}
