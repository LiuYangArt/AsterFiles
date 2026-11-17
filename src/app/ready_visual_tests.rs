fn number_after(source: &str, prefix: &str, suffix: &str) -> f64 {
    source
        .split_once(prefix)
        .expect("geometry expression exists")
        .1
        .split_once(suffix)
        .expect("geometry expression has its unit")
        .0
        .parse()
        .expect("geometry expression contains a number")
}

#[test]
fn issue_186_progress_slot_clips_entire_indeterminate_cycle() {
    let source = include_str!("../../ui/operation-window.slint");
    let slot = source
        .split_once("y: 110px;")
        .expect("operation progress slot exists")
        .1
        .split_once("HorizontalLayout {")
        .expect("progress slot ends before operation statistics")
        .0;
    let (slot_geometry, fill) = slot
        .split_once("Rectangle {")
        .expect("progress fill exists");
    assert!(slot_geometry.contains("clip: true;"));
    assert!(slot_geometry.contains("border-radius: 11px;"));
    assert!(slot_geometry.contains("height: 22px;"));
    assert!(fill.contains("-parent.width + (parent.width * mod(animation-tick(), 2s) / 1s)"));
    assert!(fill.contains("max(0px, min(parent.width, parent.width * operation.progress))"));
    assert!(fill.contains("Text { text: operation.file-progress;"));

    for scale in [1.0, 1.25, 1.5, 2.0] {
        let width = 540.0 * scale;
        let mut saw_left_overflow = false;
        let mut saw_right_overflow = false;
        for tick in 0..=2000 {
            let phase = f64::from(tick % 2000) / 1000.0;
            let fill_left = -width + width * phase;
            let fill_right = fill_left + width;
            saw_left_overflow |= fill_left < 0.0;
            saw_right_overflow |= fill_right > width;
            let clipped_left = fill_left.max(0.0);
            let clipped_right = fill_right.min(width);
            assert!(clipped_left >= 0.0);
            assert!(clipped_right <= width);
            assert!(clipped_right >= clipped_left);
        }
        assert!(saw_left_overflow && saw_right_overflow);
        for progress in [-0.1_f64, 0.0, 0.5, 1.0, 1.1] {
            let filled = (width * progress).clamp(0.0, width);
            assert!((0.0..=width).contains(&filled));
            if progress == 0.0 {
                assert_eq!(filled, 0.0);
            } else if progress == 1.0 {
                assert_eq!(filled, width);
            }
        }
    }
}

#[test]
fn issue_189_active_tab_paints_through_fractional_navigation_boundary() {
    let tabs = include_str!("../../ui/tab-bar.slint");
    let controls = include_str!("../../ui/tab-controls.slint");
    let address = include_str!("../../ui/address-bar.slint");
    let shape = controls
        .split_once("export component WindowButton")
        .unwrap()
        .0;
    let clip = tabs
        .split_once("width: root.tab-strip-width; height: 34px;")
        .expect("tab layout retains its original height")
        .1
        .split_once("tab-strip := Flickable {")
        .expect("the extended paint clip contains the tab flickable")
        .0;
    let clip_extension = number_after(clip, "height: parent.height + ", "px;");
    assert!(clip.contains("clip: true;"));
    let path_extension = number_after(shape, "height: parent.height + ", "px;");
    let contour_extension = number_after(
        shape,
        "LineTo { x: root.width / 1px; y: root.height / 1px + ",
        "; }",
    );
    assert!(shape.contains(&format!(
        "LineTo {{ x: 0; y: root.height / 1px + {contour_extension}; }}"
    )));
    assert_eq!(shape.matches("Path {").count(), 1);
    assert_eq!(shape.matches("CubicTo {").count(), 4);
    assert!(shape.contains("x: root.width / 1px; y: root.height / 1px;"));
    assert!(tabs.contains("width: root.tab-width; height: 34px;"));
    assert!(tabs.contains("shape-color: VisualStyle.navigation-surface-background;"));
    assert!(address.contains("background: VisualStyle.navigation-surface-background;"));

    // The original contour and clip stopped at the shared edge. A physical pixel
    // crossing that edge must instead be covered before the navigation is painted.
    let overlap = clip_extension.min(path_extension).min(contour_extension);
    let mut cases = 0;
    for scale in [1.0, 1.25, 1.5, 2.0] {
        for physical_origin in [0.0, 0.125, 0.5, 0.875] {
            let shared_edge: f64 = (12.0 + 34.0) * scale + physical_origin;
            let paint_bottom = shared_edge + overlap * scale;
            assert!(paint_bottom >= shared_edge.ceil());
            assert!(overlap >= 1.0);
            for width in [80.0, 113.125, 137.333, 178.0] {
                for index in 0..4 {
                    for viewport_x in [0.0, -17.125, -83.5] {
                        let tab_left = f64::from(index) * (width + 5.0) + viewport_x;
                        let tab_right = tab_left + width;
                        let strip_width = 4.0 * width + 15.0;
                        let visible_left = tab_left.max(0.0);
                        let visible_right = tab_right.min(strip_width);
                        if visible_right <= visible_left {
                            continue;
                        }
                        // The bottom connector spans both shoulder endpoints and
                        // remains subject to the unchanged horizontal viewport clip.
                        for fraction in [0.0, 0.001, 0.5, 0.999, 1.0] {
                            let x = visible_left + (visible_right - visible_left) * fraction;
                            assert!((0.0..=strip_width).contains(&x));
                            assert!(x >= tab_left && x <= tab_right);
                            cases += 1;
                        }
                    }
                }
            }
        }
    }
    eprintln!(
        "issue_189 geometry: {cases} shoulder/center samples; DPI=100,125,150,200; overlap={overlap}px"
    );
}
