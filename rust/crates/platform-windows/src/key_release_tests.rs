use super::{held_after_prefix, KeyId};

#[test]
fn chord_each_inserted_prefix() {
    let (ctrl, shift, a) = (KeyId::Vk(17), KeyId::Vk(16), KeyId::Vk(65));
    let edges = [
        (ctrl, false),
        (shift, false),
        (a, false),
        (a, true),
        (shift, true),
        (ctrl, true),
    ];
    for (inserted, expected) in [
        vec![],
        vec![0],
        vec![1, 0],
        vec![2, 1, 0],
        vec![1, 0],
        vec![0],
        vec![],
        vec![],
    ]
    .into_iter()
    .enumerate()
    {
        assert_eq!(
            held_after_prefix(&edges, inserted),
            expected,
            "prefix {inserted}"
        );
    }
}

#[test]
fn scan_codes_do_not_alias_and_extended_is_distinct() {
    let ctrl = KeyId::Scan {
        scan: 29,
        extended: false,
    };
    let right_ctrl = KeyId::Scan {
        scan: 29,
        extended: true,
    };
    let a = KeyId::Scan {
        scan: 30,
        extended: false,
    };
    let edges = [
        (ctrl, false),
        (right_ctrl, false),
        (a, false),
        (a, true),
        (right_ctrl, true),
        (ctrl, true),
    ];
    assert_eq!(held_after_prefix(&edges, 4), vec![1, 0]);
    assert_eq!(held_after_prefix(&edges, 5), vec![0]);
}

#[test]
fn repeated_down_up_down_tracks_latest_press() {
    let a = KeyId::Vk(65);
    let edges = [(a, false), (a, true), (a, false), (a, true)];
    assert_eq!(held_after_prefix(&edges, 3), vec![2]);
    let edges = [(a, false), (a, false), (a, true), (KeyId::Vk(66), false)];
    assert_eq!(held_after_prefix(&edges, 3), Vec::<usize>::new());
}

#[test]
fn unicode_surrogates_are_independent_packets() {
    let (high, low) = (KeyId::Unicode(0xd83d), KeyId::Unicode(0xde42));
    let edges = [(high, false), (high, true), (low, false), (low, true)];
    assert_eq!(held_after_prefix(&edges, 1), vec![0]);
    assert_eq!(held_after_prefix(&edges, 2), Vec::<usize>::new());
    assert_eq!(held_after_prefix(&edges, 3), vec![2]);
    assert_eq!(held_after_prefix(&edges, 4), Vec::<usize>::new());
}
