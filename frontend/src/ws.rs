//! Live updates from the backend over a WebSocket, with reconnect.

use crate::app::{Ctx, MAX_FLOWS};
use crate::util::{set_timeout, window};
use common::ServerMsg;
use leptos::prelude::*;
use std::collections::{HashSet, VecDeque};
use wasm_bindgen::closure::Closure;
use wasm_bindgen::JsCast;
use web_sys::{MessageEvent, WebSocket};

pub fn connect(ctx: Ctx) {
    let loc = window().location();
    let scheme = if loc.protocol().as_deref() == Ok("https:") { "wss" } else { "ws" };
    let url = format!("{scheme}://{}/api/ws", loc.host().unwrap_or_default());
    let Ok(ws) = WebSocket::new(&url) else {
        set_timeout(move || connect(ctx), 3000);
        return;
    };

    let on_open = Closure::<dyn FnMut()>::new(move || {
        ctx.connected.set(true);
        // Every connection starts with the server's recent flows, so start
        // the table over from them (unless it is paused). Old rows would
        // repeat, and after a server restart their ids would name other flows.
        if !ctx.paused.get_untracked() {
            ctx.flows.update(VecDeque::clear);
        }
    });
    let on_message = Closure::<dyn FnMut(MessageEvent)>::new(move |e: MessageEvent| {
        let Some(text) = e.data().as_string() else { return };
        match serde_json::from_str::<ServerMsg>(&text) {
            Ok(ServerMsg::Graph(g)) => ctx.graph.set(g),
            Ok(ServerMsg::Status(s)) => ctx.status.set(s),
            Ok(ServerMsg::Flows(batch)) => {
                if ctx.paused.get_untracked() {
                    return;
                }
                ctx.flows.update(|q| {
                    // The table is keyed by id, and a repeated key breaks it.
                    // The first batch on a connection can repeat flows from
                    // the opening snapshot.
                    let mut seen: HashSet<u64> = q.iter().map(|f| f.id).collect();
                    // Newest first.
                    for f in batch {
                        if !seen.insert(f.id) {
                            continue;
                        }
                        if q.len() >= MAX_FLOWS {
                            q.pop_back();
                        }
                        q.push_front(f);
                    }
                });
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
