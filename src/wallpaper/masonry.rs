use gtk4::prelude::*;
use gtk4::{
    Align, EventControllerMotion, EventControllerScroll, EventControllerScrollFlags, Fixed,
    DrawingArea, GestureClick, GestureDrag, Label, Overlay, Picture, ScrolledWindow,
};
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{BufRead, BufReader};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tracing::{debug, info, warn};

use crate::cache::ImageCache;
use crate::config::{xkb_keycodes, Config, KeysConfig, ZoomMod};
use crate::pinterest::{FeedManager, PinItem};

use super::menu::PinMenu;

/// A monitor's rectangle on the shared canvas, in pixels.
#[derive(Clone)]
pub struct MonitorSlot {
    pub name: String,
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

/// One masonry laid out across every monitor.
///
/// At scroll (0, 0) a column never crosses a monitor edge, so a pin stays on
/// one screen. Scrolling pans the whole canvas, and then a pin may slide
/// across the bezel. The pan is shared and unbounded in both axes.
pub struct Canvas {
    views: Vec<(String, Overlay)>,
}

impl Canvas {
    pub fn new(
        cfg: Arc<Config>,
        cache: Arc<ImageCache>,
        feed: Arc<tokio::sync::Mutex<FeedManager>>,
        runtime: Arc<tokio::runtime::Runtime>,
        monitors: Vec<MonitorSlot>,
    ) -> Self {
        let gap = cfg.gap.max(0);
        let origin_x = monitors.iter().map(|m| m.x).min().unwrap_or(0);
        let origin_y = monitors.iter().map(|m| m.y).min().unwrap_or(0);
        let reference = monitors
            .iter()
            .min_by_key(|m| (m.y, m.x))
            .map(|m| m.width)
            .unwrap_or(1)
            .max(1);
        let cols = cfg.columns.max(1) as i32;
        let (_, col_w) = column_geom(0, origin_x, reference, cols, gap);
        info!(
            "canvas grid: {cols} columns of {col_w}px on a {reference}px period, gap {gap}px"
        );

        let seams_x = vertical_seams(&monitors);
        let seams_y = horizontal_seams(&monitors);
        let max_tile_h = max_tile_height(&monitors, &seams_y, gap);

        let mut views = Vec::new();
        let mut fixeds = Vec::new();
        let mut scrolls = Vec::new();
        let mut spinners = Vec::new();
        let mut marks = Vec::new();
        let spin_angle = Rc::new(Cell::new(0.0));
        for mon in &monitors {
            let fixed = Fixed::new();
            fixed.set_size_request(mon.width, mon.height);
            fixed.set_overflow(gtk4::Overflow::Visible);
            // Both screens must share one huge child box. Otherwise the right
            // monitor's scroll stops at the last pin while the left one keeps
            // moving into that same content.
            pin_canvas_extent(&fixed);
            let scroll = ScrolledWindow::builder()
                .child(&fixed)
                .hscrollbar_policy(gtk4::PolicyType::External)
                .vscrollbar_policy(gtk4::PolicyType::External)
                .hexpand(true)
                .vexpand(true)
                .propagate_natural_width(false)
                .propagate_natural_height(false)
                .kinetic_scrolling(false)
                .overlay_scrolling(false)
                .build();
            scroll.add_css_class("pin-canvas");
            // The canvas is padded far past the pictures, so Gtk would keep a
            // scrollbar up after the visible pins have already loaded.
            bury_scrollbar(&scroll.hscrollbar());
            bury_scrollbar(&scroll.vscrollbar());
            scroll.set_size_request(mon.width, mon.height);
            scroll.set_max_content_width(mon.width);
            scroll.set_max_content_height(mon.height);
            let overlay = Overlay::new();
            overlay.set_child(Some(&scroll));
            overlay.set_size_request(mon.width, mon.height);

            let spinner = DrawingArea::new();
            spinner.set_content_width(44);
            spinner.set_content_height(44);
            spinner.set_halign(Align::Center);
            spinner.set_valign(Align::Center);
            spinner.set_can_target(false);
            spinner.set_visible(false);
            let angle = spin_angle.clone();
            spinner.set_draw_func(move |_, cr, w, h| draw_loader(cr, w, h, angle.get()));
            overlay.add_overlay(&spinner);

            let mark = Label::new(Some("+"));
            mark.add_css_class("autoscroll-mark");
            mark.set_halign(Align::Start);
            mark.set_valign(Align::Start);
            mark.set_can_target(false);
            mark.set_visible(false);
            overlay.add_overlay(&mark);

            views.push((mon.name.clone(), overlay));
            fixeds.push(fixed);
            scrolls.push(scroll);
            spinners.push(spinner);
            marks.push(mark);
        }

        let pan_moved = Rc::new(Cell::new(false));
        let typing = Rc::new(Cell::new(0));
        let menu = PinMenu::new(runtime.clone(), cache.clone(), &scrolls, typing.clone());
        let state = Rc::new(RefCell::new(State {
            monitors,
            fixeds,
            scrolls: scrolls.clone(),
            menu,
            columns: HashMap::new(),
            tiles: Vec::new(),
            library: Vec::new(),
            seen_pins: HashSet::new(),
            next_unique: 0,
            stalls: 0,
            feed_done: false,
            loading: false,
            scroll_x: 0.0,
            scroll_y: 0.0,
            vx: 0.0,
            vy: 0.0,
            fling_gen: 0,
            fling_armed: false,
            spring_gen: 0,
            over_x: 0.0,
            over_y: 0.0,
            dragging: false,
            pointer: None,
            auto_anchor: None,
            auto_gen: 0,
            spinners,
            marks,
            dir_x: 0.0,
            dir_y: 0.0,
            place_turn: 0,
            laid_x: f64::MAX,
            laid_y: f64::MAX,
            pan_moved: pan_moved.clone(),
            period: reference,
            cols,
            gap,
            origin_x,
            origin_y,
            seams_x,
            seams_y,
            max_tile_h,
            zoom: 1.0,
            zoom_target: 1.0,
            zoom_focus_x: 0.0,
            zoom_focus_y: 0.0,
            zoom_live: false,
        }));

        let ctx = Ctx {
            state: state.clone(),
            cache,
            feed,
            runtime,
            syncing: Rc::new(Cell::new(false)),
            typing,
            ensure_queued: Rc::new(Cell::new(false)),
            spin_angle,
            spin_live: Rc::new(Cell::new(false)),
            paint_queued: Rc::new(Cell::new(false)),
            keys: InputBindings::from_config(&cfg.keys),
        };
        wire_viewports(&ctx);
        wire_drag(&ctx, &scrolls);
        wire_autoscroll(&ctx, &scrolls);
        install_input_watch(&cfg.keys);
        start_key_pan(&ctx);
        publish_scroll(&ctx, 0.0, 0.0);
        ensure(&ctx);

        Canvas { views }
    }

    pub fn widget(&self, name: &str) -> Option<&gtk4::Widget> {
        self.views
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, w)| w.upcast_ref())
    }
}

struct State {
    monitors: Vec<MonitorSlot>,
    fixeds: Vec<Fixed>,
    scrolls: Vec<ScrolledWindow>,
    menu: Rc<PinMenu>,
    columns: HashMap<i32, Column>,
    tiles: Vec<Tile>,
    library: Vec<Image>,
    seen_pins: HashSet<String>,
    next_unique: usize,
    stalls: u8,
    feed_done: bool,
    loading: bool,
    scroll_x: f64,
    scroll_y: f64,
    vx: f64,
    vy: f64,
    fling_gen: u64,
    fling_armed: bool,
    spring_gen: u64,
    /// Finger travel past the pictures. The view shows `rubber(over)`, not this raw value.
    over_x: f64,
    over_y: f64,
    dragging: bool,
    /// Cursor position in the shared monitor layout, so both screens agree.
    pointer: Option<(f64, f64)>,
    /// Middle-click autoscroll anchor in the same coordinates. None when idle.
    auto_anchor: Option<(f64, f64)>,
    auto_gen: u64,
    spinners: Vec<DrawingArea>,
    marks: Vec<Label>,
    /// Last scroll direction, kept so the next pins grow that way, diagonals included.
    dir_x: f64,
    dir_y: f64,
    place_turn: usize,
    laid_x: f64,
    laid_y: f64,
    /// Set while a pointer drag is in progress, so the click does not open a pin.
    pan_moved: Rc<Cell<bool>>,
    period: i32,
    cols: i32,
    gap: i32,
    origin_x: i32,
    origin_y: i32,
    seams_x: Vec<i32>,
    seams_y: Vec<i32>,
    max_tile_h: i32,
    /// Display scale. 1 shows the columns at their laid-out size.
    zoom: f64,
    zoom_target: f64,
    /// Screen point that stays still while the scale eases toward the target.
    zoom_focus_x: f64,
    zoom_focus_y: f64,
    zoom_live: bool,
}

struct Column {
    /// Y where the next pin downward starts.
    down: i32,
    /// Y just above the top pin; the next upward pin ends here.
    up: i32,
    started: bool,
}

#[derive(Clone)]
struct Image {
    path: PathBuf,
    aspect: f64,
    link: Option<String>,
    pin_id: String,
}

struct Tile {
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    path: PathBuf,
    link: Option<String>,
    pin_id: String,
    views: Vec<Option<Picture>>,
}

#[derive(Clone)]
struct Ctx {
    state: Rc<RefCell<State>>,
    cache: Arc<ImageCache>,
    feed: Arc<tokio::sync::Mutex<FeedManager>>,
    runtime: Arc<tokio::runtime::Runtime>,
    syncing: Rc<Cell<bool>>,
    /// Entries in the pin sheet that currently have the keyboard.
    typing: Rc<Cell<i32>>,
    ensure_queued: Rc<Cell<bool>>,
    spin_angle: Rc<Cell<f64>>,
    spin_live: Rc<Cell<bool>>,
    /// A drag can deliver several motions per frame. One paint is enough.
    paint_queued: Rc<Cell<bool>>,
    keys: InputBindings,
}

const PRELOAD: i32 = 640;
const PLACE_BUDGET: usize = 1;
/// Shifts canvas coordinates into the positive range so the first screen can
/// scroll left. Gtk will not show children at negative positions.
const CANVAS_PAD: f64 = 80_000.0;
/// Visual stretch at the edge, and the raw finger travel that maps into it.
const RUBBER_CAP: f64 = 120.0;
const RUBBER_RANGE: f64 = 240.0;
/// Inertia dies after this much travel, so a flick cannot fly into empty black.
const FLING_MAX: f64 = 640.0;
const FLING_VMAX: f64 = 2200.0;
const ZOOM_MIN: f64 = 0.45;
const ZOOM_MAX: f64 = 3.2;
const ZOOM_TAU: f64 = 0.12;

#[derive(Clone)]
struct InputBindings {
    zoom: Vec<ZoomMod>,
    zoom_step: f64,
    move_speed: f64,
}

impl InputBindings {
    fn from_config(keys: &KeysConfig) -> Self {
        Self {
            zoom: keys.zoom_mods(),
            zoom_step: keys.zoom_step(),
            move_speed: keys.move_speed(),
        }
    }
}

enum Cover {
    Done,
    Budget,
    NeedImages,
}

