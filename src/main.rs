//! blitz + gpui + subsecond experiment — buwiz shell redesign port
//!
//! Architecture:
//!   GPUI owns the window + chrome (a slim status bar). Each app "view" is a
//!   serialized DOM dump of the JS prototype (`src/views/*.html`), rendered by
//!   `blitz-dom` offscreen via anyrender/vello_cpu into RGBA, channel-swapped
//!   to BGRA, and shown as a `gpui::RenderImage`. GPUI mouse events over the
//!   DOM region are translated into `UiEvent`s and driven through
//!   `EventDriver`; `DomEventData::Click` chains are walked for `data-act`
//!   attributes and dispatched to a Rust-side action handler — this is the
//!   port of the prototype's `document.addEventListener('click', …)` +
//!   `el.dataset.act` delegation.
//!
//!   What is NOT ported: text input/editing, overlays/menus, mode:ds +
//!   mode:forms tabs, and any act that depended on live JS state. Those acts
//!   still dispatch (you see them in the status bar) but are no-ops.
//!
//!   All interaction/render logic sits behind `subsecond::HotFn::current` so
//!   `dx serve --hot-patch` can patch it live. `view`, `theme`, `hist` and
//!   `fwd` live in GPUI state and survive patches and DOM rebuilds.

use std::sync::{Arc, mpsc};

use anyrender::render_to_buffer;
use anyrender_vello_cpu::VelloCpuImageRenderer;
use atomic_refcell::AtomicRefCell;
use blitz_dom::{
    BaseDocument, Document, DocumentConfig, EventDriver, EventHandler, LocalName, QualName, ns,
};
use blitz_html::HtmlDocument;
use blitz_paint::paint_scene;
use blitz_traits::events::{
    BlitzPointerEvent, BlitzPointerId, BlitzWheelDelta, BlitzWheelEvent, DomEvent, EventState,
    MouseEventButton, MouseEventButtons, Point as DomPoint, PointerCoords, PointerDetails, UiEvent,
};
use blitz_traits::node_id::NodeId;
use blitz_traits::shell::{ColorScheme, Viewport};
use gpui::*;
use image::Frame;
use keyboard_types::Modifiers;
use smallvec::SmallVec;

// ----------------------------- constants ----------------------------------

const TOOLBAR_H: f32 = 36.0;
const WIN_W: f32 = 1280.0;
const WIN_H: f32 = 860.0;
const DOM_W: u32 = WIN_W as u32; // css px
const DOM_H: u32 = (WIN_H - TOOLBAR_H) as u32;
const SCALE: f32 = 2.0; // render blitz at 2x for retina sharpness

// ----------------------------- model --------------------------------------

struct BlitzView {
    /// Which dump is live — GPUI state, survives patches.
    view: &'static str,
    /// Back / forward stacks (the shell's own ◀ ▶ buttons hit `back`/`fwd`).
    hist: Vec<&'static str>,
    fwd: Vec<&'static str>,
    /// "light" | "dark" | "system" — driven via the `data-theme` html attr.
    theme: &'static str,
    /// One-line status for the GPUI bar (last dispatched action).
    status: String,
    doc: HtmlDocument,
    /// (view, theme) the current `doc` was built for.
    doc_key: (&'static str, &'static str),
    /// The HTML `doc` was parsed from — compared against `view_html()` output
    /// so a hot-patched view file triggers a rebuild.
    src_cache: Option<&'static str>,
    /// Cost of the last full restyle+layout+raster, for the status bar.
    last_paint_ms: f32,
    frame: Option<Arc<RenderImage>>,
    buttons: MouseEventButtons,
    active_pointers: Arc<AtomicRefCell<Vec<BlitzPointerEvent>>>,
}

impl BlitzView {
    /// Build a blitz pointer event for a point in DOM css-px coords.
    fn pointer_ev(&self, x: f32, y: f32, button: MouseEventButton) -> BlitzPointerEvent {
        BlitzPointerEvent {
            id: BlitzPointerId::Mouse,
            is_primary: true,
            coords: PointerCoords {
                page_x: x,
                page_y: y,
                screen_x: x,
                screen_y: y,
                client_x: x,
                client_y: y,
            },
            button,
            buttons: self.buttons,
            mods: Modifiers::empty(),
            details: PointerDetails::default(),
            element: DomPoint::default(),
            active_pointers: Arc::clone(&self.active_pointers),
        }
    }

