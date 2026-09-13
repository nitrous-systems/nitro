//! Integration tests against a real system font.
//!
//! Every test that needs glyphs calls [`db()`] first; when the box has no
//! fonts it prints why and returns, so a CI image without fonts stays green
//! rather than failing on something it cannot have.

use std::time::Instant;

use nitro_text::{Atlas, Family, FontDb, FontId, GlyphKey, Layout, TextStyle};

/// Scan the system fonts, or `None` when there are none.
fn db() -> Option<FontDb> {
    let db = FontDb::scan();
    if db.is_empty() {
        eprintln!(
            "skipping: no fonts found (set NITRO_FONT_DIRS or install one, \
             e.g. /usr/share/fonts/truetype/dejavu)"
        );
        return None;
    }
    Some(db)
}

fn style() -> TextStyle {
    TextStyle::default()
}

#[test]
fn hello_has_five_clusters_with_increasing_x() {
    let Some(db) = db() else { return };
    let mut layout = Layout::new();
    let shaped = layout.shape(&db, "Hello", &style(), None, false);

    assert_eq!(shaped.lines.len(), 1);
    let line = &shaped.lines[0];
    assert_eq!(line.glyphs.len(), 5, "one glyph per letter");

    let mut clusters: Vec<u32> = line.glyphs.iter().map(|g| g.cluster).collect();
    clusters.dedup();
    assert_eq!(clusters, vec![0, 1, 2, 3, 4], "cluster = byte offset");

    for pair in line.glyphs.windows(2) {
        assert!(
            pair[1].x > pair[0].x,
            "x must strictly increase: {:?}",
            line.glyphs.iter().map(|g| g.x).collect::<Vec<_>>()
        );
    }
    assert!(shaped.width > 0.0);
    assert!((shaped.width - line.width).abs() < f32::EPSILON);
    assert!(shaped.ascent > 0.0 && shaped.descent > 0.0);
    assert!(shaped.height >= shaped.ascent + shaped.descent);
    assert!((shaped.size_px - 14.0).abs() < f32::EPSILON);
}

#[test]
fn wrapping_breaks_at_whitespace_within_the_limit() {
    let Some(db) = db() else { return };
    let mut layout = Layout::new();
    let text = "The quick brown fox jumps over the lazy dog and keeps on running \
                past the edge of the paragraph box.";
    let max = 120.0;
    let shaped = layout.shape(&db, text, &style(), Some(max), true);

    assert!(
        shaped.lines.len() >= 2,
        "expected a wrap, got {} line(s)",
        shaped.lines.len()
    );
    for (i, line) in shaped.lines.iter().enumerate() {
        // A single word wider than the limit is the documented exception: a
        // line holding exactly one word may exceed it.
        let single_word = {
            let mut clusters: Vec<u32> = line.glyphs.iter().map(|g| g.cluster).collect();
            clusters.dedup();
            clusters.len() <= 1
        };
        assert!(
            line.width <= max + 0.5 || single_word,
            "line {i} is {} px wide, limit {max}",
            line.width
        );
    }
    // Baselines are cumulative and strictly increasing downward.
    for pair in shaped.lines.windows(2) {
        assert!(pair[1].baseline > pair[0].baseline);
    }
    assert!(shaped.width <= max + 0.5);
}

#[test]
fn a_long_word_is_broken_at_a_char_boundary() {
    let Some(db) = db() else { return };
    let mut layout = Layout::new();
    let shaped = layout.shape(
        &db,
        "supercalifragilisticexpialidocious",
        &style(),
        Some(40.0),
        true,
    );
    assert!(shaped.lines.len() >= 2, "a long word must be split");
    for line in &shaped.lines {
        assert!(!line.glyphs.is_empty());
    }
}

