#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Event {
    Motion,
    KeyPress(u8),
    ButtonPress(u8),
    ButtonRelease(u8),
    KeyRelease(u8),
}

trait Requests {
    fn submit(
        &mut self,
        event: Event,
    ) -> Result<(), cua_driver_core::guarded_dispatch::SendFailure>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Resource {
    Key(u8),
    Button(u8),
}

struct Sink<R> {
    requests: R,
    events: Vec<Event>,
    resources: cua_driver_core::input_resource::ResourceTable<Resource>,
}

impl<R: Requests> Sink<R> {
    fn send_event(
        &mut self,
        event: Event,
    ) -> Result<(), cua_driver_core::guarded_dispatch::SendFailure> {
        let (resource, press) = match event {
            Event::KeyPress(key) => (Some(Resource::Key(key)), true),
            Event::KeyRelease(key) => (Some(Resource::Key(key)), false),
            Event::ButtonPress(button) => (Some(Resource::Button(button)), true),
            Event::ButtonRelease(button) => (Some(Resource::Button(button)), false),
            Event::Motion => (None, false),
        };
        if let Some(resource) = &resource {
            if press {
                self.resources.press_submit(resource.clone());
            } else {
                self.resources.release_submit(resource);
            }
        }
        let result = self.requests.submit(event);
        if let Some(resource) = &resource {
            let state = result
                .as_ref()
                .map(|_| ())
                .map_err(|failure| failure.emitted);
            if press {
                self.resources.press_result(resource, state);
            } else {
                self.resources.release_result(resource, state);
            }
        }
        result
    }

    fn release_all(&mut self) -> Vec<serde_json::Value> {
        self.resources.needing_cleanup().into_iter().map(|resource| {
            let event = match resource { Resource::Key(key) => Event::KeyRelease(key), Resource::Button(button) => Event::ButtonRelease(button) };
            let result = self.send_event(event);
            serde_json::json!({"resource": format!("{resource:?}"), "released": result.is_ok(), "error": result.err().map(|error| error.error), "state": format!("{:?}", self.resources.state(&resource))})
        }).collect()
    }
}

impl<R: Requests> cua_driver_core::guarded_dispatch::InputSink for Sink<R> {
    fn prepare(&mut self) -> Vec<&'static str> {
        vec![]
    }
    fn send(&mut self, i: usize) -> Result<(), cua_driver_core::guarded_dispatch::SendFailure> {
        self.send_event(self.events[i])
    }
    fn cleanup(&mut self, _: usize) -> Vec<serde_json::Value> {
        self.release_all()
    }
    fn finish(&mut self) -> Vec<serde_json::Value> {
        self.release_all()
    }
}

fn plan(foreground: bool, modifiers: &[u8], button: u8, count: usize) -> Vec<Event> {
    let mut events = Vec::new();
    if foreground {
        events.push(Event::Motion);
        events.extend(modifiers.iter().copied().map(Event::KeyPress));
    }
    for _ in 0..count {
        events.push(Event::ButtonPress(button));
        events.push(Event::ButtonRelease(button));
    }
    events
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lost_press_reply_still_releases_button() {
        use cua_driver_core::guarded_dispatch::{run_guarded, Emitted, GuardedResult, SendFailure};
        use cua_driver_core::window_basis::DispatchEffect;
        struct Fake {
            held: bool,
        }
        impl Requests for Fake {
            fn submit(&mut self, e: Event) -> Result<(), SendFailure> {
                match e {
                    Event::ButtonPress(_) => {
                        self.held = true;
                        Err(SendFailure {
                            error: "lost reply".into(),
                            emitted: Emitted::Unknown,
                        })
                    }
                    Event::ButtonRelease(_) => {
                        self.held = false;
                        Ok(())
                    }
                    _ => Ok(()),
                }
            }
        }
        let mut sink = Sink {
            requests: Fake { held: false },
            events: plan(false, &[], 1, 1),
            resources: Default::default(),
        };
        let GuardedResult::Sent { outcome } = run_guarded(&mut sink, 2, || Ok(())) else {
            panic!()
        };
        assert_eq!(outcome.effect, DispatchEffect::Uncertain);
        assert!(!sink.requests.held);
        assert!(!outcome.cleanup_results.is_empty());
    }

    #[test]
    fn plans_foreground_and_background_clicks() {
        for button in [1, 3] {
            for count in [1, 2] {
                for modifiers in [&[][..], &[37, 50][..]] {
                    let mut expected = vec![Event::Motion];
                    expected.extend(modifiers.iter().copied().map(Event::KeyPress));
                    for _ in 0..count {
                        expected.push(Event::ButtonPress(button));
                        expected.push(Event::ButtonRelease(button));
                    }
                    assert_eq!(plan(true, modifiers, button, count), expected);
                    let mut background = vec![];
                    for _ in 0..count {
                        background.push(Event::ButtonPress(button));
                        background.push(Event::ButtonRelease(button));
                    }
                    assert_eq!(plan(false, &[], button, count), background);
                }
            }
        }
    }
}
