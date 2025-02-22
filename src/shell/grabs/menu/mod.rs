use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};

use calloop::LoopHandle;
use cosmic::{
    iced::{Alignment, Background},
    iced_core::{alignment::Horizontal, Border, Length, Rectangle as IcedRectangle},
    iced_widget::{self, text::Style as TextStyle, Column, Row},
    theme,
    widget::{button, divider, horizontal_space, icon::from_name, text},
    Apply as _, Task,
};
use smithay::{
    backend::{
        input::ButtonState,
        renderer::{
            element::{memory::MemoryRenderBufferRenderElement, AsRenderElements},
            ImportMem, Renderer,
        },
    },
    desktop::space::SpaceElement,
    input::{
        pointer::{
            AxisFrame, ButtonEvent, GestureHoldBeginEvent, GestureHoldEndEvent,
            GesturePinchBeginEvent, GesturePinchEndEvent, GesturePinchUpdateEvent,
            GestureSwipeBeginEvent, GestureSwipeEndEvent, GestureSwipeUpdateEvent,
            GrabStartData as PointerGrabStartData, MotionEvent, PointerGrab, PointerInnerHandle,
            PointerTarget, RelativeMotionEvent,
        },
        Seat,
    },
    output::Output,
    utils::{Logical, Point, Rectangle, Size},
};

use crate::{
    shell::focus::target::PointerFocusTarget,
    shell::SeatExt,
    state::State,
    utils::{
        iced::{IcedElement, Program},
        prelude::{Global, OutputExt, PointGlobalExt, PointLocalExt, SizeExt},
    },
};

use super::ResizeEdge;

mod default;
mod item;
pub use self::default::*;

pub struct MenuGrabState {
    elements: Arc<Mutex<Vec<Element>>>,
}
pub type SeatMenuGrabState = Mutex<Option<MenuGrabState>>;

impl MenuGrabState {
    pub fn render<I, R>(&self, renderer: &mut R, output: &Output) -> Vec<I>
    where
        R: Renderer + ImportMem,
        <R as Renderer>::TextureId: Send + Clone + 'static,
        I: From<MemoryRenderBufferRenderElement<R>>,
    {
        let scale = output.current_scale().fractional_scale();
        self.elements
            .lock()
            .unwrap()
            .iter()
            .flat_map(|elem| {
                elem.iced.render_elements(
                    renderer,
                    elem.position
                        .to_local(output)
                        .as_logical()
                        .to_physical_precise_round(scale),
                    scale.into(),
                    1.0,
                )
            })
            .collect()
    }

    pub fn set_theme(&self, theme: cosmic::Theme) {
        for element in &*self.elements.lock().unwrap() {
            element.iced.set_theme(theme.clone())
        }
    }
}

#[derive(Clone)]
pub enum Item {
    Separator,
    Submenu {
        title: String,
        items: Vec<Item>,
    },
    Entry {
        title: String,
        shortcut: Option<String>,
        on_press: Arc<Box<dyn Fn(&LoopHandle<'_, State>) + Send + Sync>>,
        toggled: bool,
        submenu: bool,
        disabled: bool,
    },
}

impl Item {
    pub fn new<S: Into<String>, F: Fn(&LoopHandle<'_, State>) + Send + Sync + 'static>(
        title: S,
        on_press: F,
    ) -> Item {
        Item::Entry {
            title: title.into(),
            shortcut: None,
            on_press: Arc::new(Box::new(on_press)),
            toggled: false,
            submenu: false,
            disabled: false,
        }
    }

    pub fn new_submenu<S: Into<String>>(title: S, items: Vec<Item>) -> Item {
        Item::Submenu {
            title: title.into(),
            items,
        }
    }

    pub fn shortcut(mut self, shortcut: impl Into<Option<String>>) -> Self {
        if let Item::Entry {
            shortcut: ref mut s,
            ..
        } = self
        {
            *s = shortcut.into();
        }
        self
    }

    pub fn toggled(mut self, toggled: bool) -> Self {
        if let Item::Entry {
            toggled: ref mut t, ..
        } = self
        {
            *t = toggled;
        }
        self
    }

    pub fn disabled(mut self, disabled: bool) -> Self {
        if let Item::Entry {
            disabled: ref mut d,
            ..
        } = self
        {
            *d = disabled;
        }
        self
    }
}

/// Menu that comes up when right-clicking an application header bar
pub struct ContextMenu {
    items: Vec<Item>,
    selected: AtomicBool,
    row_width: Mutex<Option<f32>>,
}

impl ContextMenu {
    pub fn new(items: Vec<Item>) -> ContextMenu {
        ContextMenu {
            items,
            selected: AtomicBool::new(false),
            row_width: Mutex::new(None),
        }
    }
}

#[derive(Debug, Clone)]
pub enum Message {
    ItemEntered(usize, IcedRectangle<f32>),
    ItemPressed(usize),
    ItemLeft(usize, IcedRectangle<f32>),
}

impl item::CursorEvents for Message {
    fn cursor_entered(idx: usize, bounds: IcedRectangle<f32>) -> Self {
        Message::ItemEntered(idx, bounds)
    }