    /// Drive a UiEvent through the document, returning the ancestor chains of
    /// any generated `click` DOM events.
    fn dispatch(&mut self, ev: UiEvent) -> Vec<Vec<NodeId>> {
        #[derive(Default)]
        struct Sink {
            click_chains: Vec<Vec<NodeId>>,
        }
        impl EventHandler for &mut Sink {
            fn handle_event(
                &mut self,
                chain: &[NodeId],
                ev: &mut DomEvent,
                _doc: &mut dyn Document,
                _state: &mut EventState,
            ) {
                if ev.name() == "click" {
                    self.click_chains.push(chain.to_vec());
                }
            }
        }
        let mut sink = Sink::default();
        EventDriver::new(&mut self.doc, &mut sink).handle_ui_event(ev);
        sink.click_chains
    }

    /// First `data-act` on the click chain (innermost element wins).
    fn click_act(&self, chains: &[Vec<NodeId>]) -> Option<String> {
        let name = LocalName::from("data-act");
        chains.iter().flatten().find_map(|&id| {
            self.doc
                .get_node(id)
                .and_then(|n| n.attr(name.clone()))
                .map(str::to_owned)
        })
    }

    /// Re-resolve + repaint the document into `self.frame`.
    fn repaint_doc(&mut self) {
        let t = std::time::Instant::now();
        self.doc.resolve(0.0);
        if std::env::var_os("BLITZ_DEBUG").is_some() {
            for sel in [".window", ".titlebar", ".body", ".sidebar", ".main", ".main-scroll", ".content", ".rpanel"] {
                if let Ok(Some(id)) = self.doc.query_selector(sel) {
                    eprintln!("{sel:>14} → {:?}", self.doc.get_client_bounding_rect(id));
                }
            }
        }
        self.frame = Some(paint_dom_to_image(&mut self.doc, DOM_W, DOM_H));
        self.last_paint_ms = t.elapsed().as_secs_f32() * 1000.0;
    }
}

// --------------------- hot-patchable code (below) --------------------------

/// Hot-patchable view table. Each entry is a serialized DOM dump of the JS
/// prototype (scripts stripped). `include_str!` is read at *compile* time, so
/// editing a file under `src/views/` triggers a rebuild whose patch dylib
/// carries the new markup.
fn view_html(view: &str) -> Option<&'static str> {
    Some(match view {
        "home" => include_str!("views/go_home.html"),
        "library" => include_str!("views/go_library.html"),
        "calendar" => include_str!("views/go_calendar.html"),
        "filings" => include_str!("views/go_filings.html"),
        "profile" => include_str!("views/go_profile.html"),
        "forms" => include_str!("views/go_forms.html"),
        "templates" => include_str!("views/go_templates.html"),
        "settings" => include_str!("views/go_settings.html"),
        "form:0619E" => include_str!("views/form_0619E.html"),
        "form:1601C" => include_str!("views/form_1601C.html"),
        _ => return None,
    })
}

/// The action protocol: a Rust port of the prototype's `act(key, arg)` switch.
/// Anything we haven't ported lands in the status bar instead of dying.
fn handle_act(this: &mut BlitzView, act: &str) {
    let (key, arg) = act.split_once(':').map(|(k, a)| (k, Some(a))).unwrap_or((act, None));
    match (key, arg) {
        ("go", Some(v)) => match v {
            "home" | "library" | "calendar" | "filings" | "profile" | "forms" | "templates"
            | "settings" => nav(this, leak(v), true),
            _ => this.status = format!("go:{v} — view not dumped"),
        },
        ("form", Some(code)) => {
            let v = format!("form:{code}");
            if view_html(&v).is_some() {
                nav(this, leak(&v), true)
            } else {
                this.status = format!("form {code} — not dumped (only 0619E, 1601C)")
            }
        }
        ("back", _) => {
            if let Some(v) = this.hist.pop() {
                this.fwd.push(this.view);
                nav(this, v, false);
            }
        }
        ("fwd", _) => {
            if let Some(v) = this.fwd.pop() {
                this.hist.push(this.view);
                nav(this, v, false);
            }
        }
        ("theme", Some(t)) if matches!(t, "light" | "dark" | "system") => set_theme(this, leak(t)),
        ("cycle-theme", _) => {
            let next = match this.theme {
                "light" => "dark",
                "dark" => "system",
                _ => "light",
            };
            set_theme(this, next);
        }
        _ => this.status = format!("act: {act} (not ported)"),
    }
}

fn leak(s: &str) -> &'static str {
    // Views are a small fixed vocabulary; leaking keeps lifetimes trivial.
    Box::leak(s.to_string().into_boxed_str())
}

fn nav(this: &mut BlitzView, view: &'static str, record: bool) {
    if record && this.view != view {
        this.hist.push(this.view);
        this.fwd.clear();
    }
    this.view = view;
    this.status = view.to_string();
}

/// Re-apply `data-theme` to the doc root and restyle. "system" clears the
/// attribute so `prefers-color-scheme` (viewport = Light) takes over.
fn apply_theme_attr(doc: &mut HtmlDocument, theme: &str) {
    let root_id = doc.root_element().id;
    let name = QualName::new(None, ns!(), LocalName::from("data-theme"));
    let mut m = doc.mutate();
    match theme {
        "system" => m.clear_attribute(root_id, name),
        t => m.set_attribute(root_id, name, t),
    }
}

