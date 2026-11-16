//! Live updates from the backend over a WebSocket, with reconnect.

use crate::app::Ctx;
use crate::store::FlowStore;
use crate::util::{set_timeout, window};
use common::ServerMsg;
use leptos::prelude::*;
use wasm_bindgen::closure::Closure;
use wasm_bindgen::JsCast;
use web_sys::{MessageEvent, WebSocket};

pub fn connect(ctx: Ctx) {
    let loc = window().location();
    let scheme = if loc.protocol().as_deref() == Ok("https:") { "wss" } else { "ws" };
    // Start with as many flows as the tables keep, or, back on the same
    // server run, with just the ones missed while away.
    let (limit, resume) = ctx.store.with_untracked(|s| (s.limit(), s.resume_point()));
    let mut url = format!("{scheme}://{}/api/ws?flows={limit}", loc.host().unwrap_or_default());
    if let Some((instance, after)) = resume {
        url.push_str(&format!("&instance={instance}&after={after}"));
    }
    let Ok(ws) = WebSocket::new(&url) else {
        set_timeout(move || connect(ctx), 3000);
        return;
    };

    let on_open = Closure::<dyn FnMut()>::new(move || {
        ctx.connected.set(true);
        // A server too old to say which run it is may have restarted, and
        // then the kept flows' ids name other flows: start over.
        if ctx.store.with_untracked(FlowStore::run_unknown) {
            let paused = ctx.paused.get_untracked();
            ctx.store.update(|s| s.server_restarted(paused));
        }
    });
    let on_message = Closure::<dyn FnMut(MessageEvent)>::new(move |e: MessageEvent| {
        let Some(text) = e.data().as_string() else { return };
        match serde_json::from_str::<ServerMsg>(&text) {
            Ok(ServerMsg::Graph(g)) => ctx.graph.set(g),
            // Each connection starts with the status, which names the server
            // run; a new one numbers its flows from 1 again.
            Ok(ServerMsg::Status(s)) => {
                let instance = s.instance;
                ctx.status.set(s);
                if instance != 0 && ctx.store.with_untracked(FlowStore::instance) != instance {
                    let paused = ctx.paused.get_untracked();
                    ctx.store.update(|st| st.set_instance(instance, paused));
                }
            }
            Ok(ServerMsg::Flows(batch)) => {
                if ctx.paused.get_untracked() {
                    return;
                }
                ctx.index.with_untracked(|ix| ctx.store.update(|s| s.push(batch, ix)));
            }
            Err(err) => leptos::logging::warn!("bad message: {err}"),
        }
    });
    let on_close = Closure::<dyn FnMut()>::new(move || {
        ctx.connected.set(false);
        set_timeout(move || connect(ctx), 2000);
    });

    ws.set_onopen(Some(on_open.as_ref().unchecked_ref()));
    ws.set_onmessage(Some(on_message.as_ref().unchecked_ref()));
    ws.set_onclose(Some(on_close.as_ref().unchecked_ref()));
    // The socket keeps the handlers alive; a few bytes leak per reconnect.
    on_open.forget();
    on_message.forget();
    on_close.forget();
}