    fn cursor_left(idx: usize, bounds: IcedRectangle<f32>) -> Self {
        Message::ItemLeft(idx, bounds)
    }
}

impl Program for ContextMenu {
    type Message = Message;

    fn update(
        &mut self,
        message: Self::Message,
        loop_handle: &LoopHandle<'static, crate::state::State>,
    ) -> Task<Self::Message> {
        match message {
            Message::ItemPressed(idx) => {
                if let Some(Item::Entry { on_press, .. }) = self.items.get_mut(idx) {
                    (on_press)(loop_handle);
                    self.selected.store(true, Ordering::SeqCst);
                }
            }
            Message::ItemEntered(idx, bounds) => {
                if let Some(Item::Submenu { items, .. }) = self.items.get_mut(idx) {
                    let items = items.clone();
                    let _ = loop_handle.insert_idle(move |state| {
                        let seat = state
                            .common
                            .shell
                            .read()
                            .unwrap()
                            .seats
                            .last_active()
                            .clone();
                        let grab_state = seat
                            .user_data()
                            .get::<SeatMenuGrabState>()
                            .unwrap()
                            .lock()
                            .unwrap();

                        if let Some(grab_state) = &*grab_state {
                            let mut elements = grab_state.elements.lock().unwrap();

                            let position = elements.last().unwrap().position;
                            let element = IcedElement::new(
                                ContextMenu::new(items),
                                Size::default(),
                                state.common.event_loop_handle.clone(),
                                state.common.theme.clone(),
                            );

                            let min_size = element.minimum_size();
                            element.with_program(|p| {
                                *p.row_width.lock().unwrap() = Some(min_size.w as f32);
                            });
                            element.resize(min_size);

                            let output = seat.active_output();
                            let position = [
                                // to the right -> down
                                Rectangle::new(
                                    position
                                        + Point::from((
                                            bounds.width.ceil() as i32,
                                            bounds.y.ceil() as i32,
                                        )),
                                    min_size.as_global(),
                                ),
                                // to the right -> up
                                Rectangle::new(
                                    position
                                        + Point::from((
                                            bounds.width.ceil() as i32,
                                            bounds.y.ceil() as i32 + bounds.height.ceil() as i32
                                                - min_size.h,
                                        )),
                                    min_size.as_global(),
                                ),
                                // to the left -> down
                                Rectangle::new(
                                    position + Point::from((-min_size.w, bounds.y.ceil() as i32)),
                                    min_size.as_global(),
                                ),
                                // to the left -> up
                                Rectangle::new(
                                    position
                                        + Point::from((
                                            -min_size.w,
                                            bounds.y.ceil() as i32 + bounds.height.ceil() as i32
                                                - min_size.h,
                                        )),
                                    min_size.as_global(),
                                ),
                            ]
                            .iter()
                            .rev() // preference of max_by_key is backwards
                            .max_by_key(|rect| {
                                output
                                    .geometry()
                                    .intersection(**rect)
                                    .map(|rect| rect.size.w * rect.size.h)
                            })
                            .unwrap()
                            .loc;
                            element.output_enter(&output, element.bbox());

                            elements.push(Element {
                                iced: element,
                                position,
                                pointer_entered: false,
                            })
                        }
                    });
                }
            }
            Message::ItemLeft(idx, _) => {
                if let Some(Item::Submenu { .. }) = self.items.get_mut(idx) {
                    let _ = loop_handle.insert_idle(|state| {
                        let seat = state
                            .common
                            .shell
                            .read()
                            .unwrap()
                            .seats
                            .last_active()
                            .clone();
                        let grab_state = seat
                            .user_data()
                            .get::<SeatMenuGrabState>()
                            .unwrap()
                            .lock()
                            .unwrap();

                        if let Some(grab_state) = &*grab_state {
                            let mut elements = grab_state.elements.lock().unwrap();
                            elements.pop();
                        }
                    });
                }
            }
        };

        Task::none()
    }

    fn view(&self) -> cosmic::Element<'_, Self::Message> {
        let width = self
            .row_width
            .lock()
            .unwrap()
            .map(|size| Length::Fixed(size))
            .unwrap_or(Length::Shrink);
        let mode = match width {
            Length::Shrink => Length::Shrink,
            _ => Length::Fill,
        };

