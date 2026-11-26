//! Paints one commit-graph row from its [`RowGeometry`].
//!
//! Per row: straight segments as thin quads (cheap), diagonals as stroked
//! paths, the commit dot as a rounded quad. Lanes beyond the graph column's
//! width are skipped, so very wide graphs cost no more than narrow ones.

use fgc_actor::RowGeometry;
use gpui::{
    BorderStyle, Bounds, IntoElement, PathBuilder, Pixels, Point, Styled, Window, canvas, fill,
    point, px, quad, size,
};

use crate::theme::{self, LANE_W, ROW_H};

pub fn graph_cell(geom: Option<RowGeometry>, lanes: u16) -> impl IntoElement {
    canvas(
        move |_, _, _| geom,
        move |bounds, geom, window, _| {
            if let Some(g) = geom {
                paint_row(&g, bounds, lanes, window);
            }
        },
    )
    .w(px(lanes as f32 * LANE_W))
    .h(px(ROW_H))
    .flex_shrink_0()
}

fn lane_x(origin: Point<Pixels>, lane: u16) -> Pixels {
    origin.x + px(lane as f32 * LANE_W + LANE_W / 2.0)
}

fn vline(window: &mut Window, x: Pixels, y0: Pixels, y1: Pixels, lane: u16) {
    window.paint_quad(fill(
        Bounds::new(point(x - px(1.0), y0), size(px(2.0), y1 - y0)),
        theme::lane(lane),
    ));
}

fn diagonal(window: &mut Window, from: Point<Pixels>, to: Point<Pixels>, lane: u16) {
    let mut b = PathBuilder::stroke(px(2.0));
    b.move_to(from);
    // A slight curve reads better than a straight diagonal at row scale.
    let ctrl = point(to.x, from.y);
    b.curve_to(to, ctrl);
    if let Ok(path) = b.build() {
        window.paint_path(path, theme::lane(lane));
    }
}

fn paint_row(g: &RowGeometry, bounds: Bounds<Pixels>, lanes: u16, window: &mut Window) {
    let o = bounds.origin;
    let (top, mid, bot) = (o.y, o.y + px(ROW_H / 2.0), o.y + px(ROW_H));
    let visible = |l: u16| l < lanes;

    for &l in &g.passthrough {
        if visible(l) {
            vline(window, lane_x(o, l), top, bot, l);
        }
    }
    if !visible(g.lane) {
        return;
    }
    let cx = lane_x(o, g.lane);
    if g.has_incoming {
        vline(window, cx, top, mid, g.lane);
    }
    for &t in &g.outgoing {
        if t == g.lane {
            vline(window, cx, mid, bot, t);
        } else if visible(t) {
            diagonal(window, point(cx, mid), point(lane_x(o, t), bot), t);
        } else {
            // Target lane is clipped: draw a stub towards the edge.
            let edge = o.x + px(lanes as f32 * LANE_W);
            diagonal(window, point(cx, mid), point(edge, bot), t);
        }
    }

    let merge = g.outgoing.len() > 1;
    let r = px(4.5);
    let dot = Bounds::new(point(cx - r, mid - r), size(r * 2.0, r * 2.0));
    if merge {
        window.paint_quad(quad(
            dot,
            r,
            theme::bg(),
            px(2.0),
            theme::lane(g.lane),
            BorderStyle::Solid,
        ));
    } else {
        window.paint_quad(fill(dot, theme::lane(g.lane)).corner_radii(r));
    }
}