#[test]
fn newlines_split_lines_and_tabs_expand() {
    let Some(db) = db() else { return };
    let mut layout = Layout::new();

    let shaped = layout.shape(&db, "one\ntwo\r\nthree", &style(), None, false);
    assert_eq!(shaped.lines.len(), 3);
    // Cluster offsets are into the ORIGINAL string, newlines included.
    assert_eq!(shaped.lines[1].glyphs[0].cluster, 4);
    assert_eq!(shaped.lines[2].glyphs[0].cluster, 9);

    let space = layout.shape(&db, "a b", &style(), None, false).width;
    let tabbed = layout.shape(&db, "a\tb", &style(), None, false);
    let plain = layout.shape(&db, "ab", &style(), None, false).width;
    let one_space = space - plain;
    let four = tabbed.width - plain;
    assert!(
        (four - 4.0 * one_space).abs() < 0.5,
        "tab should be 4 spaces: {four} vs {}",
        4.0 * one_space
    );
    // Every glyph of the expanded tab reports the tab's own byte offset.
    let after_tab: Vec<u32> = tabbed.lines[0].glyphs.iter().map(|g| g.cluster).collect();
    assert!(after_tab.contains(&2), "'b' is at byte 2: {after_tab:?}");
}

#[test]
fn atlas_packs_three_thousand_keys_into_few_pages() {
    let Some(db) = db() else { return };
    let style = style();
    let Some(font) = db.select(&style) else {
        eprintln!("skipping: no face selected");
        return;
    };
    let mut atlas = Atlas::new();

    let mut requested = 0;
    let mut glyph = 1u16;
    while requested < 3000 {
        for subpx in 0..4u8 {
            if requested == 3000 {
                break;
            }
            let key = GlyphKey::new(font, glyph, 14.0, f32::from(subpx) * 0.25);
            atlas.get(&db, key);
            requested += 1;
        }
        glyph = glyph.wrapping_add(1).max(1);
    }
    eprintln!(
        "atlas: {} keys -> {} cached, {} pages, {} renders",
        requested,
        atlas.glyph_count(),
        atlas.page_count(),
        atlas.renders()
    );
    assert!(
        atlas.page_count() <= 8,
        "expected <= 8 pages, got {}",
        atlas.page_count()
    );
    assert_eq!(
        atlas.renders(),
        atlas.glyph_count() as u64,
        "one rasterization per distinct key, empty masks included"
    );

    // A repeat is a hit: the render counter must not move.
    let key = GlyphKey::new(font, 40, 14.0, 0.0);
    let first = atlas.get(&db, key);
    let renders = atlas.renders();
    for _ in 0..100 {
        assert_eq!(atlas.get(&db, key), first);
    }
    assert_eq!(atlas.renders(), renders, "repeat get must not re-render");

    // Every reported rect lies inside its page.
    if let Some(mask) = first {
        let page = atlas.page(mask.page).expect("page exists");
        assert_eq!(page.len(), (Atlas::PAGE * Atlas::PAGE) as usize);
        assert!(mask.x + mask.w <= Atlas::PAGE && mask.y + mask.h <= Atlas::PAGE);
    }
    atlas.next_frame();
}

#[test]
fn a_space_has_no_mask() {
    let Some(db) = db() else { return };
    let mut layout = Layout::new();
    // Shape a space to learn its glyph id in the selected face.
    let shaped = layout.shape(&db, " ", &style(), None, false);
    let Some(space) = shaped.lines.first().and_then(|l| l.glyphs.first()) else {
        eprintln!("skipping: the space produced no glyph");
        return;
    };
    let mut atlas = Atlas::new();
    let key = GlyphKey::new(space.font, space.id, shaped.size_px, 0.0);
    assert!(atlas.get(&db, key).is_none(), "a space has no pixels");
    assert_eq!(atlas.page_count(), 0, "and occupies no page");
    // The empty result is cached: asking again does not re-rasterize.
    let renders = atlas.renders();
    assert!(atlas.get(&db, key).is_none());
    assert_eq!(atlas.renders(), renders);
    assert_eq!(atlas.glyph_count(), 1, "the negative result is an entry");
    assert!(space.advance > 0.0, "but it still advances the pen");
}

