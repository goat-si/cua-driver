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
                self.resources.press_submit(*resource);
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
            serde_json::json!({"resource": format!("{resource:?}"), "released": result.is_ok(), "cleanup_failed": result.is_err(), "error": result.err().map(|error| error.error), "state": format!("{:?}", self.resources.state(&resource))})
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
    fn guard_refusal_sends_no_input_or_cleanup_release() {
        use cua_driver_core::guarded_dispatch::{run_guarded, GuardedResult, SendFailure};
        use cua_driver_core::window_basis::BasisRefusal;
        struct Fake(Vec<Event>);
        impl Requests for Fake {
            fn submit(&mut self, event: Event) -> Result<(), SendFailure> {
                self.0.push(event);
                Ok(())
            }
        }
        let events = plan(true, &[37], 1, 1);
        let mut sink = Sink {
            requests: Fake(vec![]),
            events,
            resources: Default::default(),
        };
        let GuardedResult::Refused { outcome, .. } = run_guarded(&mut sink, 4, || {
            Err(BasisRefusal {
                code: "stale_basis",
                reason: "window_moved",
            })
        }) else {
            panic!()
        };
        assert_eq!(outcome.events_sent, 0);
        assert!(outcome.cleanup_results.is_empty());
        assert!(sink.requests.0.is_empty());
    }

    #[test]
    fn success_releases_modifiers_in_reverse_order() {
        use cua_driver_core::guarded_dispatch::{run_guarded, GuardedResult, SendFailure};
        struct Fake(Vec<Event>);
        impl Requests for Fake {
            fn submit(&mut self, event: Event) -> Result<(), SendFailure> {
                self.0.push(event);
                Ok(())
            }
        }
        let events = plan(true, &[37, 50], 1, 1);
        let mut sink = Sink {
            requests: Fake(vec![]),
            events: events.clone(),
            resources: Default::default(),
        };
        let GuardedResult::Sent { outcome } = run_guarded(&mut sink, events.len(), || Ok(()))
        else {
            panic!()
        };
        assert_eq!(
            &sink.requests.0[events.len()..],
            &[Event::KeyRelease(50), Event::KeyRelease(37)]
        );
        assert_eq!(outcome.cleanup_results.len(), 2);
        assert!(sink.resources.needing_cleanup().is_empty());
    }

    #[test]
    fn failed_cleanup_reports_remaining_state() {
        use cua_driver_core::guarded_dispatch::{run_guarded, Emitted, GuardedResult, SendFailure};
        use cua_driver_core::window_basis::DispatchEffect;
        struct Fake;
        impl Requests for Fake {
            fn submit(&mut self, event: Event) -> Result<(), SendFailure> {
                if matches!(event, Event::ButtonRelease(_)) {
                    Err(SendFailure {
                        error: "release lost".into(),
                        emitted: Emitted::Unknown,
                    })
                } else {
                    Ok(())
                }
            }
        }
        let events = plan(false, &[], 1, 1);
        let mut sink = Sink {
            requests: Fake,
            events,
            resources: Default::default(),
        };
        let GuardedResult::Sent { outcome } = run_guarded(&mut sink, 2, || Ok(())) else {
            panic!()
        };
        assert_eq!(outcome.effect, DispatchEffect::Uncertain);
        assert_eq!(outcome.cleanup_results[0]["cleanup_failed"], true);
        assert_eq!(outcome.cleanup_results[0]["state"], "PossiblyReleased");
    }

    #[test]
    fn every_failure_index_cleans_possible_resources() {
        use cua_driver_core::guarded_dispatch::{run_guarded, Emitted, GuardedResult, SendFailure};
        use cua_driver_core::window_basis::DispatchEffect;
        use std::collections::HashSet;
        #[derive(Clone, Copy)]
        enum Failure {
            NotEmitted,
            UnknownBefore,
            UnknownAfter,
        }
        struct Fake {
            held: HashSet<(bool, u8)>,
            index: usize,
            fail: usize,
            failure: Failure,
        }
        impl Requests for Fake {
            fn submit(&mut self, event: Event) -> Result<(), SendFailure> {
                let fail = self.index == self.fail;
                self.index += 1;
                if !fail || matches!(self.failure, Failure::UnknownAfter) {
                    match event {
                        Event::KeyPress(k) => {
                            self.held.insert((true, k));
                        }
                        Event::KeyRelease(k) => {
                            self.held.remove(&(true, k));
                        }
                        Event::ButtonPress(b) => {
                            self.held.insert((false, b));
                        }
                        Event::ButtonRelease(b) => {
                            self.held.remove(&(false, b));
                        }
                        Event::Motion => {}
                    }
                }
                if fail {
                    Err(SendFailure {
                        error: "injected".into(),
                        emitted: if matches!(self.failure, Failure::NotEmitted) {
                            Emitted::NotEmitted
                        } else {
                            Emitted::Unknown
                        },
                    })
                } else {
                    Ok(())
                }
            }
        }
        for foreground in [false, true] {
            for modifiers in [&[][..], &[37, 50][..]] {
                if !foreground && !modifiers.is_empty() {
                    continue;
                }
                for button in [1, 3] {
                    for count in [1, 2] {
                        let events = plan(foreground, modifiers, button, count);
                        for index in 0..events.len() {
                            for failure in [
                                Failure::NotEmitted,
                                Failure::UnknownBefore,
                                Failure::UnknownAfter,
                            ] {
                                let mut sink = Sink {
                                    requests: Fake {
                                        held: HashSet::new(),
                                        index: 0,
                                        fail: index,
                                        failure,
                                    },
                                    events: events.clone(),
                                    resources: Default::default(),
                                };
                                let GuardedResult::Sent { outcome } =
                                    run_guarded(&mut sink, events.len(), || Ok(()))
                                else {
                                    panic!()
                                };
                                assert_eq!(outcome.events_sent, index as u32);
                                assert_eq!(
                                    outcome.effect,
                                    if matches!(failure, Failure::NotEmitted) {
                                        if index == 0 {
                                            DispatchEffect::Refused
                                        } else {
                                            DispatchEffect::Partial
                                        }
                                    } else {
                                        DispatchEffect::Uncertain
                                    }
                                );
                                assert!(sink.requests.held.is_empty(), "event {index} {events:?}");
                                assert!(sink.resources.needing_cleanup().is_empty());
                                assert_eq!(
                                    outcome.cleanup_results.last().unwrap()["error"],
                                    "injected"
                                );
                            }
                        }
                    }
                }
            }
        }
    }

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