fn wire_viewports(ctx: &Ctx) {
    let n = ctx.state.borrow().scrolls.len();
    for index in 0..n {
        let (scroll, width, height) = {
            let st = ctx.state.borrow();
            let mon = &st.monitors[index];
            (st.scrolls[index].clone(), mon.width as f64, mon.height as f64)
        };
        install_axis(ctx, index, true, &scroll.hadjustment(), width);
        install_axis(ctx, index, false, &scroll.vadjustment(), height);
    }
}

/// Gtk keeps rewriting the adjustment to the child's size. Put the padded
/// range back and keep the value we chose, on every monitor the same way.
fn install_axis(ctx: &Ctx, index: usize, horizontal: bool, adj: &gtk4::Adjustment, page: f64) {
    configure_axis(adj, page);
    let ctx_range = ctx.clone();
    let fixing = Rc::new(Cell::new(false));
    let fixing_changed = fixing.clone();
    adj.connect_changed(move |adj| {
        if fixing_changed.get() {
            return;
        }
        // Gtk sizes the range to whichever pictures this monitor happens to
        // hold. The right screen then stops a full monitor earlier than the
        // left. Put the shared range back, even while we are scrolling.
        fixing_changed.set(true);
        let want = {
            let st = ctx_range.state.borrow();
            axis_value(&st, index, horizontal)
        };
        configure_axis(adj, page);
        adj.set_value(want);
        fixing_changed.set(false);
    });
    // Adding a pin changes the child size and Gtk shoves the viewport into the
    // empty padding. Put it back on the pictures; this is not a user scroll.
    let ctx_value = ctx.clone();
    adj.connect_value_changed(move |adj| {
        if ctx_value.syncing.get() {
            return;
        }
        let want = {
            let st = ctx_value.state.borrow();
            axis_value(&st, index, horizontal)
        };
        if (adj.value() - want).abs() < 0.5
            && (adj.upper() - CANVAS_PAD * 2.0).abs() < 1.0
        {
            return;
        }
        ctx_value.syncing.set(true);
        configure_axis(adj, page);
        adj.set_value(want);
        ctx_value.syncing.set(false);
    });
}

/// Keep the fixed's box as wide as the padded canvas, on every monitor.
fn pin_canvas_extent(fixed: &Fixed) {
    for (x, y) in [(0.0, 0.0), (CANVAS_PAD * 2.0 - 1.0, CANVAS_PAD * 2.0 - 1.0)] {
        let dot = DrawingArea::new();
        dot.set_size_request(1, 1);
        dot.set_can_target(false);
        fixed.put(&dot, x, y);
    }
}

fn bury_scrollbar(bar: &impl WidgetExt) {
    bar.set_visible(false);
    bar.set_can_target(false);
    bar.connect_map(|bar| {
        bar.set_visible(false);
    });
    bar.connect_show(|bar| {
        bar.set_visible(false);
    });
}

/// Drop Gtk's wheel handler. Our controller is named `pin-wheel` and stays.
fn silence_builtin_scroll(scroll: &ScrolledWindow) {
    let model = scroll.observe_controllers();
    for i in 0..model.n_items() {
        let Some(obj) = model.item(i) else {
            continue;
        };
        let Ok(ctrl) = obj.downcast::<EventControllerScroll>() else {
            continue;
        };
        if ctrl.name().as_deref() == Some("pin-wheel") {
            continue;
        }
        ctrl.set_propagation_phase(gtk4::PropagationPhase::None);
    }
}

fn configure_axis(adj: &gtk4::Adjustment, page: f64) {
    if adj.lower().abs() < 0.1
        && (adj.upper() - CANVAS_PAD * 2.0).abs() < 1.0
        && (adj.page_size() - page).abs() < 0.5
    {
        return;
    }
    adj.set_lower(0.0);
    adj.set_upper(CANVAS_PAD * 2.0);
    adj.set_page_size(page);
    adj.set_step_increment(48.0);
    adj.set_page_increment(page * 0.8);
}

fn close_menu(ctx: &Ctx) {
    let menu = ctx.state.borrow().menu.clone();
    menu.dismiss();
}

fn attach_wheel(ctx: &Ctx, widget: &impl IsA<gtk4::Widget>, host: Option<&ScrolledWindow>) {
    let wheel = EventControllerScroll::new(EventControllerScrollFlags::BOTH_AXES);
    wheel.set_propagation_phase(gtk4::PropagationPhase::Capture);
    let idle = Rc::new(Cell::new(0u64));
    let ctx_wheel = ctx.clone();
    wheel.connect_scroll_begin(move |_| {
        let mut st = ctx_wheel.state.borrow_mut();
        st.fling_gen = st.fling_gen.wrapping_add(1);
        st.fling_armed = false;
        st.spring_gen = st.spring_gen.wrapping_add(1);
        st.vx = 0.0;
        st.vy = 0.0;
    });
    let ctx_wheel = ctx.clone();
    let host_wheel = host.cloned();
    wheel.connect_scroll(move |ctrl, dx, dy| {
        if scroll_inside_sheet(ctrl) {
            return glib::Propagation::Proceed;
        }
        close_menu(&ctx_wheel);
        let surface = matches!(ctrl.unit(), gdk4::ScrollUnit::Surface);
        let mods = ctrl.current_event_state();
        if dy.abs() > 0.01 && zoom_chord_held(&ctx_wheel, mods) {
            // Wheel up (negative dy) zooms in. A notch is one step; a
            // trackpad sends pixels, which land between notches.
            let steps = if surface { -dy / 80.0 } else { -dy };
            zoom_by(&ctx_wheel, steps.clamp(-1.5, 1.5));
            return glib::Propagation::Stop;
        }
        let scale = if surface { 1.0 } else { 52.0 };
        nudge(&ctx_wheel, dx * scale, dy * scale);
        // A notch often never sends scroll-end, so the stretch would stay
        // and the loader would keep spinning after the pins arrived.
        let gen = idle.get().wrapping_add(1);
        idle.set(gen);
        let idle_later = idle.clone();
        let ctx_idle = ctx_wheel.clone();
        let host_idle = host_wheel.clone();
        glib::timeout_add_local_once(std::time::Duration::from_millis(80), move || {
            if idle_later.get() != gen {
                return;
            }
            if let Some(host) = host_idle {
                start_spring(&ctx_idle, &host);
            }
        });
        glib::Propagation::Stop
    });
    let ctx_end = ctx.clone();
    let host_end = host.cloned();
    wheel.connect_scroll_end(move |_| {
        let dragging = ctx_end.state.borrow().dragging;
        if dragging {
            return;
        }
        if let Some(host) = host_end.clone() {
            start_spring(&ctx_end, &host);
        }
    });
    wheel.set_name(Some("pin-wheel"));
    widget.add_controller(wheel);
}

fn wire_drag(ctx: &Ctx, scrolls: &[ScrolledWindow]) {
    let host = scrolls.first().cloned();
    for scroll in scrolls {
        attach_wheel(ctx, scroll, host.as_ref());
        // Gtk's own wheel controller is also capture-phase and kinetic. It
        // keeps sliding the scrollbar across the padded canvas, and our
        // position reset means that animation never arrives, so the bar
        // spins after the pins have already loaded.
        silence_builtin_scroll(&scroll);
        let scroll_map = scroll.clone();
        scroll.connect_map(move |scroll| {
            scroll.hscrollbar().set_visible(false);
            scroll.vscrollbar().set_visible(false);
            silence_builtin_scroll(&scroll_map);
        });

        let drag = GestureDrag::new();
        drag.set_button(1);
        drag.set_propagation_phase(gtk4::PropagationPhase::Bubble);
        let last = Rc::new(Cell::new((0.0, 0.0)));
        let velocity = Rc::new(Cell::new((0.0, 0.0, 0i64)));
        let ctx_begin = ctx.clone();
        let last_begin = last.clone();
        let velocity_begin = velocity.clone();
        let scroll_begin = scroll.clone();
        drag.connect_drag_begin(move |_, _, _| {
            stop_autoscroll(&ctx_begin);
            let mut st = ctx_begin.state.borrow_mut();
            st.fling_gen = st.fling_gen.wrapping_add(1);
            st.fling_armed = false;
            st.spring_gen = st.spring_gen.wrapping_add(1);
            st.vx = 0.0;
            st.vy = 0.0;
            st.dragging = true;
            st.pan_moved.set(false);
            last_begin.set((0.0, 0.0));
            velocity_begin.set((0.0, 0.0, glib::monotonic_time()));
            drop(st);
            scroll_begin.set_cursor_from_name(Some("grabbing"));
        });
        let ctx_update = ctx.clone();
        let last_update = last.clone();
        let velocity_update = velocity.clone();
        drag.connect_drag_update(move |gesture, ox, oy| {
            if ox.hypot(oy) >= 4.0 {
                ctx_update.state.borrow().pan_moved.set(true);
                let _ = gesture.set_state(gtk4::EventSequenceState::Claimed);
            }
            let (px, py) = last_update.get();
            last_update.set((ox, oy));
            nudge(&ctx_update, -(ox - px), -(oy - py));
            let now = glib::monotonic_time();
            let (mut vx, mut vy, last_us) = velocity_update.get();
            let dt = (now - last_us) as f64 / 1_000_000.0;
            if (0.0..0.05).contains(&dt) {
                vx = vx * 0.35 + ((px - ox) / dt) * 0.65;
                vy = vy * 0.35 + ((py - oy) / dt) * 0.65;
            }
            let vx = clamp_speed(vx);
            let vy = clamp_speed(vy);
            velocity_update.set((vx, vy, now));
            let mut st = ctx_update.state.borrow_mut();
            st.vx = vx;
            st.vy = vy;
        });
        let ctx_end = ctx.clone();
        let scroll_end = scroll.clone();
        let host = host.clone();
        drag.connect_drag_end(move |_, _, _| {
            scroll_end.set_cursor_from_name(None);
            let (vx, vy, moved, over) = {
                let mut st = ctx_end.state.borrow_mut();
                st.dragging = false;
                (
                    st.vx,
                    st.vy,
                    st.pan_moved.get(),
                    st.over_x.abs().max(st.over_y.abs()),
                )
            };
            let ctx_clear = ctx_end.clone();
            glib::timeout_add_local_once(std::time::Duration::from_millis(120), move || {
                ctx_clear.state.borrow().pan_moved.set(false);
            });
            schedule_ensure(&ctx_end);
            let Some(host) = host.clone() else {
                return;
            };
            if over > 1.0 || !moved || (vx.abs() < 180.0 && vy.abs() < 180.0) {
                start_spring(&ctx_end, &host);
                return;
            }
            start_fling(&ctx_end, &host, vx, vy);
        });
        scroll.add_controller(drag);
    }
}

/// Middle click anchors a point. Moving the cursor away from it pans the grid,
/// the same way a browser does. Another middle click or a left drag stops it.
fn wire_autoscroll(ctx: &Ctx, scrolls: &[ScrolledWindow]) {
    let host = scrolls.first().cloned();
    for (index, scroll) in scrolls.iter().enumerate() {
        let motion = EventControllerMotion::new();
        motion.set_propagation_phase(gtk4::PropagationPhase::Capture);
        let ctx_motion = ctx.clone();
        motion.connect_motion(move |_, x, y| {
            let origin = {
                let st = ctx_motion.state.borrow();
                let mon = &st.monitors[index];
                (mon.x as f64, mon.y as f64)
            };
            ctx_motion.state.borrow_mut().pointer = Some((origin.0 + x, origin.1 + y));
        });
        scroll.add_controller(motion);

        let click = GestureClick::new();
        click.set_button(2);
        click.set_propagation_phase(gtk4::PropagationPhase::Capture);
        let ctx_click = ctx.clone();
        let host = host.clone();
        click.connect_pressed(move |gesture, _, x, y| {
            let _ = gesture.set_state(gtk4::EventSequenceState::Claimed);
            toggle_autoscroll(&ctx_click, index, x, y, host.as_ref());
        });
        scroll.add_controller(click);
    }
}