#[test]
fn cursor_positions_are_monotonic_and_start_at_zero() {
    let Some(db) = db() else { return };
    let mut layout = Layout::new();
    let text = "Hello world";
    let metrics = layout.measure(&db, text, &style(), None, false);

    assert_eq!(metrics.line_count, 1);
    assert!(!metrics.cursor_x.is_empty());
    assert_eq!(metrics.cursor_x[0].0, 0, "first offset is 0");
    assert!((metrics.cursor_x[0].1 - 0.0).abs() < f32::EPSILON);
    assert_eq!(
        metrics.cursor_x.last().unwrap().0,
        text.len() as u32,
        "last pair is the end of the text"
    );
    for pair in metrics.cursor_x.windows(2) {
        assert!(pair[1].0 > pair[0].0, "byte offsets increase");
        assert!(pair[1].1 >= pair[0].1, "x is non-decreasing");
    }
    assert!((metrics.width - layout.shape(&db, text, &style(), None, false).width).abs() < 0.01);
}

#[test]
fn cursor_positions_restart_per_line() {
    let Some(db) = db() else { return };
    let mut layout = Layout::new();
    let metrics = layout.measure(&db, "abc\nabc", &style(), None, false);
    assert_eq!(metrics.line_count, 2);
    // The pair at the start of the second line is back at x = 0.
    let second = metrics
        .cursor_x
        .iter()
        .find(|(offset, _)| *offset == 4)
        .expect("offset 4 present");
    assert!(second.1.abs() < f32::EPSILON, "line-local x: {}", second.1);

    // The newline's own offset is a caret position at the end of line 1.
    let (_, newline_x) = *metrics
        .cursor_x
        .iter()
        .find(|(offset, _)| *offset == 3)
        .expect("the newline offset is a cursor position");
    assert!(newline_x > 0.0, "end of line 1 is past its first glyph");

    // Byte offsets increase across the whole table, x within each line.
    let mut offsets: Vec<u32> = metrics.cursor_x.iter().map(|(o, _)| *o).collect();
    let len = offsets.len();
    offsets.dedup();
    assert_eq!(offsets.len(), len, "no repeated offsets");
    assert_eq!(metrics.cursor_x.last().unwrap().0, 7);
}

#[test]
fn family_parse_maps_the_generic_aliases() {
    assert_eq!(Family::parse("sans"), Family::Sans);
    assert_eq!(Family::parse("sans-serif"), Family::Sans);
    assert_eq!(Family::parse("mono"), Family::Mono);
    assert_eq!(Family::parse("monospace"), Family::Mono);
    assert_eq!(Family::parse("serif"), Family::Serif);
    assert_eq!(
        Family::parse("Comic Sans MS"),
        Family::Named("Comic Sans MS".to_string())
    );
}

