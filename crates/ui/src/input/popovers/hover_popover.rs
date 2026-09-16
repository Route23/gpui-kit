use std::{ops::Range, rc::Rc};

use gpui::{
    deferred, div, point, prelude::FluentBuilder as _, px, AnyElement, App, AppContext as _,
    AvailableSpace, Bounds, Element, ElementId, Entity, InteractiveElement, IntoElement,
    MouseDownEvent, ParentElement as _, Pixels, Render, StatefulInteractiveElement as _,
    StyleRefinement, Styled, Window,
};

use crate::{
    input::{popovers::render_markdown, InputState},
    StyledExt,
};

pub struct HoverPopover {
    editor: Entity<InputState>,
    /// The symbol range byte of the hover trigger.
    pub(crate) symbol_range: Range<usize>,
    pub(crate) hover: Rc<lsp_types::Hover>,
}

impl HoverPopover {
    pub fn new(
        editor: Entity<InputState>,
        symbol_range: Range<usize>,
        hover: &lsp_types::Hover,
        cx: &mut App,
    ) -> Entity<Self> {
        let hover = Rc::new(hover.clone());

        cx.new(|_| Self {
            editor,
            symbol_range,
            hover,
        })
    }

    pub(crate) fn is_same(&self, offset: usize) -> bool {
        self.symbol_range.contains(&offset)
    }
}

impl Render for HoverPopover {
    fn render(&mut self, _: &mut Window, cx: &mut gpui::Context<Self>) -> impl IntoElement {
        let contents = match self.hover.contents.clone() {
            lsp_types::HoverContents::Scalar(scalar) => match scalar {
                lsp_types::MarkedString::String(s) => s,
                lsp_types::MarkedString::LanguageString(ls) => ls.value,
            },
            lsp_types::HoverContents::Array(arr) => arr
                .into_iter()
                .map(|item| match item {
                    lsp_types::MarkedString::String(s) => s,
                    lsp_types::MarkedString::LanguageString(ls) => ls.value,
                })
                .collect::<Vec<_>>()
                .join("\n\n"),
            lsp_types::HoverContents::Markup(markup) => markup.value,
        };

        let above = self.editor.read(cx).mode.hover_above();
        Popover::new(
            "hover-popover",
            self.editor.clone(),
            self.symbol_range.clone(),
            move |window, cx| render_markdown("message", contents.clone(), window, cx),
        )
        .prefer_above(above)
        .report_hovered(true)
        .into_any_element()
    }
}

pub(crate) struct Popover {
    id: ElementId,
    style: StyleRefinement,
    editor: Entity<InputState>,
    range: Range<usize>,
    width_limit: Range<Pixels>,
    /// Which side of the line to try first.
    ///
    /// **A preference, not a promise**: the popover is measured before the
    /// side is picked, so one that does not fit still flips to the other side.
    prefer_above: bool,
    /// Whether to report the pointer being over the popover back to the input.
    ///
    /// Only the hover popover wants this -- the editor cannot see it for
    /// itself, because the content occludes.
    report_hovered: bool,
    content_builder: Box<dyn Fn(&mut Window, &mut App) -> AnyElement>,
}

impl Styled for Popover {
    fn style(&mut self) -> &mut StyleRefinement {
        &mut self.style
    }
}

impl Popover {
    pub fn new<F, E>(
        id: impl Into<ElementId>,
        editor: Entity<InputState>,
        range: Range<usize>,
        f: F,
    ) -> Self
    where
        F: Fn(&mut Window, &mut App) -> E + 'static,
        E: IntoElement,
    {
        Self {
            id: id.into(),
            editor,
            range,
            style: StyleRefinement::default(),
            width_limit: px(200.)..px(500.),
            prefer_above: true,
            report_hovered: false,
            content_builder: Box::new(move |window, cx| (f)(window, cx).into_any_element()),
        }
    }

    /// Try the space above the line first (the default), or below.
    pub(crate) fn prefer_above(mut self, above: bool) -> Self {
        self.prefer_above = above;
        self
    }

    /// Tell the input when the pointer is over the popover.
    fn report_hovered(mut self, report: bool) -> Self {
        self.report_hovered = report;
        self
    }

    /// Get the bounds of the range in the editor, if it is visible.
    fn trigger_bounds(&self, cx: &App) -> Option<Bounds<Pixels>> {
        let editor = self.editor.read(cx);
        let Some(last_layout) = editor.last_layout.as_ref() else {
            return None;
        };

        let Some(last_bounds) = editor.last_bounds else {
            return None;
        };

        let (_, _, start_pos) = editor.line_and_position_for_offset(self.range.start);
        let (_, _, end_pos) = editor.line_and_position_for_offset(self.range.end);

        let Some(start_pos) = start_pos else {
            return None;
        };
        let Some(end_pos) = end_pos else {
            return None;
        };

        Some(Bounds::from_corners(
            last_bounds.origin + start_pos,
            last_bounds.origin + end_pos + point(px(0.), last_layout.line_height),
        ))
    }
}