fn toggle_autoscroll(ctx: &Ctx, index: usize, x: f64, y: f64, host: Option<&ScrolledWindow>) {
    let already = ctx.state.borrow().auto_anchor.is_some();
    if already {
        stop_autoscroll(ctx);
        if let Some(host) = host {
            start_spring(ctx, host);
        }
        return;
    }
    let anchor = {
        let mut st = ctx.state.borrow_mut();
        let mon = &st.monitors[index];
        let anchor = (mon.x as f64 + x, mon.y as f64 + y);
        st.fling_gen = st.fling_gen.wrapping_add(1);
        st.fling_armed = false;
        st.spring_gen = st.spring_gen.wrapping_add(1);
        st.vx = 0.0;
        st.vy = 0.0;
        st.auto_anchor = Some(anchor);
        st.auto_gen = st.auto_gen.wrapping_add(1);
        for (i, mark) in st.marks.iter().enumerate() {
            let here = i == index;
            mark.set_visible(here);
            if here {
                mark.set_margin_start((x as i32 - 12).max(0));
                mark.set_margin_top((y as i32 - 14).max(0));
            }
        }
        for scroll in &st.scrolls {
            scroll.set_cursor_from_name(Some("all-scroll"));
        }
        st.auto_gen
    };
    if let Some(host) = host {
        start_autoscroll(ctx, host, anchor);
    }
}

fn stop_autoscroll(ctx: &Ctx) {
    let mut st = ctx.state.borrow_mut();
    if st.auto_anchor.is_none() {
        return;
    }
    st.auto_anchor = None;
    st.auto_gen = st.auto_gen.wrapping_add(1);
    for mark in &st.marks {
        mark.set_visible(false);
    }
    for scroll in &st.scrolls {
        scroll.set_cursor_from_name(None);
    }
}

fn start_autoscroll(ctx: &Ctx, host: &ScrolledWindow, gen: u64) {
    let ctx = ctx.clone();
    let last = Cell::new(glib::monotonic_time());
    host.add_tick_callback(move |_, clock| {
        let now = clock.frame_time();
        let mut dt = (now - last.get()) as f64 / 1_000_000.0;
        last.set(now);
        if !(0.0..0.05).contains(&dt) {
            dt = 0.016;
        }
        let (anchor, pointer) = {
            let st = ctx.state.borrow();
            if st.auto_gen != gen || st.auto_anchor.is_none() {
                return glib::ControlFlow::Break;
            }
            (st.auto_anchor, st.pointer)
        };
        let (Some(anchor), Some(pointer)) = (anchor, pointer) else {
            return glib::ControlFlow::Continue;
        };
        let vx = auto_speed(pointer.0 - anchor.0);
        let vy = auto_speed(pointer.1 - anchor.1);
        if vx != 0.0 || vy != 0.0 {
            nudge(&ctx, vx * dt, vy * dt);
        }
        glib::ControlFlow::Continue
    });
}

fn auto_speed(delta: f64) -> f64 {
    let past = delta.abs() - 16.0;
    if past <= 0.0 {
        return 0.0;
    }
    let speed = (past * 5.0 + past * past * 0.015).min(2200.0);
    speed.copysign(delta)
}

fn nudge(ctx: &Ctx, dx: f64, dy: f64) {
    let pulling = {
        let mut st = ctx.state.borrow_mut();
        if dx.abs() > 0.4 {
            st.dir_x = dx.signum();
        }
        if dy.abs() > 0.4 {
            st.dir_y = dy.signum();
        }
        // Deltas arrive in screen pixels. The canvas is stored unscaled, so a
        // zoomed-in grid moves fewer logical pixels for the same finger travel.
        let z = zoom_of(&st);
        let (dx, over_x) = absorb(st.over_x, dx);
        let (dy, over_y) = absorb(st.over_y, dy);
        let want_x = st.scroll_x + dx / z;
        let want_y = st.scroll_y + dy / z;
        let (cx, cy) = clamp_scroll(&st, want_x, want_y);
        st.scroll_x = cx;
        st.scroll_y = cy;
        st.over_x = (over_x + (want_x - cx) * z).clamp(-RUBBER_RANGE, RUBBER_RANGE);
        st.over_y = (over_y + (want_y - cy) * z).clamp(-RUBBER_RANGE, RUBBER_RANGE);
        st.over_x.abs() > 4.0 || st.over_y.abs() > 4.0
    };
    // While the finger is down, motions arrive faster than the display.
    // Painting each one reallocates the grid and the drag stutters.
    if ctx.state.borrow().dragging {
        request_paint(ctx);
    } else {
        paint(ctx);
    }
    if pulling {
        let ready = {
            let st = ctx.state.borrow();
            !st.loading && !st.feed_done
        };
        if ready {
            fetch_page(ctx);
        }
    }
    schedule_ensure(ctx);
}

/// Move the rejected part of a drag back out of the stretch before the pictures move.
fn absorb(pull: f64, delta: f64) -> (f64, f64) {
    if pull == 0.0 || delta == 0.0 || pull.signum() == delta.signum() {
        return (delta, pull);
    }
    let cancel = delta.abs().min(pull.abs());
    let pull = pull + delta.signum() * cancel;
    let rest = delta - delta.signum() * cancel;
    (rest, pull)
}

fn rubber(v: f64) -> f64 {
    let distance = v.abs();
    if distance < 0.4 {
        return 0.0;
    }
    v.signum() * RUBBER_CAP * (1.0 - (-distance / 90.0).exp())
}

fn axis_value(st: &State, index: usize, horizontal: bool) -> f64 {
    let mon = &st.monitors[index];
    let z = zoom_of(st);
    if horizontal {
        CANVAS_PAD + st.scroll_x * z + rubber(st.over_x) + mon.x as f64
    } else {
        CANVAS_PAD + st.scroll_y * z + rubber(st.over_y) + mon.y as f64
    }
}

fn zoom_of(st: &State) -> f64 {
    st.zoom.max(0.2)
}

/// Shift `scroll` so the logical point under `focus_screen` does not move
/// when the scale goes from `z0` to `z1`.
fn zoom_anchor(scroll: f64, focus_screen: f64, z0: f64, z1: f64) -> f64 {
    let z0 = z0.max(0.2);
    let z1 = z1.max(0.2);
    scroll + focus_screen * (1.0 / z0 - 1.0 / z1)
}

fn zoom_by(ctx: &Ctx, steps: f64) {
    if steps.abs() < 0.0001 {
        return;
    }
    let start = {
        let mut st = ctx.state.borrow_mut();
        st.fling_gen = st.fling_gen.wrapping_add(1);
        st.fling_armed = false;
        st.vx = 0.0;
        st.vy = 0.0;
        let (fx, fy) = st.pointer.unwrap_or_else(|| {
            let (x0, x1, y0, y1) = view_extent(&st);
            ((x0 + x1) * 0.5, (y0 + y1) * 0.5)
        });
        st.zoom_focus_x = fx;
        st.zoom_focus_y = fy;
        let step = ctx.keys.zoom_step;
        st.zoom_target = (st.zoom_target * step.powf(steps)).clamp(ZOOM_MIN, ZOOM_MAX);
        if st.zoom_live {
            false
        } else {
            st.zoom_live = true;
            true
        }
    };
    if start {
        start_zoom(ctx);
    }
}

fn start_zoom(ctx: &Ctx) {
    let host = ctx.state.borrow().scrolls[0].clone();
    let ctx = ctx.clone();
    let last = Cell::new(0i64);
    host.add_tick_callback(move |_, clock| {
        let now = clock.frame_time();
        let mut dt = if last.get() == 0 {
            0.016
        } else {
            (now - last.get()) as f64 / 1_000_000.0
        };
        last.set(now);
        if !(0.0..0.05).contains(&dt) {
            dt = 0.016;
        }
        let (done, starved) = {
            let mut st = ctx.state.borrow_mut();
            if !st.zoom_live {
                return glib::ControlFlow::Break;
            }
            let z0 = zoom_of(&st);
            let k = 1.0 - (-dt / ZOOM_TAU).exp();
            let mut z1 = z0 + (st.zoom_target - z0) * k;
            let done = (st.zoom_target - z1).abs() < 0.0006;
            if done {
                z1 = st.zoom_target;
                st.zoom_live = false;
            }
            st.scroll_x = zoom_anchor(st.scroll_x, st.zoom_focus_x, z0, z1);
            st.scroll_y = zoom_anchor(st.scroll_y, st.zoom_focus_y, z0, z1);
            st.zoom = z1;
            let (sx, sy) = clamp_scroll(&st, st.scroll_x, st.scroll_y);
            st.scroll_x = sx;
            st.scroll_y = sy;
            // A few new widgets per frame. Filling the whole zoomed-out view
            // here is what stalls the animation. While the scale is still
            // moving, existing pictures keep their pixel size and are only
            // transformed — resizing them every frame flashes the tile background.
            let budget = if done { 6 } else { 3 };
            let starved = retarget_limited(&mut st, budget, !done);
            st.laid_x = st.scroll_x;
            st.laid_y = st.scroll_y;
            (done, starved)
        };
        paint(&ctx);
        if done {
            if starved {
                reveal_rest(&ctx);
            } else {
                schedule_ensure(&ctx);
            }
            glib::ControlFlow::Break
        } else {
            glib::ControlFlow::Continue
        }
    });
}

/// Attach pictures that zoom-out uncovered, a handful at a time, then load
/// pins for whatever is still empty.
fn reveal_rest(ctx: &Ctx) {
    let ctx = ctx.clone();
    glib::timeout_add_local_once(std::time::Duration::from_millis(16), move || {
        if ctx.state.borrow().zoom_live {
            return;
        }
        let starved = {
            let mut st = ctx.state.borrow_mut();
            retarget_limited(&mut st, 6, false)
        };
        if starved {
            reveal_rest(&ctx);
        } else {
            schedule_ensure(&ctx);
        }
    });
}

fn request_paint(ctx: &Ctx) {
    if ctx.paint_queued.get() {
        return;
    }
    ctx.paint_queued.set(true);
    let ctx = ctx.clone();
    let Some(host) = ctx.state.borrow().scrolls.first().cloned() else {
        ctx.paint_queued.set(false);
        paint(&ctx);
        return;
    };
    host.add_tick_callback(move |_, _| {
        ctx.paint_queued.set(false);
        paint(&ctx);
        glib::ControlFlow::Break
    });
}

fn publish_scroll(ctx: &Ctx, sx: f64, sy: f64) {
    {
        let mut st = ctx.state.borrow_mut();
        let (sx, sy) = clamp_scroll(&st, sx, sy);
        st.scroll_x = sx;
        st.scroll_y = sy;
    }
    paint(ctx);
}

