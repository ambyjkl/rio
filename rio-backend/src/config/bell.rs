use crate::config::defaults::default_bool_true;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Bell {
    #[serde(default = "default_audio_bell")]
    pub audio: bool,
    /// Mark background tabs that rang the bell with a dot in the tab strip.
    #[serde(default = "default_bool_true", rename = "tab-indicator")]
    pub tab_indicator: bool,
}

impl Default for Bell {
    fn default() -> Self {
        Bell {
            audio: default_audio_bell(),
            tab_indicator: default_bool_true(),
        }
    }
}

fn default_audio_bell() -> bool {
    // These platforms use the system sound.
    #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
    {
        true
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
    {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::Bell;

    #[test]
    fn audio_default_and_explicit_false() {
        let default: Bell = toml::from_str("").unwrap();
        assert_eq!(default.audio, Bell::default().audio);
        #[cfg(target_os = "linux")]
        assert!(default.audio);
        let disabled: Bell = toml::from_str("audio = false").unwrap();
        assert!(!disabled.audio);
    }
}
