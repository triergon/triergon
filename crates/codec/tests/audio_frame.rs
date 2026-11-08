use triergon_codec::{AudioFrame, Budget, StorageError};

#[test]
fn resizing_charges_the_whole_frame_before_it_grows() {
    let budget = Budget::new(64 * 1024).unwrap();
    let mut charge = budget.reserve(0).unwrap();
    let mut frame = AudioFrame::default();
    frame.try_resize(6, 1536, &mut charge).unwrap();
    assert_eq!(frame.planes.len(), 6);
    assert!(frame.planes.iter().all(|p| p.len() == 1536));
    let capacity: usize = frame.planes.iter().map(Vec::capacity).sum();
    let header = frame.planes.capacity() * size_of::<Vec<f32>>();
    assert_eq!(charge.bytes(), capacity * size_of::<f32>() + header);
    let charged = budget.used();
    frame.clear();
    frame.try_resize(6, 1536, &mut charge).unwrap();
    assert_eq!(budget.used(), charged);
    frame.try_resize(2, 1536, &mut charge).unwrap();
    assert_eq!(frame.planes.len(), 2);
    assert_eq!(budget.used(), charged, "the charge keeps the largest frame");
}

#[test]
fn a_reserved_bound_lets_the_frame_grow_without_another_claim() {
    let budget = Budget::new(256 * 1024).unwrap();
    let mut charge = budget.reserve(0).unwrap();
    let mut frame = AudioFrame::default();
    frame.try_resize(2, 1024, &mut charge).unwrap();
    frame.charge_for(8, 4096, &mut charge).unwrap();
    let reserved = charge.bytes();
    assert!(reserved >= 8 * 4096 * size_of::<f32>());
    assert_eq!(frame.planes.iter().map(Vec::len).sum::<usize>(), 2 * 1024);
    let held = budget.reserve(budget.available()).unwrap();
    frame.try_resize(8, 4096, &mut charge).unwrap();
    assert_eq!(charge.bytes(), reserved);
    drop(held);
}

#[test]
fn a_frame_above_the_budget_is_refused_with_the_charge_unchanged() {
    let budget = Budget::new(16 * 1024).unwrap();
    let mut charge = budget.reserve(0).unwrap();
    let mut frame = AudioFrame::default();
    frame.try_resize(2, 1024, &mut charge).unwrap();
    let before = charge.bytes();
    assert!(matches!(
        frame.try_resize(8, 1024, &mut charge),
        Err(StorageError::Limit { .. })
    ));
    assert_eq!(charge.bytes(), before);
    assert!(frame.planes.iter().all(|p| p.len() == 1024));
    assert_eq!(
        frame.try_resize(usize::MAX, 1, &mut charge),
        Err(StorageError::Overflow)
    );
}
