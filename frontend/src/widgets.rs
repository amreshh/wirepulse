//! Small shared pieces of UI: dropdown menus that stay on screen, drag
//! handles that resize panels and columns, and the details panel beside a
//! table.

use crate::app::use_ctx;
use crate::settings::{ViewSettings, SIDE_WIDTH};
use crate::util::viewport;
use leptos::ev;
use leptos::prelude::*;
use wasm_bindgen::JsCast;

/// Inline style that keeps a dropdown on screen: under the button that
/// opened it (over it, when there's more room there), as wide as `width`
/// where the window allows, moved left as far as needed, and scrolling when
/// it's taller than the room it has.
fn menu_style(button: &web_sys::Element, width: f64) -> String {
    const MARGIN: f64 = 8.0;
    const GAP: f64 = 6.0;
    let r = button.get_bounding_client_rect();
    let (vw, vh) = viewport();
    let width = width.min(vw - 2.0 * MARGIN);
    let left = r.left().min(vw - MARGIN - width).max(MARGIN);
    let below = vh - r.bottom() - GAP - MARGIN;
    let above = r.top() - GAP - MARGIN;
    let vertical = if below >= 320.0 || below >= above {
        format!("top:{}px;bottom:auto;max-height:{}px", r.bottom() + GAP, below.max(120.0))
    } else {
        format!("top:auto;bottom:{}px;max-height:{}px", vh - r.top() + GAP, above.max(120.0))
    };
    format!("position:fixed;left:{left}px;right:auto;width:{width}px;{vertical};overflow-y:auto")
}

/// A button that opens a dropdown menu with `children`. `open` is passed in
/// so the menu's own items can close it.
#[component]
pub fn Dropdown(
    label: &'static str,
    /// Classes of the menu besides `menu`.
    menu_class: &'static str,
    /// Width of the menu, when the window is wide enough.
    width: f64,
    open: RwSignal<bool>,
    #[prop(default = "btn")] button_class: &'static str,
    #[prop(optional)] title: Option<&'static str>,
    children: ChildrenFn,
) -> impl IntoView {
    let style = RwSignal::new(String::new());
    view! {
        <div class="menu-wrap">
            <button
                class=button_class
                title=title
                class:active=move || open.get()
                on:click=move |ev| {
                    if !open.get_untracked() {
                        if let Some(button) = ev.current_target().and_then(|t| t.dyn_into::<web_sys::Element>().ok()) {
                            style.set(menu_style(&button, width));
                        }
                    }
                    open.update(|o| *o = !*o);
                }
            >
                {label}
            </button>
            {move || {
                open.get()
                    .then(|| {
                        let resize = window_event_listener(ev::resize, move |_| open.set(false));
                        on_cleanup(move || resize.remove());
                        // Placed for the window as it was when it opened.
                        view! {
                            <div class="backdrop" on:click=move |_| open.set(false)></div>
                            <div class=format!("menu {menu_class}") style=move || style.get()>
                                {children()}
                            </div>
                        }
                    })
            }}
        </div>
    }
}

/// A handle that resizes something by dragging it sideways, kept in the
/// view settings: `get` reads the size and `set` writes it (`None` puts the
/// default back, on double-click). `grow` is 1.0 when dragging right makes it
/// wider (a column's right edge) and -1.0 when dragging left does (the left
/// edge of a panel on the right).
pub fn grip(
    class: &'static str,
    grow: f64,
    min: f64,
    max: impl Fn() -> f64 + Copy + 'static,
    get: impl Fn(&ViewSettings) -> f64 + Copy + 'static,
    set: impl Fn(&mut ViewSettings, Option<f64>) + Copy + 'static,
) -> impl IntoView {
    let ctx = use_ctx();
    let drag = RwSignal::new(None::<(f64, f64)>);
    view! {
        <div
            class=class
            class:dragging=move || drag.with(Option::is_some)
            title="Drag to resize, double-click for the default size"
            on:pointerdown=move |ev| {
                if ev.button() != 0 {
                    return;
                }
                ev.prevent_default();
                ev.stop_propagation();
                let el = event_target::<web_sys::Element>(&ev);
                let _ = el.set_pointer_capture(ev.pointer_id());
                drag.set(Some((f64::from(ev.client_x()), ctx.view.with_untracked(get))));
            }
            on:pointermove=move |ev| {
                if let Some((x0, w0)) = drag.get_untracked() {
                    let w = (w0 + grow * (f64::from(ev.client_x()) - x0)).clamp(min, max().max(min));
                    ctx.view.update(|v| set(v, Some(w.round())));
                }
            }
            on:pointerup=move |_| drag.set(None)
            on:pointercancel=move |_| drag.set(None)
            on:click=move |ev| ev.stop_propagation()
            on:dblclick=move |ev| {
                ev.stop_propagation();
                ctx.view.update(|v| set(v, None));
            }
        ></div>
    }
}

/// Narrowest the details panels get.
pub const MIN_PANEL: f64 = 260.0;

/// The details panel on the right of the flows and endpoints tables; drag
/// its left edge to resize it.
#[component]
pub fn SidePanel(children: Children) -> impl IntoView {
    let ctx = use_ctx();
    // Leave the table some room.
    let max = || viewport().0 - 300.0;
    view! {
        {grip("side-grip", -1.0, MIN_PANEL, max, |v| v.side_width, |v, w| v.side_width = w.unwrap_or(SIDE_WIDTH))}
        <aside class="flow-detail" style=move || format!("--side-w: {}px", ctx.view.with(|v| v.side_width))>
            {children()}
        </aside>
    }
}
