//! Settings the plugin declares to the host (`initialize` → `settings`),
//! and the values the host sends back (`initialize` params, then
//! `settings.changed`).

use serde_json::{json, Map, Value};

/// Best format to ask Deezer for; the subscription and the output may lower it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Quality {
    Mp3_128,
    Mp3_320,
    Flac,
}

impl Quality {
    /// Value in the settings.
    pub fn key(self) -> &'static str {
        match self {
            Quality::Flac => "flac",
            Quality::Mp3_320 => "mp3_320",
            Quality::Mp3_128 => "mp3_128",
        }
    }

    /// Format name of the media service.
    pub fn format(self) -> &'static str {
        match self {
            Quality::Flac => "FLAC",
            Quality::Mp3_320 => "MP3_320",
            Quality::Mp3_128 => "MP3_128",
        }
    }

    pub fn from_format(name: &str) -> Option<Self> {
        [Quality::Flac, Quality::Mp3_320, Quality::Mp3_128].into_iter().find(|q| q.format() == name)
    }

    fn from_key(key: &str) -> Option<Self> {
        [Quality::Flac, Quality::Mp3_320, Quality::Mp3_128].into_iter().find(|q| q.key() == key)
    }
}

/// Values in effect.
#[derive(Debug, Clone, PartialEq)]
pub struct Settings {
    /// Best format asked for.
    pub quality: Quality,
    /// Tell Deezer which tracks are played (`log.listen`), as its web
    /// player does.
    pub report_playback: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings { quality: Quality::Flac, report_playback: true }
    }
}

impl Settings {
    /// Values from the host's `{key: value}` map. Unknown keys and values of
    /// the wrong type are ignored; missing keys keep their default.
    pub fn from_values(values: Option<&Value>) -> Self {
        let mut s = Settings::default();
        let Some(values) = values.and_then(Value::as_object) else { return s };
        let get = |values: &Map<String, Value>, key: &str| values.get(key).cloned();
        if let Some(q) = get(values, "quality").as_ref().and_then(Value::as_str).and_then(Quality::from_key) {
            s.quality = q;
        }
        if let Some(on) = get(values, "report_playback").as_ref().and_then(Value::as_bool) {
            s.report_playback = on;
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
            "key": "quality", "type": "choice", "default": d.quality.key(),
            "section": t("Playback", "Lecture"),
            "label": t("Best quality", "Qualité maximale"),
            "description": t(
                "The best format asked for. FLAC (CD quality, 16 bits / 44.1 kHz) needs a subscription that \
                 includes it; otherwise, or when the output cannot play it as is, MP3 is used.",
                "Le meilleur format demandé. Le FLAC (qualité CD, 16 bits / 44,1 kHz) demande un abonnement \
                 qui le comprend ; sinon, ou si la sortie ne peut pas le lire tel quel, le MP3 est utilisé."
            ),
            "options": [
                {"value": "flac", "label": t("FLAC (CD quality)", "FLAC (qualité CD)")},
                {"value": "mp3_320", "label": "MP3 320 kbps"},
                {"value": "mp3_128", "label": "MP3 128 kbps"},
            ],
        },
        {
            "key": "report_playback", "type": "bool", "default": d.report_playback,
            "section": t("Playback", "Lecture"),
            "label": t("Report plays to Deezer", "Signaler les écoutes à Deezer"),
            "description": t(
                "Tell Deezer which tracks you play and for how long, as its web player does. Plays then \
                 count for the artists, your history and your recommendations (Flow).",
                "Indique à Deezer les titres écoutés et leur durée d'écoute, comme le fait son lecteur \
                 web. Les écoutes comptent alors pour les artistes, votre historique et vos \
                 recommandations (Flow)."
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
        assert_eq!(Settings::default(), Settings { quality: Quality::Flac, report_playback: true });
        let s = Settings::from_values(Some(&json!({"report_playback": false, "quality": "mp3_320", "gone": 3})));
        assert_eq!(s, Settings { quality: Quality::Mp3_320, report_playback: false });
        let s = Settings::from_values(Some(&json!({"report_playback": "no", "quality": "hires"})));
        assert_eq!(s, Settings::default(), "values of the wrong type or unknown are ignored");
    }

    #[test]
    fn declaration_is_translated_and_complete() {
        let fr = declaration("fr");
        let en = declaration("de");
        assert_eq!(fr[1]["label"], "Signaler les écoutes à Deezer");
        assert_eq!(en[1]["label"], "Report plays to Deezer");
        assert_eq!(fr[0]["default"], "flac");
        let values: Vec<&str> = fr[0]["options"].as_array().unwrap().iter().map(|o| o["value"].as_str().unwrap()).collect();
        assert_eq!(values, ["flac", "mp3_320", "mp3_128"]);
        for entry in fr.as_array().unwrap() {
            assert!(entry["description"].is_string());
            assert!(entry.get("restart").is_none(), "applied without restarting the plugin");
        }
        assert!(Quality::Flac > Quality::Mp3_320 && Quality::Mp3_320 > Quality::Mp3_128);
        assert_eq!(Quality::from_format("MP3_320"), Some(Quality::Mp3_320));
    }
}