fn set_theme(this: &mut BlitzView, theme: &'static str) {
    this.theme = theme;
    apply_theme_attr(&mut this.doc, theme);
    this.doc_key.1 = theme;
    this.status = format!("theme → {theme}");
}

/// The hot-patchable render body. Detoured through the jump table by
/// `Render::render` below — never edit the shim.
fn render_body(this: &mut BlitzView, _window: &mut Window, cx: &mut Context<BlitzView>) -> AnyElement {
    // Rebuild the document if the view changed or the source was hot-patched.
    // `&str` equality is by content, so a patched file rebuilds the doc.
    if view_html(this.view) != this.src_cache || this.frame.is_none() {
        rebuild_doc(this);
    }

    let dom_img = this
        .frame
        .clone()
        .map(|f| img(ImageSource::Render(f)).w(px(DOM_W as f32)).h(px(DOM_H as f32)));

    div()
        .flex()
        .flex_col()
        .size_full()
        .bg(rgb(0x18181b))
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .h(px(TOOLBAR_H))
                .w_full()
                .gap_3()
                .px_3()
                .bg(rgb(0x27272a))
                .border_b_1()
                .border_color(rgb(0x3f3f46))
                .child(
                    div()
                        .text_color(rgb(0xe4e4e7))
                        .text_sm()
                        .child(format!("blitz→gpui · {}", this.view)),
                )
                .child(
                    div()
                        .ml_auto()
                        .text_color(rgb(0x71717a))
                        .text_xs()
                        .child(format!("{} · theme {} · paint {:.0}ms", this.status, this.theme, this.last_paint_ms)),
                ),
        )
        .child(
            div()
                .id("dom")
                .w(px(DOM_W as f32))
                .h(px(DOM_H as f32))
                .on_mouse_move(cx.listener(|this, ev: &MouseMoveEvent, _w, cx| {
                    let x: f32 = ev.position.x.into();
                    let y: f32 = ev.position.y.into();
                    subsecond::HotFn::current(dom_pointer_move)
                        .call((this, x, y - TOOLBAR_H, cx));
                }))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, ev: &MouseDownEvent, _w, cx| {
                        let x: f32 = ev.position.x.into();
                        let y: f32 = ev.position.y.into();
                        subsecond::HotFn::current(dom_pointer_down)
                            .call((this, x, y - TOOLBAR_H, cx));
                    }),
                )
                .on_mouse_up(
                    MouseButton::Left,
                    cx.listener(|this, ev: &MouseUpEvent, _w, cx| {
                        let x: f32 = ev.position.x.into();
                        let y: f32 = ev.position.y.into();
                        subsecond::HotFn::current(dom_pointer_up)
                            .call((this, x, y - TOOLBAR_H, cx));
                    }),
                )
                .on_scroll_wheel(cx.listener(|this, ev: &ScrollWheelEvent, _w, cx| {
                    let x: f32 = ev.position.x.into();
                    let y: f32 = ev.position.y.into();
                    let d = match ev.delta {
                        ScrollDelta::Pixels(p) => (f32::from(p.x), f32::from(p.y)),
                        ScrollDelta::Lines(p) => (p.x * 20.0, p.y * 20.0),
                    };
                    subsecond::HotFn::current(dom_wheel).call((this, x, y - TOOLBAR_H, d, cx));
                }))
                .children(dom_img),
        )
        .into_any_element()
}

/// Parse + theme + paint the document for `this.view`. Used by render_body
/// and by nav actions so a view switch costs exactly one raster.
fn rebuild_doc(this: &mut BlitzView) {
    let Some(src) = view_html(this.view) else {
        this.status = format!("view {} missing", this.view);
        return;
    };
    this.doc = make_doc(src);
    apply_theme_attr(&mut this.doc, this.theme);
    this.src_cache = Some(src);
    this.doc_key = (this.view, this.theme);
    this.repaint_doc();
}

fn dom_pointer_move(this: &mut BlitzView, x: f32, y: f32, cx: &mut Context<BlitzView>) {
    let had_changes = this.doc.has_changes();
    this.dispatch(UiEvent::PointerMove(this.pointer_ev(x, y, MouseEventButton::Main)));
    if had_changes || this.doc.has_changes() {
        this.repaint_doc();
        cx.notify();
    }
}

fn dom_pointer_down(this: &mut BlitzView, x: f32, y: f32, cx: &mut Context<BlitzView>) {
    this.buttons |= MouseEventButton::Main.into();
    this.dispatch(UiEvent::PointerDown(this.pointer_ev(x, y, MouseEventButton::Main)));
    this.repaint_doc();
    cx.notify();
}