        Column::with_children(self.items.iter().enumerate().map(|(idx, item)| {
            match item {
                Item::Separator => divider::horizontal::light().into(),
                Item::Submenu { title, .. } => Row::with_children(vec![
                    horizontal_space().width(16).into(),
                    text::body(title).width(mode).into(),
                    from_name("go-next-symbolic")
                        .size(16)
                        .prefer_svg(true)
                        .icon()
                        .into(),
                ])
                .spacing(8)
                .width(width)
                .padding([8, 16])
                .align_y(Alignment::Center)
                .apply(|row| item::SubmenuItem::new(row, idx))
                .style(theme::Button::MenuItem)
                .into(),
                Item::Entry {
                    title,
                    shortcut,
                    toggled,
                    disabled,
                    ..
                } => {
                    let mut components = vec![
                        if *toggled {
                            from_name("object-select-symbolic")
                                .size(16)
                                .prefer_svg(true)
                                .icon()
                                .class(theme::Svg::custom(|theme| iced_widget::svg::Style {
                                    color: Some(theme.cosmic().accent.base.into()),
                                }))
                                .into()
                        } else {
                            horizontal_space().width(16).into()
                        },
                        text::body(title)
                            .width(mode)
                            .class(if *disabled {
                                theme::Text::Custom(|theme| {
                                    let mut color = theme.cosmic().background.component.on;
                                    color.alpha *= 0.5;
                                    TextStyle {
                                        color: Some(color.into()),
                                    }
                                })
                            } else {
                                theme::Text::Default
                            })
                            .into(),
                        horizontal_space().width(16).into(),
                    ];
                    if let Some(shortcut) = shortcut.as_ref() {
                        components.push(
                            text::body(shortcut)
                                .align_x(Horizontal::Right)
                                .width(Length::Shrink)
                                .class(theme::Text::Custom(|theme| {
                                    let mut color = theme.cosmic().background.component.on;
                                    color.alpha *= 0.75;
                                    TextStyle {
                                        color: Some(color.into()),
                                    }
                                }))
                                .into(),
                        );
                    }

                    Row::with_children(components)
                        .spacing(8)
                        .width(mode)
                        .align_y(Alignment::Center)
                        .apply(button::custom)
                        .width(width)
                        .padding([8, 16])
                        .on_press_maybe((!disabled).then_some(Message::ItemPressed(idx)))
                        .class(theme::Button::MenuItem)
                        .into()
                }
            }
        }))
        .width(Length::Shrink)
        .apply(iced_widget::container)
        .padding(1)
        .class(theme::Container::custom(|theme| {
            let cosmic = theme.cosmic();
            let component = &cosmic.background.component;
            iced_widget::container::Style {
                icon_color: Some(cosmic.accent.base.into()),
                text_color: Some(component.on.into()),
                background: Some(Background::Color(component.base.into())),
                border: Border {
                    radius: cosmic.radius_s().into(),
                    width: 1.0,
                    color: component.divider.into(),
                },
                shadow: Default::default(),
            }
        }))
        .width(Length::Shrink)
        .into()
    }
}

pub struct Element {
    iced: IcedElement<ContextMenu>,
    position: Point<i32, Global>,
    pointer_entered: bool,
}

pub struct MenuGrab {
    elements: Arc<Mutex<Vec<Element>>>,
    start_data: PointerGrabStartData<State>,
    seat: Seat<State>,
}

impl PointerGrab<State> for MenuGrab {
    fn motion(
        &mut self,
        state: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        _focus: Option<(PointerFocusTarget, Point<f64, Logical>)>,
        event: &MotionEvent,
    ) {
        {
            let mut guard = self.elements.lock().unwrap();
            let elements = &mut *guard;
            if let Some(i) = elements.iter().position(|elem| {
                let mut bbox = elem.iced.bbox();
                bbox.loc = elem.position.as_logical();

                bbox.contains(event.location.to_i32_round())
            }) {
                let element = &mut elements[i];

                let new_event = MotionEvent {
                    location: event.location - element.position.as_logical().to_f64(),
                    serial: event.serial,
                    time: event.time,
                };
                if !element.pointer_entered {
                    PointerTarget::enter(&element.iced, &self.seat, state, &new_event);
                    element.pointer_entered = true;
                } else {
                    element.iced.motion(&self.seat, state, &new_event);
                }
            } else {
                elements.iter_mut().for_each(|element| {
                    if element.pointer_entered {
                        PointerTarget::leave(
                            &element.iced,
                            &self.seat,
                            state,
                            event.serial,
                            event.time,
                        );
                        element.pointer_entered = false;
                    }
                })
            }
        }
        handle.motion(state, None, event);
    }

    fn relative_motion(
        &mut self,
        state: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        _focus: Option<(PointerFocusTarget, Point<f64, Logical>)>,
        event: &RelativeMotionEvent,
    ) {
        // While the grab is active, no client has pointer focus
        handle.relative_motion(state, None, event);
    }

