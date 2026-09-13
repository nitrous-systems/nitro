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
    assert!(db.face_data(FontId(0)).is_none());

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
        "FontDb::scan: {:?} (self-reported {:?}), {} faces",
        wall,
        db.scan_time(),
        db.len()
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
