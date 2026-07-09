use super::*;

fn press() -> RawControlEvent {
    RawControlEvent::DivertedButtons([reprog_controls::GESTURE_BUTTON_CID, 0, 0, 0])
}

fn release() -> RawControlEvent {
    RawControlEvent::DivertedButtons([0, 0, 0, 0])
}

fn handle(
    acc: &mut CaptureAccum,
    event: RawControlEvent,
    dpi: &[u16],
    sink: &mpsc::UnboundedSender<CapturedInput>,
) {
    handle_reprog(acc, event, dpi, &[], &[], sink);
}

#[test]
fn quick_tap_is_a_click_even_while_the_cursor_moves() {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut acc = CaptureAccum::default();

    handle(&mut acc, press(), &[], &tx);
    handle(
        &mut acc,
        RawControlEvent::RawXy { dx: 120, dy: 5 },
        &[],
        &tx,
    );
    handle(&mut acc, release(), &[], &tx);

    assert_eq!(
        rx.try_recv(),
        Ok(CapturedInput::Gesture(GestureDirection::Click))
    );
    assert!(
        rx.try_recv().is_err(),
        "a quick tap emits exactly one click"
    );
}

#[test]
fn a_held_gesture_commits_a_swipe_and_does_not_also_click() {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut acc = CaptureAccum::default();

    handle(&mut acc, press(), &[], &tx);
    // Pretend the button has been held well past the swipe gate.
    acc.swipe.backdate_hold_for_test();
    handle(
        &mut acc,
        RawControlEvent::RawXy { dx: 120, dy: 5 },
        &[],
        &tx,
    );

    assert_eq!(
        rx.try_recv(),
        Ok(CapturedInput::Gesture(GestureDirection::Right))
    );

    handle(&mut acc, release(), &[], &tx);
    assert!(
        rx.try_recv().is_err(),
        "a committed swipe must not also click on release"
    );
}

#[test]
fn a_held_dpi_button_presses_once_on_the_rising_edge() {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut acc = CaptureAccum::default();
    let dpi = reprog_controls::DPI_MODE_SHIFT_CIDS[0];
    let down = RawControlEvent::DivertedButtons([dpi, 0, 0, 0]);

    handle(&mut acc, down, &[dpi], &tx);
    handle(&mut acc, down, &[dpi], &tx);

    assert_eq!(
        rx.try_recv(),
        Ok(CapturedInput::ButtonPressed(ButtonId::DpiToggle))
    );
    assert!(rx.try_recv().is_err(), "a held DPI button presses once");
}

#[test]
fn a_dpi_button_re_presses_after_a_release() {
    // Rising-edge detection must re-arm: press → release → press is two
    // distinct presses. The release (a frame without the CID) is what resets
    // the edge; without it a re-press would be swallowed as "still held".
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut acc = CaptureAccum::default();
    let dpi = reprog_controls::DPI_MODE_SHIFT_CIDS[0];
    let down = RawControlEvent::DivertedButtons([dpi, 0, 0, 0]);
    let up = RawControlEvent::DivertedButtons([0, 0, 0, 0]);

    handle(&mut acc, down, &[dpi], &tx);
    handle(&mut acc, up, &[dpi], &tx);
    handle(&mut acc, down, &[dpi], &tx);

    assert_eq!(
        rx.try_recv(),
        Ok(CapturedInput::ButtonPressed(ButtonId::DpiToggle))
    );
    assert_eq!(
        rx.try_recv(),
        Ok(CapturedInput::ButtonPressed(ButtonId::DpiToggle)),
        "a release re-arms the rising edge"
    );
    assert!(rx.try_recv().is_err());
}

#[test]
fn back_button_presses_once_on_rising_edge() {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut acc = CaptureAccum::default();
    let back = reprog_controls::BACK_CIDS[0];
    let down = RawControlEvent::DivertedButtons([back, 0, 0, 0]);

    handle_reprog(&mut acc, down, &[], &[back], &[], &tx);
    handle_reprog(&mut acc, down, &[], &[back], &[], &tx);

    assert_eq!(
        rx.try_recv(),
        Ok(CapturedInput::ButtonPressed(ButtonId::Back))
    );
    assert!(rx.try_recv().is_err(), "a held Back button presses once");
}

#[test]
fn forward_button_presses_once_on_rising_edge() {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut acc = CaptureAccum::default();
    let forward = reprog_controls::FORWARD_CIDS[0];
    let down = RawControlEvent::DivertedButtons([forward, 0, 0, 0]);

    handle_reprog(&mut acc, down, &[], &[], &[forward], &tx);
    handle_reprog(&mut acc, down, &[], &[], &[forward], &tx);

    assert_eq!(
        rx.try_recv(),
        Ok(CapturedInput::ButtonPressed(ButtonId::Forward))
    );
    assert!(rx.try_recv().is_err(), "a held Forward button presses once");
}
