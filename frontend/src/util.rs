//! Small browser helpers and formatting.

use serde::de::DeserializeOwned;
use wasm_bindgen::closure::Closure;
use wasm_bindgen::JsCast;

pub fn window() -> web_sys::Window {
    web_sys::window().expect("no window")
}

pub fn set_timeout(f: impl FnOnce() + 'static, ms: i32) {
    let cb = Closure::once_into_js(f);
    let _ = window().set_timeout_with_callback_and_timeout_and_arguments_0(cb.unchecked_ref(), ms);
}

/// Runs `f` every `ms` milliseconds for the lifetime of the page.
pub fn set_interval(f: impl FnMut() + 'static, ms: i32) {
    let cb = Closure::<dyn FnMut()>::new(f);
    let _ = window().set_interval_with_callback_and_timeout_and_arguments_0(cb.as_ref().unchecked_ref(), ms);
    cb.forget();
}

pub async fn get_json<T: DeserializeOwned>(path: &str) -> Result<T, String> {
    let r = gloo_net::http::Request::get(path).send().await.map_err(|e| e.to_string())?;
    if !r.ok() {
        return Err(format!("{path}: HTTP {}", r.status()));
    }
    r.json::<T>().await.map_err(|e| e.to_string())
}

/// `shop/web-7d9f-x2k` -> `web-7d9f-x2k`, `reserved:world` -> `world`.
pub fn short_name(id: &str) -> &str {
    if let Some(r) = id.strip_prefix("reserved:") {
        return r;
    }
    if let Some(r) = id.strip_prefix("ns:") {
        return r;
    }
    id.rsplit_once('/').map_or(id, |(_, n)| n)
}

/// Namespace part of a pod id, or empty.
pub fn namespace_of(id: &str) -> &str {
    match id.split_once('/') {
        Some((ns, _)) if !id.starts_with("reserved:") => ns,
        _ => "",
    }
}

pub fn fmt_rate(r: f32) -> String {
    if r <= 0.0 {
        "0/s".into()
    } else if r < 0.1 {
        format!("{r:.2}/s")
    } else if r < 10.0 {
        format!("{r:.1}/s")
    } else {
        format!("{r:.0}/s")
    }
}

pub fn fmt_time(ms: i64) -> String {
    let d = js_sys::Date::new(&wasm_bindgen::JsValue::from_f64(ms as f64));
    format!("{:02}:{:02}:{:02}.{:03}", d.get_hours(), d.get_minutes(), d.get_seconds(), d.get_milliseconds())
}

/// Stable hue per namespace so colours stay the same across reloads.
pub fn hue(s: &str) -> u32 {
    let mut h: u32 = 2_166_136_261;
    for b in s.bytes() {
        h ^= u32::from(b);
        h = h.wrapping_mul(16_777_619);
    }
    h % 360
}
