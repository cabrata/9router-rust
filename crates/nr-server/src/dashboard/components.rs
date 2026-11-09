//! Shared maud components for dashboard pages (port of src/shared/components:
//! Modal, Card, Badge, Toggle, Pagination, Select, Input, Button, Tooltip).
//!
//! Plain functions returning `Markup`; styling lives in `static/dashboard.css`,
//! the tiny modal wiring in `static/dashboard.js`. Sibling lanes may adopt them.

use maud::{html, Markup, PreEscaped};

/// `Card.js`: titled box. `actions` renders at the right of the header.
pub fn card(title: Option<&str>, subtitle: Option<&str>, actions: Option<Markup>, body: Markup) -> Markup {
    html! {
        div class="card" {
            @if title.is_some() || actions.is_some() {
                div class="card-header" {
                    @if let Some(t) = title { h3 { (t) } }
                    @if let Some(a) = actions { div { (a) } }
                }
            }
            @if let Some(s) = subtitle { p class="card-subtitle" { (s) } }
            (body)
        }
    }
}

/// `Badge.js` variants.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BadgeVariant { Default, Primary, Success, Warning, Danger, Info }

impl BadgeVariant {
    fn class(self) -> &'static str {
        match self {
            Self::Default => "badge",
            Self::Primary => "badge badge-primary",
            Self::Success => "badge badge-success",
            Self::Warning => "badge badge-warning",
            Self::Danger => "badge badge-danger",
            Self::Info => "badge badge-info",
        }
    }
}

pub fn badge(variant: BadgeVariant, text: &str) -> Markup {
    html! { span class=(variant.class()) { (text) } }
}

/// `Button.js` variants.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ButtonVariant { Primary, Secondary, Ghost, Danger }

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ButtonSize { Sm, Md, Lg }

pub fn button_class(variant: ButtonVariant, size: ButtonSize) -> String {
    let v = match variant {
        ButtonVariant::Primary => " btn-primary",
        ButtonVariant::Secondary => " btn-secondary",
        ButtonVariant::Ghost => " btn-ghost",
        ButtonVariant::Danger => " btn-danger",
    };
    let s = match size { ButtonSize::Sm => " btn-sm", ButtonSize::Md => "", ButtonSize::Lg => " btn-lg" };
    format!("btn{v}{s}")
}

/// `type` is `button` | `submit`. Extra attributes (hx-*) can be added by wrapping
/// pages with their own markup; keep this the simple case.
pub fn button(variant: ButtonVariant, size: ButtonSize, kind: &str, disabled: bool, label: &str) -> Markup {
    html! { button type=(kind) class=(button_class(variant, size)) disabled[disabled] { (label) } }
}

/// `Input.js`: label + input + optional hint/error.
pub fn input(label: Option<&str>, name: &str, kind: &str, value: &str, placeholder: &str, hint: Option<&str>, error: Option<&str>) -> Markup {
    html! {
        div class="form-row" {
            @if let Some(l) = label { label for=(name) { (l) } }
            input type=(kind) id=(name) name=(name) value=(value) placeholder=(placeholder);
            @if let Some(e) = error { div class="field-error" { (e) } }
            @else if let Some(h) = hint { div class="field-hint" { (h) } }
        }
    }
}

/// `Select.js`: `options` are `(value, label)`; `selected` matches a value.
pub fn select(label: Option<&str>, name: &str, options: &[(&str, &str)], selected: &str) -> Markup {
    html! {
        div class="form-row" {
            @if let Some(l) = label { label for=(name) { (l) } }
            select id=(name) name=(name) {
                @for (v, text) in options { option value=(v) selected[*v == selected] { (text) } }
            }
        }
    }
}

/// `Toggle.js`: a checkbox styled as a switch (no JS). Submits `name=on` when checked,
/// so it works in plain forms and with `hx-trigger="change"`.
pub fn toggle(name: &str, checked: bool, disabled: bool, label: Option<&str>) -> Markup {
    html! {
        label class="toggle" {
            input type="checkbox" name=(name) checked[checked] disabled[disabled];
            span class="track" {}
            @if let Some(l) = label { span { (l) } }
        }
    }
}