impl IntoElement for Popover {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

pub(crate) struct PopoverLayoutState {
    state: Entity<bool>,
    bounds: Bounds<Pixels>,
    element: Option<AnyElement>,
}

impl Element for Popover {
    type RequestLayoutState = PopoverLayoutState;
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        Some(self.id.clone())
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&gpui::GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (gpui::LayoutId, Self::RequestLayoutState) {
        let open_state = window.use_keyed_state("popover-open", cx, |_, _| true);
        let trigger_bounds = match self.trigger_bounds(cx) {
            Some(bounds) => bounds,
            None => {
                return (
                    div().into_any_element().request_layout(window, cx),
                    PopoverLayoutState {
                        bounds: Bounds::default(),
                        element: None,
                        state: open_state,
                    },
                )
            }
        };

        let max_width = self
            .width_limit
            .end
            .min(window.bounds().size.width - SNAP_TO_EDGE * 2)
            .max(px(200.));
        let max_height = (window.bounds().size.height - SNAP_TO_EDGE * 2).min(px(320.));

        let is_open = *open_state.read(cx);

        let editor = self.editor.clone();
        let report_hovered = self.report_hovered;
        let mut popover = deferred(
            div()
                .id("hover-popover-content")
                .when(!is_open, |s| s.invisible())
                .flex_none()
                .occlude()
                .when(report_hovered, |this| {
                    this.on_hover(move |hovered, _, cx| {
                        let hovered = *hovered;
                        editor.update(cx, |state, _| {
                            state.hover_popover_hovered = hovered;
                        });
                    })
                })
                .p_1()
                .text_xs()
                .popover_style(cx)
                .shadow_md()
                .max_w(max_width)
                .max_h(max_height)
                .overflow_y_scroll()
                .refine_style(&self.style)
                .child((self.content_builder)(window, cx)),
        )
        .into_any_element();

        let popover_size = popover.layout_as_root(AvailableSpace::min_size(), window, cx);
        const SNAP_TO_EDGE: Pixels = px(8.);
        let top_space = trigger_bounds.top() - SNAP_TO_EDGE;
        let right_space = window.bounds().size.width - trigger_bounds.left() - SNAP_TO_EDGE;

        let bottom_space =
            window.bounds().size.height - trigger_bounds.bottom() - SNAP_TO_EDGE;
        let above = point(
            trigger_bounds.left(),
            trigger_bounds.top() - popover_size.height,
        );
        let below = point(trigger_bounds.left(), trigger_bounds.bottom());
        // The side that is asked for, unless the popover does not fit there.
        let mut pos = if self.prefer_above {
            if popover_size.height > top_space {
                below
            } else {
                above
            }
        } else if popover_size.height > bottom_space {
            above
        } else {
            below
        };
        if popover_size.width > right_space {
            pos.x = trigger_bounds.right() - popover_size.width;
        }

        let mut empty = div().into_any_element();
        let layout_id = empty.request_layout(window, cx);
        (
            layout_id,
            PopoverLayoutState {
                bounds: Bounds {
                    origin: pos,
                    size: popover_size,
                },
                element: Some(popover),
                state: open_state,
            },
        )
    }

    fn prepaint(
        &mut self,
        _: Option<&gpui::GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        _: Bounds<Pixels>,
        request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let bounds = request_layout.bounds;
        let Some(popover) = request_layout.element.as_mut() else {
            return;
        };

        window.with_absolute_element_offset(bounds.origin, |window| {
            popover.prepaint(window, cx);
        })
    }

    fn paint(
        &mut self,
        _: Option<&gpui::GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        _: Bounds<Pixels>,
        request_layout: &mut Self::RequestLayoutState,
        _: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let bounds = request_layout.bounds;
        let Some(popover) = request_layout.element.as_mut() else {
            return;
        };

        popover.paint(window, cx);

        let open_state = request_layout.state.clone();
        // Mouse down out to hide.
        window.on_mouse_event(move |event: &MouseDownEvent, _, _, cx| {
            if !bounds.contains(&event.position) {
                open_state.update(cx, |open, cx| {
                    *open = false;
                    cx.notify();
                })
            }
        });

        if !self.report_hovered {
            return;
        }
        // Watching the pointer has to happen at the window, not on the input:
        // the input only hears about moves inside itself, so a pointer that
        // leaves the editor altogether would leave the popover up for good --
        // the very thing the hiding delay exists to stop.
        let editor = self.editor.clone();
        let trigger = self.trigger_bounds(cx).unwrap_or_default();
        window.on_mouse_event(move |event: &gpui::MouseMoveEvent, _, _, cx| {
            let on_popover = bounds.contains(&event.position);
            let on_symbol = trigger.contains(&event.position);
            editor.update(cx, |state, cx| {
                if state.hover_popover.is_none() {
                    return;
                }
                state.hover_popover_hovered = on_popover;
                // Being on the popover only counts as "still wanted" when it
                // is sticky; otherwise moving onto it starts the clock like
                // anywhere else.
                let keep = on_symbol || (on_popover && state.mode.hover_sticky());
                if keep {
                    state.cancel_hover_hide();
                } else {
                    state.schedule_hover_hide(cx);
                }
            });
        })
    }
}
