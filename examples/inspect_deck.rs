//! Diagnostic harness — run our import pipeline against a real .apkg
//! and dump everything the FFI would expose. Mirrors what
//! `flashify_open_package` + `flashify_render_template` produce on
//! iOS, so we can see exactly what HTML/CSS/media references the
//! WebView is being asked to render.
//!
//! Usage:
//!   cd Vendor/RustImportCore
//!   cargo run --release --example inspect_deck -- "<path to .apkg>"

use anki::card::CardId;
use anki::collection::CollectionBuilder;
use anki::decks::DeckId;
use anki::notes::NoteId;
use anki::search::SearchNode;
use anki_proto::import_export::ImportAnkiPackageOptions;
use std::env;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        eprintln!("usage: inspect_deck <path-to-apkg>");
        return ExitCode::from(2);
    }
    let pkg_path = PathBuf::from(&args[1]);
    if !pkg_path.exists() {
        eprintln!("file does not exist: {}", pkg_path.display());
        return ExitCode::from(2);
    }

    let workspace = std::env::temp_dir().join(format!(
        "flashify-inspect-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default()
    ));
    std::fs::create_dir_all(&workspace).unwrap();
    let collection_path = workspace.join("collection.anki2");
    let media_folder = workspace.join("collection.media");
    let media_db = workspace.join("collection.media.db");
    std::fs::create_dir_all(&media_folder).unwrap();

    println!("=== opening package ===");
    println!("source : {}", pkg_path.display());
    println!("work   : {}", workspace.display());

    let mut col = match CollectionBuilder::new(&collection_path)
        .set_media_paths(media_folder.clone(), media_db)
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            eprintln!("collection build failed: {e}");
            return ExitCode::FAILURE;
        }
    };

    let options = ImportAnkiPackageOptions {
        with_scheduling: true,
        with_deck_configs: true,
        merge_notetypes: true,
        ..Default::default()
    };
    if let Err(e) = col.import_apkg(&pkg_path, options) {
        eprintln!("import_apkg failed: {e}");
        return ExitCode::FAILURE;
    }

    // Decks
    println!("\n=== decks (skip_default=false, filter empty) ===");
    let pairs = col.get_all_deck_names(false).expect("deck names");
    for (DeckId(id), name) in &pairs {
        let has_card: bool = col
            .storage
            .db()
            .query_row(
                "SELECT 1 FROM cards WHERE did = ? LIMIT 1",
                rusqlite::params![*id],
                |_| Ok(()),
            )
            .is_ok();
        println!("  did={id:>14}  has_cards={has_card}  name={name}");
    }
    let deck_with_cards: Option<DeckId> = pairs
        .iter()
        .find(|(DeckId(id), _)| {
            col.storage
                .db()
                .query_row(
                    "SELECT 1 FROM cards WHERE did = ? LIMIT 1",
                    rusqlite::params![*id],
                    |_| Ok(()),
                )
                .is_ok()
        })
        .map(|(d, _)| *d);

    let did = match deck_with_cards {
        Some(d) => d,
        None => {
            eprintln!("no deck with cards");
            return ExitCode::FAILURE;
        }
    };

    // Notes
    println!("\n=== notes in deck {did:?} ===");
    let note_ids: Vec<NoteId> = col
        .search_notes_unordered(SearchNode::DeckIdWithChildren(did))
        .expect("search");
    println!("  count: {}", note_ids.len());
    let sample = note_ids.iter().take(3).copied().collect::<Vec<_>>();

    for nid in &sample {
        let note = col.storage.get_note(*nid).expect("get note").expect("note exists");
        println!(
            "\n  --- note nid={} notetype_id={} ---",
            nid.0, note.notetype_id.0
        );
        for (i, f) in note.fields().iter().enumerate() {
            let preview: String = f.chars().take(80).collect();
            println!("    field[{i}] len={} preview={:?}", f.len(), preview);
        }
        println!("    tags: {:?}", note.tags);

        // Cards for this note
        let mut stmt = col
            .storage
            .db()
            .prepare("SELECT id, ord FROM cards WHERE nid = ? ORDER BY ord")
            .unwrap();
        let cards: Vec<(i64, i64)> = stmt
            .query_map(rusqlite::params![nid.0], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        println!("    cards:");
        for (cid, ord) in &cards {
            println!("      cid={cid} ord={ord}");
        }
    }

    // Render the FIRST card and dump everything
    if let Some(nid) = sample.first() {
        // Drop the prepared statement before calling render_existing_card,
        // which needs &mut col.
        let card_lookup: Option<(i64, i64)> = {
            let mut stmt = col
                .storage
                .db()
                .prepare("SELECT id, ord FROM cards WHERE nid = ? ORDER BY ord LIMIT 1")
                .unwrap();
            stmt.query_row(rusqlite::params![nid.0], |row| {
                let cid: i64 = row.get(0)?;
                let ord: i64 = row.get(1)?;
                Ok((cid, ord))
            })
            .ok()
        };
        if let Some((cid, ord)) = card_lookup {
            println!("\n=== rendering nid={} cid={} ord={} ===", nid.0, cid, ord);
            match col.render_existing_card(CardId(cid), false, false) {
                Ok(out) => {
                    let q = out.question().into_owned();
                    let a = out.answer().into_owned();
                    println!("\n--- question html ({} bytes) ---", q.len());
                    println!("{q}");
                    println!("\n--- answer html ({} bytes) ---", a.len());
                    println!("{a}");
                    println!("\n--- css ({} bytes) ---", out.css.len());
                    println!("{}", out.css);
                    println!("\n--- image src refs in q+a ---");
                    for src in extract_img_srcs(&format!("{q}\n{a}")) {
                        println!("  {src}");
                    }
                    println!("\n--- url(...) refs in css ---");
                    for u in extract_css_urls(&out.css) {
                        println!("  {u}");
                    }
                }
                Err(e) => eprintln!("render failed: {e}"),
            }
        }
    }

    // Notetype schema dump — what the iOS CardEditCapability sees.
    if let Some(nid) = sample.first() {
        let ntid = col
            .storage
            .get_note(*nid)
            .expect("get note")
            .expect("note")
            .notetype_id;
        if let Ok(Some(nt)) = col.get_notetype(ntid) {
            println!("\n=== notetype schema (id {}) ===", ntid.0);
            println!("name: {}", nt.name);
            println!(
                "fields: {:?}",
                nt.fields.iter().map(|f| f.name.clone()).collect::<Vec<_>>()
            );
            let unsupported_tokens = [
                "{{#", "{{^", "{{/", "{{hint:", "{{type:", "{{tts", "{{furigana:",
                "{{kana:", "{{kanji:", "{{Tags", "{{cloze:",
            ];
            for t in &nt.templates {
                let combined = format!("{}\n{}", t.config.q_format, t.config.a_format);
                let present: Vec<&str> = unsupported_tokens
                    .iter()
                    .filter(|tok| combined.contains(**tok))
                    .copied()
                    .collect();
                println!("template '{}': flagged_tokens={:?}", t.name, present);
                println!(
                    "  qfmt[0..220]: {}",
                    t.config.q_format.chars().take(220).collect::<String>().replace('\n', " ")
                );
            }
        }
    }

    // Media folder listing
    println!("\n=== collection.media listing ({}) ===", media_folder.display());
    print_dir(&media_folder, 30);

    // Cleanup
    let _ = col.close(None);
    let _ = std::fs::remove_dir_all(&workspace);

    ExitCode::SUCCESS
}

