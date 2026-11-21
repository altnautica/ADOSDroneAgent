use super::*;

fn problem_eval() -> (Eval, Vec<f64>) {
    let boxes = [
        Aabb::new([-0.5, -1.0, 0.0], [4.0, 1.0, 2.0]),
        Aabb::new([3.0, -1.0, 0.0], [5.0, 6.0, 2.0]),
        Aabb::new([3.5, 4.0, 0.5], [9.0, 6.0, 2.5]),
    ];
    let mut e = Eval {
        m: 3,
        start: [[0.0, 0.0, 1.0], [1.0, 0.5, 0.0], [0.0, 0.2, 0.0]],
        end: [[8.0, 5.0, 1.5], [0.0; 3], [0.0; 3]],
        lim: [2.0, 2.0, 5.0],
        w: Weights::default(),
        samples: 8,
        ..Default::default()
    };
    e.boxes = boxes.iter().map(|b| Box64::from(b, SHRINK)).collect();
    for i in 0..2 {
        let o = e.boxes[i].overlap(&e.boxes[i + 1]).unwrap();
        e.mid.push([
            0.5 * (o.lo[0] + o.hi[0]),
            0.5 * (o.lo[1] + o.hi[1]),
            0.5 * (o.lo[2] + o.hi[2]),
        ]);
        e.half.push([
            0.5 * (o.hi[0] - o.lo[0]),
            0.5 * (o.hi[1] - o.lo[1]),
            0.5 * (o.hi[2] - o.lo[2]),
        ]);
    }
    e.c = vec![[0.0; 3]; 18];
    e.gc = vec![[0.0; 3]; 18];
    e.t = vec![0.0; 3];
    e.dt = vec![0.0; 3];
    e.gt = vec![0.0; 3];
    // Durations short enough that velocity, acceleration and jerk penalties
    // are all active.
    let x = vec![0.3, -0.2, 0.1, -0.4, 0.5, 0.2, -0.6, -0.3, 0.2];
    (e, x)
}

#[test]
fn gradient_matches_finite_differences() {
    let (mut e, x) = problem_eval();
    let mut g = vec![0.0; x.len()];
    let f0 = e.cost(&x, &mut g);
    assert!(f0.is_finite());
    let mut scratch = vec![0.0; x.len()];
    for i in 0..x.len() {
        let h = 1e-6;
        let mut xp = x.clone();
        xp[i] += h;
        let fp = e.cost(&xp, &mut scratch);
        let mut xm = x.clone();
        xm[i] -= h;
        let fm = e.cost(&xm, &mut scratch);
        let num = (fp - fm) / (2.0 * h);
        let tol = 1e-4 * num.abs().max(g[i].abs()).max(1.0);
        assert!(
            (num - g[i]).abs() < tol,
            "var {i}: numeric {num} analytic {}",
            g[i]
        );
    }
}

#[test]
fn trajectory_meets_boundary_and_waypoint_constraints() {
    let (mut e, x) = problem_eval();
    let mut g = vec![0.0; x.len()];
    e.cost(&x, &mut g);
    let segs: Vec<Segment> = (0..3)
        .map(|i| {
            let mut c = [[0.0; 3]; 6];
            c.copy_from_slice(&e.c[6 * i..6 * i + 6]);
            Segment { dur: e.t[i], c }
        })
        .collect();
    let tr = Trajectory::new(segs.clone());
    for o in 0..3 {
        let s = tr.eval(0.0, o);
        let f = tr.eval(segs.iter().map(|s| s.dur).sum(), o);
        for k in 0..3 {
            assert!((s[k] - e.start[o][k]).abs() < 1e-9);
            assert!((f[k] - e.end[o][k]).abs() < 1e-9);
        }
    }
    // Continuity up to snap at the junctions.
    for i in 0..2 {
        for o in 0..5 {
            let a = segs[i].eval(segs[i].dur, o);
            let b = segs[i + 1].eval(0.0, o);
            for k in 0..3 {
                assert!((a[k] - b[k]).abs() < 1e-6, "junction {i} order {o}");
            }
        }
    }
}