fn paint(ctx: &Ctx) {
    ctx.syncing.set(true);
    // Snapshot first. Changing an adjustment while `state` is borrowed re-enters
    // the changed handler, which borrows again and aborts the process.
    let frame = {
        let st = ctx.state.borrow();
        let axes: Vec<_> = st
            .monitors
            .iter()
            .enumerate()
            .map(|(i, mon)| {
                (
                    st.scrolls[i].clone(),
                    mon.width as f64,
                    mon.height as f64,
                    axis_value(&st, i, true),
                    axis_value(&st, i, false),
                )
            })
            .collect();
        place_spinner(&st);
        let spinning = st.spinners.iter().any(|spinner| spinner.is_visible());
        (axes, spinning)
    };
    for (scroll, width, height, hx, vy) in frame.0 {
        configure_axis(&scroll.hadjustment(), width);
        configure_axis(&scroll.vadjustment(), height);
        if (scroll.hadjustment().value() - hx).abs() >= 0.25 {
            scroll.hadjustment().set_value(hx);
        }
        if (scroll.vadjustment().value() - vy).abs() >= 0.25 {
            scroll.vadjustment().set_value(vy);
        }
    }
    ctx.syncing.set(false);
    if frame.1 {
        ensure_spin(ctx);
    } else {
        ctx.spin_live.set(false);
    }
}

fn ensure_spin(ctx: &Ctx) {
    if ctx.spin_live.get() {
        return;
    }
    ctx.spin_live.set(true);
    let ctx = ctx.clone();
    let host = ctx.state.borrow().scrolls[0].clone();
    let last = Cell::new(0i64);
    host.add_tick_callback(move |_, clock| {
        if !ctx.spin_live.get() {
            return glib::ControlFlow::Break;
        }
        let now = clock.frame_time();
        if last.get() == 0 {
            last.set(now);
        }
        let dt = ((now - last.get()) as f64 / 1_000_000.0).clamp(0.0, 0.05);
        last.set(now);
        let angle = ctx.spin_angle.get() + dt * std::f64::consts::TAU;
        ctx.spin_angle.set(angle);
        let st = ctx.state.borrow();
        for spinner in &st.spinners {
            if spinner.is_visible() {
                spinner.queue_draw();
            }
        }
        glib::ControlFlow::Continue
    });
}

fn draw_loader(cr: &gtk4::cairo::Context, width: i32, height: i32, angle: f64) {
    let cx = f64::from(width) / 2.0;
    let cy = f64::from(height) / 2.0;
    let radius = f64::from(width.min(height)) / 2.0 - 4.0;
    cr.set_line_cap(gtk4::cairo::LineCap::Round);
    cr.set_line_width(5.0);
    cr.set_source_rgba(0.0, 0.0, 0.0, 0.55);
    cr.arc(cx, cy, radius, angle, angle + std::f64::consts::PI * 1.35);
    let _ = cr.stroke();
    cr.set_line_width(3.2);
    cr.set_source_rgba(1.0, 1.0, 1.0, 1.0);
    cr.arc(cx, cy, radius, angle, angle + std::f64::consts::PI * 1.35);
    let _ = cr.stroke();
}

enum Side {
    Left,
    Right,
    Top,
    Bottom,
}

fn place_spinner(st: &State) {
    let side = spinner_side(st);
    let under_pointer = st.pointer.and_then(|(px, py)| {
        st.monitors.iter().position(|mon| {
            px >= mon.x as f64
                && px < (mon.x + mon.width) as f64
                && py >= mon.y as f64
                && py < (mon.y + mon.height) as f64
        })
    });
    for (i, spinner) in st.spinners.iter().enumerate() {
        let show = side.is_some() && under_pointer.map(|p| p == i).unwrap_or(true);
        spinner.set_visible(show);
        let (h, v, left, right, top, bottom) = match side {
            Some(Side::Left) => (Align::Start, Align::Center, 28, 0, 0, 0),
            Some(Side::Right) => (Align::End, Align::Center, 0, 28, 0, 0),
            Some(Side::Top) => (Align::Center, Align::Start, 0, 0, 24, 0),
            Some(Side::Bottom) | None => (Align::Center, Align::End, 0, 0, 0, 48),
        };
        spinner.set_halign(h);
        spinner.set_valign(v);
        spinner.set_margin_start(left);
        spinner.set_margin_end(right);
        spinner.set_margin_top(top);
        spinner.set_margin_bottom(bottom);
    }
}

fn spinner_side(st: &State) -> Option<Side> {
    if st.feed_done || !view_has_holes(st) {
        return None;
    }
    if st.over_x.abs() > 6.0 || st.over_y.abs() > 6.0 {
        return Some(if st.over_x.abs() >= st.over_y.abs() {
            if st.over_x > 0.0 {
                Side::Right
            } else {
                Side::Left
            }
        } else if st.over_y > 0.0 {
            Side::Bottom
        } else {
            Side::Top
        });
    }
    if st.loading {
        edge_hit(st)
    } else {
        None
    }
}

/// True when a column the user can see still has an empty gap. Pages fetched
/// for the ring around that view do not count.
fn view_has_holes(st: &State) -> bool {
    let (left, top, right, bottom) = view_bounds(st, 0);
    if bottom - top < 80 {
        return false;
    }
    let (k0, k1) = column_span(st.origin_x, st.period, st.cols, left, right);
    let center = top.saturating_add(bottom) / 2;
    let mut saw_column = false;
    for k in k0..=k1 {
        let (x, w) = column_geom(k, st.origin_x, st.period, st.cols, st.gap);
        if x + w <= left || x >= right || crosses(x, w, &st.seams_x) {
            continue;
        }
        saw_column = true;
        let spans = column_spans(&st.tiles, x, w);
        if !open_gaps(&spans, top, bottom, st.gap, center).is_empty() {
            return true;
        }
    }
    !saw_column && st.tiles.is_empty()
}

fn edge_hit(st: &State) -> Option<Side> {
    let (min_x, min_y, max_x, max_y) = content_span(st)?;
    let (min_mx, max_mr, min_my, max_mb) = logical_view_extent(st);
    let (lox, hix) = axis_limits(min_x, max_x, min_mx, max_mr, 0.0);
    let (loy, hiy) = axis_limits(min_y, max_y, min_my, max_mb, 0.0);
    let mut best_dist = 72.0 / zoom_of(st);
    let mut best = None;
    for (dist, side) in [
        (st.scroll_x - lox, Side::Left),
        (hix - st.scroll_x, Side::Right),
        (st.scroll_y - loy, Side::Top),
        (hiy - st.scroll_y, Side::Bottom),
    ] {
        if dist < best_dist {
            best_dist = dist;
            best = Some(side);
        }
    }
    best
}

fn clamp_speed(v: f64) -> f64 {
    v.clamp(-FLING_VMAX, FLING_VMAX)
}

/// Keep every screen overlapping pictures. A little slack lets the next page
/// of cards load, but the view cannot fly off into empty black.
fn clamp_scroll(st: &State, sx: f64, sy: f64) -> (f64, f64) {
    let Some((min_x, min_y, max_x, max_y)) = content_span(st) else {
        return (0.0, 0.0);
    };
    let (min_mx, max_mr, min_my, max_mb) = logical_view_extent(st);
    // The pictures stop here. Anything past them is the rubber-band stretch.
    (
        clamp_offset(sx, min_x, max_x, min_mx, max_mr, 0.0),
        clamp_offset(sy, min_y, max_y, min_my, max_mb, 0.0),
    )
}

fn logical_view_extent(st: &State) -> (f64, f64, f64, f64) {
    let (min_mx, max_mr, min_my, max_mb) = view_extent(st);
    let z = zoom_of(st);
    (min_mx / z, max_mr / z, min_my / z, max_mb / z)
}

fn view_extent(st: &State) -> (f64, f64, f64, f64) {
    let min_mx = st.monitors.iter().map(|m| m.x).min().unwrap_or(0) as f64;
    let max_mr = st
        .monitors
        .iter()
        .map(|m| m.x + m.width)
        .max()
        .unwrap_or(0) as f64;
    let min_my = st.monitors.iter().map(|m| m.y).min().unwrap_or(0) as f64;
    let max_mb = st
        .monitors
        .iter()
        .map(|m| m.y + m.height)
        .max()
        .unwrap_or(0) as f64;
    (min_mx, max_mr, min_my, max_mb)
}

fn content_span(st: &State) -> Option<(f64, f64, f64, f64)> {
    let mut min_x = f64::MAX;
    let mut min_y = f64::MAX;
    let mut max_x = f64::MIN;
    let mut max_y = f64::MIN;
    for tile in &st.tiles {
        min_x = min_x.min(tile.x as f64);
        min_y = min_y.min(tile.y as f64);
        max_x = max_x.max(tile.x as f64 + tile.w as f64);
        max_y = max_y.max(tile.y as f64 + tile.h as f64);
    }
    if min_x > max_x {
        None
    } else {
        Some((min_x, min_y, max_x, max_y))
    }
}

fn axis_limits(
    content_min: f64,
    content_max: f64,
    view_min: f64,
    view_max: f64,
    slack: f64,
) -> (f64, f64) {
    let lo = content_min - slack - view_min;
    let hi = content_max + slack - view_max;
    if lo <= hi {
        (lo, hi)
    } else {
        (0.0, 0.0)
    }
}

fn clamp_offset(
    scroll: f64,
    content_min: f64,
    content_max: f64,
    view_min: f64,
    view_max: f64,
    slack: f64,
) -> f64 {
    let lo = content_min - slack - view_min;
    let hi = content_max + slack - view_max;
    if lo <= hi {
        scroll.clamp(lo, hi)
    } else {
        0.0
    }
}

fn schedule_ensure(ctx: &Ctx) {
    if ctx.ensure_queued.get() {
        return;
    }
    ctx.ensure_queued.set(true);
    let ctx = ctx.clone();
    glib::timeout_add_local_once(std::time::Duration::from_millis(70), move || {
        ctx.ensure_queued.set(false);
        ensure(&ctx);
    });
}

fn start_fling(ctx: &Ctx, host: &ScrolledWindow, vx: f64, vy: f64) {
    let vx = clamp_speed(vx);
    let vy = clamp_speed(vy);
    let gen = {
        let mut st = ctx.state.borrow_mut();
        st.fling_armed = true;
        st.vx = vx;
        st.vy = vy;
        st.fling_gen = st.fling_gen.wrapping_add(1);
        st.fling_gen
    };
    let ctx = ctx.clone();
    let host_tick = host.clone();
    let last = Cell::new(glib::monotonic_time());
    let traveled = Cell::new(0.0);
    host.add_tick_callback(move |_, clock| {
        let now = clock.frame_time();
        let mut dt = (now - last.get()) as f64 / 1_000_000.0;
        last.set(now);
        if !(0.0..0.05).contains(&dt) {
            dt = 0.016;
        }
        let (stop, step_x, step_y) = {
            let mut st = ctx.state.borrow_mut();
            if st.fling_gen != gen {
                st.fling_armed = false;
                return glib::ControlFlow::Break;
            }
            // Short glide: fast decay and a hard cap on distance.
            let decay = (-dt / 0.22).exp();
            st.vx *= decay;
            st.vy *= decay;
            let mut step_x = st.vx * dt;
            let mut step_y = st.vy * dt;
            let step = (step_x * step_x + step_y * step_y).sqrt();
            let next_travel = traveled.get() + step;
            let hit_cap = if next_travel > FLING_MAX && step > 0.0 {
                let scale = ((FLING_MAX - traveled.get()).max(0.0) / step).min(1.0);
                traveled.set(FLING_MAX);
                step_x *= scale;
                step_y *= scale;
                true
            } else {
                traveled.set(next_travel);
                false
            };
            let stop = hit_cap || (st.vx.abs() < 80.0 && st.vy.abs() < 80.0);
            if stop {
                st.fling_armed = false;
                st.vx = 0.0;
                st.vy = 0.0;
            }
            (stop, step_x, step_y)
        };
        nudge(&ctx, step_x, step_y);
        let over = {
            let st = ctx.state.borrow();
            st.over_x.abs().max(st.over_y.abs())
        };
        if stop || over > 8.0 {
            {
                let mut st = ctx.state.borrow_mut();
                st.fling_armed = false;
                st.vx = 0.0;
                st.vy = 0.0;
            }
            start_spring(&ctx, &host_tick);
            glib::ControlFlow::Break
        } else {
            glib::ControlFlow::Continue
        }
    });
}