/// `Tooltip.js`: CSS-only hover/focus tooltip.
pub fn tooltip(text: &str, child: Markup) -> Markup {
    html! { span class="tooltip" data-tip=(text) tabindex="0" { (child) } }
}

/// `Modal.js` on a native `<dialog>`. Open with an element carrying
/// `data-modal-open="<id>"` (see [`modal_trigger`]); `data-modal-close` closes it.
pub fn modal(id: &str, title: &str, body: Markup, footer: Option<Markup>) -> Markup {
    html! {
        dialog id=(id) class="modal" {
            div class="modal-head" {
                h3 { (title) }
                button type="button" class="btn btn-sm btn-ghost" data-modal-close aria-label="Close" { (PreEscaped("&times;")) }
            }
            div class="modal-body" { (body) }
            @if let Some(f) = footer { div class="modal-foot" { (f) } }
        }
    }
}

pub fn modal_trigger(id: &str, class: &str, label: &str) -> Markup {
    html! { button type="button" class=(class) data-modal-open=(id) { (label) } }
}

/// Page window used by [`pagination`]: 1-based pages, `None` = ellipsis.
/// Always shows first, last and `current ± 1`.
pub fn page_window(current: usize, total_pages: usize) -> Vec<Option<usize>> {
    let mut out = Vec::new();
    let mut last = 0;
    for p in 1..=total_pages {
        if p == 1 || p == total_pages || p.abs_diff(current) <= 1 {
            if last != 0 && p - last > 1 { out.push(None); }
            out.push(Some(p));
            last = p;
        }
    }
    out
}

/// `Pagination.js`. `base` is the page URL; links append `?page=N` (or `&page=N`
/// when `base` already has a query). `current` is clamped into `1..=total_pages`.
pub fn pagination(base: &str, current: usize, page_size: usize, total_items: usize) -> Markup {
    let total_pages = total_items.div_ceil(page_size.max(1)).max(1);
    let current = current.clamp(1, total_pages);
    let sep = if base.contains('?') { '&' } else { '?' };
    let href = |p: usize| format!("{base}{sep}page={p}");
    html! {
        nav class="pagination" aria-label="Pagination" {
            @if current > 1 { a class="page" href=(href(current - 1)) { "‹" } }
            @else { span class="page disabled" { "‹" } }
            @for p in page_window(current, total_pages) {
                @match p {
                    None => span class="gap" { "…" },
                    Some(p) if p == current => span class="page current" aria-current="page" { (p) },
                    Some(p) => a class="page" href=(href(p)) { (p) },
                }
            }
            @if current < total_pages { a class="page" href=(href(current + 1)) { "›" } }
            @else { span class="page disabled" { "›" } }
            span class="info" { (total_items) " items" }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_has_ellipsis() {
        assert_eq!(page_window(1, 3), vec![Some(1), Some(2), Some(3)]);
        assert_eq!(page_window(5, 10), vec![Some(1), None, Some(4), Some(5), Some(6), None, Some(10)]);
    }

    #[test]
    fn components_escape_and_render() {
        let s = badge(BadgeVariant::Success, "<x>").into_string();
        assert_eq!(s, r#"<span class="badge badge-success">&lt;x&gt;</span>"#);
        let p = pagination("/dashboard/x?a=1", 99, 10, 25).into_string();
        assert!(p.contains("a=1&amp;page=2") && p.contains("page current"));
        assert!(toggle("on", true, false, None).into_string().contains("checked"));
        assert!(select(None, "s", &[("a", "A"), ("b", "B")], "b").into_string().contains(r#"<option value="b" selected>"#));
        assert!(modal("m", "T", html! {}, None).into_string().contains("<dialog id=\"m\""));
    }
}
