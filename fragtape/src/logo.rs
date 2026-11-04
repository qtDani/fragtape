//! The Fragtape logo (a winking bomb), rasterized in code so no image files ship with the exe.
//! Coordinates are in a 100x100 box, matching `assets/icon/bomb-*.svg`.

type P = (f64, f64);

const BODY: &[P] = &[(16., 62.), (21., 44.), (35., 33.), (53., 31.), (69., 38.), (79., 54.), (78., 72.),
                     (66., 86.), (47., 91.), (29., 85.), (18., 75.)];
const CAP: &[P] = &[(58., 34.), (64., 26.), (74., 31.), (69., 39.)];
const WINK: &[P] = &[(53., 59.), (67., 54.), (68., 60.), (54., 63.)];
const SPARK: &[P] = &[(88., 4.), (90., 11.), (97., 10.), (91., 15.), (95., 21.), (88., 18.), (84., 24.),
                      (84., 16.), (77., 14.), (84., 11.)];
const EYE: (P, f64) = ((36., 58.), 9.);
const FUSE_W: f64 = 4.5;

fn quad(a: P, c: P, b: P, n: usize) -> Vec<P> {
    (0..=n).map(|i| {
        let t = i as f64 / n as f64;
        let u = 1. - t;
        (u * u * a.0 + 2. * u * t * c.0 + t * t * b.0, u * u * a.1 + 2. * u * t * c.1 + t * t * b.1)
    }).collect()
}

fn inside(poly: &[P], (x, y): P) -> bool {
    let mut odd = false;
    let mut j = poly.len() - 1;
    for i in 0..poly.len() {
        let ((xi, yi), (xj, yj)) = (poly[i], poly[j]);
        if (yi > y) != (yj > y) && x < (xj - xi) * (y - yi) / (yj - yi) + xi { odd = !odd; }
        j = i;
    }
    odd
}

fn near_line(line: &[P], (x, y): P, r: f64) -> bool {
    line.windows(2).any(|w| {
        let ((ax, ay), (bx, by)) = (w[0], w[1]);
        let (dx, dy) = (bx - ax, by - ay);
        let t = (((x - ax) * dx + (y - ay) * dy) / (dx * dx + dy * dy)).clamp(0., 1.);
        (x - ax - t * dx).powi(2) + (y - ay - t * dy).powi(2) <= r * r
    })
}

struct Shapes { mouth: Vec<P>, fuse: Vec<P> }

fn shapes() -> Shapes {
    let mut mouth = quad((38., 75.), (48., 81.), (60., 74.), 12);
    mouth.push((57., 80.));
    mouth.extend(quad((57., 80.), (48., 85.), (40., 80.), 12));
    Shapes { mouth, fuse: quad((69., 28.), (74., 14.), (86., 14.), 16) }
}

/// What covers a point: 0 = nothing, 1 = bomb, 2 = spark.
fn hit(s: &Shapes, p: P) -> u8 {
    if inside(SPARK, p) { return 2; }
    let ((ex, ey), er) = EYE;
    let body = inside(BODY, p) && (p.0 - ex).powi(2) + (p.1 - ey).powi(2) > er * er
        && !inside(WINK, p) && !inside(&s.mouth, p);
    (body || inside(CAP, p) || near_line(&s.fuse, p, FUSE_W / 2.)) as u8
}

pub type Rgb = (u8, u8, u8);

/// Top-down RGBA pixels. With `tile`, the bomb sits on a rounded square of that color.
pub fn rgba(size: u32, body: Rgb, spark: Rgb, tile: Option<Rgb>) -> Vec<[u8; 4]> {
    let s = shapes();
    let ss = if size <= 32 { 6 } else { 3 };
    let n = (ss * ss) as f64;
    // the logo fills 90% of the icon (a bit less on a tile)
    let (scale, off) = if tile.is_some() { (0.74, 0.13) } else { (0.96, 0.02) };
    let mut out = Vec::with_capacity((size * size) as usize);
    for y in 0..size {
        for x in 0..size {
            let (mut cb, mut cs, mut ct) = (0., 0., 0.);
            for sy in 0..ss {
                for sx in 0..ss {
                    let fx = (x as f64 + (sx as f64 + 0.5) / ss as f64) / size as f64;
                    let fy = (y as f64 + (sy as f64 + 0.5) / ss as f64) / size as f64;
                    if tile.is_some() {
                        let r = 0.2;
                        let (cx, cy) = (fx.clamp(r, 1. - r), fy.clamp(r, 1. - r));
                        if (fx - cx).powi(2) + (fy - cy).powi(2) <= r * r { ct += 1.; }
                    }
                    match hit(&s, (((fx - off) / scale) * 100., ((fy - off) / scale) * 100.)) {
                        1 => cb += 1.,
                        2 => cs += 1.,
                        _ => {}
                    }
                }
            }
            let (ab, as_, at) = (cb / n, cs / n, ct / n);
            let fg = ab + as_;
            let base = tile.unwrap_or((0, 0, 0));
            let ch = |i: usize| {
                let c = |v: Rgb| [v.0, v.1, v.2][i] as f64;
                if fg > 0. && tile.is_none() {
                    (c(body) * ab + c(spark) * as_) / fg
                } else {
                    c(base) * (1. - fg) + c(body) * ab + c(spark) * as_
                }
            };
            let a = if tile.is_some() { at.max(fg) } else { fg };
            out.push([ch(0) as u8, ch(1) as u8, ch(2) as u8, (a.min(1.) * 255.).round() as u8]);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn eye_is_a_hole_and_spark_is_colored() {
        let s = shapes();
        assert_eq!(hit(&s, (36., 58.)), 0); // eye
        assert_eq!(hit(&s, (30., 45.)), 1); // body
        assert_eq!(hit(&s, (88., 14.)), 2); // spark
        assert_eq!(hit(&s, (5., 5.)), 0);
        let px = rgba(16, (255, 255, 255), (255, 0, 0), None);
        assert_eq!(px.len(), 256);
        assert!(px.iter().any(|p| p[3] == 255));
    }
}
