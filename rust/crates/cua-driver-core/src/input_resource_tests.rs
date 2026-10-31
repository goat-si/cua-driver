use crate::guarded_dispatch::Emitted;
use crate::input_resource::{ResourceState, ResourceTable};

#[test]
fn resource_transitions_restore_and_cleanup_in_reverse_order() {
    let mut table = ResourceTable::default();
    assert_eq!(table.state(&1), ResourceState::Released);
    table.press_submit(1);
    assert_eq!(table.state(&1), ResourceState::PossiblyPressed);
    table.press_result(&1, Err(Emitted::NotEmitted));
    assert_eq!(table.state(&1), ResourceState::Released);
    table.press_submit(1);
    table.press_result(&1, Err(Emitted::Unknown));
    assert_eq!(table.state(&1), ResourceState::PossiblyPressed);
    table.press_submit(2);
    table.press_result(&2, Ok(()));
    assert_eq!(table.state(&2), ResourceState::Pressed);
    assert_eq!(table.needing_cleanup(), vec![2, 1]);
    table.release_submit(&1);
    assert_eq!(table.state(&1), ResourceState::PossiblyReleased);
    table.release_result(&1, Err(Emitted::NotEmitted));
    assert_eq!(table.state(&1), ResourceState::PossiblyPressed);
    table.release_submit(&2);
    table.release_result(&2, Err(Emitted::NotEmitted));
    assert_eq!(table.state(&2), ResourceState::Pressed);
    table.release_submit(&2);
    table.release_result(&2, Err(Emitted::Unknown));
    assert_eq!(table.state(&2), ResourceState::PossiblyReleased);
    table.release_submit(&1);
    table.release_result(&1, Ok(()));
    assert_eq!(table.state(&1), ResourceState::Released);
    assert_eq!(table.needing_cleanup(), vec![2]);
}
