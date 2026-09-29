//! The `Surface` path in the toolkit: `SurfaceView`, `present_surface`,
//! the events that come back, and the window-state request (#3906).

use std::os::fd::AsFd as _;

use nitro_core::{Color, IRect, Size};
use nitro_shm::MappingMut;
use nitro_ui::build::StyleBuilder as _;
use nitro_ui::surface::{OverlayAlign, SurfaceEvent, SurfaceView, surface_view};
use nitro_ui::test::Harness;
use nitro_ui::widgets::panel;
use nitro_ui::{Ui, WidgetId};
use nitro_wire::msg::{CreateSurfaceBuffer, PresentSurface};
use nitro_wire::types::{BufferId, ColorMatrix, ColorRange, WindowState, format};

#[derive(Default)]
struct St {
    view: Option<WidgetId>,
    events: Vec<SurfaceEvent>,
    states: Vec<WindowState>,
}

const W: u32 = 64;
const H: u32 = 32;
const BAR: Color = Color::rgb(0xd0, 0x20, 0x20);

fn tree(ui: &mut Ui<St>) -> WidgetId {
    ui.enable_surfaces();
    ui.on_surface(|s: &mut St, _ui: &mut Ui<St>, ev: &SurfaceEvent| s.events.push(*ev));
    ui.on_window_state(|s: &mut St, _ui: &mut Ui<St>, ws| s.states.push(ws));
    ui.build(surface_view().overlay(
        panel().background(BAR).radius(0.0).height(20.0),
        OverlayAlign::Bottom,
    ))
}

fn harness() -> Harness<St> {
    let mut h = Harness::sized("surface", St::default(), Size::new(200.0, 100.0), tree);
    let root = h.ui().root().expect("root");
    h.state_mut().view = Some(root);
    h.settle();
    h
}

/// A white NV12 buffer, registered and flushed.
fn white_buffer(h: &mut Harness<St>) -> (BufferId, MappingMut) {
    let len = (W * H * 3 / 2) as usize;
    let fd = nitro_shm::create_sealed("surface-test", len as u64).expect("memfd");
    let mut map = MappingMut::map_mut(fd.as_fd(), len).expect("map");
    let (y, uv) = map.as_bytes_mut().split_at_mut((W * H) as usize);
    y.fill(235);
    uv.fill(128);
    let id = h.ui().alloc_buffer_id();
    h.ui()
        .create_surface_buffer(CreateSurfaceBuffer {
            id,
            width: W,
            height: H,
            format: format::NV12,
            size: len as u32,
            offset0: 0,
            stride0: W,
            offset1: W * H,
            stride1: W,
            fd,
        })
        .expect("create");
    h.ui().flush().expect("flush");
    h.settle();
    (id, map)
}

fn present(h: &mut Harness<St>, buffer: BufferId) -> u32 {
    let view = h.state().view.expect("view");
    let node = h.widget::<SurfaceView<St>>(view).node();
    assert!(!node.is_none(), "the view painted a surface node");
    let serial = h.ui().next_serial();
    h.ui()
        .present_surface(PresentSurface {
            id: node,
            buffer,
            serial,
            src: IRect::new(0, 0, W.cast_signed(), H.cast_signed()),
            matrix: ColorMatrix::Bt709,
            range: ColorRange::Limited,
            damage: Vec::new(),
        })
        .expect("present");
    serial
}

#[test]
fn surfaces_are_granted_when_asked_for() {
    let mut h = harness();
    assert!(h.ui().has_surfaces());
}

#[test]
fn without_asking_there_are_no_surfaces() {
    let mut h = Harness::new("nosurface", (), |ui| ui.build(surface_view()));
    assert!(!h.ui().has_surfaces());
}

#[test]
fn a_remote_link_masks_surfaces() {
    let mut h = Harness::remote("remotesurface", (), |ui| {
        ui.enable_surfaces();
        ui.build(surface_view())
    });
    assert!(!h.ui().has_surfaces());
}

#[test]
fn presented_and_released_reach_on_surface_and_the_overlay_is_on_top() {
    let mut h = harness();
    let (a, _map_a) = white_buffer(&mut h);
    let (b, _map_b) = white_buffer(&mut h);
    let s1 = present(&mut h, a);
    h.wait_for("Presented for the first frame", |h| {
        h.pump();
        h.state()
            .events
            .iter()
            .any(|e| matches!(e, SurfaceEvent::Presented { serial, .. } if *serial == s1))
    });

    // The video shows white above the bar; the bar paints over it.
    let pos = h.ui().window_position();
    let shot = h.server().shot();
    let px = |x: f32, y: f32| shot.pixel((pos.x + x) as u32, (pos.y + y) as u32);
    let top = px(100.0, 20.0);
    for c in [top >> 16, top >> 8, top] {
        assert!((c & 0xff) > 0xe0, "white video at the top: {top:#08x}");
    }
    let bar = px(100.0, 90.0);
    assert_eq!(bar & 0xff_ffff, 0xd0_2020, "the overlay is above the video");

    // A second frame replaces the first: its buffer comes back.
    let _ = present(&mut h, b);
    h.wait_for("BufferReleased for the first buffer", |h| {
        h.pump();
        h.state().events.contains(&SurfaceEvent::Released(a))
    });
}