fn dom_pointer_up(this: &mut BlitzView, x: f32, y: f32, cx: &mut Context<BlitzView>) {
    this.buttons ^= MouseEventButton::Main.into();
    let chains = this.dispatch(UiEvent::PointerUp(this.pointer_ev(x, y, MouseEventButton::Main)));
    if let Some(act) = this.click_act(&chains) {
        handle_act(this, &act);
    }
    if this.doc_key.0 != this.view {
        rebuild_doc(this); // nav requested a different view
    } else {
        this.repaint_doc();
    }
    cx.notify();
}

fn dom_wheel(this: &mut BlitzView, x: f32, y: f32, d: (f32, f32), cx: &mut Context<BlitzView>) {
    let ev = BlitzWheelEvent {
        delta: BlitzWheelDelta::Pixels(d.0 as f64, d.1 as f64),
        coords: this.pointer_ev(x, y, MouseEventButton::Main).coords,
        buttons: this.buttons,
        mods: Modifiers::empty(),
        element: DomPoint::default(),
    };
    this.dispatch(UiEvent::Wheel(ev));
    if this.doc.has_changes() {
        // Wheel only mutates scroll offsets — repaint without a restyle pass.
        let t = std::time::Instant::now();
        this.frame = Some(paint_dom_to_image(&mut this.doc, DOM_W, DOM_H));
        this.last_paint_ms = t.elapsed().as_secs_f32() * 1000.0;
        cx.notify();
    }
}

/// Rasterize the document to a GPUI RenderImage (BGRA premultiplied).
fn paint_dom_to_image(doc: &mut BaseDocument, css_w: u32, css_h: u32) -> Arc<RenderImage> {
    let w = (css_w as f32 * SCALE) as u32;
    let h = (css_h as f32 * SCALE) as u32;
    let mut buf = render_to_buffer::<VelloCpuImageRenderer, _>(
        |scene| paint_scene(scene, doc, SCALE as f64, w, h, 0, 0),
        w,
        h,
    );
    // vello_cpu emits premultiplied RGBA; gpui wants premultiplied BGRA.
    for px in buf.chunks_exact_mut(4) {
        px.swap(0, 2);
    }
    let rgba = image::RgbaImage::from_raw(w, h, buf).expect("buffer size matches");
    Arc::new(RenderImage::new(SmallVec::from_const([Frame::new(rgba)])))
}

// --------------------- cold scaffolding (never patched) --------------------

fn make_doc(html: &str) -> HtmlDocument {
    let mut doc = HtmlDocument::from_html(
        html,
        DocumentConfig {
            viewport: Some(Viewport::new(
                (DOM_W as f32 * SCALE) as u32,
                (DOM_H as f32 * SCALE) as u32,
                SCALE,
                ColorScheme::Light,
            )),
            ..Default::default()
        },
    );
    doc.resolve(0.0);
    doc
}

impl Render for BlitzView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        subsecond::HotFn::current(render_body).call((self, window, cx))
    }
}

fn main() {
    // Receives JumpTables from `dx serve --hot-patch` and applies them.
    dioxus_devtools::connect_subsecond();

    // Ping the UI every time a patch lands so the next render picks it up.
    let (tx, rx) = mpsc::channel::<()>();
    subsecond::register_handler(Arc::new(move || {
        let _ = tx.send(());
    }));

    gpui_kit::application().run(move |cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(WIN_W), px(WIN_H)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                is_resizable: false,
                ..Default::default()
            },
            |_window, cx| {
                let view = cx.new(|_cx| {
                    let src = view_html("home").expect("home view embedded");
                    let mut doc = make_doc(src);
                    apply_theme_attr(&mut doc, "system");
                    let mut view = BlitzView {
                        view: "home",
                        hist: Vec::new(),
                        fwd: Vec::new(),
                        theme: "system",
                        status: "home".to_string(),
                        doc_key: ("home", "system"),
                        frame: None,
                        buttons: MouseEventButtons::None,
                        active_pointers: Arc::new(AtomicRefCell::new(Vec::new())),
                        src_cache: Some(src),
                        last_paint_ms: 0.0,
                        doc,
                    };
                    view.repaint_doc();
                    view
                });
                cx.spawn(async move |cx: &mut AsyncApp| {
                    loop {
                        cx.background_executor()
                            .timer(std::time::Duration::from_millis(60))
                            .await;
                        let mut patched = false;
                        while rx.try_recv().is_ok() {
                            patched = true;
                        }
                        if patched {
                            let _ = cx.update(|cx| cx.refresh_windows());
                        }
                    }
                })
                .detach();
                view
            },
        )
        .unwrap();
        cx.activate(true);
    });
}