    fn button(
        &mut self,
        state: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        event: &ButtonEvent,
    ) {
        let any_entered = self
            .elements
            .lock()
            .unwrap()
            .iter()
            .any(|elem| elem.pointer_entered);
        if !any_entered {
            if event.state == ButtonState::Pressed {
                handle.unset_grab(self, state, event.serial, event.time, true);
            }
        } else {
            let selected = {
                let elements = self.elements.lock().unwrap();
                let mut selected = false;
                for element in elements.iter().filter(|elem| elem.pointer_entered) {
                    element.iced.button(&self.seat, state, event);
                    selected = true;
                }
                selected
            };
            if selected && event.state == ButtonState::Released {
                handle.unset_grab(self, state, event.serial, event.time, true);
            } else {
                handle.button(state, event);
            }
        }
    }

    fn axis(
        &mut self,
        state: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        details: AxisFrame,
    ) {
        handle.axis(state, details);
    }

    fn frame(&mut self, data: &mut State, handle: &mut PointerInnerHandle<'_, State>) {
        handle.frame(data)
    }

    fn gesture_swipe_begin(
        &mut self,
        data: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        event: &GestureSwipeBeginEvent,
    ) {
        handle.gesture_swipe_begin(data, event)
    }

    fn gesture_swipe_update(
        &mut self,
        data: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        event: &GestureSwipeUpdateEvent,
    ) {
        handle.gesture_swipe_update(data, event)
    }

    fn gesture_swipe_end(
        &mut self,
        data: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        event: &GestureSwipeEndEvent,
    ) {
        handle.gesture_swipe_end(data, event)
    }

    fn gesture_pinch_begin(
        &mut self,
        data: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        event: &GesturePinchBeginEvent,
    ) {
        handle.gesture_pinch_begin(data, event)
    }

    fn gesture_pinch_update(
        &mut self,
        data: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        event: &GesturePinchUpdateEvent,
    ) {
        handle.gesture_pinch_update(data, event)
    }

    fn gesture_pinch_end(
        &mut self,
        data: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        event: &GesturePinchEndEvent,
    ) {
        handle.gesture_pinch_end(data, event)
    }

    fn gesture_hold_begin(
        &mut self,
        data: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        event: &GestureHoldBeginEvent,
    ) {
        handle.gesture_hold_begin(data, event)
    }

    fn gesture_hold_end(
        &mut self,
        data: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        event: &GestureHoldEndEvent,
    ) {
        handle.gesture_hold_end(data, event)
    }

    fn start_data(&self) -> &PointerGrabStartData<State> {
        &self.start_data
    }

    fn unset(&mut self, _data: &mut State) {}
}

impl MenuGrab {
    pub fn new(
        start_data: PointerGrabStartData<State>,
        seat: &Seat<State>,
        items: impl Iterator<Item = Item>,
        position: Point<i32, Global>,
        handle: LoopHandle<'static, crate::state::State>,
        theme: cosmic::Theme,
    ) -> MenuGrab {
        let items = items.collect::<Vec<_>>();
        let element = IcedElement::new(ContextMenu::new(items), Size::default(), handle, theme);
        let min_size = element.minimum_size();
        element.with_program(|p| {
            *p.row_width.lock().unwrap() = Some(min_size.w as f32);
        });
        element.resize(min_size);

        let output = seat.active_output();
        let position = [
            Rectangle::new(position, min_size.as_global()), // normal
            Rectangle::new(
                position - Point::from((min_size.w, 0)),
                min_size.as_global(),
            ), // flipped left
            Rectangle::new(
                position - Point::from((0, min_size.h)),
                min_size.as_global(),
            ), // flipped up
            Rectangle::new(
                position - Point::from((min_size.w, min_size.h)),
                min_size.as_global(),
            ), // flipped left & up
        ]
        .iter()
        .rev() // preference of max_by_key is backwards
        .max_by_key(|rect| {
            output
                .geometry()
                .intersection(**rect)
                .map(|rect| rect.size.w * rect.size.h)
        })
        .unwrap()
        .loc;

        element.output_enter(&output, element.bbox());

        let elements = Arc::new(Mutex::new(vec![Element {
            iced: element,
            position,
            pointer_entered: false,
        }]));

        let grab_state = MenuGrabState {
            elements: elements.clone(),
        };

        *seat
            .user_data()
            .get::<SeatMenuGrabState>()
            .unwrap()
            .lock()
            .unwrap() = Some(grab_state);

        MenuGrab {
            elements,
            start_data,
            seat: seat.clone(),
        }
    }
}

impl Drop for MenuGrab {
    fn drop(&mut self) {
        self.seat
            .user_data()
            .get::<SeatMenuGrabState>()
            .unwrap()
            .lock()
            .unwrap()
            .take();
    }
}