fn start_spring(ctx: &Ctx, host: &ScrolledWindow) {
    let Ok(mut st) = ctx.state.try_borrow_mut() else {
        // A click or a short drag can release the rubber band while the
        // same state is still borrowed. Defer instead of aborting.
        let ctx = ctx.clone();
        let host = host.clone();
        glib::idle_add_local_once(move || start_spring(&ctx, &host));
        return;
    };
    let gen = {
        if st.dragging || st.auto_anchor.is_some() {
            return;
        }
        if st.over_x.abs() < 0.8 && st.over_y.abs() < 0.8 {
            st.over_x = 0.0;
            st.over_y = 0.0;
            return;
        }
        st.spring_gen = st.spring_gen.wrapping_add(1);
        st.spring_gen
    };
    drop(st);
    let ctx = ctx.clone();
    let last = Cell::new(glib::monotonic_time());
    host.add_tick_callback(move |_, clock| {
        let now = clock.frame_time();
        let mut dt = (now - last.get()) as f64 / 1_000_000.0;
        last.set(now);
        if !(0.0..0.05).contains(&dt) {
            dt = 0.016;
        }
        let done = {
            let mut st = ctx.state.borrow_mut();
            if st.spring_gen != gen || st.dragging || st.auto_anchor.is_some() {
                return glib::ControlFlow::Break;
            }
            let decay = (-dt / 0.14).exp();
            st.over_x *= decay;
            st.over_y *= decay;
            if st.over_x.abs() < 0.8 && st.over_y.abs() < 0.8 {
                st.over_x = 0.0;
                st.over_y = 0.0;
                true
            } else {
                false
            }
        };
        paint(&ctx);
        if done {
            glib::ControlFlow::Break
        } else {
            glib::ControlFlow::Continue
        }
    });
}

fn ensure(ctx: &Ctx) {
    // Zoom-out already has to rescale every picture. Placing new pins in the
    // same frames is what makes the animation hitch.
    if ctx.state.borrow().zoom_live || ctx.state.borrow().dragging {
        // Building pictures while the finger is still moving is what freezes
        // a fast drag. The release schedules a fill.
        return;
    }
    // Gtk moves the viewport while pictures are inserted. Ignore that until
    // placement finishes, then put both screens back on the same offset.
    ctx.syncing.set(true);
    let mut again = false;
    let mut fetch_more = false;
    let (sx, sy) = {
        let mut st = ctx.state.borrow_mut();
        // Pins used to float in the middle of a hole, which left a black band
        // above and below them. Pull each column tight before placing more.
        compact_columns(&mut st);
        let moved = (st.scroll_x - st.laid_x).hypot(st.scroll_y - st.laid_y);
        if moved * zoom_of(&st) > 120.0 {
            retarget(&mut st);
            st.laid_x = st.scroll_x;
            st.laid_y = st.scroll_y;
        }
        // Keep cursors on the run that actually crosses the screen. A stack
        // that only sits above or below must not steal the next pins.
        anchor_columns(&mut st);
        // The open view first, then a ring around it. Each pin is used once.
        let outcome = match cover(&mut st, 0) {
            Cover::Done => cover(&mut st, PRELOAD),
            other => other,
        };
        match outcome {
            Cover::Budget => again = true,
            Cover::NeedImages => fetch_more = !st.feed_done,
            Cover::Done => {}
        }
        (st.scroll_x, st.scroll_y)
    };
    ctx.syncing.set(false);
    publish_scroll(ctx, sx, sy);
    if again {
        let ctx = ctx.clone();
        // One pin per beat, so a page does not appear as a single hitch.
        glib::timeout_add_local_once(std::time::Duration::from_millis(16), move || ensure(&ctx));
    }
    if fetch_more {
        fetch_page(ctx);
    }
    // The visible pins are in. Let go of the wheel stretch so the loader
    // does not stay up after the user has already stopped.
    let release = {
        let st = ctx.state.borrow();
        !st.dragging
            && !view_has_holes(&st)
            && st.over_x.abs().max(st.over_y.abs()) > 0.8
    };
    if release {
        let host = ctx.state.borrow().scrolls.first().cloned();
        if let Some(host) = host {
            start_spring(ctx, &host);
        }
    }
}

/// Point each column at the stack that crosses the screen. A stack that only
/// sits above or below stays where it is, so the empty view is filled first.
fn anchor_columns(st: &mut State) {
    let (left, top, right, bottom) = view_bounds(st, 0);
    let (k0, k1) = column_span(st.origin_x, st.period, st.cols, left, right);
    let gap = st.gap;
    for k in k0..=k1 {
        if !st.columns.contains_key(&k) {
            continue;
        }
        let (x, w) = column_geom(k, st.origin_x, st.period, st.cols, st.gap);
        if crosses(x, w, &st.seams_x) {
            continue;
        }
        let spans = column_spans(&st.tiles, x, w);
        let col = st.columns.get_mut(&k).unwrap();
        if let Some((run_top, run_bot)) = run_crossing(&spans, top, bottom, gap) {
            col.up = run_top - gap;
            col.down = run_bot + gap;
            col.started = true;
        } else {
            let y0 = top.max(st.origin_y + gap);
            col.up = y0;
            col.down = y0;
            col.started = false;
        }
    }
}

fn column_spans(tiles: &[Tile], x: i32, w: i32) -> Vec<(i32, i32)> {
    let mut spans: Vec<(i32, i32)> = tiles
        .iter()
        .filter(|tile| tile.x < x + w && tile.x + tile.w > x)
        .map(|tile| (tile.y, tile.y + tile.h))
        .collect();
    spans.sort_by_key(|span| span.0);
    spans
}

/// The continuous stack that overlaps the band. Pins separated only by the
/// column gutter belong to one stack. A hole does not glue two stacks together.
fn run_crossing(spans: &[(i32, i32)], band_top: i32, band_bot: i32, gutter: i32) -> Option<(i32, i32)> {
    let mut runs: Vec<(i32, i32)> = Vec::new();
    for &(top, bot) in spans {
        if let Some((_, run_bot)) = runs.last_mut() {
            if top - *run_bot <= gutter {
                *run_bot = (*run_bot).max(bot);
                continue;
            }
        }
        runs.push((top, bot));
    }
    runs.into_iter()
        .find(|(top, bot)| *bot > band_top && *top < band_bot)
}

#[cfg(test)]
fn span_distance(top: i32, bot: i32, view_top: i32, view_bot: i32) -> i32 {
    if bot >= view_top && top <= view_bot {
        0
    } else if bot < view_top {
        view_top - bot
    } else {
        top - view_bot
    }
}

fn cover(st: &mut State, margin: i32) -> Cover {
    // `margin` is a screen distance. The grid itself is stored unscaled.
    let margin = if margin <= 0 {
        0
    } else {
        ((margin as f64) / zoom_of(st)).round().max(1.0) as i32
    };
    let (pad_l, pad_t, pad_r, pad_b) = leading_pad(st, margin);
    let (left, top, right, bottom) = view_bounds_sides(st, pad_l, pad_t, pad_r, pad_b);
    let (vleft, vtop, vright, _) = view_bounds(st, 0);
    let (k0, k1) = column_span(st.origin_x, st.period, st.cols, left, right);
    let mut on_screen = Vec::new();
    let mut off_screen = Vec::new();
    for k in k0..=k1 {
        let (x, w) = column_geom(k, st.origin_x, st.period, st.cols, st.gap);
        if x + w <= left || x >= right || crosses(x, w, &st.seams_x) {
            continue;
        }
        if !st.columns.contains_key(&k) {
            // Start in the band on screen, so a diagonal step fills that corner
            // instead of growing the new column all the way from the top.
            let y0 = vtop.max(st.origin_y + st.gap);
            st.columns.insert(
                k,
                Column {
                    down: y0,
                    up: y0,
                    started: false,
                },
            );
        }
        let visible = x < vright && x + w > vleft;
        if visible {
            on_screen.push(k);
        } else {
            off_screen.push(k);
        }
    }
    // The open screen first. Once that is covered, the ring around it — the
    // next column as well as the band above and below — fills together.
    if margin <= 0 {
        match fill_columns(st, &on_screen, top, bottom) {
            Cover::Done => fill_columns(st, &off_screen, top, bottom),
            other => other,
        }
    } else {
        let mut all = on_screen;
        all.extend(off_screen);
        fill_columns(st, &all, top, bottom)
    }
}

/// A ring around the view: one column to either side and about a screen above
/// and below. The direction of travel gets a little more, the other sides
/// still load, so one strip cannot finish while its neighbor stays empty.
fn leading_pad(st: &State, margin: i32) -> (i32, i32, i32, i32) {
    if margin <= 0 {
        return (0, 0, 0, 0);
    }
    let pitch = (st.period / st.cols.max(1)).max(1);
    let beside = pitch + st.gap;
    let view_h = st
        .monitors
        .iter()
        .map(|mon| mon.height)
        .max()
        .unwrap_or(margin) as f64
        / zoom_of(st);
    let around = (view_h.round() as i32).max(margin);
    let extra = margin / 2;
    let (left, right) = if st.dir_x > 0.0 {
        (beside, beside + extra)
    } else if st.dir_x < 0.0 {
        (beside + extra, beside)
    } else {
        (beside, beside)
    };
    let (top, bottom) = if st.dir_y > 0.0 {
        (around, around + extra)
    } else if st.dir_y < 0.0 {
        (around + extra, around)
    } else {
        (around, around)
    };
    (left, top, right, bottom)
}

fn fill_columns(st: &mut State, columns: &[i32], top: i32, bottom: i32) -> Cover {
    if columns.is_empty() {
        return Cover::Done;
    }
    let mut order = columns.to_vec();
    if st.dir_x > 0.0 {
        order.sort_by_key(|k| -column_geom(*k, st.origin_x, st.period, st.cols, st.gap).0);
    } else if st.dir_x < 0.0 {
        order.sort_by_key(|k| column_geom(*k, st.origin_x, st.period, st.cols, st.gap).0);
    }
    let n = order.len();
    let start = st.place_turn % n;
    let center = top.saturating_add(bottom) / 2;
    let mut placed = 0usize;
    for step in 0..n {
        let k = order[(start + step) % n];
        let (x, w) = column_geom(k, st.origin_x, st.period, st.cols, st.gap);
        let spans = column_spans(&st.tiles, x, w);
        let gaps = open_gaps(&spans, top, bottom, st.gap, center);
        if gaps.is_empty() {
            continue;
        }
        let Some(image) = take_image(st) else {
            return Cover::NeedImages;
        };
        let landed = gaps.into_iter().any(|(gap_top, gap_bot)| {
            place_in_gap(st, k, x, w, &image, gap_top, gap_bot, bottom)
        });
        if !landed {
            st.next_unique = st.next_unique.saturating_sub(1);
            continue;
        }
        st.place_turn = start + step + 1;
        placed += 1;
        if placed >= PLACE_BUDGET {
            return Cover::Budget;
        }
    }
    if placed == 0 {
        Cover::Done
    } else {
        Cover::Budget
    }
}

