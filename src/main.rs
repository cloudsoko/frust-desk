//! Desk v1 — the product's face on the hardened kernel.
//!
//! The Desk is a pure renderer of
//! (DocType metadata, record JSON) per the headless contract, and a
//! CLIENT of the kernel REST surface — every byte of data it shows or writes
//! travels through `frust serve` under the acting user's session token. No
//! root SQL, no direct SurrealDB, no role headers.
//!
//! v1 boundaries (standing): list refresh is SSE-driven — the browser opens a
//! kernel event stream, with a 60 s meta-refresh tag as the fallback when the
//! stream can't open; no spreadsheet grids; dependent-field logic is
//! client-side via per-field runtime signals, so a field enabled or disabled
//! by another field's value updates without a kernel round-trip.

/// The Desk's design system, wired into the real pages. Lives
/// here — Desk-local — and never in the vendored `topcoat` tree, which keeps
/// the kernel and the framework lean, and the Desk owns its skin.
mod frust_ui;
mod brand;
mod money;
mod pages;
mod realtime;
mod workspace;

use serde::Deserialize;
use topcoat::{
    Result,
    context::Cx,
    cookie::{Cookie, Cookies, RouterBuilderCookieExt, cookies},
    router::{
        IntoResponse,
        Response,
        Router,
        RouterBuilderDiscoverExt,
        // v0.5.0 namespaced the content types under `content::` (the router
        // root no longer globs them out); `Wasm` is still ours.
        content::{Form, Js, Wasm},
        error::{SeeOther, bad_request, redirect, see_other},
        layout,
        page,
        path_param,
        query_params,
        route,
    },
    runtime::{Decimal, Signal, SignalDeclaration},
    view::{component, view},
};

#[tokio::main]
async fn main() {
    // no asset bundle, no client runtime: Desk v1 is fully server-rendered
    // (zero signals/shards — the v1 field set has no dependent fields)
    topcoat::start(Router::builder().cookies().discover().build())
        .await
        .unwrap();
}

// ── Kernel client ───────────────────────────────────────────────────────────

mod kernel;

use kernel::{
    Session, admit_or_busy as admit, err500, flash, kernel_status, request_tenant, require_session,
    session, take_flash,
};
use brand::{RawCss, brand_settings_meta, brand_style};
use money::{MONEY_SCALE, money_sub, pad_money};
use pages::DocType;
use realtime::live_updates;
use workspace::{workspace_links, workspace_list};
