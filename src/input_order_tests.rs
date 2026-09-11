use std::{cell::RefCell, rc::Rc};

use slint::{ComponentHandle, LogicalPosition, platform::WindowEvent};

slint::slint! {
    export component InputOrderProbe inherits Window {
        width: 200px;
        height: 200px;
        in-out property <length> item-offset: 0px;
        callback observed(int, bool, float);
        out property <float> touch-x: touch.absolute-position.x / 1px;
        out property <float> touch-y: touch.absolute-position.y / 1px;

        Rectangle {
            x: 0px;
            y: root.item-offset;
            width: 100px;
            height: 100px;
            touch := TouchArea {
                width: parent.width;
                height: parent.height;
                pointer-event(event) => {
                    if event.button == PointerEventButton.left && event.kind == PointerEventKind.down {
                        root.observed(1, self.pressed, (self.absolute-position.y + self.mouse-y) / 1px);
                    }
                    if event.button == PointerEventButton.left && event.kind == PointerEventKind.up {
                        root.observed(3, self.pressed, (self.absolute-position.y + self.mouse-y) / 1px);
                    }
                }
                clicked => {
                    root.observed(2, self.pressed, (self.absolute-position.y + self.mouse-y) / 1px);
                    root.item-offset = 40px;
                }
            }
        }
    }
}

#[test]
fn issue_93_slint_click_relayouts_before_up_and_clears_pressed() {
    i_slint_backend_testing::init_no_event_loop();
    let probe = InputOrderProbe::new().expect("testing backend creates an in-memory component");
    let observations = Rc::new(RefCell::new(Vec::new()));
    let captured = observations.clone();
    probe.on_observed(move |phase, pressed, y| {
        captured.borrow_mut().push((phase, pressed, y));
    });
    // The testing backend owns this window; no native window or filesystem operation exists.
    probe.show().expect("testing backend initializes layout");
    probe
        .window()
        .set_size(slint::LogicalSize::new(200.0, 200.0));
    let position = LogicalPosition::new(probe.get_touch_x() + 20.0, probe.get_touch_y() + 20.0);
    probe
        .window()
        .dispatch_event(WindowEvent::PointerMoved { position });
    probe.window().dispatch_event(WindowEvent::PointerPressed {
        position,
        button: slint::platform::PointerEventButton::Left,
    });
    probe.window().dispatch_event(WindowEvent::PointerReleased {
        position,
        button: slint::platform::PointerEventButton::Left,
    });

    let observations = observations.borrow();
    assert_eq!(
        observations
            .iter()
            .map(|(phase, pressed, _)| (*phase, *pressed))
            .collect::<Vec<_>>(),
        vec![(1, true), (2, true), (3, false)],
        "release must be treated as cleanup after activation, regardless of pressed"
    );
    assert_eq!(observations[0].2, 20.0);
    assert_eq!(observations[1].2, 20.0);
    assert_eq!(
        observations[2].2, 60.0,
        "a stationary physical release can appear displaced after clicked changes layout"
    );
}