fn take_image(st: &mut State) -> Option<Image> {
    if st.next_unique < st.library.len() {
        let image = st.library[st.next_unique].clone();
        st.next_unique += 1;
        return Some(image);
    }
    None
}

/// Empty stretches inside the band, nearest to `center` first. The gutter
/// between two stacked pins is not a hole.
fn open_gaps(
    spans: &[(i32, i32)],
    band_top: i32,
    band_bot: i32,
    gutter: i32,
    center: i32,
) -> Vec<(i32, i32)> {
    if band_bot - band_top < 80 {
        return Vec::new();
    }
    let mut runs: Vec<(i32, i32)> = Vec::new();
    for &(top, bot) in spans {
        if let Some((_, run_bot)) = runs.last_mut() {
            if top - *run_bot <= gutter {
                *run_bot = (*run_bot).max(bot);
                continue;
            }
        }
        runs.push((top, bot));
    }
    let mut free = Vec::new();
    let mut cursor = band_top;
    for (top, bot) in runs {
        // The same gutter as between columns, above and below each pin.
        let block_top = top - gutter;
        let block_bot = bot + gutter;
        if block_bot <= band_top {
            continue;
        }
        if block_top >= band_bot {
            break;
        }
        let block_top = block_top.max(band_top);
        if block_top > cursor {
            free.push((cursor, block_top));
        }
        cursor = cursor.max(block_bot.min(band_bot));
    }
    if cursor < band_bot {
        free.push((cursor, band_bot));
    }
    free.retain(|(top, bot)| bot - top >= 80);
    free.sort_by_key(|(top, bot)| {
        let mid = top.saturating_add(*bot) / 2;
        (mid - center).abs()
    });
    free
}

fn place_in_gap(
    st: &mut State,
    k: i32,
    x: i32,
    w: i32,
    image: &Image,
    gap_top: i32,
    gap_bot: i32,
    band_bot: i32,
) -> bool {
    let h = tile_height(w, image.aspect, st.max_tile_h);
    // A hole that stops before the band is blocked by the next pin. Sit on
    // that edge. An open hole grows down from the pin above. Either way the
    // only space left is the configured gutter, not a band in the middle.
    let bounded_below = gap_bot < band_bot;
    let packed = if bounded_below {
        fit_up(gap_bot, h, &st.seams_y, st.gap)
    } else {
        fit_down(gap_top, h, &st.seams_y, st.gap)
    };
    let y = if inside_gap(packed, h, gap_top, gap_bot) && !overlaps_column(st, x, w, packed, h) {
        packed
    } else if !bounded_below {
        let hung = fit_down(gap_top, h, &st.seams_y, st.gap);
        if hung >= gap_bot || overlaps_column(st, x, w, hung, h) {
            return false;
        }
        hung
    } else {
        return false;
    };
    let gap = st.gap;
    if let Some(col) = st.columns.get_mut(&k) {
        col.down = y + h + gap;
        col.up = y - gap;
        col.started = true;
    }
    push_tile(st, x, y, w, h, image);
    true
}

/// Close vertical holes inside a column. The top pin stays; every pin under
/// it moves up until only `gap` remains, unless a monitor seam is in the way.
fn compact_columns(st: &mut State) {
    let gap = st.gap.max(0);
    let seams = st.seams_y.clone();
    let mut groups: HashMap<i32, Vec<usize>> = HashMap::new();
    for (index, tile) in st.tiles.iter().enumerate() {
        groups.entry(tile.x).or_default().push(index);
    }
    let mut moved = false;
    for indices in groups.values_mut() {
        indices.sort_by_key(|&index| st.tiles[index].y);
        let Some(&first) = indices.first() else {
            continue;
        };
        let mut cursor = st.tiles[first].y;
        for &index in indices.iter() {
            let h = st.tiles[index].h;
            let y = fit_down(cursor, h, &seams, gap);
            if st.tiles[index].y != y {
                st.tiles[index].y = y;
                moved = true;
            }
            cursor = st.tiles[index].y + h + gap;
        }
    }
    if moved {
        retarget(st);
    }
}

fn inside_gap(y: i32, h: i32, gap_top: i32, gap_bot: i32) -> bool {
    y >= gap_top && y + h <= gap_bot
}

fn overlaps_column(st: &State, x: i32, w: i32, y: i32, h: i32) -> bool {
    let gap = st.gap.max(0);
    st.tiles.iter().any(|tile| {
        tile.x < x + w
            && tile.x + tile.w > x
            && tile.y < y + h + gap
            && tile.y + tile.h + gap > y
    })
}

fn push_tile(st: &mut State, x: i32, y: i32, w: i32, h: i32, image: &Image) {
    let mut tile = Tile {
        x,
        y,
        w,
        h,
        path: image.path.clone(),
        link: image.link.clone(),
        pin_id: image.pin_id.clone(),
        views: vec![None; st.monitors.len()],
    };
    let monitors = st.monitors.clone();
    let fixeds = st.fixeds.clone();
    let menu = st.menu.clone();
    let scroll_x = st.scroll_x;
    let scroll_y = st.scroll_y;
    let zoom = st.zoom;
    let mut budget = usize::MAX;
    sync_tile(
        &mut tile,
        &monitors,
        &fixeds,
        scroll_x,
        scroll_y,
        zoom,
        &menu,
        &mut budget,
        false,
    );
    st.tiles.push(tile);
}

fn retarget(st: &mut State) {
    let _ = retarget_limited(st, usize::MAX, false);
}

/// Move pictures that are already on screen. `budget` caps how many new ones
/// may be created; returns whether a visible pin was left without a widget.
/// `animating` keeps every existing picture and scales it instead of resizing.
fn retarget_limited(st: &mut State, mut budget: usize, animating: bool) -> bool {
    let monitors = st.monitors.clone();
    let fixeds = st.fixeds.clone();
    let menu = st.menu.clone();
    let scroll_x = st.scroll_x;
    let scroll_y = st.scroll_y;
    let zoom = st.zoom;
    let mut starved = false;
    for tile in &mut st.tiles {
        if sync_tile(
            tile,
            &monitors,
            &fixeds,
            scroll_x,
            scroll_y,
            zoom,
            &menu,
            &mut budget,
            animating,
        ) {
            starved = true;
        }
    }
    starved
}

fn sync_tile(
    tile: &mut Tile,
    monitors: &[MonitorSlot],
    fixeds: &[Fixed],
    scroll_x: f64,
    scroll_y: f64,
    zoom: f64,
    menu: &Rc<PinMenu>,
    budget: &mut usize,
    animating: bool,
) -> bool {
    let z = zoom.max(0.2);
    let (px, py, pw, ph) = tile_place(tile, z);
    let mut starved = false;
    for (i, mon) in monitors.iter().enumerate() {
        let view_x = scroll_x + mon.x as f64 / z;
        let view_y = scroll_y + mon.y as f64 / z;
        let view_w = mon.width as f64 / z;
        let view_h = mon.height as f64 / z;
        // Keep widgets that are already on screen a bit longer so a fast drag
        // does not destroy and recreate them every few pixels.
        let margin = if tile.views[i].is_some() {
            f64::from(PRELOAD + 700) / z
        } else {
            f64::from(PRELOAD) / z
        };
        let visible = tile.x as f64 + f64::from(tile.w) > view_x - margin
            && (tile.x as f64) < view_x + view_w + margin
            && tile.y as f64 + f64::from(tile.h) > view_y - margin
            && (tile.y as f64) < view_y + view_h + margin;
        if visible {
            if let Some(picture) = &tile.views[i] {
                place_picture(&fixeds[i], picture, px, py, pw, ph, animating);
            } else if *budget > 0 {
                *budget -= 1;
                let picture = make_picture(tile, menu, i, !animating);
                picture.set_size_request(pw, ph);
                fixeds[i].put(&picture, px, py);
                tile.views[i] = Some(picture);
            } else {
                starved = true;
            }
        } else if animating {
            if let Some(picture) = &tile.views[i] {
                place_picture(&fixeds[i], picture, px, py, pw, ph, true);
            }
        } else if let Some(picture) = tile.views[i].take() {
            fixeds[i].remove(&picture);
        }
    }
    starved
}

/// Position a pin on the canvas. During a zoom animation the widget keeps the
/// pixel size it already has and a scale transform does the rest, so GTK does
/// not repaint an empty rounded rectangle between frames.
fn place_picture(fixed: &Fixed, picture: &Picture, x: f64, y: f64, w: i32, h: i32, animating: bool) {
    if animating {
        let bw = picture.width_request().max(1) as f32;
        let bh = picture.height_request().max(1) as f32;
        let point = gtk4::graphene::Point::new(x as f32, y as f32);
        // Scale in the pin's own coordinates, then move the top-left onto the canvas.
        let transform = gtk4::gsk::Transform::new()
            .translate(&point)
            .scale(w as f32 / bw, h as f32 / bh);
        fixed.set_child_transform(picture, Some(&transform));
    } else {
        if picture.width_request() != w || picture.height_request() != h {
            picture.set_size_request(w, h);
        }
        let (cx, cy) = fixed.child_position(picture);
        if (cx - x).abs() > 0.5 || (cy - y).abs() > 0.5 {
            fixed.move_(picture, x, y);
        }
    }
}

fn tile_place(tile: &Tile, zoom: f64) -> (f64, f64, i32, i32) {
    let z = zoom.max(0.2);
    let w = (tile.w as f64 * z).round().max(1.0) as i32;
    let h = (tile.h as f64 * z).round().max(1.0) as i32;
    (
        tile.x as f64 * z + CANVAS_PAD,
        tile.y as f64 * z + CANVAS_PAD,
        w,
        h,
    )
}

fn make_picture(tile: &Tile, menu: &Rc<PinMenu>, monitor: usize, fade: bool) -> Picture {
    let picture = Picture::for_filename(&tile.path);
    picture.set_can_shrink(true);
    picture.set_size_request(tile.w, tile.h);
    picture.set_content_fit(gtk4::ContentFit::Cover);
    picture.add_css_class("pin-tile");
    if fade {
        picture.set_opacity(0.0);
        fade_in(&picture);
    }
    let aspect = f64::from(tile.h) / f64::from(tile.w.max(1));
    menu.attach(
        &picture,
        monitor,
        &tile.pin_id,
        tile.link.as_deref(),
        &tile.path,
        aspect,
    );
    picture
}

fn fade_in(picture: &Picture) {
    let start = Cell::new(0i64);
    picture.add_tick_callback(move |widget, clock| {
        let now = clock.frame_time();
        if start.get() == 0 {
            start.set(now);
        }
        let t = ((now - start.get()) as f64 / 1_000_000.0 / 0.45).clamp(0.0, 1.0);
        let eased = 1.0 - (1.0 - t).powi(3);
        widget.set_opacity(eased);
        if t >= 1.0 {
            widget.set_opacity(1.0);
            glib::ControlFlow::Break
        } else {
            glib::ControlFlow::Continue
        }
    });
}