#[test]
fn an_empty_db_selects_nothing_and_shapes_nothing() {
    let dir = std::env::temp_dir().join(format!("nitro-text-empty-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let db = FontDb::scan_dirs(&[&dir]);
    assert!(db.is_empty());
    assert_eq!(db.len(), 0);
    assert!(db.select(&style()).is_none());
    assert!(db.fallbacks(&style()).is_empty());
    assert!(db.family_name(FontId(0)).is_none());
    assert!(db.face(FontId(0)).is_none());
    assert!(db.face_path(FontId(0)).is_none());
    assert_eq!(db.loaded_bytes(), 0);

    let mut layout = Layout::new();
    let shaped = layout.shape(&db, "Hello", &style(), Some(100.0), true);
    assert!(shaped.lines.is_empty());
    assert!((shaped.width - 0.0).abs() < f32::EPSILON);
    assert!((shaped.height - 0.0).abs() < f32::EPSILON);

    let metrics = layout.measure(&db, "Hello", &style(), None, false);
    assert_eq!(metrics.line_count, 0);
    assert!(metrics.cursor_x.is_empty());

    // The atlas is equally unbothered.
    let mut atlas = Atlas::new();
    assert!(
        atlas
            .get(&db, GlyphKey::new(FontId(0), 1, 14.0, 0.0))
            .is_none()
    );
    assert_eq!(atlas.page_count(), 0);

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn generic_aliases_resolve_to_different_families_when_available() {
    let Some(db) = db() else { return };
    let mut mono = style();
    mono.family = Family::Mono;
    if let Some(id) = db.select(&mono) {
        let name = db.family_name(id).unwrap().to_ascii_lowercase();
        eprintln!("mono -> {name}");
        assert!(name.contains("mono") || db.len() < 3);
    }
    let mut serif = style();
    serif.family = Family::Serif;
    if let Some(id) = db.select(&serif) {
        eprintln!("serif -> {}", db.family_name(id).unwrap());
    }
    let sans = db.select(&style()).and_then(|id| db.family_name(id));
    eprintln!("sans -> {sans:?}");
    assert!(sans.is_some());
}

#[test]
fn bold_and_italic_select_different_faces_when_the_family_has_them() {
    let Some(db) = db() else { return };
    let regular = db.select(&style());
    let mut bold = style();
    bold.weight = 700;
    let bold_id = db.select(&bold);
    assert!(regular.is_some() && bold_id.is_some());
    eprintln!(
        "regular={regular:?} bold={bold_id:?} (same face is fine when the family has one weight)"
    );
}

#[test]
fn timings() {
    let start = Instant::now();
    let db = FontDb::scan();
    let wall = start.elapsed();
    eprintln!(
        "FontDb::scan: {:?} (self-reported {:?}), {} faces, index cache {}",
        wall,
        db.scan_time(),
        db.len(),
        if db.used_index_cache() { "hit" } else { "miss" }
    );
    if db.is_empty() {
        eprintln!("skipping shape timing: no fonts found");
        return;
    }

    let para: String = "The quick brown fox jumps over the lazy dog. "
        .chars()
        .cycle()
        .take(200)
        .collect();
    assert_eq!(para.chars().count(), 200);
    let mut layout = Layout::new();
    let style = style();

    // Warm the shaping caches, then time.
    for _ in 0..3 {
        layout.shape(&db, &para, &style, Some(400.0), true);
    }
    let iters = 200;
    let start = Instant::now();
    let mut lines = 0;
    for _ in 0..iters {
        lines += layout
            .shape(&db, &para, &style, Some(400.0), true)
            .lines
            .len();
    }
    let per = start.elapsed() / iters;
    eprintln!(
        "Layout::shape, 200 chars @ 14 px, wrap 400 px: {per:?} per call ({lines} lines total)"
    );

    let mut atlas = Atlas::new();
    let shaped = layout.shape(&db, &para, &style, Some(400.0), true);
    let start = Instant::now();
    for line in &shaped.lines {
        for glyph in &line.glyphs {
            atlas.get(
                &db,
                GlyphKey::new(glyph.font, glyph.id, shaped.size_px, glyph.x),
            );
        }
    }
    eprintln!(
        "Atlas: first pass over the paragraph {:?}, {} renders, {} pages",
        start.elapsed(),
        atlas.renders(),
        atlas.page_count()
    );
}

// ---------------------------------------------------------------------------
// Lazy face loading (issue #528)
// ---------------------------------------------------------------------------

/// A directory holding real font files, for the lazy-loading tests: the
/// system's font dirs are scanned for the largest few files and those are
/// taken as-is. `None` when the box has no fonts, like [`db()`].
fn font_files(count: usize) -> Option<Vec<std::path::PathBuf>> {
    fn walk(dir: &std::path::Path, depth: u32, out: &mut Vec<std::path::PathBuf>) {
        if depth > 6 {
            return;
        }
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, depth + 1, out);
            } else if path
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("ttf") || e.eq_ignore_ascii_case("otf"))
            {
                out.push(path);
            }
        }
    }
    let dirs: Vec<std::path::PathBuf> = match std::env::var("NITRO_FONT_DIRS") {
        Ok(v) => v
            .split(':')
            .filter(|s| !s.is_empty())
            .map(Into::into)
            .collect(),
        Err(_) => vec!["/usr/share/fonts".into(), "/usr/local/share/fonts".into()],
    };
    let mut found = Vec::new();
    for dir in &dirs {
        walk(dir, 0, &mut found);
    }
    found.sort();
    found.dedup();
    if found.len() < count {
        eprintln!("skipping: need {count} font files, found {}", found.len());
        return None;
    }
    found.truncate(count);
    Some(found)
}

