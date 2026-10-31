#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Event {
    Motion,
    KeyPress(u8),
    ButtonPress(u8),
    ButtonRelease(u8),
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