fn fetch_page(ctx: &Ctx) {
    {
        let mut st = ctx.state.borrow_mut();
        if st.loading || st.feed_done {
            return;
        }
        st.loading = true;
    }
    paint(ctx);
    let ctx = ctx.clone();
    glib::spawn_future_local(async move {
        let feed = ctx.feed.clone();
        let runtime = ctx.runtime.clone();
        let result = runtime
            .spawn(async move {
                let mut mgr = feed.lock().await;
                mgr.next_page().await
            })
            .await;
        let pins = match result {
            Ok(Ok(pins)) => pins,
            Ok(Err(e)) => {
                warn!("feed page failed: {e:#}");
                ctx.state.borrow_mut().loading = false;
                paint(&ctx);
                return;
            }
            Err(e) => {
                warn!("feed task failed: {e}");
                ctx.state.borrow_mut().loading = false;
                paint(&ctx);
                return;
            }
        };
        if pins.is_empty() {
            debug!("feed exhausted");
            {
                let mut st = ctx.state.borrow_mut();
                st.feed_done = true;
                st.loading = false;
            }
            paint(&ctx);
            ensure(&ctx);
            return;
        }
        let fresh = {
            let st = ctx.state.borrow();
            let mut fresh = Vec::new();
            for pin in pins {
                if pin.id.is_empty() || !st.seen_pins.contains(&pin.id) {
                    fresh.push(pin);
                }
            }
            if fresh.is_empty() {
                drop(st);
                let mut st = ctx.state.borrow_mut();
                st.stalls = st.stalls.saturating_add(1);
                if st.stalls >= 2 {
                    st.feed_done = true;
                }
                st.loading = false;
                drop(st);
                paint(&ctx);
                ensure(&ctx);
                return;
            }
            fresh
        };
        info!("качаю {} пинов", fresh.len());
        let pins = fresh;
        let queue = Arc::new(Mutex::new(VecDeque::<Image>::new()));
        let finished = Arc::new(AtomicBool::new(false));
        let queue_bg = queue.clone();
        let finished_bg = finished.clone();
        let cache = ctx.cache.clone();
        let handle = runtime.spawn(async move {
            struct Done(Arc<AtomicBool>);
            impl Drop for Done {
                fn drop(&mut self) {
                    self.0.store(true, Ordering::Relaxed);
                }
            }
            let _done = Done(finished_bg);
            let mut waiting = tokio::task::JoinSet::new();
            let mut pins = pins.into_iter();
            for _ in 0..6 {
                let Some(pin) = pins.next() else {
                    break;
                };
                let cache = cache.clone();
                waiting.spawn(async move { load_pin(&cache, pin).await });
            }
            while let Some(joined) = waiting.join_next().await {
                if let Ok(Some(image)) = joined {
                    if let Ok(mut queued) = queue_bg.lock() {
                        queued.push_back(image);
                    }
                }
                let Some(pin) = pins.next() else {
                    continue;
                };
                let cache = cache.clone();
                waiting.spawn(async move { load_pin(&cache, pin).await });
            }
        });

        let ctx_tick = ctx.clone();
        glib::timeout_add_local(std::time::Duration::from_millis(16), move || {
            let image = queue.lock().ok().and_then(|mut queued| queued.pop_front());
            if let Some(image) = image {
                accept_image(&ctx_tick, image);
                ensure(&ctx_tick);
                return glib::ControlFlow::Continue;
            }
            if finished.load(Ordering::Relaxed) {
                ctx_tick.state.borrow_mut().loading = false;
                paint(&ctx_tick);
                ensure(&ctx_tick);
                glib::ControlFlow::Break
            } else {
                glib::ControlFlow::Continue
            }
        });

        let _ = handle.await;
    });
}

fn accept_image(ctx: &Ctx, image: Image) -> bool {
    let mut st = ctx.state.borrow_mut();
    if !image.pin_id.is_empty() && !st.seen_pins.insert(image.pin_id.clone()) {
        return false;
    }
    st.library.push(image);
    st.stalls = 0;
    true
}

fn view_bounds(st: &State, margin: i32) -> (i32, i32, i32, i32) {
    view_bounds_sides(st, margin, margin, margin, margin)
}

fn view_bounds_sides(
    st: &State,
    pad_l: i32,
    pad_t: i32,
    pad_r: i32,
    pad_b: i32,
) -> (i32, i32, i32, i32) {
    let z = zoom_of(st);
    let mut left = i32::MAX;
    let mut top = i32::MAX;
    let mut right = i32::MIN;
    let mut bottom = i32::MIN;
    for mon in &st.monitors {
        let x0 = st.scroll_x + mon.x as f64 / z - f64::from(pad_l);
        let y0 = st.scroll_y + mon.y as f64 / z - f64::from(pad_t);
        let x1 = st.scroll_x + (mon.x + mon.width) as f64 / z + f64::from(pad_r);
        let y1 = st.scroll_y + (mon.y + mon.height) as f64 / z + f64::from(pad_b);
        left = left.min(x0.floor() as i32);
        top = top.min(y0.floor() as i32);
        right = right.max(x1.ceil() as i32);
        bottom = bottom.max(y1.ceil() as i32);
    }
    (left, top, right, bottom)
}

fn column_span(origin_x: i32, period: i32, cols: i32, left: i32, right: i32) -> (i32, i32) {
    let period = period.max(1);
    let cols = cols.max(1);
    let start = (left - origin_x).div_euclid(period) - 1;
    let end = (right - origin_x).div_euclid(period) + 1;
    (start * cols, end * cols + cols - 1)
}

/// Column rectangle for index `k`. The monitor period is a gap, not a pin:
/// nothing crosses `origin + n * period`.
fn column_geom(k: i32, origin_x: i32, period: i32, cols: i32, gap: i32) -> (i32, i32) {
    let cols = cols.max(1);
    let gap = gap.max(0);
    let period = period.max(1);
    let left_edge = gap / 2;
    let mut usable = period - gap * cols;
    if usable < cols {
        usable = cols;
    }
    let base = usable / cols;
    let extra = usable % cols;
    let width = |i: i32| base + if i < extra { 1 } else { 0 };
    let period_index = k.div_euclid(cols);
    let local = k.rem_euclid(cols);
    let mut x = origin_x + period_index * period + left_edge;
    for i in 0..local {
        x += width(i) + gap;
    }
    (x, width(local).max(1))
}

fn crosses(x: i32, w: i32, seams: &[i32]) -> bool {
    seams.iter().any(|&s| x < s && x + w > s)
}

fn fit_down(mut y: i32, h: i32, seams: &[i32], gap: i32) -> i32 {
    for _ in 0..8 {
        let Some(seam) = seams.iter().copied().find(|&s| y < s && y + h > s) else {
            return y;
        };
        y = seam + gap;
    }
    y
}

fn fit_up(bottom_limit: i32, h: i32, seams: &[i32], gap: i32) -> i32 {
    let mut y = bottom_limit - h;
    for _ in 0..8 {
        let Some(seam) = seams.iter().copied().find(|&s| y < s && y + h > s) else {
            return y;
        };
        y = seam - gap - h;
    }
    y
}

fn tile_height(col_w: i32, aspect: f64, max_h: i32) -> i32 {
    let h = (f64::from(col_w) * aspect).round() as i32;
    h.clamp(80, max_h.max(80))
}

fn vertical_seams(monitors: &[MonitorSlot]) -> Vec<i32> {
    let mut seams = Vec::new();
    for a in monitors {
        for b in monitors {
            let y_overlap = a.y < b.y + b.height && b.y < a.y + a.height;
            if y_overlap && a.x == b.x + b.width {
                seams.push(a.x);
            }
        }
    }
    seams.sort_unstable();
    seams.dedup();
    seams
}

fn horizontal_seams(monitors: &[MonitorSlot]) -> Vec<i32> {
    let mut seams = Vec::new();
    for a in monitors {
        for b in monitors {
            let x_overlap = a.x < b.x + b.width && b.x < a.x + a.width;
            if x_overlap && a.y == b.y + b.height {
                seams.push(a.y);
            }
        }
    }
    seams.sort_unstable();
    seams.dedup();
    seams
}

fn max_tile_height(monitors: &[MonitorSlot], seams_y: &[i32], gap: i32) -> i32 {
    if seams_y.is_empty() {
        return 1400;
    }
    let mut marks = seams_y.to_vec();
    if let Some(top) = monitors.iter().map(|m| m.y).min() {
        marks.push(top);
    }
    if let Some(bot) = monitors.iter().map(|m| m.y + m.height).max() {
        marks.push(bot);
    }
    marks.sort_unstable();
    marks.dedup();
    let band = marks
        .windows(2)
        .map(|w| w[1] - w[0])
        .filter(|h| *h > 0)
        .min()
        .unwrap_or(1400);
    (band - gap * 2).clamp(120, 1400)
}

async fn load_pin(cache: &ImageCache, pin: PinItem) -> Option<Image> {
    let aspect = if pin.width == 0 {
        1.35
    } else {
        f64::from(pin.height) / f64::from(pin.width)
    };
    let link = pin.link.clone();
    let pin_id = pin.id.clone();
    if let Some(path) = pin.image_url.strip_prefix("file://") {
        let path = PathBuf::from(path);
        if path.exists() {
            return Some(Image {
                path,
                aspect,
                link,
                pin_id,
            });
        }
    }
    match cache.fetch(&pin.image_url).await {
        Ok(path) => Some(Image {
            path,
            aspect,
            link,
            pin_id,
        }),
        Err(e) => {
            warn!("image {}: {e:#}", pin.id);
            None
        }
    }
}

#[derive(Clone, Copy, Default)]
struct KeyBits {
    ctrl: bool,
    alt: bool,
    shift: bool,
    super_key: bool,
    left: bool,
    right: bool,
    up: bool,
    down: bool,
}

fn read_key_state() -> KeyBits {
    let Some(path) = input_flag_path() else {
        return KeyBits::default();
    };
    let Ok(bytes) = std::fs::read(path) else {
        return KeyBits::default();
    };
    let bit = |index: usize| bytes.get(index).copied() == Some(b'1');
    if bytes.len() < 8 {
        return KeyBits {
            ctrl: bit(0),
            ..KeyBits::default()
        };
    }
    KeyBits {
        ctrl: bit(0),
        alt: bit(1),
        shift: bit(2),
        super_key: bit(3),
        left: bit(4),
        right: bit(5),
        up: bit(6),
        down: bit(7),
    }
}

fn zoom_chord_held(ctx: &Ctx, gdk: gdk4::ModifierType) -> bool {
    let held = read_key_state();
    ctx.keys.zoom.iter().all(|modifier| match modifier {
        ZoomMod::Ctrl => held.ctrl || gdk.contains(gdk4::ModifierType::CONTROL_MASK),
        ZoomMod::Alt => held.alt || gdk.contains(gdk4::ModifierType::ALT_MASK),
        ZoomMod::Shift => held.shift || gdk.contains(gdk4::ModifierType::SHIFT_MASK),
        ZoomMod::Super => {
            held.super_key
                || gdk.contains(gdk4::ModifierType::SUPER_MASK)
                || gdk.contains(gdk4::ModifierType::META_MASK)
        }
    })
}