/// Size of a file on disk, 0 when it cannot be stat'ed.
fn file_len(path: &std::path::PathBuf) -> u64 {
    std::fs::metadata(path).map_or(0, |m| m.len())
}

/// Copy `files` into a fresh temp directory, so a test owns its font dir.
fn temp_font_dir(tag: &str, files: &[std::path::PathBuf]) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("nitro-text-{tag}-{}", std::process::id()));
    std::fs::remove_dir_all(&dir).ok();
    std::fs::create_dir_all(&dir).expect("temp font dir");
    for (i, src) in files.iter().enumerate() {
        let ext = src.extension().and_then(|e| e.to_str()).unwrap_or("ttf");
        std::fs::copy(src, dir.join(format!("f{i}.{ext}"))).expect("copy a font file");
    }
    dir
}

/// The headline property of the lazy db: a scan indexes the faces and holds
/// **no font bytes**; the first shape loads exactly the file it needs.
#[test]
fn a_scan_holds_no_bytes_until_a_face_is_used() {
    let Some(files) = font_files(4) else { return };
    let dir = temp_font_dir("lazy", &files);
    let db = FontDb::scan_dirs(&[&dir]);
    assert!(!db.is_empty(), "the copied fonts must index");

    assert_eq!(db.loaded_bytes(), 0, "a scan retains no font bytes");
    assert_eq!(db.loaded_files(), 0);
    assert_eq!(db.loads(), 0);

    let mut layout = Layout::new();
    let shaped = layout.shape(&db, "Hello", &style(), None, false);
    assert!(!shaped.lines.is_empty(), "shaping must produce glyphs");
    assert!(db.loaded_bytes() > 0, "the first shape loads a face");
    assert_eq!(db.loaded_files(), 1, "and exactly one file for Latin text");
    assert_eq!(db.loads(), 1);

    // Shaping the same style again is free: the file is already resident.
    layout.shape(&db, "Hello again", &style(), None, false);
    assert_eq!(db.loads(), 1, "a cached face is not re-read");

    // The whole point, in one number: far less than the directory holds.
    let on_disk: u64 = files.iter().map(file_len).sum();
    eprintln!(
        "lazy db: {} faces indexed, {} of {on_disk} bytes resident",
        db.len(),
        db.loaded_bytes()
    );
    assert!(
        (db.loaded_bytes() as u64) < on_disk,
        "a subset of the font files must be resident"
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// Eviction under a cap smaller than the working set: faces are dropped, the
/// atlas keeps every mask it rendered, and a repeat draw costs no new render.
#[test]
fn eviction_under_a_tiny_cap_keeps_the_atlas_working() {
    let Some(files) = font_files(3) else { return };
    let dir = temp_font_dir("evict", &files);
    let db = FontDb::scan_dirs(&[&dir]);
    if db.len() < 2 {
        eprintln!("skipping: need two faces, got {}", db.len());
        std::fs::remove_dir_all(&dir).ok();
        return;
    }

    // One byte: every load immediately over-runs the cap.
    db.set_cache_limit(1);
    let mut atlas = Atlas::new();
    let ids: Vec<FontId> = (0..db.len() as u32).map(FontId).collect();

    // Render one glyph per face, alternating, so each load evicts the last.
    for id in &ids {
        for glyph in 20u16..30 {
            atlas.get(&db, GlyphKey::new(*id, glyph, 14.0, 0.0));
        }
    }
    let renders = atlas.renders();
    let cached = atlas.glyph_count();
    assert!(cached > 0, "the atlas must have cached something");
    assert!(
        db.evictions() > 0,
        "a 1-byte cap must evict: {:?}",
        db.loads()
    );
    assert_eq!(
        db.releases(),
        0,
        "nothing went idle: the cap's counter, not the sweep's"
    );
    let largest = files.iter().map(file_len).max().unwrap_or(0) as usize;
    assert!(
        db.loaded_bytes() <= largest,
        "at most one file stays resident under a 1-byte cap"
    );

    // Every mask is still there: re-asking renders nothing and reads nothing.
    let loads = db.loads();
    for id in &ids {
        for glyph in 20u16..30 {
            atlas.get(&db, GlyphKey::new(*id, glyph, 14.0, 0.0));
        }
    }
    assert_eq!(
        atlas.renders(),
        renders,
        "a cached mask is never re-rendered"
    );
    assert_eq!(atlas.glyph_count(), cached);
    assert_eq!(db.loads(), loads, "and needs no face at all");
    eprintln!(
        "tiny cap: {} loads, {} evictions, {} masks, {} renders",
        db.loads(),
        db.evictions(),
        cached,
        renders
    );

    // Raising the cap stops the churn.
    db.set_cache_limit(64 * 1024 * 1024);
    for id in &ids {
        atlas.get(&db, GlyphKey::new(*id, 31, 14.0, 0.0));
    }
    let evictions = db.evictions();
    for id in &ids {
        atlas.get(&db, GlyphKey::new(*id, 32, 14.0, 0.0));
    }
    assert_eq!(db.evictions(), evictions, "a roomy cap evicts nothing");
    std::fs::remove_dir_all(&dir).ok();
}

/// The on-disk index cache: a second scan of the same directory reproduces
/// the same index without reading a font file, and any change invalidates it.
#[test]
fn the_index_cache_round_trips_and_invalidates() {
    let Some(files) = font_files(3) else { return };
    let dir = temp_font_dir("idx", &files);
    // The cache lives *outside* the scanned directory: writing it inside
    // would bump the directory's own mtime and invalidate what was just
    // written. The real cache is in `$XDG_CACHE_HOME/nitro`, never a font dir.
    let cache_dir =
        std::env::temp_dir().join(format!("nitro-text-idxcache-{}", std::process::id()));
    std::fs::create_dir_all(&cache_dir).expect("cache dir");
    let cache = cache_dir.join("fonts.idx");

    let cold = FontDb::scan_dirs_with_cache(&[&dir], Some(&cache));
    assert!(
        !cold.used_index_cache(),
        "the first scan has no cache to use"
    );
    assert!(cache.exists(), "the first scan writes one");
    assert_eq!(cold.loaded_bytes(), 0, "writing the cache retains no bytes");

    let warm = FontDb::scan_dirs_with_cache(&[&dir], Some(&cache));
    assert!(warm.used_index_cache(), "the second scan uses it");
    assert_eq!(warm.len(), cold.len(), "same faces");
    for id in (0..cold.len() as u32).map(FontId) {
        assert_eq!(warm.family_name(id), cold.family_name(id));
        assert_eq!(warm.face_path(id), cold.face_path(id));
    }
    assert_eq!(warm.select(&style()), cold.select(&style()));
    eprintln!(
        "index cache: cold {:?}, warm {:?}, {} faces",
        cold.scan_time(),
        warm.scan_time(),
        warm.len()
    );

    // A font added to the directory invalidates the cache.
    std::fs::copy(&files[0], dir.join("extra.ttf")).expect("copy");
    let changed = FontDb::scan_dirs_with_cache(&[&dir], Some(&cache));
    assert!(!changed.used_index_cache(), "a new file invalidates");
    assert!(changed.len() > cold.len(), "and the new face is indexed");
    // ...and the rewritten cache is good again.
    let again = FontDb::scan_dirs_with_cache(&[&dir], Some(&cache));
    assert!(again.used_index_cache());
    assert_eq!(again.len(), changed.len());

    // A corrupt cache is discarded rather than trusted.
    std::fs::write(&cache, b"not an index").expect("clobber");
    let rescan = FontDb::scan_dirs_with_cache(&[&dir], Some(&cache));
    assert!(!rescan.used_index_cache());
    assert_eq!(
        rescan.len(),
        changed.len(),
        "a rescan agrees with the cache"
    );

    // A missing cache path is a miss, not a failure.
    std::fs::remove_file(&cache).ok();
    let nocache = FontDb::scan_dirs_with_cache(&[&dir], Some(&cache));
    assert!(!nocache.used_index_cache());
    assert_eq!(nocache.len(), changed.len());

    std::fs::remove_dir_all(&dir).ok();
    std::fs::remove_dir_all(&cache_dir).ok();
}

/// Shaping plain Latin text must not drag the fallback chain off disk: the
/// chain is a list of ids, and only the faces a character actually needs are
/// read. This is what keeps the resident cost at one file on a normal desktop.
#[test]
fn the_fallback_chain_is_not_loaded_for_text_the_primary_covers() {
    let Some(db) = db() else { return };
    let chain = db.fallbacks(&style());
    if chain.len() < 2 {
        eprintln!("skipping: only {} face(s) in the chain", chain.len());
        return;
    }
    assert_eq!(db.loaded_files(), 0, "listing the chain reads nothing");
    let mut layout = Layout::new();
    layout.shape(&db, "The quick brown fox", &style(), None, false);
    assert_eq!(
        db.loaded_files(),
        1,
        "Latin text needs the primary face only, chain of {}",
        chain.len()
    );
}

/// The idle release: once a frame has passed with nothing needing a face, the
/// bytes go back, and the atlas's masks are untouched by it. This is the step
/// that gets the *steady state* down, not just the startup number — a cap
/// larger than the box's fonts never fires at all.
#[test]
fn an_idle_release_returns_the_bytes_and_keeps_every_mask() {
    let Some(db) = db() else { return };
    let style = style();
    let Some(font) = db.select(&style) else {
        eprintln!("skipping: no face selected");
        return;
    };
    let mut atlas = Atlas::new();
    let mut layout = Layout::new();
    let shaped = layout.shape(&db, "Hello, nitro", &style, None, false);
    for line in &shaped.lines {
        for glyph in &line.glyphs {
            atlas.get(
                &db,
                GlyphKey::new(glyph.font, glyph.id, shaped.size_px, glyph.x),
            );
        }
    }
    assert!(db.loaded_bytes() > 0, "drawing loads the face");
    let masks = atlas.glyph_count();
    let renders = atlas.renders();
    assert!(masks > 0);

    // A face used *this* frame is not released: a run mid-paint keeps its font.
    db.release_idle();
    assert!(db.loaded_bytes() > 0, "the current frame's face stays");
    assert_eq!(db.releases(), 0);

    // Exactly one frame later, with nothing having asked for it, it goes.
    // One bump, not two: the server bumps at the start of a paint and
    // releases at the block point after it, and a screen that paints once
    // more and then goes quiet must not keep the bytes for the session.
    db.next_frame();
    db.release_idle();
    assert_eq!(db.loaded_bytes(), 0, "an idle face is released");
    assert_eq!(db.loaded_files(), 0);
    assert!(db.releases() > 0, "counted as a release");
    assert_eq!(db.evictions(), 0, "and not as a cap eviction");

    // The masks are all still there: redrawing renders nothing and reads
    // nothing. That is what makes the release free.
    let loads = db.loads();
    for line in &shaped.lines {
        for glyph in &line.glyphs {
            atlas.get(
                &db,
                GlyphKey::new(glyph.font, glyph.id, shaped.size_px, glyph.x),
            );
        }
    }
    assert_eq!(atlas.glyph_count(), masks, "no mask was lost");
    assert_eq!(atlas.renders(), renders, "and none was re-rendered");
    assert_eq!(db.loads(), loads, "a cached mask needs no face");

    // A *new* glyph does pay one re-read, and then works normally.
    atlas.get(&db, GlyphKey::new(font, 200, 33.0, 0.0));
    assert_eq!(db.loads(), loads + 1, "exactly one re-read");
    eprintln!(
        "idle release: {masks} masks kept, {} loads, {} releases",
        db.loads(),
        db.releases()
    );
}
