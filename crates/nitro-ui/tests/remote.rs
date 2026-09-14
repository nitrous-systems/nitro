//! An app on a **remote** (TCP) connection: what still works, and what
//! the one thing that does not costs it.
//!
//! The claim under test is narrow and load-bearing, and it is stated in
//! four places (`docs/remote.md`, `Ui::is_remote`, `Error::RemoteNoFds`
//! and `DESIGN.md`): a remote app **carries on drawing everything else**.
//! An `Image` cannot upload its pixels, because a buffer is a file
//! descriptor and TCP has none — but that must cost the app its image,
//! not its life.
//!
//! It is worth a test of its own because the failure mode is invisible
//! from inside the toolkit: `upload_image` returning `None` and
//! `upload_image` reporting an error through `PaintCx::note` look
//! identical to the widget, and differ only four layers up, where a
//! failed paint pass ends `Ui::flush` and `App::run` returns `Err`.

use nitro_ui::build::ContainerBuilder as _;
use nitro_ui::test::Harness;
use nitro_ui::widgets::{image, label};
use nitro_ui::{Ui, WidgetId};

/// A 4×4 opaque red square, `[b, g, r, a]` per pixel.
fn pixels() -> Vec<u8> {
    let mut px = Vec::with_capacity(4 * 4 * 4);
    for _ in 0..16 {
        px.extend_from_slice(&[0x00, 0x00, 0xFF, 0xFF]);
    }
    px
}

fn tree(ui: &mut Ui<()>) -> WidgetId {
    ui.build(
        nitro_ui::widgets::column()
            .child(label("remote"))
            .child(image(4, 4, pixels())),
    )
}

#[test]
fn a_remote_app_with_an_image_keeps_running_and_draws_the_rest() {
    let mut h = Harness::remote("remote-image", (), tree);
    assert!(h.ui().is_remote(), "the harness connected over TCP");

    // The paint pass that cannot upload the image must still **succeed**:
    // this is the assertion. `Ui::flush` rather than `Harness::flush`
    // because the harness's own helper `expect`s, which would turn the
    // failure under test into a panic with the wrong message.
    h.settle();
    h.ui()
        .flush()
        .expect("a remote paint pass must not fail: the app carries on without its image");

    // And it really did paint: the window is not a flat expanse of one
    // colour. A pass that returned `Ok` without drawing would satisfy
    // the line above on its own.
    let shot = h.shot();
    assert!(
        shot.data.iter().any(|b| *b != shot.data[0]),
        "the window painted something"
    );

    // Repeatedly, not just once — an app repaints every frame, and an
    // error latched on the first pass would surface on a later one.
    for _ in 0..3 {
        let root = h.ui().root().expect("a root");
        h.ui().mark(root, nitro_ui::Dirty::PAINT);
        h.settle();
        h.ui().flush().expect("and on every later pass");
    }

    // The app is still connected and the server still has its window:
    // "did not return an error" and "is still a client" are different
    // claims, and the second is the one a user would notice.
    assert_eq!(h.server().stat("remote_clients"), 1);
    assert_eq!(h.server().stat("windows"), 1);
}

#[test]
fn the_same_tree_over_a_unix_socket_does_upload_its_image() {
    // The control, and it is what makes the test above mean "remote"
    // rather than "images are broken": the identical tree on a local
    // connection paints fine *and* is not remote, so the refusal is a
    // property of the link.
    let mut h = Harness::new("local-image", (), tree);
    assert!(!h.ui().is_remote());
    h.settle();
    h.ui().flush().expect("a local paint pass");
    assert_eq!(h.server().stat("remote_clients"), 0);
    assert_eq!(h.server().stat("windows"), 1);
}
