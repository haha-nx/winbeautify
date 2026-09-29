//! Reading and writing a config field by its dotted path.
//!
//! # Why this goes through TOML
//!
//! The obvious implementation is a `match path { "widget.opacity" => ... }` with
//! an arm per field, which for this page means well over a hundred arms that
//! have to stay in step with [`crate::schema`]. Instead the config is converted
//! to its own serialised shape and the path walks that. There is no per-field
//! code to get wrong, and the paths are guaranteed to be the ones the config
//! file already uses.
//!
//! A path that does not exist returns `None` rather than panicking, and the test
//! suite drives every path in the schema through here — so a typo fails the
//! build's tests instead of silently doing nothing when a slider is dragged.

use beautify_core::config::Config;
use serde::{Deserialize, Serialize};

/// A value as the settings UI handles it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Value {
    Bool(bool),
    Integer(i64),
    Float(f64),
    Text(String),
    /// An ordered list of strings — the ticked audio devices, where the order
    /// *is* the setting (it is the order a double-click walks).
    List(Vec<String>),
}

impl Value {
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Value::Bool(value) => Some(*value),
            _ => None,
        }
    }

    pub fn as_number(&self) -> Option<f64> {
        match self {
            Value::Integer(value) => Some(*value as f64),
            Value::Float(value) => Some(*value),
            _ => None,
        }
    }

    pub fn as_text(&self) -> Option<&str> {
        match self {
            Value::Text(value) => Some(value),
            _ => None,
        }
    }

    pub fn as_list(&self) -> Option<&[String]> {
        match self {
            Value::List(value) => Some(value),
            _ => None,
        }
    }
}

/// Convert the config to its serialised shape.
fn to_table(config: &Config) -> Option<toml::Value> {
    toml::Value::try_from(config).ok()
}

/// The leaf at `path`, or `None` when the path does not resolve.
fn leaf<'a>(root: &'a toml::Value, path: &str) -> Option<&'a toml::Value> {
    let mut current = root;
    for part in path.split('.') {
        current = current.get(part)?;
    }
    Some(current)
}

fn to_value(raw: &toml::Value) -> Option<Value> {
    match raw {
        toml::Value::Boolean(value) => Some(Value::Bool(*value)),
        toml::Value::Integer(value) => Some(Value::Integer(*value)),
        toml::Value::Float(value) => Some(Value::Float(*value)),
        toml::Value::String(value) => Some(Value::Text(value.clone())),
        // A list of strings, which is what the device selections are. A mixed
        // or non-string array is refused rather than coerced: nothing in the
        // schema declares one, so it would be a bug rather than a value.
        toml::Value::Array(items) => {
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                out.push(item.as_str()?.to_string());
            }
            Some(Value::List(out))
        }
        _ => None,
    }
}

/// Read a field.
///
/// `None` when the path is unknown, or when it names a group rather than a
/// leaf.
pub fn read(config: &Config, path: &str) -> Option<Value> {
    let root = to_table(config)?;
    to_value(leaf(&root, path)?)
}

/// Write a field, returning whether the path existed.
///
/// The value is coerced to the type the field already holds: writing a float
/// into an integer field stores an integer, and writing a number into a string
/// field stores its decimal form. That keeps a slider (which naturally produces
/// floats) from turning an `i32` field into a float in the config file.
///
/// The caller is responsible for clamping; [`Config::clamp`] runs when the
/// update reaches the config manager.
pub fn write(config: &mut Config, path: &str, value: Value) -> bool {
    let Some(mut root) = to_table(config) else {
        return false;
    };

    let parts: Vec<&str> = path.split('.').collect();
    let Some((last, parents)) = parts.split_last() else {
        return false;
    };

    let mut current = &mut root;
    for part in parents {
        match current.get_mut(*part) {
            Some(next) => current = next,
            None => return false,
        }
    }
    let Some(table) = current.as_table_mut() else {
        return false;
    };
    let Some(existing) = table.get(*last) else {
        return false;
    };

    let coerced = match (existing, value) {
        (toml::Value::Boolean(_), Value::Bool(value)) => toml::Value::Boolean(value),
        (toml::Value::Integer(_), Value::Integer(value)) => toml::Value::Integer(value),
        (toml::Value::Integer(_), Value::Float(value)) => toml::Value::Integer(value.round() as i64),
        (toml::Value::Float(_), Value::Float(value)) => toml::Value::Float(value),
        (toml::Value::Float(_), Value::Integer(value)) => toml::Value::Float(value as f64),
        (toml::Value::String(_), Value::Text(value)) => toml::Value::String(value),
        (toml::Value::String(_), Value::Integer(value)) => toml::Value::String(value.to_string()),
        (toml::Value::String(_), Value::Float(value)) => toml::Value::String(value.to_string()),
        // A string list replaces a string list. The whole list is written at
        // once — the device rows are reordered by replacing the array, not by
        // mutating an element — so there is no per-element coercion to do.
        (toml::Value::Array(_), Value::List(items)) => {
            toml::Value::Array(items.into_iter().map(toml::Value::String).collect())
        }
        // A mismatch the UI should not produce; refuse rather than corrupt the
        // field's type.
        _ => return false,
    };
    table.insert((*last).to_string(), coerced);

    match root.try_into::<Config>() {
        Ok(updated) => {
            *config = updated;
            true
        }
        Err(_) => false,
    }
}

