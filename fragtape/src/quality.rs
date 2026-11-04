//! Recording quality: presets and advanced settings -> OBS encoder + output settings.

use serde_json::{json, Map, Value};

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Vendor {
    Nvidia,
    Amd,
    Intel,
    Cpu,
}

pub fn vendor_of(gpus: &[String]) -> Vendor {
    let g: Vec<String> = gpus.iter().map(|s| s.to_lowercase()).collect();
    if g.iter().any(|s| s.contains("nvidia")) {
        Vendor::Nvidia
    } else if g.iter().any(|s| s.contains("radeon") || s.contains("amd")) {
        Vendor::Amd
    } else if g.iter().any(|s| s.contains("intel")) {
        Vendor::Intel
    } else {
        Vendor::Cpu
    }
}

pub fn encoder_id(vendor: Vendor, codec: &str) -> &'static str {
    match (vendor, codec) {
        (Vendor::Nvidia, "hevc") => "obs_nvenc_hevc_tex",
        (Vendor::Nvidia, "av1") => "obs_nvenc_av1_tex",
        (Vendor::Nvidia, _) => "obs_nvenc_h264_tex",
        (Vendor::Amd, "hevc") => "h265_texture_amf",
        (Vendor::Amd, "av1") => "av1_texture_amf",
        (Vendor::Amd, _) => "h264_texture_amf",
        (Vendor::Intel, "hevc") => "obs_qsv11_hevc",
        (Vendor::Intel, "av1") => "obs_qsv11_av1",
        (Vendor::Intel, _) => "obs_qsv11_v2",
        (Vendor::Cpu, _) => "obs_x264",
    }
}

pub fn encoder_name(id: &str) -> String {
    match id {
        "obs_nvenc_h264_tex" => "NVIDIA NVENC H.264", "obs_nvenc_hevc_tex" => "NVIDIA NVENC HEVC",
        "obs_nvenc_av1_tex" => "NVIDIA NVENC AV1", "h264_texture_amf" => "AMD H.264", "h265_texture_amf" => "AMD HEVC",
        "av1_texture_amf" => "AMD AV1", "obs_qsv11_v2" => "Intel QuickSync H.264", "obs_qsv11_hevc" => "Intel QuickSync HEVC",
        "obs_qsv11_av1" => "Intel QuickSync AV1", "obs_x264" => "x264 (CPU)", other => other,
    }
    .to_string()
}

fn codec_of(id: &str) -> &'static str {
    if id.contains("hevc") || id.contains("h265") { "hevc" } else if id.contains("av1") { "av1" } else { "h264" }
}

/// Which encoder to use. `available` = encoders the hidden OBS reported (None = not known yet).
pub fn pick_encoder(prefs: &Map<String, Value>, vendor: Vendor, available: Option<&[String]>) -> String {
    let custom = prefs["quality"] == "custom";
    let codec = if custom { prefs["adv_codec"].as_str().unwrap_or("h264") } else { "h264" };
    let force_cpu = custom && prefs["adv_encoder"] == "x264";
    let wanted = if force_cpu { "obs_x264" } else { encoder_id(vendor, codec) };
    let ok = |id: &str| available.map_or(true, |a| a.iter().any(|x| x == id));
    for id in [wanted, encoder_id(vendor, "h264"), "obs_x264"] {
        if ok(id) {
            return id.to_string();
        }
    }
    "obs_x264".into()
}

const HW_QP: [(&str, i64); 4] = [("small", 28), ("balanced", 24), ("high", 20), ("max", 16)];
const X264_CRF: [(&str, i64); 4] = [("small", 26), ("balanced", 22), ("high", 19), ("max", 16)];

fn lookup(table: &[(&str, i64)], key: &str) -> i64 {
    table.iter().find(|(k, _)| *k == key).map(|(_, v)| *v).unwrap_or(table[2].1)
}