fn extract_img_srcs(html: &str) -> Vec<String> {
    use regex::Regex;
    let re = Regex::new(r#"<img\b[^>]*\bsrc\s*=\s*["']([^"']+)["']"#).unwrap();
    re.captures_iter(html)
        .map(|c| c[1].to_string())
        .collect()
}

fn extract_css_urls(css: &str) -> Vec<String> {
    use regex::Regex;
    let re = Regex::new(r#"url\(\s*["']?([^"')]+)["']?\s*\)"#).unwrap();
    re.captures_iter(css)
        .map(|c| c[1].to_string())
        .collect()
}

fn print_dir(dir: &Path, limit: usize) {
    let mut entries = match std::fs::read_dir(dir) {
        Ok(e) => e.filter_map(|r| r.ok()).collect::<Vec<_>>(),
        Err(e) => {
            println!("  (cannot read: {e})");
            return;
        }
    };
    entries.sort_by_key(|e| e.file_name());
    println!("  count: {}", entries.len());
    for e in entries.iter().take(limit) {
        let size = e.metadata().map(|m| m.len()).unwrap_or(0);
        println!("    {:>10}  {}", size, e.file_name().to_string_lossy());
    }
    if entries.len() > limit {
        println!("    ... ({} more)", entries.len() - limit);
    }
}