/// Flip a switch, returning its new state.
pub fn toggle(config: &mut Config, path: &str) -> Option<bool> {
    let current = read(config, path)?.as_bool()?;
    write(config, path, Value::Bool(!current)).then_some(!current)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{self, Kind, SECTIONS};

    /// Every editable field, as `(path, kind)`.
    fn editable_fields() -> Vec<(&'static str, Kind)> {
        let config = Config::default();
        let mut out = Vec::new();
        for section in SECTIONS {
            // Visibility is irrelevant here: walk the raw cards so a hidden row
            // is still checked.
            for card in section.cards {
                for field in card.fields {
                    if field.kind.holds_value() {
                        out.push((field.path, field.kind));
                    }
                }
            }
        }
        let _ = config;
        out
    }

    #[test]
    fn every_field_reads_a_value_of_its_own_type() {
        let config = Config::default();
        for (path, kind) in editable_fields() {
            let value = read(&config, path).unwrap_or_else(|| panic!("{path} does not resolve"));
            let matches = match kind {
                Kind::Switch | Kind::Number { .. } => {
                    // Numbers are integers in the config, switches are bools.
                    matches!(value, Value::Bool(_) | Value::Integer(_))
                }
                Kind::Slider(_) => matches!(value, Value::Float(_) | Value::Integer(_)),
                Kind::Select(_) | Kind::Color | Kind::Text { .. } | Kind::Hotkey => {
                    matches!(value, Value::Text(_))
                }
                Kind::CheckboxList(_) => matches!(value, Value::List(_)),
                Kind::Status(_) | Kind::Info(_) | Kind::Action(_) => false,
            };
            assert!(matches, "{path} read back as {value:?}, which its control cannot use");
        }
    }

    /// The test that makes the whole path-based scheme safe: touch every field
    /// the way its control would, and check the value survives the trip.
    #[test]
    fn every_field_round_trips_through_its_control() {
        for (path, kind) in editable_fields() {
            let mut config = Config::default();

            match kind {
                Kind::Switch => {
                    let before = read(&config, path).unwrap().as_bool().unwrap();
                    assert!(
                        write(&mut config, path, Value::Bool(!before)),
                        "{path} refused a write"
                    );
                    assert_eq!(
                        read(&config, path).unwrap().as_bool().unwrap(),
                        !before,
                        "{path} did not keep the value"
                    );
                }
                Kind::Slider(slider) => {
                    // A slider always produces floats; the field may be an int.
                    let target = slider.max;
                    assert!(write(&mut config, path, Value::Float(target)), "{path} refused");
                    let stored = read(&config, path).unwrap().as_number().unwrap();
                    assert!(
                        (stored - target).abs() < 1e-6,
                        "{path} stored {stored}, wanted {target}"
                    );
                }
                Kind::Number { min, max, .. } => {
                    assert!(write(&mut config, path, Value::Integer(max)), "{path} refused");
                    assert_eq!(read(&config, path).unwrap().as_number().unwrap(), max as f64);
                    assert!(write(&mut config, path, Value::Integer(min)), "{path} refused");
                }
                Kind::Color => {
                    assert!(
                        write(&mut config, path, Value::Text("#123456".into())),
                        "{path} refused"
                    );
                    assert_eq!(read(&config, path).unwrap().as_text().unwrap(), "#123456");
                }
                Kind::Text { .. } | Kind::Hotkey => {
                    assert!(
                        write(&mut config, path, Value::Text("Ctrl+Alt+Q".into())),
                        "{path} refused"
                    );
                    assert_eq!(read(&config, path).unwrap().as_text().unwrap(), "Ctrl+Alt+Q");
                }
                Kind::Select(choices) => {
                    // Every option has to be storable, or picking it would snap
                    // back to the first entry.
                    for choice in choices {
                        assert!(
                            write(&mut config, path, Value::Text(choice.value.into())),
                            "{path} refused the option {}",
                            choice.value
                        );
                        assert_eq!(
                            read(&config, path).unwrap().as_text().unwrap(),
                            choice.value,
                            "{path} did not keep the option {}",
                            choice.value
                        );
                    }
                }
                Kind::CheckboxList(_) => {
                    // Exercised in full by `a_checkbox_list_round_trips_in_order`;
                    // here it only has to accept a write of its own shape.
                    let wanted: Vec<String> = vec!["x".into(), "y".into()];
                    assert!(
                        write(&mut config, path, Value::List(wanted.clone())),
                        "{path} refused a list write"
                    );
                    assert_eq!(read(&config, path).unwrap().as_list().unwrap(), wanted);
                }
                Kind::Status(_) | Kind::Info(_) | Kind::Action(_) => unreachable!("filtered out"),
            }
        }
    }

    /// The device selections are the one list-valued control: reading one must
    /// give a list, writing one must store exactly what was written *in order*,
    /// and the order is the setting — a `Vec` that came back sorted would
    /// silently change which device a double-click reaches next.
    #[test]
    fn a_checkbox_list_round_trips_in_order() {
        for kind in crate::devices::DeviceListKind::ALL {
            let path = kind.path();
            let mut config = Config::default();

            let initial = read(&config, path).expect("the list must resolve");
            assert!(
                initial.as_list().is_some_and(|list| list.is_empty()),
                "{path} should start empty, got {initial:?}"
            );

            let wanted: Vec<String> = vec!["c".into(), "a".into(), "b".into()];
            assert!(
                write(&mut config, path, Value::List(wanted.clone())),
                "{path} refused a list write"
            );
            let stored = read(&config, path).unwrap();
            assert_eq!(
                stored.as_list().unwrap(),
                wanted.as_slice(),
                "{path} must keep the order it was given"
            );

            // Emptying it again is a legitimate state: the user unticked
            // everything.
            assert!(write(&mut config, path, Value::List(Vec::new())));
            assert!(read(&config, path).unwrap().as_list().unwrap().is_empty());
        }
    }

    /// The two device lists must be independent, or ticking a speaker would
    /// tick a microphone.
    #[test]
    fn the_two_device_lists_do_not_share_storage() {
        use crate::devices::DeviceListKind;

        let mut config = Config::default();
        write(
            &mut config,
            DeviceListKind::Speakers.path(),
            Value::List(vec!["s".into()]),
        );
        assert_eq!(config.audio_switch.speakers, ["s".to_string()]);
        assert!(
            config.audio_switch.microphones.is_empty(),
            "writing the speaker list must not touch the microphone list"
        );
    }

    /// A list field must refuse a scalar rather than turn it into a one-element
    /// list, which would look like a successful tick.
    #[test]
    fn a_list_field_refuses_a_scalar() {
        use crate::devices::DeviceListKind;

        let mut config = Config::default();
        assert!(!write(
            &mut config,
            DeviceListKind::Speakers.path(),
            Value::Text("not a list".into())
        ));
        assert!(config.audio_switch.speakers.is_empty());
    }

    /// Writing a value the field cannot hold must be reported, not swallowed.
    #[test]
    fn a_type_mismatch_is_refused() {
        let mut config = Config::default();
        let before = config.widget.opacity;
        assert!(!write(&mut config, "widget.opacity", Value::Text("nope".into())));
        // …and the config is untouched.
        assert_eq!(config.widget.opacity, before);
    }

    #[test]
    fn an_unknown_path_is_reported_not_panicked_on() {
        let mut config = Config::default();
        assert!(read(&config, "nope.not.here").is_none());
        assert!(read(&config, "widget").is_none(), "a group is not a field");
        assert!(!write(&mut config, "nope.not.here", Value::Bool(true)));
        assert!(read(&config, "widget.opacity").is_some());
    }

    #[test]
    fn an_integer_field_stays_an_integer() {
        let mut config = Config::default();
        write(&mut config, "widget.offset_x", Value::Float(12.0));
        let root = to_table(&config).unwrap();
        assert!(
            matches!(leaf(&root, "widget.offset_x"), Some(toml::Value::Integer(12))),
            "a slider feeding an integer field must not turn it into a float"
        );
    }

    #[test]
    fn toggling_a_switch_reports_the_new_state() {
        let mut config = Config::default();
        let before = config.clipboard.enabled;
        assert_eq!(toggle(&mut config, "clipboard.enabled"), Some(!before));
        assert_eq!(config.clipboard.enabled, !before);
        assert_eq!(toggle(&mut config, "nope"), None);
    }

    #[test]
    fn the_schema_helpers_agree_with_the_config_enums() {
        use beautify_core::config::{SpectrumStyle, TaskbarMode, Theme, WidgetAnchor, WidgetColorMode};
        assert_eq!(schema::mode_from_id("acrylic"), TaskbarMode::Acrylic);
        assert_eq!(schema::mode_from_id("opaque").id(), "opaque");
        // Mica is gone from the page but must still resolve, and to something
        // that keeps the effect the user chose rather than to "no effect".
        assert_eq!(schema::mode_from_id("mica"), TaskbarMode::Acrylic);
        assert_eq!(schema::theme_from_id("auto"), Theme::Auto);
        assert_eq!(
            schema::anchor_from_id("bottom-center"),
            WidgetAnchor::BottomCenter
        );
        assert_eq!(
            schema::anchor_from_id("taskbar-right"),
            WidgetAnchor::TaskbarRight
        );
        assert_eq!(
            schema::color_mode_from_id("custom"),
            WidgetColorMode::Custom
        );
        assert_eq!(schema::color_mode_from_id("theme"), WidgetColorMode::Theme);
        assert_eq!(
            schema::spectrum_style_from_id("bounce"),
            SpectrumStyle::Bounce
        );
        assert_eq!(schema::spectrum_style_from_id("bars"), SpectrumStyle::Bars);
    }
}
