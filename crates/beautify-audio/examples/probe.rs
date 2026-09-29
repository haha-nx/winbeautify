//! Ad-hoc probe: prove the private `IPolicyConfig` path really switches the
//! default endpoint on this machine, and restores it afterwards.
//!
//! Everything goes to stderr, which is unbuffered: a crash loses buffered
//! stdout, and the whole point of this probe is to find out *where* it dies.
//!
//! Run with `cargo run -p beautify-audio --example probe`.

use beautify_audio::{device, policy, switch, Flow, SwitchMode};

macro_rules! say {
    ($($arg:tt)*) => { eprintln!($($arg)*) };
}

fn main() {
    say!("step 1: policy::available()");
    let available = policy::available();
    say!("  available: {available}");

    say!("step 2: enumerate render endpoints");
    let devices = device::list(Flow::Render).expect("enumeration");
    say!("  {} endpoints", devices.len());
    for d in &devices {
        say!(
            "  {}{}  [{}]",
            if d.is_default { "* " } else { "  " },
            d.name,
            d.kind.label()
        );
    }

    let Some(original) = devices.iter().find(|d| d.is_default).map(|d| d.id.clone()) else {
        say!("no default; stopping");
        return;
    };
    say!("  default: {original}");

    let Some(other) = devices.iter().find(|d| d.id != original) else {
        say!("only one endpoint; stopping");
        return;
    };

    say!("step 3: set_default_for -> {}", other.name);
    match policy::set_default_for(Flow::Render, &other.id) {
        Ok(()) => say!("  Ok"),
        Err(e) => {
            say!("  FAILED: {e}");
            return;
        }
    }

    say!("step 4: read the default back");
    let after = device::default_endpoint_id(Flow::Render);
    say!(
        "  now {after:?} -- honoured: {}",
        after.as_deref() == Some(other.id.as_str())
    );

    say!("step 5: cycle through switch::switch three times");
    let selected = vec![original.clone(), other.id.clone()];
    for step in 0..3 {
        match switch::switch(SwitchMode::Speakers, &selected, &[]) {
            Ok(outcomes) => {
                for o in &outcomes {
                    say!(
                        "  click {}: {} -> {}",
                        step + 1,
                        o.previous.as_deref().unwrap_or("?"),
                        o.name
                    );
                }
            }
            Err(e) => {
                say!("  click {}: {}", step + 1, e.message());
                break;
            }
        }
    }

    say!("step 6: restore {original}");
    match policy::set_default_for(Flow::Render, &original) {
        Ok(()) => say!("  restored"),
        Err(e) => say!("  COULD NOT RESTORE: {e}"),
    }
    say!(
        "final default: {:?}",
        device::default_endpoint_id(Flow::Render)
    );
}