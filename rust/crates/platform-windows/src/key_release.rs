#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum KeyId {
    Vk(u16),
    Scan { scan: u16, extended: bool },
    Unicode(u16),
}

/// Return the last unmatched down for each key, in reverse press order.
/// A complete insert is not an interrupted sequence and needs no correction.
pub(crate) fn held_after_prefix(edges: &[(KeyId, bool)], inserted: usize) -> Vec<usize> {
    if inserted >= edges.len() {
        return Vec::new();
    }
    let mut held: Vec<(KeyId, usize)> = Vec::new();
    for (index, &(id, up)) in edges.iter().take(inserted).enumerate() {
        held.retain(|(held_id, _)| *held_id != id);
        if !up {
            held.push((id, index));
        }
    }
    held.into_iter().rev().map(|(_, index)| index).collect()
}

#[cfg(test)]
#[path = "key_release_tests.rs"]
mod tests;