/// recordEncoder.json for `encoder`.
pub fn encoder_settings(encoder: &str, prefs: &Map<String, Value>, ten_bit: bool) -> Value {
    let custom = prefs["quality"] == "custom";
    let q = prefs["quality"].as_str().unwrap_or("high");
    let int = |k: &str, d: i64| prefs.get(k).and_then(|v| v.as_i64()).unwrap_or(d);
    let rc = if custom { prefs["adv_rate_control"].as_str().unwrap_or("cqp") } else { "cqp" };
    let (bitrate, max_bitrate) = (int("adv_bitrate", 30000), int("adv_max_bitrate", 45000).max(int("adv_bitrate", 30000)));
    let cq = if custom { int("adv_cq", 20) } else { lookup(&HW_QP, q) };
    let preset = if custom { int("adv_preset", 5).clamp(1, 7) } else { 5 };
    let keyint = if custom { int("adv_keyint", 2) } else { 2 };
    let bf = if custom { int("adv_bframes", 2) } else { 2 };
    let lookahead = !custom || prefs["adv_lookahead"] == true;
    let codec = codec_of(encoder);
    let mut s = Map::new();
    let mut set = |k: &str, v: Value| { s.insert(k.into(), v); };
    if encoder == "obs_x264" {
        let presets = ["ultrafast", "superfast", "veryfast", "faster", "fast", "medium", "slow"];
        // CPU encoding while gaming: never slower than "veryfast" unless asked for.
        set("preset", json!(presets[if custom { (preset - 1) as usize } else { 2 }]));
        set("keyint_sec", json!(keyint));
        set("bf", json!(bf));
        match rc {
            "cbr" => { set("rate_control", json!("CBR")); set("bitrate", json!(bitrate)); }
            "vbr" => { set("rate_control", json!("VBR")); set("bitrate", json!(bitrate)); set("max_bitrate", json!(max_bitrate)); }
            _ => { set("rate_control", json!("CRF")); set("crf", json!(if custom { cq } else { lookup(&X264_CRF, q) })); }
        }
        return Value::Object(s);
    }
    let family = if encoder.contains("nvenc") { "nvenc" } else if encoder.contains("amf") { "amf" } else { "qsv" };
    set("keyint_sec", json!(keyint));
    match family {
        "nvenc" => {
            // OBS 31+ reads "preset"; older NVENC versions read "preset2".
            set("preset", json!(format!("p{preset}")));
            set("preset2", json!(format!("p{preset}")));
            set("tune", json!("hq"));
            set("multipass", json!(if preset >= 4 { "qres" } else { "disabled" }));
            set("bf", json!(bf));
            set("lookahead", json!(lookahead));
            set("profile", json!(match (codec, ten_bit) { ("hevc", true) => "main10", ("h264", _) => "high", _ => "main" }));
        }
        "amf" => {
            set("preset", json!(match preset { 1..=2 => "speed", 3..=5 => "balanced", _ => "quality" }));
            set("bf", json!(bf));
            set("profile", json!(if codec == "h264" { "high" } else { "main" }));
        }
        _ => {
            set("target_usage", json!(format!("TU{}", 8 - preset)));
            set("bframes", json!(bf));
            set("profile", json!(if codec == "h264" { "high" } else { "main" }));
        }
    }
    match rc {
        "cbr" => { set("rate_control", json!("CBR")); set("bitrate", json!(bitrate)); }
        "vbr" => {
            set("rate_control", json!("VBR"));
            set("bitrate", json!(bitrate));
            set("max_bitrate", json!(max_bitrate));
        }
        _ => {
            set("rate_control", json!("CQP"));
            set("cqp", json!(cq));
            if family == "qsv" {
                for k in ["qpi", "qpp", "qpb"] { set(k, json!(cq)); }
            }
        }
    }
    Value::Object(s)
}

/// Output size for the chosen resolution (0 = same as the monitor). Never upscales; even numbers.
pub fn output_size(base: (u32, u32), height: i64) -> (u32, u32) {
    let (w, h) = base;
    let short = w.min(h);
    if height <= 0 || height as u32 >= short {
        return base;
    }
    let scale = height as f64 / short as f64;
    let even = |x: f64| ((x / 2.0).round() as u32) * 2;
    (even(w as f64 * scale), even(h as f64 * scale))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::library::pref_specs;

    fn prefs(over: Value) -> Map<String, Value> {
        let mut m: Map<String, Value> = pref_specs().into_iter().map(|(k, _, d)| (k.to_string(), d)).collect();
        for (k, v) in over.as_object().unwrap() {
            m.insert(k.clone(), v.clone());
        }
        m
    }

    #[test]
    fn presets_use_cqp() {
        let s = encoder_settings("obs_nvenc_h264_tex", &prefs(json!({"quality": "balanced"})), false);
        assert_eq!((s["rate_control"].clone(), s["cqp"].clone(), s["preset"].clone()), (json!("CQP"), json!(24), json!("p5")));
        let x = encoder_settings("obs_x264", &prefs(json!({"quality": "max"})), false);
        assert_eq!((x["rate_control"].clone(), x["crf"].clone(), x["preset"].clone()), (json!("CRF"), json!(16), json!("veryfast")));
    }

    #[test]
    fn custom_bitrate_and_preset() {
        let p = prefs(json!({"quality": "custom", "adv_rate_control": "vbr", "adv_bitrate": 50000, "adv_max_bitrate": 40000, "adv_preset": 7, "adv_bframes": 0, "adv_keyint": 1}));
        let s = encoder_settings("obs_nvenc_hevc_tex", &p, true);
        assert_eq!(s["rate_control"], "VBR");
        assert_eq!((s["bitrate"].clone(), s["max_bitrate"].clone()), (json!(50000), json!(50000))); // max never below target
        assert_eq!((s["preset"].clone(), s["bf"].clone(), s["keyint_sec"].clone(), s["profile"].clone()), (json!("p7"), json!(0), json!(1), json!("main10")));
        assert_eq!(s["preset2"], "p7");
        let q = encoder_settings("obs_qsv11_v2", &prefs(json!({"quality": "custom", "adv_cq": 22, "adv_preset": 2})), false);
        assert_eq!((q["qpi"].clone(), q["target_usage"].clone()), (json!(22), json!("TU6")));
    }

    #[test]
    fn encoder_choice_falls_back() {
        let p = prefs(json!({"quality": "custom", "adv_codec": "av1"}));
        assert_eq!(pick_encoder(&p, Vendor::Nvidia, None), "obs_nvenc_av1_tex");
        let only_h264 = vec!["obs_nvenc_h264_tex".to_string(), "obs_x264".to_string()];
        assert_eq!(pick_encoder(&p, Vendor::Nvidia, Some(&only_h264)), "obs_nvenc_h264_tex");
        assert_eq!(pick_encoder(&prefs(json!({"quality": "high", "adv_codec": "av1"})), Vendor::Amd, None), "h264_texture_amf");
        assert_eq!(vendor_of(&["AMD Radeon(TM) Graphics".into(), "NVIDIA GeForce RTX 5080".into()]), Vendor::Nvidia);
    }

    #[test]
    fn output_scaling() {
        assert_eq!(output_size((2560, 1440), 1080), (1920, 1080));
        assert_eq!(output_size((1920, 1080), 1440), (1920, 1080)); // no upscaling
        assert_eq!(output_size((1080, 1920), 720), (720, 1280));   // portrait monitor
        assert_eq!(output_size((3440, 1440), 0), (3440, 1440));
    }
}
