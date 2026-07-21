//! Pane pop-out: hosting a pane's existing view in its own OS window.
//!
//! The terminal entity is window-independent — popping out only moves where
//! the `TerminalView` renders. The PTY keeps running across every move.

use gpui::{div, prelude::*, px, Context, SharedString, WeakEntity, Window};

use crate::app::SeanceApp;
use crate::theme::SeancePalette;

/// Root content of a popped-out pane window: slim title strip + the pane's
/// own `TerminalView` (the same entity the main window renders when tiled).
pub struct PopoutView {
    pub slug: String,
    pub name: String,
    pub view: gpui::AnyView,
    pub app: WeakEntity<SeanceApp>,
}

impl Render for PopoutView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let name = self.name.clone();
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(SeancePalette::bg())
            .child(
                div()
                    .flex_none()
                    .h(px(28.))
                    .px_2()
                    .flex()
                    .items_center()
                    .gap_2()
                    .bg(SeancePalette::bg_elevated())
                    .border_b_1()
                    .border_color(SeancePalette::border())
                    .child(
                        div()
                            .text_color(SeancePalette::flame())
                            .text_sm()
                            .child("✦"),
                    )
                    .child(
                        div()
                            .flex_1()
                            .text_xs()
                            .text_color(SeancePalette::text_dim())
                            .overflow_hidden()
                            .child(name),
                    )
                    .child(
                        div()
                            .id(SharedString::from(format!("return-{}", self.slug)))
                            .px_2()
                            .py_0p5()
                            .rounded_md()
                            .text_xs()
                            .text_color(SeancePalette::violet())
                            .bg(SeancePalette::surface())
                            .hover(|s| s.bg(SeancePalette::border()))
                            .cursor_pointer()
                            .on_click(cx.listener(|this, _, _window, cx| {
                                let slug = this.slug.clone();
                                if let Some(app) = this.app.upgrade() {
                                    app.update(cx, |app, cx| app.pop_in(&slug, cx));
                                }
                            }))
                            .child("⇲ return to circle"),
                    ),
            )
            .child(div().flex_1().overflow_hidden().child(self.view.clone()))
    }
}
