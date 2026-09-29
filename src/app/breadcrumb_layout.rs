//! Width-only breadcrumb layout policy for #145.

#[derive(Debug, Clone, PartialEq)]
pub(super) struct Layout {
    pub visible: Vec<(usize, f32, f32)>,
    pub hidden: Vec<usize>,
    pub overflow_x: f32,
    pub overflow_width: f32,
}

pub(super) fn layout(widths: &[f32], available: f32) -> Layout {
    let available = finite_width(available);
    let widths: Vec<_> = widths.iter().copied().map(finite_width).collect();
    let mut result = Layout {
        visible: vec![],
        hidden: vec![],
        overflow_x: 0.0,
        overflow_width: 0.0,
    };
    if widths.is_empty() {
        return result;
    }
    let last = widths.len() - 1;
    let full = widths.iter().sum::<f32>() + 2.0 * last as f32;
    if full <= available {
        let mut x = 0.0;
        for (i, &width) in widths.iter().enumerate() {
            result.visible.push((i, x, width));
            x += width + 2.0;
        }
        return result;
    }
    if last == 0 {
        result.visible.push((0, 0.0, available));
        return result;
    }
    if last == 1 {
        let gap = 2.0_f32.min(available);
        let room = available - gap;
        let root = widths[0].min(room / 3.0);
        result.visible = vec![(0, 0.0, root), (1, root + gap, widths[1].min(room - root))];
        return result;
    }
    // Reserve useful space for the current location before retaining ancestors.
    let overflow = 44.0_f32.min(available / 3.0);
    let gap = 2.0_f32.min((available - overflow) / 2.0);
    let room = (available - overflow - 2.0 * gap).max(0.0);
    let root = widths[0].min(180.0).min(room / 3.0);
    let current = widths[last].min(320.0).min(room - root);
    let mut start = last;
    let mut used = root + overflow + current + 2.0 * gap;
    // The visible ancestors must form a continuous suffix.
    for i in (2..last).rev() {
        let width = widths[i].min(180.0);
        if used + 2.0 + width > available {
            break;
        }
        used += 2.0 + width;
        start = i;
    }
    result.hidden = (1..start).collect();
    result.visible.push((0, 0.0, root));
    result.overflow_x = root + gap;
    result.overflow_width = overflow;
    let mut x = root + gap + overflow + gap;
    for (i, &width) in widths.iter().enumerate().take(last).skip(start) {
        let width = width.min(180.0);
        result.visible.push((i, x, width));
        x += width + 2.0;
    }
    // Spare space benefits the current name, including a single very long name.
    result
        .visible
        .push((last, x, widths[last].min((available - x).max(0.0))));
    result
}

fn finite_width(width: f32) -> f32 {
    if width.is_finite() {
        width.max(0.0)
    } else {
        0.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn issue_145_full_names_return_with_sufficient_width() {
        let l = layout(&[500.0, 600.0, 700.0], 1804.0);
        assert_eq!(
            l.visible,
            vec![(0, 0.0, 500.0), (1, 502.0, 600.0), (2, 1104.0, 700.0)]
        );
        assert!(l.hidden.is_empty());
        assert_eq!(l.overflow_width, 0.0);
    }

    #[test]
    fn issue_145_preserves_root_and_nearest_continuous_ancestors() {
        let l = layout(&[40.0; 6], 170.0);
        assert_eq!(
            l.visible.iter().map(|x| x.0).collect::<Vec<_>>(),
            vec![0, 4, 5]
        );
        assert_eq!(l.hidden, vec![1, 2, 3]);
        assert!(l.overflow_x >= l.visible[0].2);
        assert!(l.overflow_x + l.overflow_width <= l.visible[1].1);
    }

    #[test]
    fn issue_145_long_root_does_not_starve_current() {
        let l = layout(&[10000.0, 50.0, 300.0], 200.0);
        assert!(l.visible[0].2 <= 200.0 / 3.0);
        assert!(l.visible[1].2 > l.visible[0].2);
    }

    #[test]
    fn issue_145_one_and_two_never_fabricate_overflow() {
        for widths in [&[500.0][..], &[500.0, 500.0][..]] {
            let l = layout(widths, 20.0);
            assert!(l.hidden.is_empty());
            assert_eq!(l.overflow_width, 0.0);
            assert_eq!(l.visible.len(), widths.len());
        }
    }

    #[test]
    fn issue_145_every_width_keeps_all_items_ordered_and_bounded() {
        for budget in 0..2000 {
            for widths in [
                &[][..],
                &[500.0][..],
                &[500.0, 50.0][..],
                &[900.0, 1.0, 200.0, 80.0, 600.0][..],
            ] {
                let l = layout(widths, budget as f32);
                let mut end = 0.0;
                for &(_, x, w) in &l.visible {
                    assert!(
                        w >= 0.0 && x >= end - 0.001 && x + w <= budget as f32 + 0.001,
                        "{budget}: {l:?}"
                    );
                    end = x + w;
                }
                assert!(l.overflow_x + l.overflow_width <= budget as f32 + 0.001);
                let mut ids: Vec<_> = l
                    .visible
                    .iter()
                    .map(|x| x.0)
                    .chain(l.hidden.iter().copied())
                    .collect();
                ids.sort_unstable();
                assert_eq!(ids, (0..widths.len()).collect::<Vec<_>>());
            }
        }
    }

    #[test]
    fn issue_145_invalid_measurements_do_not_propagate() {
        let l = layout(&[-1.0, f32::NAN, f32::INFINITY], f32::NAN);
        assert!(
            l.visible
                .iter()
                .all(|(_, x, w)| x.is_finite() && w.is_finite())
        );
    }
}