fn start_key_pan(ctx: &Ctx) {
    let Some(host) = ctx.state.borrow().scrolls.first().cloned() else {
        return;
    };
    let ctx = ctx.clone();
    let last = Cell::new(0i64);
    host.add_tick_callback(move |_, clock| {
        note_window_focus(&ctx);
        let now = clock.frame_time();
        let mut dt = if last.get() == 0 {
            0.016
        } else {
            (now - last.get()) as f64 / 1_000_000.0
        };
        last.set(now);
        if !(0.0..0.05).contains(&dt) {
            dt = 0.016;
        }
        let held = read_key_state();
        // A focused text field owns these keycodes, including the Russian
        // layout: фыва are the same physical keys as WASD.
        if ctx.typing.get() > 0 {
            return glib::ControlFlow::Continue;
        }
        let speed = ctx.keys.move_speed;
        let mut dx = 0.0;
        let mut dy = 0.0;
        if held.left {
            dx -= speed * dt;
        }
        if held.right {
            dx += speed * dt;
        }
        if held.up {
            dy -= speed * dt;
        }
        if held.down {
            dy += speed * dt;
        }
        if dx != 0.0 || dy != 0.0 {
            nudge(&ctx, dx, dy);
        }
        glib::ControlFlow::Continue
    });
}

fn input_flag_path() -> Option<PathBuf> {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")?;
    Some(PathBuf::from(runtime).join("pinterest-wallpaper-ctrl"))
}

fn focus_stamp_path() -> Option<PathBuf> {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")?;
    Some(PathBuf::from(runtime).join("pinterest-wallpaper-focus"))
}

fn seed_window_focus() {
    let Ok(out) = Command::new("hyprctl").args(["activewindow", "-j"]).output() else {
        return;
    };
    if !out.status.success() {
        return;
    }
    let Ok(text) = String::from_utf8(out.stdout) else {
        return;
    };
    let Some(addr) = text
        .split("\"address\"")
        .nth(1)
        .and_then(|rest| rest.split('"').nth(1))
    else {
        return;
    };
    if addr.is_empty() {
        return;
    }
    let Some(path) = focus_stamp_path() else {
        return;
    };
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let _ = std::fs::write(path, format!("{addr}\t{ms}"));
}

fn note_window_focus(ctx: &Ctx) {
    let menu = ctx.state.borrow().menu.clone();
    if !menu.is_open() {
        return;
    }
    let Some(path) = focus_stamp_path() else {
        return;
    };
    let Ok(text) = std::fs::read_to_string(path) else {
        return;
    };
    let mut parts = text.split('\t');
    let address = parts.next().unwrap_or("").trim();
    let when = parts.next().unwrap_or("0").trim().parse().unwrap_or(0);
    menu.on_window_focus(address, when);
}

fn scroll_inside_sheet(ctrl: &EventControllerScroll) -> bool {
    let Some(event) = ctrl.current_event() else {
        return false;
    };
    let Some(surface) = event.surface() else {
        return false;
    };
    let Some(widget) = ctrl.widget() else {
        return false;
    };
    let Some(native) = widget.native() else {
        return false;
    };
    let Some(ours) = native.surface() else {
        return false;
    };
    ours != surface
}

fn install_input_watch(keys: &KeysConfig) {
    let lua_source = input_lua(keys);
    std::thread::spawn(move || {
        if let Err(err) = input_watch(lua_source) {
            warn!("input watch: {err:#}");
        }
    });
}

fn input_watch(lua_source: String) -> std::io::Result<()> {
    let lua = input_lua_path()?;
    std::fs::write(&lua, lua_source)?;
    register_input_hook(&lua);
    let Some(sock) = hypr_event_socket() else {
        return Ok(());
    };
    let stream = UnixStream::connect(&sock)?;
    seed_window_focus();
    let reader = BufReader::new(stream);
    for line in reader.lines() {
        let line = line?;
        if line.starts_with("configreloaded") {
            register_input_hook(&lua);
        } else if let Some(addr) = line.strip_prefix("activewindowv2>>") {
            if let Some(path) = focus_stamp_path() {
                let ms = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis())
                    .unwrap_or(0);
                let _ = std::fs::write(path, format!("{}\t{ms}", addr.trim()));
            }
        }
    }
    Ok(())
}

fn register_input_hook(lua: &Path) {
    let script = format!("dofile('{}')", lua.display());
    match Command::new("hyprctl").args(["repl", &script]).output() {
        Ok(out) if out.status.success() => {}
        Ok(out) => {
            let err = String::from_utf8_lossy(&out.stderr);
            warn!("input hook: {err}");
        }
        Err(err) => warn!("input hook: {err}"),
    }
}

fn input_lua_path() -> std::io::Result<PathBuf> {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR").ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::NotFound, "XDG_RUNTIME_DIR")
    })?;
    Ok(PathBuf::from(runtime).join("pinterest-wallpaper-zoom.lua"))
}

fn hypr_event_socket() -> Option<PathBuf> {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")?;
    let base = PathBuf::from(runtime).join("hypr");
    let dir = if let Some(sig) = std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE") {
        base.join(sig)
    } else {
        let mut found = None;
        for entry in std::fs::read_dir(&base).ok()?.flatten() {
            if entry.path().join(".socket2.sock").is_file() {
                if found.is_some() {
                    return None;
                }
                found = Some(entry.path());
            }
        }
        found?
    };
    let sock = dir.join(".socket2.sock");
    sock.is_file().then_some(sock)
}

fn lua_codes(names: &[String]) -> String {
    let codes = xkb_keycodes(names);
    if codes.is_empty() {
        String::new()
    } else {
        codes
            .iter()
            .map(|code| code.to_string())
            .collect::<Vec<_>>()
            .join(", ")
    }
}

fn input_lua(keys: &KeysConfig) -> String {
    format!(
        r#"
local path = os.getenv("XDG_RUNTIME_DIR") .. "/pinterest-wallpaper-ctrl"
local move_left = {{ {left} }}
local move_right = {{ {right} }}
local move_up = {{ {up} }}
local move_down = {{ {down} }}
local down = {{}}
local function pressed(list)
  for _, code in ipairs(list) do
    if down[code] then return true end
  end
  return false
end
local function cursor_over_window()
  local ok, hit = pcall(function()
    local cursor = hl.get_cursor_pos()
    if type(cursor) ~= "table" then return false end
    for _, win in pairs(hl.get_windows()) do
      local inside = false
      pcall(function()
        if win.mapped and not win.hidden then
          local at, size = win.at, win.size
          inside = cursor.x >= at.x and cursor.x < at.x + size.x and cursor.y >= at.y and cursor.y < at.y + size.y
        end
      end)
      if inside then return true end
    end
    return false
  end)
  return ok and hit
end
local function publish()
  local function seen(code, name)
    if down[code] ~= nil then return down[code] end
    return hl.is_key_down(name)
  end
  local ctrl = seen(37, "Control_L") or seen(105, "Control_R")
  local alt = seen(64, "Alt_L") or seen(108, "Alt_R")
  local shift = seen(50, "Shift_L") or seen(62, "Shift_R")
  local super = seen(133, "Super_L") or seen(134, "Super_R")
  local pan = not cursor_over_window()
  local function bit(v) if v then return "1" else return "0" end end
  local line = bit(ctrl) .. bit(alt) .. bit(shift) .. bit(super)
    .. bit(pan and pressed(move_left)) .. bit(pan and pressed(move_right))
    .. bit(pan and pressed(move_up)) .. bit(pan and pressed(move_down))
  local f = io.open(path, "w")
  if f then
    f:write(line)
    f:close()
  end
end
function _G.__pw_arm_ctrl()
  _G.__pw_ctrl_gen = (_G.__pw_ctrl_gen or 0) + 1
  local gen = _G.__pw_ctrl_gen
  down = {{}}
  publish()
  hl.on("input.keyboard.key", function(keycode, _t, state)
    if _G.__pw_ctrl_gen ~= gen then return end
    down[keycode] = state ~= 0
    publish()
  end)
end
if not _G.__pw_ctrl_reload then
  _G.__pw_ctrl_reload = true
  hl.on("config.reloaded", function()
    if _G.__pw_arm_ctrl then _G.__pw_arm_ctrl() end
  end)
end
_G.__pw_arm_ctrl()
"#,
        left = lua_codes(&keys.left),
        right = lua_codes(&keys.right),
        up = lua_codes(&keys.up),
        down = lua_codes(&keys.down),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn columns_stop_at_the_monitor_edge() {
        let origin = 0;
        let period = 1920;
        let cols = 5;
        let gap = 8;
        for k in -30..30 {
            let (x, w) = column_geom(k, origin, period, cols, gap);
            for n in -3..4 {
                let seam = origin + n * period;
                assert!(
                    !(x < seam && x + w > seam),
                    "column {k} at {x}+{w} crosses {seam}"
                );
            }
        }
        let (left, lw) = column_geom(4, origin, period, cols, gap);
        let (right, _) = column_geom(5, origin, period, cols, gap);
        assert_eq!(right - (left + lw), gap);
    }

    #[test]
    fn vertical_seam_pushes_a_pin_fully_below() {
        let y = fit_down(900, 400, &[1080], 8);
        assert!(y >= 1080);
        assert_eq!(y, 1088);
    }

    #[test]
    fn rubber_stays_short_and_releases() {
        assert!(rubber(10_000.0) < 130.0);
        assert!(rubber(-10_000.0) > -130.0);
        let (rest, pull) = absorb(-40.0, 15.0);
        assert!(rest.abs() < 0.01, "{rest}");
        assert!((pull + 25.0).abs() < 0.01, "{pull}");
    }

    #[test]
    fn a_far_jump_is_not_the_same_island() {
        assert_eq!(span_distance(0, 1000, 8000, 9080), 7000);
        assert!(span_distance(0, 1000, 8000, 9080) > 1080);
        assert_eq!(span_distance(0, 5000, 4200, 5280), 0);
    }

    #[test]
    fn a_column_keeps_only_the_gutter() {
        let heights = [200, 180, 240];
        let mut ys = [0, 520, 900];
        let gap = 8;
        let mut cursor = ys[0];
        for index in 0..ys.len() {
            ys[index] = fit_down(cursor, heights[index], &[], gap);
            cursor = ys[index] + heights[index] + gap;
        }
        assert_eq!(ys, [0, 208, 396]);
    }

    #[test]
    fn the_open_view_comes_before_the_grid_outside_it() {
        // A stack ends just above the screen. The visible band is still empty.
        assert_eq!(open_gaps(&[(0, 900)], 1000, 2080, 8, 1540), vec![(1000, 2080)]);
        // The strip already on screen leaves the hole under it, plus the gutter.
        assert_eq!(
            open_gaps(&[(1000, 1600)], 1000, 2080, 8, 1540),
            vec![(1608, 2080)]
        );
        // The gutter between stacked pins is not a second strip to fill.
        assert!(open_gaps(&[(1000, 1400), (1408, 1800)], 1000, 1800, 8, 1400).is_empty());
    }

    #[test]
    fn zoom_keeps_the_point_under_the_cursor() {
        let scroll = 120.0;
        let focus = 500.0;
        let z0 = 1.0;
        let z1 = 1.8;
        let before = scroll + focus / z0;
        let after_scroll = zoom_anchor(scroll, focus, z0, z1);
        let after = after_scroll + focus / z1;
        assert!((before - after).abs() < 1e-6, "{before} vs {after}");
    }

    #[test]
    fn scroll_stays_near_existing_pictures() {
        // Two screens, pictures only on the initial canvas. Left peek is limited.
        let sx = clamp_offset(-5000.0, 0.0, 3840.0, 0.0, 3840.0, 480.0);
        assert!(sx > -500.0, "{sx}");
        assert!(sx < 0.0, "{sx}");
        let sy = clamp_offset(9000.0, 0.0, 4000.0, 0.0, 1080.0, 480.0);
        assert!(sy < 4000.0, "{sy}");
    }
}
