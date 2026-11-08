//! Which outputs the step tests: the motors and control surfaces the FC's
//! output functions map to channels.

use super::*;

/// The output channel (1-based) carrying output function `f`.
pub(super) fn channel_of(cx: &Cx, f: u8) -> Option<u8> {
    (1..=32u8).find(|k| {
        cx.params
            .get(&format!("SERVO{k}_FUNCTION"))
            .is_some_and(|v| v as u8 == f)
    })
}

pub(super) fn surfaces(cx: &Cx) -> Vec<Item> {
    let mut v = Vec::new();
    for k in 1..=16u8 {
        if let Some(f) = cx.params.get(&format!("SERVO{k}_FUNCTION")) {
            let f = f as u8;
            if ardupilot_plane::surface_test(f).is_some() {
                v.push(Item::Surface {
                    channel: k,
                    function: f,
                });
            }
        }
    }
    v
}

pub(super) fn motors(cx: &Cx) -> Result<(Option<&'static str>, Vec<Item>), ActErr> {
    if cx.adapter.fw == Fw::Px4 {
        let n = cx.params.get("CA_ROTOR_COUNT").unwrap_or(4.0) as u8;
        let v = (1..=n)
            .map(|i| Item::Motor {
                def: MotorDef {
                    num: i,
                    angle: 0,
                    cw: false,
                    seq: i,
                },
                px4: true,
                dir: true,
            })
            .collect();
        return Ok((None, v));
    }
    let (c, t) = if cx.adapter.class == Class::Vtol {
        ("Q_FRAME_CLASS", "Q_FRAME_TYPE")
    } else {
        ("FRAME_CLASS", "FRAME_TYPE")
    };
    let fc = cx.params.get(c).unwrap_or(1.0) as u8;
    let ft = cx.params.get(t).unwrap_or(1.0) as u8;
    if cx.adapter.class == Class::Vtol && fc == ardupilot_plane::FRAME_CLASS_TAILSITTER {
        // Tailsitter throttles: position only, ArduPilot sets no spin direction for them.
        let v = ardupilot_plane::TAILSITTER_MOTORS
            .iter()
            .map(|&(num, seq)| Item::Motor {
                def: MotorDef {
                    num,
                    angle: 0,
                    cw: false,
                    seq,
                },
                px4: false,
                dir: false,
            })
            .collect();
        return Ok((None, v));
    }
    let (img, layout) = ardupilot_copter::layout(fc, ft).ok_or("unsupported_frame")?;
    let mut defs: Vec<MotorDef> = layout.to_vec();
    defs.sort_by_key(|d| d.seq);
    Ok((
        Some(img),
        defs.into_iter()
            .map(|def| Item::Motor {
                def,
                px4: false,
                dir: true,
            })
            .collect(),
    ))
}
