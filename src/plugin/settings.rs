//! Settings the plugin declares to the host (`initialize` → `settings`),
//! and the values the host sends back (`initialize` params, then
//! `settings.changed`).

use serde_json::{Map, Value, json};

/// Ogg Vorbis bitrate asked for first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Quality {
    /// 320 kbps (Premium), then 160, then 96.
    High,
    /// 160 kbps, then 96, then 320.
    Normal,
}

impl Quality {
    pub fn as_str(self) -> &'static str {
        match self {
            Quality::High => "high",
            Quality::Normal => "normal",
        }
    }
}

/// Values in effect.
#[derive(Debug, Clone, PartialEq)]
pub struct Settings {
    pub quality: Quality,
    /// Tell Spotify what the player plays (see `report.rs`).
    pub report_playback: bool,
    /// Show the player as a Spotify Connect device.
    pub connect: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings { quality: Quality::High, report_playback: true, connect: true }
    }
}

impl Settings {
    /// Values from the host's `{key: value}` map. Unknown keys and values of
    /// the wrong type are ignored; missing keys keep their default.
    pub fn from_values(values: Option<&Value>) -> Self {
        let mut s = Settings::default();
        let Some(values) = values.and_then(Value::as_object) else { return s };
        let flag = |values: &Map<String, Value>, key: &str| values.get(key).and_then(Value::as_bool);
        match values.get("quality").and_then(Value::as_str) {
            Some("high") => s.quality = Quality::High,
            Some("normal") => s.quality = Quality::Normal,
            _ => {}
        }
        if let Some(on) = flag(values, "report_playback") {
            s.report_playback = on;
        }
        if let Some(on) = flag(values, "connect") {
            s.connect = on;
        }
        s
    }
}

/// The declaration, labelled in `lang` (French, else English).
pub fn declaration(lang: &str) -> Value {
    let fr = lang == "fr";
    let t = |en: &'static str, fr_text: &'static str| if fr { fr_text } else { en };
    let d = Settings::default();
    json!([
        {
            "key": "quality", "type": "choice", "default": d.quality.as_str(),
            "section": t("Playback", "Lecture"),
            "label": t("Streaming quality", "Qualité de lecture"),
            "description": t(
                "Ogg Vorbis, as Spotify serves it, handed to the player unchanged. High needs \
                 Premium; a track without the asked bitrate plays in the closest one. Applies from \
                 the next track.",
                "Ogg Vorbis, tel que Spotify le sert, transmis au lecteur sans modification. Haute \
                 nécessite Premium ; un titre sans le débit demandé est lu au débit le plus proche. \
                 S'applique à partir du titre suivant."
            ),
            "options": [
                {"value": "high", "label": t("High (320 kbps)", "Haute (320 kbit/s)")},
                {"value": "normal", "label": t("Normal (160 kbps)", "Normale (160 kbit/s)")},
            ],
        },
        {
            "key": "report_playback", "type": "bool", "default": d.report_playback,
            "section": t("Playback", "Lecture"),
            "label": t("Report plays to Spotify", "Signaler les écoutes à Spotify"),
            "description": t(
                "Tell Spotify which tracks you play, as its own apps do: the player shows up as \
                 the device playing them, and the plays are reported when they end.",
                "Indique à Spotify les titres écoutés, comme le font ses applications : le lecteur \
                 apparaît comme l'appareil qui les joue, et les écoutes sont signalées à leur fin."
            ),
        },
        {
            "key": "connect", "type": "bool", "default": d.connect,
            "section": "Spotify Connect",
            "label": t("Show the player in Spotify Connect", "Afficher le lecteur dans Spotify Connect"),
            "description": t(
                "The player appears as a device in the Spotify apps (phone, desktop) and can be \
                 controlled from them.",
                "Le lecteur apparaît comme un appareil dans les applications Spotify (téléphone, \
                 ordinateur) et peut être piloté depuis celles-ci."
            ),
        },
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_fall_back_to_defaults() {
        assert_eq!(Settings::from_values(None), Settings::default());
        let d = Settings::default();
        assert!(d.quality == Quality::High && d.report_playback && d.connect);
        let s = Settings::from_values(Some(&json!({"quality": "normal", "report_playback": false, "connect": false, "gone": 3})));
        assert_eq!(s, Settings { quality: Quality::Normal, report_playback: false, connect: false });
        let s = Settings::from_values(Some(&json!({"report_playback": "no", "quality": "lossless"})));
        assert!(s.report_playback && s.quality == Quality::High, "a value of the wrong type is ignored");
    }

    #[test]
    fn declaration_is_translated_and_complete() {
        let fr = declaration("fr");
        let en = declaration("de");
        assert_eq!(fr[1]["label"], "Signaler les écoutes à Spotify");
        assert_eq!(en[1]["label"], "Report plays to Spotify");
        let entries = en.as_array().unwrap();
        assert_eq!(entries.len(), 3);
        for entry in entries {
            assert!(entry["description"].is_string());
            assert!(entry.get("restart").is_none(), "applied without restarting the plugin");
        }
        assert_eq!(en[0]["default"], "high");
        assert_eq!(en[0]["options"][1]["value"], "normal");
        assert_eq!((en[1]["default"].as_bool(), en[2]["default"].as_bool()), (Some(true), Some(true)));
    }
}