#[test]
fn fullscreen_is_configured_to_the_output() {
    let mut h = harness();
    h.ui()
        .set_window_state(WindowState::Fullscreen)
        .expect("state");
    h.ui().flush().expect("flush");
    h.wait_for("WindowState(Fullscreen)", |h| {
        h.pump();
        h.ui().window_state() == WindowState::Fullscreen
    });
    h.settle();
    let (w, h_) = nitro_ui::test::OUTPUT;
    assert_eq!(h.ui().window_size(), Size::new(w as f32, h_ as f32));
    assert_eq!(h.state().states.last(), Some(&WindowState::Fullscreen));
    // The view follows the window.
    let view = h.state().view.expect("view");
    assert_eq!(h.bounds(view).size(), Size::new(w as f32, h_ as f32));
}

#[derive(Default)]
struct Dma {
    view: Option<WidgetId>,
    events: Vec<SurfaceEvent>,
}

#[test]
fn dmabuf_feedback_arrives_and_a_linear_dmabuf_presents() {
    use nitro_wire::msg::{CreateDmabufBuffer, DmabufPlane};
    use nitro_wire::types::{NodeId, dmabuf_flags, modifier};
    let mut h = Harness::sized("dmabuf", Dma::default(), Size::new(200.0, 100.0), |ui| {
        ui.enable_dmabuf();
        ui.on_surface(|s: &mut Dma, _ui: &mut Ui<Dma>, ev: &SurfaceEvent| s.events.push(*ev));
        ui.build(surface_view())
    });
    let root = h.ui().root().expect("root");
    h.state_mut().view = Some(root);
    assert!(h.ui().has_surfaces());
    assert!(h.ui().has_dmabuf());
    h.wait_for("the default DmabufFeedback", |h| {
        h.pump();
        h.state()
            .events
            .contains(&SurfaceEvent::Feedback { node: NodeId::NONE })
    });
    let fb = h
        .ui()
        .dmabuf_feedback(NodeId::NONE)
        .expect("stored")
        .clone();
    let nv12 = fb
        .formats
        .iter()
        .find(|f| f.format == format::NV12 && f.modifier == modifier::LINEAR)
        .expect("linear NV12");
    assert_eq!(
        nv12.flags & (dmabuf_flags::CPU | dmabuf_flags::IMPORT),
        dmabuf_flags::CPU | dmabuf_flags::IMPORT
    );
    h.settle();

    // A sealed memfd stands in for a dma-buf on the fake backend.
    let len = (W * H * 3 / 2) as usize;
    let mut ids = Vec::new();
    for _ in 0..2 {
        let fd = nitro_shm::create_sealed("dmabuf-test", len as u64).expect("memfd");
        let dup = fd.try_clone().expect("dup");
        let id = h.ui().alloc_buffer_id();
        h.ui()
            .create_dmabuf_buffer(CreateDmabufBuffer {
                id,
                width: W,
                height: H,
                format: format::NV12,
                modifier: modifier::LINEAR,
                planes: vec![
                    DmabufPlane {
                        fd,
                        offset: 0,
                        stride: W,
                    },
                    DmabufPlane {
                        fd: dup,
                        offset: W * H,
                        stride: W,
                    },
                ],
            })
            .expect("create");
        ids.push(id);
    }
    h.ui().flush().expect("flush");
    h.settle();
    let node = h.widget::<SurfaceView<Dma>>(root).node();
    let present = |h: &mut Harness<Dma>, buffer| {
        let serial = h.ui().next_serial();
        h.ui()
            .present_surface(PresentSurface {
                id: node,
                buffer,
                serial,
                src: IRect::new(0, 0, W.cast_signed(), H.cast_signed()),
                matrix: ColorMatrix::Bt709,
                range: ColorRange::Limited,
                damage: Vec::new(),
            })
            .expect("present");
        serial
    };
    let s1 = present(&mut h, ids[0]);
    h.wait_for("Presented", |h| {
        h.pump();
        h.state()
            .events
            .iter()
            .any(|e| matches!(e, SurfaceEvent::Presented { serial, .. } if *serial == s1))
    });
    let _ = present(&mut h, ids[1]);
    h.wait_for("the first dma-buf released", |h| {
        h.pump();
        h.state().events.contains(&SurfaceEvent::Released(ids[0]))
    });
    assert!(h.ui().last_server_error().is_none());
}
