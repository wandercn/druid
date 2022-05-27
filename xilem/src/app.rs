// Copyright 2022 The Druid Authors.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::sync::{Arc, Mutex};

use druid_shell::kurbo::Size;
use druid_shell::piet::{Color, Piet, RenderContext};
use druid_shell::WindowHandle;

use crate::event::AsyncWake;
use crate::id::IdPath;
use crate::widget::{CxState, EventCx, LayoutCx, PaintCx, Pod, UpdateCx, WidgetState};
use crate::{
    event::Event,
    id::Id,
    view::{Cx, View},
    widget::{RawEvent, Widget},
};

pub struct App<T, V: View<T>, F: FnMut(&mut T) -> V> {
    data: T,
    app_logic: F,
    view: Option<V>,
    id: Option<Id>,
    state: Option<V::State>,
    events: Vec<Event>,
    window_handle: WindowHandle,
    root_state: WidgetState,
    root_pod: Option<Pod>,
    size: Size,
    cx: Cx,
    wake_queue: WakeQueue,
}

/// State that's kept in a separate task for running the app
struct AppTask<T, V: View<T>, F: FnMut(&mut T) -> V> {
    req_chan: tokio::sync::mpsc::Receiver<AppReq<V, V::State>>,
    response_chan: tokio::sync::mpsc::Sender<RenderResponse<V, V::State>>,
    data: T,
    app_logic: F,
    view: Option<V>,
    state: Option<V::State>,
}

/// A message sent from the main UI thread to the app task
enum AppReq<V, S> {
    Events(Vec<Event>),
    Render,
    ReturnView(V, S),
}

/// A response sent to a render request.
struct RenderResponse<V, S> {
    prev: Option<V>,
    view: V,
    state: Option<S>,
}

#[derive(Clone, Default)]
pub struct WakeQueue(Arc<Mutex<Vec<IdPath>>>);

const BG_COLOR: Color = Color::rgb8(0x27, 0x28, 0x22);

impl<T, V: View<T>, F: FnMut(&mut T) -> V> App<T, V, F>
where
    V::Element: Widget + 'static,
{
    pub fn new(data: T, app_logic: F) -> Self {
        let wake_queue = Default::default();
        let cx = Cx::new(&wake_queue);
        App {
            data,
            app_logic,
            view: None,
            id: None,
            state: None,
            root_pod: None,
            events: Vec::new(),
            window_handle: Default::default(),
            root_state: Default::default(),
            size: Default::default(),
            cx,
            wake_queue,
        }
    }

    pub fn ensure_app(&mut self) {
        if self.view.is_none() {
            let view = (self.app_logic)(&mut self.data);
            let (id, state, element) = view.build(&mut self.cx);
            let root_pod = Pod::new(element);
            self.view = Some(view);
            self.id = Some(id);
            self.state = Some(state);
            self.root_pod = Some(root_pod);
        }
    }

    pub fn connect(&mut self, window_handle: WindowHandle) {
        self.window_handle = window_handle.clone();
        // This will be needed for wiring up async but is a stub for now.
        self.cx.set_handle(window_handle.get_idle_handle());
    }

    pub fn size(&mut self, size: Size) {
        self.size = size;
    }

    pub fn paint(&mut self, piet: &mut Piet) {
        let rect = self.size.to_rect();
        piet.fill(rect, &BG_COLOR);

        self.ensure_app();
        loop {
            let root_pod = self.root_pod.as_mut().unwrap();
            let mut cx_state = CxState::new(&self.window_handle, &mut self.events);
            let mut update_cx = UpdateCx::new(&mut cx_state, &mut self.root_state);
            root_pod.update(&mut update_cx);
            let mut layout_cx = LayoutCx::new(&mut cx_state, &mut self.root_state);
            root_pod.measure(&mut layout_cx);
            let proposed_size = self.size;
            root_pod.layout(&mut layout_cx, proposed_size);
            if cx_state.has_events() {
                // Rerun app logic, primarily for LayoutObserver
                // We might want some debugging here if the number of iterations
                // becomes extreme.
                self.run_app_logic();
                continue;
            }
            let mut layout_cx = LayoutCx::new(&mut cx_state, &mut self.root_state);
            let visible = root_pod.state.size.to_rect();
            root_pod.prepare_paint(&mut layout_cx, visible);
            if cx_state.has_events() {
                // Rerun app logic, primarily for virtualized scrolling
                self.run_app_logic();
                continue;
            }
            let mut paint_cx = PaintCx::new(&mut cx_state, &mut self.root_state, piet);
            root_pod.paint(&mut paint_cx);
            break;
        }
    }

    pub fn window_event(&mut self, event: RawEvent) {
        self.ensure_app();
        let root_pod = self.root_pod.as_mut().unwrap();
        let mut cx_state = CxState::new(&self.window_handle, &mut self.events);
        let mut event_cx = EventCx::new(&mut cx_state, &mut self.root_state);
        root_pod.event(&mut event_cx, &event);
        self.run_app_logic();
    }

    pub fn run_app_logic(&mut self) {
        for event in self.events.drain(..) {
            let id_path = &event.id_path[1..];
            self.view.as_ref().unwrap().event(
                id_path,
                self.state.as_mut().unwrap(),
                event.body,
                &mut self.data,
            );
        }
        // Re-rendering should be more lazy.
        let view = (self.app_logic)(&mut self.data);
        if let Some(element) = self.root_pod.as_mut().unwrap().downcast_mut() {
            let changed = view.rebuild(
                &mut self.cx,
                self.view.as_ref().unwrap(),
                self.id.as_mut().unwrap(),
                self.state.as_mut().unwrap(),
                element,
            );
            if changed {
                self.root_pod.as_mut().unwrap().request_update();
            }
            assert!(self.cx.is_empty(), "id path imbalance on rebuild");
        }
        self.view = Some(view);
    }

    pub fn wake_async(&mut self) {
        for id_path in self.wake_queue.take() {
            self.events.push(Event::new(id_path, AsyncWake));
        }
        self.run_app_logic();
    }
}

impl WakeQueue {
    // Returns true if the queue was empty.
    pub fn push_wake(&self, id_path: IdPath) -> bool {
        let mut queue = self.0.lock().unwrap();
        let was_empty = queue.is_empty();
        queue.push(id_path);
        was_empty
    }

    pub fn take(&self) -> Vec<IdPath> {
        std::mem::replace(&mut self.0.lock().unwrap(), Vec::new())
    }
}

impl<T, V: View<T>, F: FnMut(&mut T) -> V> AppTask<T, V, F>
where
    V::Element: Widget + 'static,
{
    async fn run(&mut self) {
        while let Some(req) = self.req_chan.recv().await {
            match req {
                AppReq::Events(events) => {
                    for event in events {
                        let id_path = &event.id_path[1..];
                        self.view.as_ref().unwrap().event(
                            id_path,
                            self.state.as_mut().unwrap(),
                            event.body,
                            &mut self.data,
                        );
                    }
                }
                AppReq::Render => self.render().await,
                AppReq::ReturnView(view, state) => {
                    self.view = Some(view);
                    self.state = Some(state);
                }
            }
        }
    }

    async fn render(&mut self) {
        let view = (self.app_logic)(&mut self.data);
        let response = RenderResponse {
            prev: self.view.take(),
            view,
            state: self.state.take(),
        };
        if self.response_chan.send(response).await.is_err() {
            println!("error sending response");
        }
    }
}